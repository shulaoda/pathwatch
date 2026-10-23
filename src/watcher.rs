//! The watcher: one thread that runs the engine, the queue into it, and the choice of the
//! backend.

use std::{
  cell::Cell,
  env, fmt, io,
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

/// How many bytes of backend signals wait at most. One more is dropped, and the engine checks
/// everything. Bytes, because a raw notification can be a few bytes or a whole buffer of the OS.
const QUEUE_BYTES: usize = 16 << 20;
/// How many messages are taken in one turn of the loop before the deadlines are served, so
/// that a flood of signals cannot hold back a flush.
const MESSAGES_PER_TURN: usize = 512;

thread_local! {
  /// The number of the watcher whose thread this is; 0 on every other thread. A cell with no
  /// destructor: it can still be read while the thread-locals of the thread are destroyed,
  /// where `std::thread::current` panics (a watcher dropped as the process exits).
  // Android keeps its thread-locals by hand, and the lint takes the `const` for a plain
  // initializer there, but only where the std source is installed, which it reads.
  #[cfg_attr(
    target_os = "android",
    allow(clippy::missing_const_for_thread_local, reason = "the initializer is `const`")
  )]
  static WATCHER_THREAD: Cell<usize> = const { Cell::new(0) };
}

/// The number of the next watcher. It starts at 1.
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
  /// The number that its thread has in `WATCHER_THREAD`.
  number: usize,
  /// Puts a command into the queue. A closure, since the raw type differs by backend. False: the
  /// thread is gone.
  send: Box<dyn Fn(Command) -> bool + Send + Sync>,
  /// The commands from inside the handler: the thread takes them when the handler has returned,
  /// before the next message of the queue.
  inside: Sender<Command>,
  stop: Arc<AtomicBool>,
  /// `close` takes it out to wait for the thread.
  thread: Mutex<Option<JoinHandle<()>>>,
}

