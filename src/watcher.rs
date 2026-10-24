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

const QUEUE_BYTES: usize = 16 << 20;
const MESSAGES_PER_TURN: usize = 512;

thread_local! {
  #[cfg_attr(
    target_os = "android",
    allow(clippy::missing_const_for_thread_local, reason = "the initializer is `const`")
  )]
  static WATCHER_THREAD: Cell<usize> = const { Cell::new(0) };
}

static NEXT_WATCHER: AtomicUsize = AtomicUsize::new(1);

pub struct Watcher {
  number: usize,
  send: Box<dyn Fn(Command) -> bool + Send + Sync>,
  inside: Sender<Command>,
  stop: Arc<AtomicBool>,
  thread: Mutex<Option<JoinHandle<()>>>,
}

impl Watcher {
  pub fn new<H>(handler: H) -> Result<Self, Error>
  where
    H: FnMut(Result<Vec<Event>, Error>) + Send + 'static,
  {
    Self::with_config(Config::default(), handler)
  }

  pub fn with_config<H>(config: Config, handler: H) -> Result<Self, Error>
  where
    H: FnMut(Result<Vec<Event>, Error>) + Send + 'static,
  {
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

  pub fn watch(&self, paths: &[impl AsRef<Path>]) -> Result<(), Error> {
    self.command(paths, Command::Watch)
  }

  pub fn unwatch(&self, paths: &[impl AsRef<Path>]) -> Result<(), Error> {
    self.command(paths, Command::Unwatch)
  }

  pub fn close(&self) {
    self.stop.store(true, Ordering::Release);
    #[cfg(windows)]
    if windows::process_is_exiting() {
      return;
    }
    let _ = (self.send)(Command::Stop);
    if self.on_its_thread() {
      return;
    }
    let mut thread = self.thread.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(thread) = thread.take() {
      let _ = thread.join();
    }
  }

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
        let mut engine = match make(sink) {
          Ok(backend) => {
            let mut engine = Engine::new(backend, StdFs, options);
            engine.fall_back_to(fallback);
            engine
          }
          Err(error) => {
            handler(Err(if is_watch_limit(&error) {
              Error::new(ErrorKind::WatchLimit, None, Some(error))
            } else {
              Error::of_watcher("the OS watcher did not start, so every path is polled", error)
            }));
            Engine::without_backend(StdFs, fallback)
          }
        };
        engine.stop_with(stop);
        Worker { engine, handler, inbox, inside: from_inside, queue, out: Output::default() }.run();
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
      self.inside.send(command(usable, None)).is_ok()
    } else {
      let (done, wait) = mpsc::sync_channel(1);
      (self.send)(command(usable, Some(done))) && wait.recv().is_ok()
    };
    if !sent {
      return Err(closed());
    }
    skipped.map_or(Ok(()), |(path, why)| Err(Error::new(ErrorKind::InvalidPath, Some(path), why)))
  }

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

enum Message<R> {
  Signal(Signal<R>, usize),
  Command(Command),
  Wake,
}

enum Command {
  Watch(Vec<PathBuf>, Done),
  Unwatch(Vec<PathBuf>, Done),
  Stop,
}

type Done = Option<SyncSender<()>>;

struct Post<R> {
  sender: Sender<Message<R>>,
  #[cfg(windows)]
  wake: Arc<windows::Wake>,
}

impl<R> Post<R> {
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

struct Inbox<R> {
  receiver: Receiver<Message<R>>,
  #[cfg(windows)]
  wake: Arc<windows::Wake>,
  #[cfg(windows)]
  timer: windows::Timer,
}

#[cfg(not(windows))]
impl<R> Inbox<R> {
  #[expect(clippy::unnecessary_wraps, reason = "the event and the timer of Windows can fail")]
  fn new() -> io::Result<(Post<R>, Self)> {
    let (sender, receiver) = mpsc::channel();
    Ok((Post { sender }, Self { receiver }))
  }

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

#[derive(Default)]
struct Queue {
  bytes: AtomicUsize,
  overflow: AtomicBool,
}

fn sink<B: Backend>(post: Post<B::Raw>, queue: Arc<Queue>) -> Sink<B::Raw> {
  Sink::new(move |signal| {
    let Signal::Raw(raw) = &signal else {
      post.send(Message::Signal(signal, 0));
      return;
    };
    let bytes = size_of::<Message<B::Raw>>() + B::raw_bytes(raw);
    if queue.bytes.fetch_add(bytes, Ordering::AcqRel) + bytes <= QUEUE_BYTES {
      post.send(Message::Signal(signal, bytes));
      return;
    }
    queue.bytes.fetch_sub(bytes, Ordering::AcqRel);
    if !queue.overflow.swap(true, Ordering::AcqRel) {
      post.send(Message::Wake);
    }
  })
}

struct Worker<B: Backend, H> {
  engine: Engine<B, StdFs>,
  handler: H,
  inbox: Inbox<B::Raw>,
  inside: Receiver<Command>,
  queue: Arc<Queue>,
  out: Output,
}

impl<B: Backend, H: FnMut(Result<Vec<Event>, Error>)> Worker<B, H> {
  fn run(mut self) {
    loop {
      let mut next = self.inbox.take(self.engine.deadline());
      let mut taken = 0;
      while let Some(message) = next {
        if self.engine.stopped() {
          return;
        }
        let done = match message {
          Message::Signal(signal, bytes) => {
            self.queue.bytes.fetch_sub(bytes, Ordering::AcqRel);
            self.engine.signal(signal, Instant::now());
            None
          }
          Message::Command(Command::Watch(paths, done)) => {
            self.engine.watch(paths, Instant::now(), &mut self.out);
            done
          }
          Message::Command(Command::Unwatch(paths, done)) => {
            self.engine.unwatch(paths, Instant::now(), &mut self.out);
            done
          }
          Message::Command(Command::Stop) => return,
          Message::Wake => {
            self.queue.overflow.store(false, Ordering::Release);
            self.engine.overflow(Instant::now());
            None
          }
        };
        self.deliver();
        if let Some(done) = done {
          let _ = done.send(());
        }
        taken += 1;
        next = if taken < MESSAGES_PER_TURN { self.inbox.receiver.try_recv().ok() } else { None };
      }

      if self.engine.stopped() {
        return;
      }
      self.engine.tick(Instant::now(), &mut self.out);
      self.deliver();
    }
  }

