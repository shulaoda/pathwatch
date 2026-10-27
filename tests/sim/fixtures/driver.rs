//! The driver: an engine on a simulated disk, and the short helpers that the tests share.

use std::{
  collections::BTreeSet,
  path::{Path, PathBuf},
  sync::Arc,
  time::{Duration, Instant},
};

use pathwatch::{
  EntryKind, Error, ErrorKind, Event, EventKind,
  backend::{Backend, Signal, Want, poll::PollBackend},
  engine::{Engine, Ignored, Options, Output},
  path::normalize,
};

use super::{Mode, SimBackend, SimFs, SimRaw, WatchCall, check_fingerprints, reportable};

/// An engine on a [`SimFs`], with a clock that only the test moves.
///
/// After every call of the engine it plays the handler: it applies the events to `known` and
/// panics when a batch breaks a rule of the handler contract, when `known` and the view of the
/// engine differ, or when an invariant of the tree does not hold.
///
/// Each time it lets everything settle, it also panics when the engine has not caught up with
/// the disk (`caught_up.rs` says what that means).
///
/// A story says how it ends: with [`caught_up`], or with [`not_caught_up`] and the reason.
pub struct Driver<B: Deliver> {
  pub engine: Engine<B, SimFs>,
  pub fs: SimFs,
  pub now: Instant,
  /// The files a handler believes to exist: the baseline of every `watch`, plus `Create`, minus
  /// `Delete`, minus what an `unwatch` covered alone. A watched path that the watcher drops
  /// takes no file away: each one gets its `Delete`.
  pub known: BTreeSet<PathBuf>,
  /// Every event since the last [`Driver::take_events`], batch after batch. A batch is sorted
  /// by path here, so that no test depends on the order in which the engine hands it over.
  pub events: Vec<Event>,
  pub errors: Vec<Error>,
  /// Every path given to `watch` that is a valid path and was not taken back by `unwatch`, in
  /// its registered form. The driver keeps its own list: the walk of the disk must not ask the
  /// engine what is watched.
  watched: BTreeSet<PathBuf>,
  ignored: Option<Ignored>,
  settle: Duration,
  /// The story said how it ends.
  ended: bool,
}

impl Driver<SimBackend> {
  pub fn sim(mode: Mode, options: Options) -> Self {
    let fs = SimFs::new();
    Self::new(SimBackend::attach(&fs, mode), fs, options)
  }

  /// Hands one raw notification to the engine, at the current time.
  pub fn signal(&mut self, raw: SimRaw) {
    self.engine.signal(Signal::Raw(raw), self.now);
  }
}

impl Driver<PollBackend> {
  pub fn poll(options: Options) -> Self {
    Self::new(PollBackend, SimFs::new(), options)
  }
}

impl<B: Deliver> Driver<B> {
  pub fn new(backend: B, fs: SimFs, options: Options) -> Self {
    assert!(!options.poll_interval.is_zero(), "settle would never end");
    // Long enough for two polls, a pending delete and the time a young file is hashed.
    let settle = options.poll_interval * 2
      + options.delete_delay
      + options.debounce * 2
      + Duration::from_secs(2);
    Self {
      ignored: options.ignored.clone(),
      engine: Engine::new(backend, fs.clone(), options),
      fs,
      now: Instant::now(),
      known: BTreeSet::new(),
      events: Vec::new(),
      errors: Vec::new(),
      watched: BTreeSet::new(),
      settle,
      ended: false,
    }
  }

  /// Gives the engine the clean form of each path, as the watcher does. A path that cannot be
  /// used is left out.
  pub fn watch<P: AsRef<Path>>(&mut self, paths: &[P]) {
    let mut out = Output::default();
    let paths: Vec<PathBuf> = paths.iter().filter_map(|path| normalize(path.as_ref())).collect();
    self.watched.extend(paths.iter().cloned());
    self.engine.watch(paths, self.now, &mut out);
    self.absorb(out, Call::Watch);
  }

  pub fn unwatch<P: AsRef<Path>>(&mut self, paths: &[P]) {
    let mut out = Output::default();
    let paths: Vec<PathBuf> = paths.iter().filter_map(|path| normalize(path.as_ref())).collect();
    for path in &paths {
      self.watched.remove(path);
    }
    self.engine.unwatch(paths, self.now, &mut out);
    self.absorb(out, Call::Unwatch);
  }

