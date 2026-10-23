use std::{
  cell::Cell,
  env, fmt, io, mem,
  path::{Path, PathBuf},
  sync::{
    Arc, Mutex, PoisonError,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc::{self, Receiver, Sender, SyncSender},
  },
  thread::{self, JoinHandle},
  time::Instant,
};

use crate::{
  backend::{Backend, Signal, Sink, is_watch_limit, poll::PollBackend},
  config::Config,
  engine::{Engine, Options, Output},
  error::{Error, ErrorKind},
  event::Event,
  fs::StdFs,
  path::normalize,
};

/// The most bytes of backend signals that wait in the queue. Past it a signal is dropped, and the
/// engine checks everything. Bytes, because a signal can be a few bytes or a whole OS buffer.
const QUEUE_BYTES: usize = 16 << 20;
/// The most messages taken in one turn of the loop, so that a flood of signals cannot hold back
/// a flush.
const MESSAGES_PER_TURN: usize = 512;

thread_local! {
  /// The number of the watcher whose thread this is, 0 on any other thread. A `Cell` has no
  /// destructor, so it can still be read while the thread exits, where `thread::current` panics.
  // `allow`, not `expect`: on Android the lint takes the `const` for a plain initializer, but
  // only where it can read the source of std.
  #[cfg_attr(
    target_os = "android",
    allow(clippy::missing_const_for_thread_local, reason = "the initializer is `const`")
  )]
  static WATCHER_THREAD: Cell<usize> = const { Cell::new(0) };
}

/// The number of the next watcher, from 1.
static NEXT_WATCHER: AtomicUsize = AtomicUsize::new(1);

/// Watches paths and reports file changes: create, update, delete.
///
/// A `Watcher` is `Send + Sync`: put it in an `Arc` to use it from several threads. Dropping
/// it calls [`close`](Self::close).
///
/// ```no_run
/// # fn main() -> Result<(), pathwatch::Error> {
/// let watcher = pathwatch::Watcher::new(|result| println!("{result:?}"))?;
/// watcher.watch(&["src", "package.json", "not-there-yet.json"])?;
/// watcher.unwatch(&["src"])?;
/// watcher.close();
/// # Ok(())
/// # }
/// ```
pub struct Watcher {
  /// The number of its thread in `WATCHER_THREAD`.
  number: usize,
  /// Sends a command to the thread; `false` when the thread is gone. A closure, since the
  /// message type depends on the backend.
  send: Box<dyn Fn(Command) -> bool + Send + Sync>,
  /// The commands from inside the handler, taken when the handler returns.
  inside: Sender<Command>,
  stop: Arc<AtomicBool>,
  /// Taken by `close` to wait for the thread.
  thread: Mutex<Option<JoinHandle<()>>>,
}

impl Watcher {
  /// Starts a watcher with the default [`Config`]. See [`with_config`](Self::with_config).
  ///
  /// # Errors
  ///
  /// The watcher thread, or the queue into it, could not be made.
  pub fn new<H>(handler: H) -> Result<Self, Error>
  where
    H: FnMut(Result<Vec<Event>, Error>) + Send + 'static,
  {
    Self::with_config(Config::default(), handler)
  }

