//! A file whose modification time is far ahead of the clock, on a real directory with the poll
//! backend: it is young for a while after its first look, and not for ever.

use std::{
  cell::Cell,
  ffi::{OsStr, OsString},
  fs::{self, File},
  io,
  path::{Path, PathBuf},
  rc::Rc,
  time::{Duration, Instant, SystemTime},
};

use pathwatch::{
  Event, EventKind,
  backend::Sink,
  engine::{Engine, Options, Output},
  fs::{Entry, Fs, Meta, StdFs},
  poll::PollBackend,
};

/// The real disk, and a count of the files whose content was read.
struct Counted(Rc<Cell<usize>>);

impl Fs for Counted {
  fn lstat(&self, path: &Path) -> io::Result<Meta> {
    StdFs.lstat(path)
  }

  fn stat(&self, path: &Path) -> io::Result<Meta> {
    StdFs.stat(path)
  }

  fn read_dir(&self, dir: &Path, each: &mut dyn FnMut(&OsStr, Entry)) -> io::Result<()> {
    StdFs.read_dir(dir, each)
  }

  fn hash(&self, path: &Path, limit: u64) -> io::Result<Option<u64>> {
    self.0.set(self.0.get() + 1);
    StdFs.hash(path, limit)
  }

  fn link_target(&self, path: &Path) -> io::Result<PathBuf> {
    StdFs.link_target(path)
  }

  fn real_path(&self, path: &Path) -> io::Result<PathBuf> {
    StdFs.real_path(path)
  }

  fn name_on_disk(&self, path: &Path) -> io::Result<OsString> {
    StdFs.name_on_disk(path)
  }

  fn now(&self) -> SystemTime {
    StdFs.now()
  }

  /// The looks take no time on this clock: no rest after a slow poll moves the next one past
  /// the step of the test.
  fn ticks(&self) -> Duration {
    Duration::ZERO
  }

  fn listing_lags(&self) -> bool {
    StdFs.listing_lags()
  }
}

struct Polled {
  engine: Engine<PollBackend, Counted>,
  reads: Rc<Cell<usize>>,
  /// The clock of the engine: the test moves it, so nothing here waits.
  now: Instant,
}

impl Polled {
  fn watch(dir: &Path) -> Self {
    let reads = Rc::new(Cell::new(0));
    let options = Options {
      poll_interval: Duration::from_millis(100),
      loose_spelling: cfg!(any(target_vendor = "apple", windows)),
      ..Options::default()
    };
    let backend = PollBackend::new(Sink::new(|_| {})).unwrap();
    let mut engine = Engine::new(backend, Counted(Rc::clone(&reads)), options);
    let (now, mut out) = (Instant::now(), Output::default());
    engine.watch(vec![dir.to_owned()], now, &mut out);
    assert!(out.events.is_empty() && out.errors.is_empty(), "{out:?}");
    Self { engine, reads, now }
  }

  /// One poll, `after` later on the clock of the engine. Gives its events and how many files
  /// it read.
  fn poll(&mut self, after: Duration) -> (Vec<Event>, usize) {
    let (before, mut out) = (self.reads.get(), Output::default());
    self.now += after;
    self.engine.tick(self.now, &mut out);
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    (out.events, self.reads.get() - before)
  }
}

fn write_at(path: &Path, content: &str, modified: SystemTime) {
  fs::write(path, content).unwrap();
  File::options().write(true).open(path).unwrap().set_modified(modified).unwrap();
}

fn update(path: &Path) -> Vec<Event> {
  vec![Event { path: path.to_owned(), kind: EventKind::Update }]
}

#[test]
fn a_time_far_ahead_is_young_for_a_while_and_not_for_ever() {
  let step = Duration::from_millis(500);
  let dir = tempfile::tempdir().unwrap();
  let dir = dir.path().canonicalize().unwrap();
  // `canonicalize` gives the `\?\` form on Windows, which the engine takes for the plain one.
  let dir = pathwatch::path::normalize(&dir).unwrap().into_owned();
  let (old, ahead) = (dir.join("old.txt"), dir.join("ahead.txt"));
  let now = SystemTime::now();
  let (past, future) = (now - Duration::from_secs(3600), now + Duration::from_secs(3600));
  write_at(&old, "one", past);
  write_at(&ahead, "one", future);

  let mut polled = Polled::watch(&dir);
  assert_eq!(polled.reads.get(), 1, "only the file with the time ahead is read");

  // While it is young, another content under the same time and size is found by its hash.
  write_at(&ahead, "two", future);
  assert_eq!(polled.poll(step), (update(&ahead), 1));
  assert_eq!(polled.poll(step), (Vec::new(), 1));

  // The long time after the first look at this fingerprint: one last read, then none.
  let mut reads = 0;
  for _ in 0..6 {
    let (events, read) = polled.poll(step);
    assert_eq!(events, []);
    reads += read;
  }
  assert!(reads <= 4, "the file is read at every poll: {reads}");
  for _ in 0..10 {
    assert_eq!(polled.poll(step), (Vec::new(), 0));
  }

  // A write gives another fingerprint, and the time of the first look counts anew.
  write_at(&ahead, "three", future + Duration::from_secs(1));
  assert_eq!(polled.poll(step), (update(&ahead), 1));
  write_at(&ahead, "four!", future + Duration::from_secs(1));
  assert_eq!(polled.poll(step), (update(&ahead), 1));
  for _ in 0..10 {
    polled.poll(step);
  }
  assert_eq!(polled.poll(step), (Vec::new(), 0));

  // A write with the time of now is found by its fingerprint.
  fs::write(&ahead, "five").unwrap();
  assert_eq!(polled.poll(step).0, update(&ahead));
  assert_eq!(polled.poll(Duration::from_secs(5)).0, []);
}