  /// Hands up to `count` waiting raw notifications to the engine. Says how many there were.
  pub fn deliver(&mut self, count: usize) -> usize {
    let raws = B::take_raw(&self.fs, count);
    let delivered = raws.len();
    for raw in raws {
      self.engine.signal(Signal::Raw(raw), self.now);
    }
    delivered
  }

  /// Moves the clock of the engine and of the disk. Nothing else happens.
  pub fn advance(&mut self, by: Duration) {
    self.now += by;
    self.fs.advance(by);
  }

  pub fn tick(&mut self) {
    let mut out = Output::default();
    self.engine.tick(self.now, &mut out);
    self.absorb(out, Call::Tick);
  }

  /// Moves the clock to the next deadline of the engine and ticks. `false`: there is none.
  pub fn step(&mut self) -> bool {
    let Some(deadline) = self.engine.deadline() else { return false };
    self.advance(deadline.saturating_duration_since(self.now));
    self.tick();
    true
  }

  /// Delivers everything and runs every deadline until the engine has nothing left to find:
  /// past the delays, the poll and the time in which a young file is checked by hash. Then the
  /// engine has caught up with the disk, and that is checked.
  pub fn settle(&mut self) {
    let end = self.now + self.settle;
    // Far more ticks than a settle needs. An engine whose deadline never passes would spin
    // in the watcher thread, and here it fails the test in place of hanging it.
    for ticks in 0.. {
      assert!(
        ticks < 100_000,
        "a deadline of the engine never passes: it has work that no tick does"
      );
      self.deliver(usize::MAX);
      match self.engine.deadline() {
        Some(deadline) if deadline <= end => {
          self.advance(deadline.saturating_duration_since(self.now));
          self.tick();
        }
        _ => break,
      }
    }
    self.advance(end - self.now);
    self.check_caught_up();
  }

  pub fn take_events(&mut self) -> Vec<Event> {
    std::mem::take(&mut self.events)
  }

  fn absorb(&mut self, mut out: Output, call: Call) {
    // The order of a batch is not promised. It is sorted here, once, for every test.
    out.events.sort_by(|a, b| a.path.cmp(&b.path));
    let paths: BTreeSet<&Path> = out.events.iter().map(|event| event.path.as_path()).collect();
    assert_eq!(paths.len(), out.events.len(), "a batch has one event for a path: {:?}", out.events);
    for event in &out.events {
      let known = match event.kind {
        EventKind::Create => !self.known.insert(event.path.clone()),
        EventKind::Update => self.known.contains(&event.path),
        EventKind::Delete => self.known.remove(&event.path),
      };
      assert_eq!(
        known,
        event.kind != EventKind::Create,
        "{event:?} does not fit what the handler knows"
      );
    }
    let view = self.engine.view();
    match call {
      Call::Watch => {
        // What `watch` found is known without an event. Nothing may go away without one.
        assert!(self.known.is_subset(&view), "a file went without an event in `watch`");
        self.known = view;
      }
      Call::Unwatch => {
        // What only the paths of `unwatch` covered is forgotten without an event. Nothing
        // may become known without one. Whether the right files were forgotten is seen when
        // everything settles: then the handler knows what a walk of the disk finds.
        assert!(view.is_subset(&self.known), "unwatch made a file known without an event");
        self.known = view;
      }
      Call::Tick => {
        assert_eq!(self.known, view, "the handler and the engine know different files");
      }
    }
    if let Err(broken) = self.engine.check_invariants() {
      panic!("invariant: {broken}");
    }
    self.events.append(&mut out.events);
    self.errors.append(&mut out.errors);
  }

  /// Panics when the engine has not caught up with the disk: a stored fingerprint is not the
  /// one on disk, or the handler knows other files than a walk of the disk finds.
  fn check_caught_up(&self) {
    let ignored = |path: &Path, kind: EntryKind| {
      self.ignored.as_ref().is_some_and(|ignored| ignored(path, kind))
    };
    let (stored, entries) = (self.engine.fingerprints(), self.engine.link_entries());
    if let Err(stale) = check_fingerprints(&self.fs, &self.watched, &stored, &entries) {
      panic!("not caught up: {stale}");
    }
    if let Err(wrong) = self.fs.quiet(|| self.engine.check_spellings()) {
      panic!("not caught up: {wrong}");
    }
    let on_disk = reportable(&self.fs, &ignored, &self.watched);
    assert_eq!(self.known, on_disk, "not caught up: the handler does not know the files on disk");
  }
}