  /// Starts the watcher thread. Nothing is watched yet.
  ///
  /// `handler` runs on that thread and gets `Ok(events)`, one batch (never empty, at most one
  /// event for a path, in no set order), or `Err(error)`, after which the watcher keeps running.
  /// It should only pass the batch on: a slow handler delays every check of the disk and every
  /// call. A `watch`, `unwatch` or `close` on another thread waits for the handler, so the
  /// handler must never wait for such a caller.
  ///
  /// A handler or an ignore filter that panics ends the thread: nothing is reported after that,
  /// and `watch` and `unwatch` return [`ErrorKind::Closed`].
  ///
  /// When the watch API of the OS does not start, or stops later, the watcher polls everything
  /// and tells the handler once ([`ErrorKind::WatchLimit`] or [`ErrorKind::Io`]).
  ///
  /// # Errors
  ///
  /// The watcher thread, or the queue into it, could not be made.
  pub fn with_config<H>(config: Config, handler: H) -> Result<Self, Error>
  where
    H: FnMut(Result<Vec<Event>, Error>) + Send + 'static,
  {
    // The one place that says which target has which native backend. Any other target polls.
    #[cfg(target_os = "macos")]
    use crate::backend::fsevents::FsEventsBackend as Native;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    use crate::backend::inotify::InotifyBackend as Native;
    #[cfg(windows)]
    use crate::backend::windows::WindowsBackend as Native;

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", windows))]
    if !config.polling() {
      let poll = config.clone().into_options(true);
      return Self::start(config.into_options(false), poll, handler, Native::new);
    }
    let poll = config.into_options(true);
    Self::start(poll.clone(), poll, handler, |_| Ok(PollBackend))
  }

  /// Watches each path: a file, a directory with everything below it, or a path that does not
  /// exist yet. Returns when every path is watched: what exists then is not reported, every
  /// later change is. Watching a path again does nothing. Inside the handler it does not wait:
  /// the paths are watched when the handler returns.
  ///
  /// A relative path is joined to the current directory, and `.`, `..` and repeated separators
  /// are cleaned by text; no link is resolved and no letter case is changed (README, promise 8).
  /// Every event path starts with the cleaned path.
  ///
  /// # Errors
  ///
  /// - [`ErrorKind::InvalidPath`]: a path cannot be used. The other paths are still watched, and
  ///   the error names the first one skipped.
  /// - [`ErrorKind::Closed`]: the watcher is closed.
  ///
  /// A path that the OS cannot watch or read is no error here: the handler gets it.
  pub fn watch(&self, paths: &[impl AsRef<Path>]) -> Result<(), Error> {
    self.command(paths, Command::Watch)
  }

  /// Takes back a [`watch`](Self::watch) of these very paths; a path that is not watched is
  /// skipped. Nothing is reported for them: a file that only they covered is forgotten, and a
  /// file that another watched path covers stays known. One exception is reported: a link inside
  /// a watched directory that the path went through is a plain file of that directory again
  /// (README, promise 12). To cut a part out of a watched directory, use
  /// [`Config::with_ignored`]. Inside the handler it does not wait.
  ///
  /// # Errors
  ///
  /// - [`ErrorKind::InvalidPath`]: a path cannot be used. The other paths are still unwatched,
  ///   and the error names the first one skipped.
  /// - [`ErrorKind::Closed`]: the watcher is closed.
  pub fn unwatch(&self, paths: &[impl AsRef<Path>]) -> Result<(), Error> {
    self.command(paths, Command::Unwatch)
  }

  /// Stops the watcher and waits for its thread. After it the handler is not called again,
  /// nothing is watched, and `watch` and `unwatch` return [`ErrorKind::Closed`]. Events not
  /// handed over yet are dropped, and a running check of the disk ends at its next step. It can
  /// be called many times. Inside the handler it does not wait: the thread ends when the handler
  /// returns. While the process exits on Windows, the OS has ended the thread already, and
  /// nothing is waited for.
  pub fn close(&self) {
    self.stop.store(true, Ordering::Release);
    // While the process exits, Windows has ended every other thread, and one of them can hold
    // the lock of the queue, and `join` would panic: nothing is sent, and nothing is waited for.
    #[cfg(windows)]
    if process_is_exiting() {
      return;
    }
    // Wakes the thread up. It fails only when the thread is gone.
    let _ = (self.send)(Command::Stop);
    if self.on_its_thread() {
      return;
    }
    // Held while waiting: a `close` on another thread returns only when the thread is gone too.
    let mut thread = self.thread.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(thread) = thread.take() {
      // `Err` when a panic of the handler ended the thread.
      let _ = thread.join();
    }
  }