  fn deliver(&mut self) {
    loop {
      let events = (!self.out.events.is_empty()).then(|| Ok(mem::take(&mut self.out.events)));
      for result in self.out.errors.drain(..).map(Err).chain(events) {
        if self.engine.stopped() {
          return;
        }
        (self.handler)(result);
      }
      if self.engine.stopped() {
        return;
      }
      match self.inside.try_recv() {
        Ok(Command::Watch(paths, _)) => self.engine.watch(paths, Instant::now(), &mut self.out),
        Ok(Command::Unwatch(paths, _)) => self.engine.unwatch(paths, Instant::now(), &mut self.out),
        Ok(Command::Stop) | Err(_) => return,
      }
    }
  }
}

fn clean(path: &Path) -> Result<PathBuf, Option<io::Error>> {
  if path.as_os_str().is_empty() {
    return Err(None);
  }
  if path.is_absolute()
    || (cfg!(windows)
      && matches!(path.as_os_str().as_encoded_bytes(), [b'\\' | b'/', b'\\' | b'/', ..]))
  {
    return normalize(path).ok_or(None);
  }
  let current = env::current_dir().map_err(Some)?;
  #[cfg(windows)]
  let path = windows::rest_of_the_drive(path, &current).ok_or(None)?;
  normalize(&current.join(path)).ok_or(None)
}

#[cfg(windows)]
mod windows {
  use std::{
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::{Component, Path, Prefix},
    ptr,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
  };

  use windows_sys::Win32::{
    Foundation::{HANDLE, WAIT_OBJECT_0},
    System::Threading::{
      CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateEventW, CreateWaitableTimerExW, INFINITE,
      SetEvent, SetWaitableTimer, TIMER_ALL_ACCESS, WaitForMultipleObjects, WaitForSingleObject,
    },
  };

  use super::{Command, Inbox, Message, Post};

  pub fn process_is_exiting() -> bool {
    #[link(name = "ntdll")]
    unsafe extern "system" {
      safe fn RtlDllShutdownInProgress() -> u8;
    }

    RtlDllShutdownInProgress() != 0
  }

  impl<R> Inbox<R> {
    pub fn new() -> io::Result<(Post<R>, Self)> {
      let (sender, receiver) = mpsc::channel();
      let wake = Arc::new(Wake::new()?);
      let post = Post { sender, wake: Arc::clone(&wake) };
      Ok((post, Self { receiver, wake, timer: Timer::new()? }))
    }

    pub fn take(&self, deadline: Option<Instant>) -> Option<Message<R>> {
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
  }

  pub struct Wake(OwnedHandle);

  impl Wake {
    pub fn new() -> io::Result<Self> {
      // SAFETY: the call reads no memory of ours.
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

    pub fn wait(&self, wake: &Wake, until: Option<Instant>) -> bool {
      let Some(until) = until else {
        // SAFETY: the handle is open.
        unsafe { WaitForSingleObject(wake.0.as_raw_handle(), INFINITE) };
        return true;
      };
      let left = until.saturating_duration_since(Instant::now());
      let due = -i64::try_from(left.as_nanos() / 100).unwrap_or(i64::MAX);
      let timer = self.0.as_raw_handle();
      // SAFETY: the handle is open, `due` lives through the call, and no routine is given.
      let set = unsafe { SetWaitableTimer(timer, &raw const due, 0, None, ptr::null(), 0) };
      if set == 0 {
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
        thread::sleep(left.min(Duration::from_millis(1)));
      }
      true
    }
  }

  pub fn rest_of_the_drive<'a>(path: &'a Path, current: &Path) -> Option<&'a Path> {
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

  #[cfg(test)]
  mod tests {
    use std::time::{Duration, Instant};

    use super::Inbox;

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
  }
}
