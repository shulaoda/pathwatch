//! The watcher: one thread that runs the engine, the queue into it, and the choice of the
//! backend.

use std::{
  borrow::Cow,
  cell::Cell,
  env, io,
  marker::PhantomData,
  path::{Path, PathBuf},
  sync::{
    Arc, Mutex, PoisonError,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender},
  },
  thread::{self, JoinHandle},
  time::Instant,
};

use crate::{
  backend::{Backend, Coverage, DirKey, Hint, Signal, Sink, Want, is_watch_limit},
  config::Config,
  engine::{Engine, Options, Output},
  error::{Error, ErrorKind, context},
  event::Event,
  fs::StdFs,
  path::normalize,
  poll::PollBackend,
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
/// watcher.watch("src")?;
/// watcher.watch_all(["package.json", "not-there-yet.json"])?;
/// watcher.unwatch("src")?;
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
  stop: Arc<AtomicBool>,
  /// `close` takes it out to wait for the thread.
  thread: Mutex<Option<JoinHandle<()>>>,
}

impl Watcher {
  /// Starts a watcher with the default [`Config`]. See [`with_config`](Self::with_config).
  ///
  /// # Errors
  ///
  /// The watcher thread could not be started.
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
  /// of the OS, [`ErrorKind::Io`] otherwise.
  ///
  /// # Errors
  ///
  /// The watcher thread could not be started.
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
      Self::start(config.into_options(false), poll, handler, crate::inotify::InotifyBackend::new)
    }
    #[cfg(target_os = "macos")]
    {
      Self::start(config.into_options(false), poll, handler, crate::fsevents::FsEventsBackend::new)
    }
    #[cfg(windows)]
    {
      Self::start(config.into_options(false), poll, handler, crate::windows::WindowsBackend::new)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos", windows)))]
    {
      Self::start(poll.clone(), poll, handler, PollBackend::new)
    }
  }

  /// Watches a path: a file, a directory with everything below it, or a path that does not exist
  /// yet. The call returns when the path is watched. What exists then is not reported, every later
  /// change is; it is silent only about files that the watcher did not know already. Watching a
  /// path again does nothing.
  ///
  /// A relative path is joined to the current directory; on Windows a path with a drive and no
  /// root (`C:name`) too, when the current directory is on that drive. The path is cleaned by
  /// its text: `.`, `..` and repeated separators, like Node does and not like the OS behind a
  /// symbolic link. No link is resolved and no letter case is changed. Every event path starts
  /// with it; a `\\?\` path of Windows is taken in its plain form.
  ///
  /// From inside the handler the call does not wait: the path is watched when the handler has
  /// returned.
  ///
  /// # Errors
  ///
  /// - [`ErrorKind::InvalidPath`]: the path cannot be used.
  /// - [`ErrorKind::Closed`]: the watcher is closed.
  ///
  /// A path that the OS cannot watch or read is no error here: the handler gets it.
  pub fn watch(&self, path: impl AsRef<Path>) -> Result<(), Error> {
    self.watch_all([path])
  }

  /// [`watch`](Self::watch) for many paths in one step, which is faster than one call for
  /// each.
  ///
  /// # Errors
  ///
  /// - [`ErrorKind::InvalidPath`]: a path cannot be used. It does not stop the others: every
  ///   usable path is watched, and the error names the first one that was skipped.
  /// - [`ErrorKind::Closed`]: the watcher is closed.
  pub fn watch_all(&self, paths: impl IntoIterator<Item = impl AsRef<Path>>) -> Result<(), Error> {
    self.command(paths, Command::Watch)
  }

  /// Takes back a [`watch`](Self::watch) of this very path. A path that is not watched is skipped.
  ///
  /// Nothing is reported for it: the files that only this path covered are forgotten, a `Delete`
  /// that was waiting below it is dropped, and a file that another watched path still covers
  /// stays known. One exception: a link inside a watched directory that this path was followed
  /// through is a plain file of that directory again, and that change is reported. It cannot cut
  /// a part out of a watched directory: use [`Config::with_ignored`] for that.
  ///
  /// From inside the handler the call does not wait.
  ///
  /// # Errors
  ///
  /// - [`ErrorKind::InvalidPath`]: the path cannot be used.
  /// - [`ErrorKind::Closed`]: the watcher is closed.
  pub fn unwatch(&self, path: impl AsRef<Path>) -> Result<(), Error> {
    self.unwatch_all([path])
  }

  /// [`unwatch`](Self::unwatch) for many paths in one step.
  ///
  /// # Errors
  ///
  /// - [`ErrorKind::InvalidPath`]: a path cannot be used. The others are still unwatched, and
  ///   the error names the first one that was skipped.
  /// - [`ErrorKind::Closed`]: the watcher is closed.
  pub fn unwatch_all(
    &self,
    paths: impl IntoIterator<Item = impl AsRef<Path>>,
  ) -> Result<(), Error> {
    self.command(paths, Command::Unwatch)
  }

  /// Stops the watcher and waits for its thread. After it the handler is not called again,
  /// nothing is watched, and `watch` and `unwatch` return [`ErrorKind::Closed`]. Events not handed
  /// over yet are dropped. It can be called many times, and from inside the handler: there it
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
  fn command<P: AsRef<Path>>(
    &self,
    paths: impl IntoIterator<Item = P>,
    command: fn(Vec<PathBuf>, Done) -> Command,
  ) -> Result<(), Error> {
    let closed = || Error::new(ErrorKind::Closed, None, None);
    if self.stop.load(Ordering::Acquire) {
      return Err(closed());
    }
    let paths = paths.into_iter();
    let mut usable = Vec::with_capacity(paths.size_hint().0);
    let mut skipped = None;
    for path in paths {
      match clean(path.as_ref()) {
        Some(path) => usable.push(path),
        None => skipped = skipped.or_else(|| Some(path.as_ref().to_path_buf())),
      }
    }
    if self.on_its_thread() {
      // Inside the handler: the thread cannot wait for itself. It takes the command from the
      // queue when the handler has returned.
      if !(self.send)(command(usable, None)) {
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
    skipped.map_or(Ok(()), |path| Err(Error::new(ErrorKind::InvalidPath, Some(path), None)))
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
  /// The watcher thread could not be started.
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
  /// is told once and the watcher polls with `fallback`.
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
    let (sender, receiver) = mpsc::channel::<Message<B::Raw>>();
    let queue = Arc::new(Queue::default());
    let stop = Arc::new(AtomicBool::new(false));
    let sink = sink::<B>(sender.clone(), Arc::clone(&queue));

    let number = NEXT_WATCHER.fetch_add(1, Ordering::Relaxed);
    let thread = {
      let stop = Arc::clone(&stop);
      thread::Builder::new().name("pathwatch".to_owned()).spawn(move || {
        WATCHER_THREAD.set(number);
        // The backend is made here: all of its methods run on this thread.
        match make(sink) {
          Ok(backend) => {
            run(Engine::new(backend, StdFs, options), handler, &receiver, &queue, &stop);
          }
          Err(error) => {
            // A limit says by itself what is missing and that the watcher polls.
            let (kind, error) = if is_watch_limit(&error) {
              (ErrorKind::WatchLimit, error)
            } else {
              let what = "the OS watcher did not start, so every path is polled";
              (ErrorKind::Io, context(what, &error))
            };
            handler(Err(Error::new(kind, None, Some(error))));
            let engine = Engine::new(NoBackend::<B::Raw>(PhantomData), StdFs, fallback);
            run(engine, handler, &receiver, &queue, &stop);
          }
        }
      })
    };
    let thread = thread.map_err(|error| {
      let error = context("the thread of the watcher did not start", &error);
      Error::new(ErrorKind::Io, None, Some(error))
    })?;

    Ok(Self {
      number,
      send: Box::new(move |command| sender.send(Message::Command(command)).is_ok()),
      stop,
      thread: Mutex::new(Some(thread)),
    })
  }
}

enum Message<R> {
  /// A signal of the backend, and the bytes that it counts for in the queue.
  Signal(Signal<R>, usize),
  Command(Command),
  /// Only wakes the thread up: a signal was dropped, and the overflow flag says the rest.
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

/// What the backend threads and the watcher thread share about the queue.
#[derive(Default)]
struct Queue {
  /// The bytes of the signals that wait.
  bytes: AtomicUsize,
  /// A signal was dropped because too many bytes waited.
  overflow: AtomicBool,
  /// A dropped signal was the word that the backend cannot go on.
  failed: Mutex<Option<io::Error>>,
}

/// The sink of the backend: it puts a signal into the queue. A backend thread never blocks:
/// when too much waits, the signal is dropped and a flag says so.
fn sink<B: Backend>(sender: Sender<Message<B::Raw>>, queue: Arc<Queue>) -> Sink<B::Raw> {
  Sink::new(move |signal| {
    let bytes = size_of::<Message<B::Raw>>()
      + match &signal {
        Signal::Raw(raw) => B::raw_bytes(raw),
        Signal::Failed(_) => 0,
      };
    if queue.bytes.fetch_add(bytes, Ordering::AcqRel) + bytes <= QUEUE_BYTES {
      // It fails only when the watcher thread is gone.
      let _ = sender.send(Message::Signal(signal, bytes));
      return;
    }
    queue.bytes.fetch_sub(bytes, Ordering::AcqRel);
    if let Signal::Failed(error) = signal {
      *queue.failed.lock().unwrap_or_else(PoisonError::into_inner) = Some(error);
    }
    // The thread may sleep with no deadline, so it is woken up to read the flag. Only the
    // drop that sets the flag sends the message: a flood cannot fill the queue with them.
    if !queue.overflow.swap(true, Ordering::AcqRel) {
      let _ = sender.send(Message::Wake);
    }
  })
}

/// The loop of the watcher thread: take messages, then do what the engine says is due.
fn run<B: Backend>(
  mut engine: Engine<B, StdFs>,
  mut handler: impl FnMut(Result<Vec<Event>, Error>),
  receiver: &Receiver<Message<B::Raw>>,
  queue: &Queue,
  stop: &AtomicBool,
) {
  let mut out = Output::default();
  loop {
    let mut next = match engine.deadline() {
      // The senders are gone: the watcher was dropped, and the thread ends like after `Stop`.
      None => match receiver.recv() {
        Ok(message) => Some(message),
        Err(mpsc::RecvError) => return,
      },
      Some(deadline) => {
        match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
          Ok(message) => Some(message),
          Err(RecvTimeoutError::Timeout) => None,
          Err(RecvTimeoutError::Disconnected) => return,
        }
      }
    };

    let mut taken = 0;
    while let Some(message) = next {
      if stop.load(Ordering::Acquire) {
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
        Message::Wake => None,
      };
      deliver(&mut out, &mut handler, stop);
      if let Some(done) = done {
        // The caller may be gone.
        let _ = done.send(());
      }
      taken += 1;
      next = if taken < MESSAGES_PER_TURN { receiver.try_recv().ok() } else { None };
    }

    if stop.load(Ordering::Acquire) {
      return;
    }
    if queue.overflow.swap(false, Ordering::AcqRel) {
      engine.overflow(Instant::now());
      if let Some(error) = queue.failed.lock().unwrap_or_else(PoisonError::into_inner).take() {
        engine.signal(Signal::Failed(error), Instant::now());
      }
    }
    let now = Instant::now();
    if engine.deadline().is_some_and(|deadline| deadline <= now) {
      engine.tick(now, &mut out);
      deliver(&mut out, &mut handler, stop);
    }
  }
}

/// Hands the output of the engine to the handler. After a `close` (the handler can call it)
/// the handler is not called again.
fn deliver(
  out: &mut Output,
  handler: &mut impl FnMut(Result<Vec<Event>, Error>),
  stop: &AtomicBool,
) {
  for error in out.errors.drain(..) {
    if !stop.load(Ordering::Acquire) {
      handler(Err(error));
    }
  }
  if !out.events.is_empty() && !stop.load(Ordering::Acquire) {
    handler(Ok(std::mem::take(&mut out.events)));
  }
}

/// The backend of a watcher whose native backend did not start: it watches nothing, but has the
/// raw type of the native one, which the queue was made for.
struct NoBackend<R>(PhantomData<fn(R)>);

impl<R: Send + 'static> Backend for NoBackend<R> {
  type Raw = R;

  fn watch(&mut self, _key: DirKey, _dir: &Path, _want: Want) -> Coverage {
    Coverage::NotWatched
  }

  fn unwatch(&mut self, _key: DirKey) {}

  fn translate(&mut self, _raw: R, _hint: &mut dyn FnMut(Hint<'_>)) {}
}

/// The form of `path` that the engine takes: absolute and clean. `None`: it cannot be used.
///
/// A relative path is joined to the current directory; [`normalize`] does the rest.
fn clean(path: &Path) -> Option<PathBuf> {
  if path.as_os_str().is_empty() {
    return None;
  }
  #[cfg(windows)]
  let absolute = path.is_absolute() || starts_like_a_share(path);
  #[cfg(not(windows))]
  let absolute = path.is_absolute();
  if absolute {
    return normalize(path).map(Cow::into_owned);
  }
  let current = env::current_dir().ok()?;
  #[cfg(windows)]
  let path = rest_of_the_drive(path, &current)?;
  normalize(&current.join(path)).map(Cow::into_owned)
}

/// Windows: a path that starts with two separators is a share, or nothing. std sees one only in
/// the form `\server\share`; joined to the current directory, the others would name a path on
/// its drive.
#[cfg(windows)]
fn starts_like_a_share(path: &Path) -> bool {
  use std::os::windows::ffi::OsStrExt;

  let mut units = path.as_os_str().encode_wide();
  let separator = |unit: Option<u16>| matches!(unit, Some(0x5C | 0x2F));
  separator(units.next()) && separator(units.next())
}

/// Windows: `C:name` names `name` in the current directory of the drive, which is the current
/// directory when that is on the drive. The rest of such a path, or the path as it is; `None`
/// for another drive, whose current directory the watcher does not know.
#[cfg(windows)]
fn rest_of_the_drive<'a>(path: &'a Path, current: &Path) -> Option<&'a Path> {
  use std::path::{Component, Prefix};

  let drive = |path: &Path| match path.components().next() {
    Some(Component::Prefix(prefix)) => match prefix.kind() {
      Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => Some(letter),
      _ => None,
    },
    _ => None,
  };
  let Some(letter) = drive(path) else { return Some(path) };
  if drive(current) != Some(letter) {
    return None;
  }
  let Some(Component::Prefix(prefix)) = path.components().next() else { return None };
  path.strip_prefix(prefix.as_os_str()).ok()
}