  /// [`with_config`](Self::with_config) with a backend of the caller's choice, for the tests.
  ///
  /// # Errors
  ///
  /// The watcher thread, or the queue into it, could not be made.
  #[doc(hidden)]
  #[cfg(feature = "test-hooks")]
  pub fn with_backend<B, H>(
    options: Options,
    handler: H,
    make: impl FnOnce(Sink<B::Raw>) -> io::Result<B> + Send + 'static,
  ) -> Result<Self, Error>
  where
    B: Backend,
    H: FnMut(Result<Vec<Event>, Error>) + Send + 'static,
  {
    Self::start(options.clone(), options, handler, make)
  }

  /// Starts the watcher thread, which makes the backend with `make`. When that fails, or the
  /// backend stops later, the handler is told once and the watcher polls with `fallback`.
  fn start<B, H>(
    options: Options,
    fallback: Options,
    mut handler: H,
    make: impl FnOnce(Sink<B::Raw>) -> io::Result<B> + Send + 'static,
  ) -> Result<Self, Error>
  where
    B: Backend,
    H: FnMut(Result<Vec<Event>, Error>) + Send + 'static,
  {
    let (post, inbox) = Inbox::<B::Raw>::new()
      .map_err(|error| Error::of_watcher("the queue of the watcher could not be made", error))?;
    let (inside, from_inside) = mpsc::channel();
    let queue = Arc::new(Queue::default());
    let stop = Arc::new(AtomicBool::new(false));
    let sink = sink::<B>(post.clone(), Arc::clone(&queue));

    let number = NEXT_WATCHER.fetch_add(1, Ordering::Relaxed);
    let thread = {
      let stop = Arc::clone(&stop);
      thread::Builder::new().name("pathwatch".to_owned()).spawn(move || {
        WATCHER_THREAD.set(number);
        // Made here: every method of the backend runs on this thread.
        let mut engine = match make(sink) {
          Ok(backend) => {
            let mut engine = Engine::new(backend, StdFs, options);
            engine.fall_back_to(fallback);
            engine
          }
          Err(error) => {
            // A limit says by itself what is missing and that the watcher polls.
            handler(Err(if is_watch_limit(&error) {
              Error::new(ErrorKind::WatchLimit, None, Some(error))
            } else {
              Error::of_watcher("the OS watcher did not start, so every path is polled", error)
            }));
            Engine::without_backend(StdFs, fallback)
          }
        };
        engine.stop_with(stop);
        run(engine, handler, &inbox, &from_inside, &queue);
      })
    }
    .map_err(|error| Error::of_watcher("the thread of the watcher did not start", error))?;

    Ok(Self {
      number,
      send: Box::new(move |command| post.send(Message::Command(command))),
      inside,
      stop,
      thread: Mutex::new(Some(thread)),
    })
  }

  /// Cleans the paths and sends the usable ones to the thread as one command.
  fn command(
    &self,
    paths: &[impl AsRef<Path>],
    command: fn(Vec<PathBuf>, Done) -> Command,
  ) -> Result<(), Error> {
    let closed = || Error::new(ErrorKind::Closed, None, None);
    if self.stop.load(Ordering::Acquire) {
      return Err(closed());
    }
    let mut usable = Vec::with_capacity(paths.len());
    let mut skipped = None;
    for path in paths {
      match clean(path.as_ref()) {
        Ok(path) => usable.push(path),
        Err(why) => skipped = skipped.or_else(|| Some((path.as_ref().to_path_buf(), why))),
      }
    }
    let sent = if self.on_its_thread() {
      // The thread cannot wait for itself: it takes the command when the handler returns.
      self.inside.send(command(usable, None)).is_ok()
    } else {
      let (done, wait) = mpsc::sync_channel(1);
      // `wait` fails when the thread stops before the command is done.
      (self.send)(command(usable, Some(done))) && wait.recv().is_ok()
    };
    if !sent {
      return Err(closed());
    }
    skipped.map_or(Ok(()), |(path, why)| Err(Error::new(ErrorKind::InvalidPath, Some(path), why)))
  }