impl<B: Deliver> Drop for Driver<B> {
  /// A story that does not say how it ends would check less than the others, and nobody would
  /// see it.
  fn drop(&mut self) {
    // A test that failed is not failed a second time.
    assert!(
      self.ended || std::thread::panicking(),
      "the story has no ending: call `caught_up`, or `not_caught_up` with the reason"
    );
  }
}

/// A backend the [`Driver`] can run: it says where its raw notifications wait.
pub trait Deliver: Backend {
  fn take_raw(fs: &SimFs, count: usize) -> Vec<Self::Raw>;
}

impl Deliver for SimBackend {
  fn take_raw(fs: &SimFs, count: usize) -> Vec<Self::Raw> {
    fs.take_raw(count)
  }
}

impl Deliver for PollBackend {
  fn take_raw(_fs: &SimFs, _count: usize) -> Vec<Self::Raw> {
    Vec::new()
  }
}

/// The call of the engine whose output the driver takes in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Call {
  Watch,
  Unwatch,
  Tick,
}

/// The part of a driver that does not depend on the backend type, so that one test body can
/// run on every backend.
pub trait Sim {
  fn fs(&self) -> &SimFs;
  /// Watches the paths and gives the events of that call.
  fn watch(&mut self, paths: &[&str]) -> Vec<String>;
  /// Takes the watched paths back and gives the events of that call.
  fn unwatch(&mut self, paths: &[&str]) -> Vec<String>;
  /// See [`Driver::advance`].
  fn advance(&mut self, by: Duration);
  /// See [`window`].
  fn window(&mut self) -> Vec<String>;
  /// Delivers everything, and says whether the engine has work then, now or later. A watcher
  /// that watches nothing has none: no poll either.
  fn something_is_due(&mut self) -> bool;
  /// Everything that still comes, batch after batch.
  fn settled(&mut self) -> Vec<String>;
  /// See [`caught_up`].
  fn caught_up(&mut self);
  /// The files the handler knows.
  fn known(&self) -> Vec<String>;
  /// How many deletes are pending.
  fn pending_deletes(&self) -> usize;
  fn errors(&self) -> Vec<ErrorKind>;
  fn overflow(&mut self);
}

impl<B: Deliver> Sim for Driver<B> {
  fn fs(&self) -> &SimFs {
    &self.fs
  }

  fn watch(&mut self, paths: &[&str]) -> Vec<String> {
    Driver::watch(self, paths);
    taken(self)
  }

  fn unwatch(&mut self, paths: &[&str]) -> Vec<String> {
    Driver::unwatch(self, paths);
    taken(self)
  }

  fn advance(&mut self, by: Duration) {
    Driver::advance(self, by);
  }

  fn window(&mut self) -> Vec<String> {
    window(self)
  }

  fn something_is_due(&mut self) -> bool {
    self.deliver(usize::MAX);
    self.engine.deadline().is_some()
  }

  fn settled(&mut self) -> Vec<String> {
    settled(self)
  }

  fn caught_up(&mut self) {
    caught_up(self);
  }

  fn known(&self) -> Vec<String> {
    self.known.iter().map(|path| path.to_str().unwrap().to_owned()).collect()
  }

  fn pending_deletes(&self) -> usize {
    self.engine.pending_deletes().len()
  }

  fn errors(&self) -> Vec<ErrorKind> {
    self.errors.iter().map(Error::kind).collect()
  }

  fn overflow(&mut self) {
    self.engine.overflow(self.now);
  }
}

/// Runs `test` on the given simulated backends, each time on a new disk. `on` names the
/// backend in a failure message.
pub fn on_modes(modes: &[Mode], options: &Options, test: impl Fn(&mut dyn Sim, &str)) {
  for &mode in modes {
    test(&mut Driver::sim(mode, options.clone()), &format!("{mode:?}"));
  }
}

/// Runs `test` on the three simulated backends and on the poll backend.
pub fn on_all(options: &Options, test: impl Fn(&mut dyn Sim, &str)) {
  on_modes(&Mode::ALL, options, &test);
  test(&mut Driver::<PollBackend>::poll(options.clone()), "Poll");
}

pub type Make = Box<dyn Fn() -> Box<dyn Sim>>;