impl fmt::Debug for Watcher {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Watcher").field("number", &self.number).finish_non_exhaustive()
  }
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
  /// event for a path, in no promised order), or `Err(error)`: something could not be watched or
  /// read, and the watcher keeps running. Send the batch to a channel and work on another thread:
  /// a slow handler delays every check of the disk and every `watch` and `unwatch` call. A
  /// `watch`, `unwatch` or `close` on another thread waits until the handler has returned, so a
  /// handler must never wait for such a caller (a lock it holds, a bounded channel it fills,
  /// a call into another watcher whose handler calls back): both would wait for ever.
  ///
  /// A handler that panics ends the thread, and so does a filter of
  /// [`Config::with_ignored`] that panics: nothing is reported after that, and `watch` and
  /// `unwatch` return [`ErrorKind::Closed`].
  ///
  /// When the watch API of the OS cannot start, the watcher polls, as with
  /// [`Config::with_polling`], and tells the handler once: [`ErrorKind::WatchLimit`] for a limit
  /// of the OS, [`ErrorKind::Io`] otherwise. The same when it stops later ([`ErrorKind::Io`]).
  ///
  /// # Errors
  ///
  /// The watcher thread, or the queue into it, could not be made.
  pub fn with_config<H>(config: Config, handler: H) -> Result<Self, Error>
  where
    H: FnMut(Result<Vec<Event>, Error>) + Send + 'static,
  {
    let poll = config.clone().into_options(true);
    if config.polling() {
      return Self::start(poll.clone(), poll, handler, PollBackend::new);
    }
    // Which target has which native backend: the one place that says it. Any other target polls.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
      use crate::backend::inotify::InotifyBackend;
      Self::start(config.into_options(false), poll, handler, InotifyBackend::new)
    }
    #[cfg(target_os = "macos")]
    {
      use crate::backend::fsevents::FsEventsBackend;
      Self::start(config.into_options(false), poll, handler, FsEventsBackend::new)
    }
    #[cfg(windows)]
    {
      use crate::backend::windows::WindowsBackend;
      Self::start(config.into_options(false), poll, handler, WindowsBackend::new)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos", windows)))]
    {
      Self::start(config.into_options(true), poll, handler, PollBackend::new)
    }
  }

  /// Watches paths, in one step: each a file, a directory with everything below it, or a path
  /// that does not exist yet. The call returns when every path is watched. What exists then is
  /// not reported, every later change is; it is silent only about files that the watcher did not
  /// know already. Watching a path again does nothing.
  ///
  /// A relative path is joined to the current directory; on Windows a path with a drive and no
  /// root (`C:name`) too, when the current directory is on that drive. The path is cleaned by
  /// its text: `.`, `..` and repeated separators, like Node does and not like the OS behind a
  /// symbolic link. No link is resolved and no letter case is changed. Every event path starts
  /// with it; a `\\?\` path of Windows is taken in its plain form.
  ///
  /// From inside the handler the call does not wait: the paths are watched when the handler has
  /// returned.
  ///
  /// # Errors
  ///
  /// - [`ErrorKind::InvalidPath`]: a path cannot be used. It does not stop the others: every
  ///   usable path is watched, and the error names the first one that was skipped.
  /// - [`ErrorKind::Closed`]: the watcher is closed.
  ///
  /// A path that the OS cannot watch or read is no error here: the handler gets it.
  pub fn watch(&self, paths: &[impl AsRef<Path>]) -> Result<(), Error> {
    self.command(paths, Command::Watch)
  }

  /// Takes back a [`watch`](Self::watch) of these very paths, in one step. A path that is not
  /// watched is skipped.
  ///
  /// Nothing is reported for a path taken back: the files that only it covered are forgotten, a
  /// `Delete` that was waiting below it is dropped, and a file that another watched path still
  /// covers stays known. One exception: a link inside a watched directory that the path was
  /// followed through is a plain file of that directory again, and that change is reported. It
  /// cannot cut a part out of a watched directory: use [`Config::with_ignored`] for that.
  ///
  /// From inside the handler the call does not wait.
  ///
  /// # Errors
  ///
  /// - [`ErrorKind::InvalidPath`]: a path cannot be used. The others are still unwatched, and
  ///   the error names the first one that was skipped.
  /// - [`ErrorKind::Closed`]: the watcher is closed.
  pub fn unwatch(&self, paths: &[impl AsRef<Path>]) -> Result<(), Error> {
    self.command(paths, Command::Unwatch)
  }

  /// Stops the watcher and waits for its thread. After it the handler is not called again,
  /// nothing is watched, and `watch` and `unwatch` return [`ErrorKind::Closed`]. Events not handed
  /// over yet are dropped, and a check of the disk that runs ends at its next step: the wait is
  /// one look at most. It can be called many times, and from inside the handler: there it
  /// does not wait, and the thread ends when the handler returns. While the process exits
  /// (a watcher in a thread-local, or dropped by the detach code of a DLL on Windows), the
  /// thread was ended by the OS already, and nothing is waited for.
  pub fn close(&self) {
    self.stop.store(true, Ordering::Release);
    if self.on_its_thread() {
      // Wakes the thread up: it reads the flag when the handler has returned.
      let _ = (self.send)(Command::Stop);
      return;
    }
    // When the process exits, Windows ends every other thread and then runs the destructors
    // of the thread-locals and the detach code of DLLs on the exiting one. The watcher thread
    // is gone then: it can hold the lock of the queue, and `join` would panic for it. Nothing
    // is sent to it, and nothing is waited for.
    #[cfg(windows)]
    if process_is_exiting() {
      return;
    }
    // Wakes the thread up. It fails only when the thread is gone already.
    let _ = (self.send)(Command::Stop);
    // The lock is held while this thread waits: a `close` on another thread returns only
    // when the watcher thread is gone too.
    let mut thread = self.thread.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(thread) = thread.take() {
      // A panic in the handler took the thread down already.
      let _ = thread.join();
    }
  }

  /// Whether this is the thread of the watcher: inside the handler, or the backend.
  fn on_its_thread(&self) -> bool {
    WATCHER_THREAD.get() == self.number
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

impl Watcher {
  /// Cleans the paths and hands the usable ones to the thread as one command.
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
    if self.on_its_thread() {
      // Inside the handler: the thread cannot wait for itself. It takes the command when the
      // handler has returned.
      if self.inside.send(command(usable, None)).is_err() {
        return Err(closed());
      }
    } else {
      let (done, wait) = mpsc::sync_channel(1);
      if !(self.send)(command(usable, Some(done))) {
        return Err(closed());
      }
      // The thread drops the channel when it stops before the command is done.
      wait.recv().map_err(|_| closed())?;
    }
    skipped.map_or(Ok(()), |(path, why)| Err(Error::new(ErrorKind::InvalidPath, Some(path), why)))
  }
}

impl Drop for Watcher {
  fn drop(&mut self) {
    self.close();
  }
}

impl Watcher {
  /// [`with_config`](Self::with_config) with a backend of the caller's choice, for the tests.
  /// It is not part of the API.
  ///
  /// # Errors
  ///
  /// The watcher thread, or the queue into it, could not be made.
  #[doc(hidden)]
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

  /// Starts the watcher thread. `make` makes the backend on that thread; when it fails, the handler
  /// is told once and the watcher polls with `fallback`, and so it does when the backend dies.
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
    let (inside, from_inside) = mpsc::channel::<Command>();
    let queue = Arc::new(Queue::default());
    let stop = Arc::new(AtomicBool::new(false));
    let sink = sink::<B>(post.clone(), Arc::clone(&queue));