  /// Whether this is the thread of the watcher: inside the handler, or the backend.
  fn on_its_thread(&self) -> bool {
    WATCHER_THREAD.get() == self.number
  }
}

impl Drop for Watcher {
  fn drop(&mut self) {
    self.close();
  }
}

impl fmt::Debug for Watcher {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Watcher").field("number", &self.number).finish_non_exhaustive()
  }
}

/// Whether the process is exiting: `ExitProcess` has ended every other thread.
#[cfg(windows)]
fn process_is_exiting() -> bool {
  #[link(name = "ntdll")]
  unsafe extern "system" {
    safe fn RtlDllShutdownInProgress() -> u8;
  }

  RtlDllShutdownInProgress() != 0
}

enum Message<R> {
  /// A signal of the backend, and the bytes it counts for in the queue.
  Signal(Signal<R>, usize),
  Command(Command),
  /// A signal was dropped. It wakes the thread up, which takes the overflow at its place in the
  /// queue.
  Wake,
}

enum Command {
  Watch(Vec<PathBuf>, Done),
  Unwatch(Vec<PathBuf>, Done),
  /// Only wakes the thread up. The stop flag says the rest.
  Stop,
}

/// Where the thread says that a command is done. `None` for a command from inside the handler,
/// for which nobody waits.
type Done = Option<SyncSender<()>>;

/// The sending end of the queue. On Windows it also wakes the thread up (`inbox`).
struct Post<R> {
  sender: Sender<Message<R>>,
  #[cfg(windows)]
  wake: Arc<inbox::Wake>,
}

impl<R> Post<R> {
  /// `false`: the watcher thread is gone.
  fn send(&self, message: Message<R>) -> bool {
    let sent = self.sender.send(message).is_ok();
    #[cfg(windows)]
    self.wake.set();
    sent
  }
}

impl<R> Clone for Post<R> {
  fn clone(&self) -> Self {
    Self {
      sender: self.sender.clone(),
      #[cfg(windows)]
      wake: Arc::clone(&self.wake),
    }
  }
}

/// The receiving end of the queue, on the watcher thread.
struct Inbox<R> {
  receiver: Receiver<Message<R>>,
  #[cfg(windows)]
  wake: Arc<inbox::Wake>,
  #[cfg(windows)]
  timer: inbox::Timer,
}

impl<R> Inbox<R> {
  #[cfg_attr(
    not(windows),
    expect(clippy::unnecessary_wraps, reason = "the event and the timer of Windows can fail")
  )]
  fn new() -> io::Result<(Post<R>, Self)> {
    let (sender, receiver) = mpsc::channel();
    #[cfg(windows)]
    {
      let wake = Arc::new(inbox::Wake::new()?);
      let post = Post { sender, wake: Arc::clone(&wake) };
      Ok((post, Self { receiver, wake, timer: inbox::Timer::new()? }))
    }
    #[cfg(not(windows))]
    Ok((Post { sender }, Self { receiver }))
  }

  /// The next message, or `None` when `deadline` comes first. `Stop` when every `Post` is gone:
  /// the watcher was dropped.
  #[cfg(windows)]
  fn take(&self, deadline: Option<Instant>) -> Option<Message<R>> {
    loop {
      match self.receiver.try_recv() {
        Ok(message) => return Some(message),
        Err(mpsc::TryRecvError::Disconnected) => return Some(Message::Command(Command::Stop)),
        Err(mpsc::TryRecvError::Empty) => {}
      }
      if !self.timer.wait(&self.wake, deadline) {
        return None;
      }
    }
  }

  /// The next message, or `None` when `deadline` comes first. `Stop` when every `Post` is gone:
  /// the watcher was dropped.
  #[cfg(not(windows))]
  fn take(&self, deadline: Option<Instant>) -> Option<Message<R>> {
    let Some(deadline) = deadline else {
      return Some(self.receiver.recv().unwrap_or(Message::Command(Command::Stop)));
    };
    match self.receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
      Ok(message) => Some(message),
      Err(mpsc::RecvTimeoutError::Timeout) => None,
      Err(mpsc::RecvTimeoutError::Disconnected) => Some(Message::Command(Command::Stop)),
    }
  }
}