/// One way to make a driver for each backend, so that a story can be told many times. The names
/// are those of [`on_all`].
pub fn makes(options: &Options) -> [(&'static str, Make); 4] {
  let with = |make: fn(Options) -> Box<dyn Sim>| -> Make {
    let options = options.clone();
    Box::new(move || make(options.clone()))
  };
  [
    ("Inode", with(|options| Box::new(Driver::sim(Mode::Inode, options)))),
    ("Prefix", with(|options| Box::new(Driver::sim(Mode::Prefix, options)))),
    ("Handle", with(|options| Box::new(Driver::sim(Mode::Handle, options)))),
    ("Poll", with(|options| Box::new(Driver::<PollBackend>::poll(options)))),
  ]
}

/// Runs the story on a thread with a quarter of the usual stack: a recursion through thousands
/// of nodes does not fit on it, and a story that runs on it has none.
pub fn on_a_small_stack(story: impl FnOnce() + Send + 'static) {
  let thread = std::thread::Builder::new().stack_size(256 * 1024).spawn(story).unwrap();
  thread.join().unwrap();
}

/// The end of a story. Everything settles, the engine has caught up with the disk (which
/// [`Driver::settle`] checks), and it has nothing left to do.
pub fn caught_up<B: Deliver>(driver: &mut Driver<B>) {
  driver.settle();
  if let Err(left) = driver.engine.check_idle() {
    panic!("the story is over and the engine is not done: {left}");
  }
  driver.ended = true;
}

/// The end of a story that stops on purpose where the engine cannot have caught up with the
/// disk. `why` says what keeps it behind.
pub fn not_caught_up<B: Deliver>(driver: &mut Driver<B>, why: &str) {
  assert!(!why.is_empty(), "say what keeps the engine behind");
  driver.ended = true;
}

/// No events.
pub const NONE: [&str; 0] = [];

/// `C /p/a`, `U /p/a`, `D /p/a`.
pub fn show(events: &[Event]) -> Vec<String> {
  let letter = |kind| match kind {
    EventKind::Create => 'C',
    EventKind::Update => 'U',
    EventKind::Delete => 'D',
  };
  events.iter().map(|event| format!("{} {}", letter(event.kind), event.path.display())).collect()
}

/// Lets everything settle and gives the events since the last call.
pub fn settled<B: Deliver>(driver: &mut Driver<B>) -> Vec<String> {
  driver.settle();
  show(&driver.take_events())
}

/// The events that are there now, without moving the clock.
pub fn taken<B: Deliver>(driver: &mut Driver<B>) -> Vec<String> {
  show(&driver.take_events())
}

/// Delivers everything and runs what is due next: after a change, that is the flush.
pub fn flush<B: Deliver>(driver: &mut Driver<B>) {
  driver.deliver(usize::MAX);
  assert!(driver.step(), "nothing is due");
}

/// One window: [`flush`], and the events of it. With the poll backend that is one poll.
pub fn window<B: Deliver>(driver: &mut Driver<B>) -> Vec<String> {
  flush(driver);
  taken(driver)
}

/// The window in which the delete is pending, then the rest.
pub fn window_then_settled(driver: &mut dyn Sim) -> Vec<String> {
  let mut events = driver.window();
  events.append(&mut driver.settled());
  sorted(events)
}

/// The list in sorted order: for events of more than one batch.
pub fn sorted<T: Ord>(mut list: Vec<T>) -> Vec<T> {
  list.sort();
  list
}

/// The directory and the want of each `watch` and `unwatch` call that the engine made to the
/// backend, in the order of the calls. `None`: the call was `unwatch`.
pub fn asked(fs: &SimFs) -> Vec<(String, Option<Want>)> {
  let call = |call: &WatchCall| (call.dir.display().to_string(), call.want);
  fs.watch_calls().iter().map(call).collect()
}

/// Where an OS watch is now, sorted by path, and what each one reports.
pub fn os_watches(fs: &SimFs) -> Vec<(String, Want)> {
  fs.os_watches().into_iter().map(|(dir, want)| (dir.display().to_string(), want)).collect()
}

/// One row of a list that [`asked`] or [`os_watches`] is compared with.
pub fn at<W>(dir: &str, want: W) -> (String, W) {
  (dir.to_owned(), want)
}

pub fn ignore_node_modules() -> Options {
  Options {
    ignored: Some(Arc::new(|path: &Path, kind| {
      kind == EntryKind::Dir && path.ends_with("node_modules")
    })),
    ..Options::default()
  }
}