    let number = NEXT_WATCHER.fetch_add(1, Ordering::Relaxed);
    let thread = {
      let stop = Arc::clone(&stop);
      thread::Builder::new().name("pathwatch".to_owned()).spawn(move || {
        WATCHER_THREAD.set(number);
        // The backend is made here: all of its methods run on this thread.
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
    };
    let thread = thread
      .map_err(|error| Error::of_watcher("the thread of the watcher did not start", error))?;

    Ok(Self {
      number,
      send: Box::new(move |command| post.send(Message::Command(command))),
      inside,
      stop,
      thread: Mutex::new(Some(thread)),
    })
  }
}

enum Message<R> {
  /// A signal of the backend, and the bytes that it counts for in the queue.
  Signal(Signal<R>, usize),
  Command(Command),
  /// A signal was dropped, and the overflow flag says the rest. It wakes the thread up, and its
  /// place in the queue is where the overflow is taken.
  Wake,
}

enum Command {
  Watch(Vec<PathBuf>, Done),
  Unwatch(Vec<PathBuf>, Done),
  /// Only wakes the thread up. The stop flag says the rest.
  Stop,
}

/// The channel on which the thread says that a command is done. `None`: nobody waits, because
/// the command came from inside the handler.
type Done = Option<SyncSender<()>>;

/// The way into the queue of the watcher thread: a sender, and on Windows the wake-up of the
/// thread (`inbox`).
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

/// The queue as the watcher thread takes from it.
struct Inbox<R> {
  receiver: Receiver<Message<R>>,
  #[cfg(windows)]
  wake: Arc<inbox::Wake>,
  #[cfg(windows)]
  timer: inbox::Timer,
}

/// Every `Post` is gone.
struct Closed;

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

  /// The next message, or `None` when `deadline` came first.
  #[cfg(windows)]
  fn take(&self, deadline: Option<Instant>) -> Result<Option<Message<R>>, Closed> {
    loop {
      match self.receiver.try_recv() {
        Ok(message) => return Ok(Some(message)),
        Err(mpsc::TryRecvError::Disconnected) => return Err(Closed),
        Err(mpsc::TryRecvError::Empty) => {}
      }
      if !self.timer.wait(&self.wake, deadline) {
        return Ok(None);
      }
    }
  }

  /// The next message, or `None` when `deadline` came first.
  #[cfg(not(windows))]
  fn take(&self, deadline: Option<Instant>) -> Result<Option<Message<R>>, Closed> {
    let Some(deadline) = deadline else {
      return self.receiver.recv().map(Some).map_err(|mpsc::RecvError| Closed);
    };
    match self.receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
      Ok(message) => Ok(Some(message)),
      Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
      Err(mpsc::RecvTimeoutError::Disconnected) => Err(Closed),
    }
  }
}

/// The wait of the watcher thread on Windows. A timed wait of a channel there ends at a tick of
/// the clock, which comes every 15.6 ms unless a program asked for more: a wait of 20 ms takes
/// 31 ms, and one of 1 ms takes 15.6 ms (measured). So the thread waits on an event that every
/// sender sets, and on a timer of high resolution, which comes within half a millisecond.
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

  /// An event that a sender sets and the next wait takes (it resets itself). A set that no wait
  /// took yet stays: a message sent between a look at the queue and the wait is not missed.
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

  /// A timer of high resolution where Windows has one (10, 1803 and later), a plain one before.
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
    let mut next = match inbox.take(engine.deadline()) {
      Ok(message) => message,
      // The senders are gone: the watcher was dropped, and the thread ends like after `Stop`.
      Err(Closed) => return,
    };

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
        // The drop that set the flag sent it: the overflow is taken where the drop was in the
        // queue, and only there. A delivery behind it must not end the doubt of a young file
        // (design.md 5.8) whose write the dropped signal can have been. Each flag that is set
        // sends one, so no drop goes untold.
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

/// Hands the output of the engine to the handler, and does what the handler asked for on the
/// way: a `watch` or `unwatch` from inside it takes effect when it has returned, before the
/// next message of the queue. One call at a time, each handing over its own batch: the disk can
/// change between two of them, and a batch has at most one event for a path. After a `close`
/// (the handler can call it) the handler is not called again.
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
      handler(Ok(std::mem::take(&mut out.events)));
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

/// The form of `path` that the engine takes: absolute and clean. `Err`: it cannot be used, with
/// the error of the current directory when a relative path needed it and it could not be read.
///
/// A relative path is joined to the current directory; [`normalize`] does the rest.
fn clean(path: &Path) -> Result<PathBuf, Option<io::Error>> {
  if path.as_os_str().is_empty() {
    return Err(None);
  }
  // On Windows a path that starts with two separators is a share, or nothing. std sees one only
  // in the form `\\server\share`; joined to the current directory, the others would name a path
  // on its drive.
  #[cfg(windows)]
  let absolute = path.is_absolute()
    || matches!(path.as_os_str().as_encoded_bytes(), [b'\\' | b'/', b'\\' | b'/', ..]);
  #[cfg(not(windows))]
  let absolute = path.is_absolute();
  if absolute {
    return normalize(path).ok_or(None);
  }
  // A relative path needs the current directory: when that cannot be read, the error says why.
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
      assert!(matches!(taken, Ok(None)));
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
    assert!(matches!(taken, Ok(Some(Message::Wake))));
    assert!(start.elapsed() < Duration::from_millis(100), "{:?}", start.elapsed());
    drop(sender.join().unwrap());
  }
}