/// The wait of the watcher thread on Windows. A timed wait of a channel ends only at a tick of
/// the clock, every 15.6 ms by default: a wait of 20 ms takes 31 ms (measured). So the thread
/// waits on an event that every sender sets, and on a high-resolution timer, which is due within
/// half a millisecond.
#[cfg(windows)]
mod inbox {
  use std::{
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr, thread,
    time::{Duration, Instant},
  };

  use windows_sys::Win32::{
    Foundation::{HANDLE, WAIT_OBJECT_0},
    System::Threading::{
      CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateEventW, CreateWaitableTimerExW, INFINITE,
      SetEvent, SetWaitableTimer, TIMER_ALL_ACCESS, WaitForMultipleObjects, WaitForSingleObject,
    },
  };

  /// An event that a sender sets and the next wait resets. It stays set until a wait takes it,
  /// so a message sent between a look at the queue and the wait is not missed.
  pub struct Wake(OwnedHandle);

  impl Wake {
    pub fn new() -> io::Result<Self> {
      // SAFETY: the call reads no memory of ours. With these arguments it makes an event that
      // resets itself and is not set.
      let event = unsafe { CreateEventW(ptr::null(), 0, 0, ptr::null()) };
      if event.is_null() {
        return Err(io::Error::last_os_error());
      }
      // SAFETY: the event is a new open handle that nothing else owns.
      Ok(Self(unsafe { OwnedHandle::from_raw_handle(event) }))
    }

    pub fn set(&self) {
      // SAFETY: the handle is open.
      unsafe { SetEvent(self.0.as_raw_handle()) };
    }
  }

  /// A high-resolution timer where Windows has one (10, 1803 and later), a plain one before.
  pub struct Timer(OwnedHandle);

  impl Timer {
    pub fn new() -> io::Result<Self> {
      // SAFETY: the call reads no memory of ours. With these arguments it makes a new timer.
      let make = |flags| unsafe {
        CreateWaitableTimerExW(ptr::null(), ptr::null(), flags, TIMER_ALL_ACCESS)
      };
      let mut timer = make(CREATE_WAITABLE_TIMER_HIGH_RESOLUTION);
      if timer.is_null() {
        timer = make(0);
      }
      if timer.is_null() {
        return Err(io::Error::last_os_error());
      }
      // SAFETY: the timer is a new open handle that nothing else owns.
      Ok(Self(unsafe { OwnedHandle::from_raw_handle(timer) }))
    }

    /// Waits until `wake` is set, or until `until` when there is one. `false`: the time came.
    pub fn wait(&self, wake: &Wake, until: Option<Instant>) -> bool {
      let Some(until) = until else {
        // SAFETY: the handle is open.
        unsafe { WaitForSingleObject(wake.0.as_raw_handle(), INFINITE) };
        return true;
      };
      let left = until.saturating_duration_since(Instant::now());
      // Relative, in units of 100 ns, and negative to say so.
      let due = -i64::try_from(left.as_nanos() / 100).unwrap_or(i64::MAX);
      let timer = self.0.as_raw_handle();
      // SAFETY: the handle is open, `due` lives through the call, and no routine is given.
      let set = unsafe { SetWaitableTimer(timer, &raw const due, 0, None, ptr::null(), 0) };
      if set == 0 {
        // Not with an open handle. A wait on the event alone could miss the time: a short
        // sleep, and the caller looks again.
        thread::sleep(left.min(Duration::from_millis(1)));
        return true;
      }
      let handles: [HANDLE; 2] = [wake.0.as_raw_handle(), timer];
      // SAFETY: both handles are open, and the array lives through the call.
      let which = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
      if which == WAIT_OBJECT_0 + 1 {
        return false;
      }
      if which != WAIT_OBJECT_0 {
        // Not with open handles. Against a spin, as above.
        thread::sleep(left.min(Duration::from_millis(1)));
      }
      true
    }
  }
}

/// What the backend threads and the watcher thread share about the queue.
#[derive(Default)]
struct Queue {
  /// The bytes of the signals that wait.
  bytes: AtomicUsize,
  /// A signal was dropped because too many bytes waited.
  overflow: AtomicBool,
}

/// The sink of the backend: it puts a signal into the queue. A backend thread never blocks:
/// when too much waits, the signal is dropped and a flag says so.
fn sink<B: Backend>(post: Post<B::Raw>, queue: Arc<Queue>) -> Sink<B::Raw> {
  Sink::new(move |signal| {
    let Signal::Raw(raw) = &signal else {
      // The word that the backend cannot go on comes once: like a command, it is never
      // dropped and not counted.
      post.send(Message::Signal(signal, 0));
      return;
    };
    let bytes = size_of::<Message<B::Raw>>() + B::raw_bytes(raw);
    if queue.bytes.fetch_add(bytes, Ordering::AcqRel) + bytes <= QUEUE_BYTES {
      // It fails only when the watcher thread is gone.
      post.send(Message::Signal(signal, bytes));
      return;
    }
    queue.bytes.fetch_sub(bytes, Ordering::AcqRel);
    // The thread may sleep with no deadline, so it is woken up to read the flag. Only the
    // drop that sets the flag sends the message: a flood cannot fill the queue with them.
    if !queue.overflow.swap(true, Ordering::AcqRel) {
      post.send(Message::Wake);
    }
  })
}

/// The loop of the watcher thread: take messages, then do what the engine says is due.
fn run<B: Backend>(
  mut engine: Engine<B, StdFs>,
  mut handler: impl FnMut(Result<Vec<Event>, Error>),
  inbox: &Inbox<B::Raw>,
  inside: &Receiver<Command>,
  queue: &Queue,
) {
  let mut out = Output::default();
  loop {
    let mut next = inbox.take(engine.deadline());
    let mut taken = 0;
    while let Some(message) = next {
      if engine.stopped() {
        return;
      }
      let done = match message {
        Message::Signal(signal, bytes) => {
          queue.bytes.fetch_sub(bytes, Ordering::AcqRel);
          engine.signal(signal, Instant::now());
          None
        }
        Message::Command(Command::Watch(paths, done)) => {
          engine.watch(paths, Instant::now(), &mut out);
          done
        }
        Message::Command(Command::Unwatch(paths, done)) => {
          engine.unwatch(paths, Instant::now(), &mut out);
          done
        }
        Message::Command(Command::Stop) => return,
        // Taken here, where the drop was in the queue: a delivery behind it must not end the
        // doubt of a young file (design.md 5.8) whose write the dropped signal may have been.
        Message::Wake => {
          queue.overflow.store(false, Ordering::Release);
          engine.overflow(Instant::now());
          None
        }
      };
      deliver(&mut engine, &mut out, &mut handler, inside);
      if let Some(done) = done {
        // The caller may be gone.
        let _ = done.send(());
      }
      taken += 1;
      next = if taken < MESSAGES_PER_TURN { inbox.receiver.try_recv().ok() } else { None };
    }

    if engine.stopped() {
      return;
    }
    engine.tick(Instant::now(), &mut out);
    deliver(&mut engine, &mut out, &mut handler, inside);
  }
}

/// Hands the output of the engine to the handler, then runs each `watch` and `unwatch` that the
/// handler called, before the next message of the queue: one at a time, each with its own batch,
/// since a batch has at most one event for a path. After a `close` the handler is not called
/// again.
fn deliver<B: Backend>(
  engine: &mut Engine<B, StdFs>,
  out: &mut Output,
  handler: &mut impl FnMut(Result<Vec<Event>, Error>),
  inside: &Receiver<Command>,
) {
  loop {
    for error in out.errors.drain(..) {
      if !engine.stopped() {
        handler(Err(error));
      }
    }
    if !out.events.is_empty() && !engine.stopped() {
      handler(Ok(mem::take(&mut out.events)));
    }
    if engine.stopped() {
      return;
    }
    match inside.try_recv() {
      Ok(Command::Watch(paths, _)) => engine.watch(paths, Instant::now(), out),
      Ok(Command::Unwatch(paths, _)) => engine.unwatch(paths, Instant::now(), out),
      Ok(Command::Stop) | Err(_) => return,
    }
  }
}

/// The form of `path` that the engine takes: absolute and clean. `Err` when it cannot be used,
/// with the error of the current directory when a relative path needed it and it could not be
/// read.
fn clean(path: &Path) -> Result<PathBuf, Option<io::Error>> {
  if path.as_os_str().is_empty() {
    return Err(None);
  }
  // On Windows a path that starts with two separators is a share or nothing. std sees a share
  // only as `\\server\share`; joined to the current directory, the others would name a path on
  // its drive.
  let absolute = path.is_absolute()
    || (cfg!(windows)
      && matches!(path.as_os_str().as_encoded_bytes(), [b'\\' | b'/', b'\\' | b'/', ..]));
  if absolute {
    return normalize(path).ok_or(None);
  }
  let current = env::current_dir().map_err(Some)?;
  #[cfg(windows)]
  let path = rest_of_the_drive(path, &current).ok_or(None)?;
  normalize(&current.join(path)).ok_or(None)
}

/// Windows: `C:name` names `name` in the current directory of the drive, which is the current
/// directory when that is on the drive. The rest of such a path, or the path as it is; `None`
/// for another drive, whose current directory the watcher does not know.
#[cfg(windows)]
fn rest_of_the_drive<'a>(path: &'a Path, current: &Path) -> Option<&'a Path> {
  use std::path::{Component, Prefix};

  let drive = |part: Option<Component<'_>>| match part {
    Some(Component::Prefix(prefix)) => match prefix.kind() {
      Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => Some(letter),
      _ => None,
    },
    _ => None,
  };
  let mut parts = path.components();
  let Some(letter) = drive(parts.next()) else { return Some(path) };
  (drive(current.components().next()) == Some(letter)).then_some(parts.as_path())
}

#[cfg(all(test, windows))]
mod tests {
  use std::{
    thread,
    time::{Duration, Instant},
  };

  use super::{Inbox, Message};

  /// The wait ends with the deadline, not with the next tick of the clock (15.6 ms).
  #[test]
  fn a_wait_ends_at_its_deadline() {
    let (_post, inbox) = Inbox::<()>::new().unwrap();
    for (deadline, at_most) in [(2, 10), (20, 28)] {
      let start = Instant::now();
      let taken = inbox.take(Some(start + Duration::from_millis(deadline)));
      let waited = start.elapsed();
      assert!(taken.is_none());
      assert!(waited >= Duration::from_millis(deadline), "{waited:?} for {deadline} ms");
      assert!(waited < Duration::from_millis(at_most), "{waited:?} for {deadline} ms");
    }
  }

  #[test]
  fn a_message_ends_the_wait() {
    let (post, inbox) = Inbox::<()>::new().unwrap();
    let sender = thread::spawn(move || {
      thread::sleep(Duration::from_millis(5));
      post.send(Message::Wake);
      post
    });
    let start = Instant::now();
    let taken = inbox.take(Some(start + Duration::from_secs(5)));
    assert!(matches!(taken, Some(Message::Wake)));
    assert!(start.elapsed() < Duration::from_millis(100), "{:?}", start.elapsed());
    drop(sender.join().unwrap());
  }
}
