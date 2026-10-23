//! The poll backend and the reads of the disk on a real directory, with a clock that the test
//! moves: a file whose time is far ahead of the clock, the pause after a poll, the engine after
//! its backend stopped, and the hash of a file.

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
  ErrorKind, Event, EventKind,
  backend::{Signal, Sink, poll::PollBackend},
  engine::{Engine, Options, Output},
  fs::{Entry, Fs, Meta, StdFs},
};

const INTERVAL: Duration = Duration::from_millis(100);

/// The real disk, with a count of the files whose content was read, and a clock that moves by
/// a price for each listing and for each read of a content, and by nothing else: with no
/// price, no pause after a slow poll moves the next one past the step of a test.
struct Disk {
  reads: Rc<Cell<usize>>,
  listing: Duration,
  read: Duration,
  ticks: Cell<Duration>,
}

impl Disk {
  fn pay(&self, price: Duration) {
    self.ticks.set(self.ticks.get() + price);
  }
}

impl Fs for Disk {
  fn lstat(&self, path: &Path) -> io::Result<Meta> {
    StdFs.lstat(path)
  }

  fn stat(&self, path: &Path) -> io::Result<Meta> {
    StdFs.stat(path)
  }

  fn read_dir(&self, dir: &Path, each: &mut dyn FnMut(&OsStr, Entry)) -> io::Result<()> {
    self.pay(self.listing);
    StdFs.read_dir(dir, each)
  }

  fn hash(&self, path: &Path, limit: u64) -> io::Result<Option<u64>> {
    self.reads.set(self.reads.get() + 1);
    self.pay(self.read);
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

  fn ticks(&self) -> Duration {
    self.ticks.get()
  }

  fn listing_lags(&self) -> bool {
    StdFs.listing_lags()
  }
}

/// The directory as the engine takes it: `canonicalize` gives the `\\?\` form on Windows, and
/// `normalize` the plain one.
fn normalized(dir: &Path) -> PathBuf {
  let dir = dir.canonicalize().unwrap();
  pathwatch::path::normalize(&dir).unwrap()
}

/// The engine with the poll backend on a real directory, polled every 100 ms.
struct Polled {
  engine: Engine<PollBackend, Disk>,
  reads: Rc<Cell<usize>>,
  /// The clock of the engine: the test moves it, so nothing here waits.
  now: Instant,
}

impl Polled {
  /// Watches `dir`, on a disk whose clock moves by these prices.
  fn watch(dir: &Path, listing: Duration, read: Duration) -> Self {
    let reads = Rc::new(Cell::new(0));
    let options = Options {
      poll_interval: INTERVAL,
      loose_spelling: cfg!(any(target_vendor = "apple", windows)),
      ..Options::default()
    };
    let backend = PollBackend::new(Sink::new(|_| {})).unwrap();
    let disk = Disk { reads: Rc::clone(&reads), listing, read, ticks: Cell::new(Duration::ZERO) };
    let mut engine = Engine::new(backend, disk, options);
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

/// A file whose modification time is far ahead of the clock is young for a while after its
/// first look, and not for ever.
#[test]
fn a_time_far_ahead_is_young_for_a_while_and_not_for_ever() {
  let step = Duration::from_millis(500);
  let temp = tempfile::tempdir().unwrap();
  let dir = normalized(temp.path());
  let (old, ahead) = (dir.join("old.txt"), dir.join("ahead.txt"));
  let now = SystemTime::now();
  let (past, future) = (now - Duration::from_secs(3600), now + Duration::from_secs(3600));
  write_at(&old, "one", past);
  write_at(&ahead, "one", future);

  let mut polled = Polled::watch(&dir, Duration::ZERO, Duration::ZERO);
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

/// A watched directory of `files` files written just now. Gives the time from the start of the
/// first poll to the next one: what the round took on the clock of the disk, and the pause
/// after it.
fn pause_after_a_poll(files: usize, listing: Duration, read: Duration) -> Duration {
  let temp = tempfile::tempdir().unwrap();
  let dir = normalized(temp.path());
  for n in 0..files {
    fs::write(dir.join(format!("f{n}.txt")), "1").unwrap();
  }
  let mut polled = Polled::watch(&dir, listing, read);

  // The first poll: the interval after the flush of `watch`, which paid for its reads too.
  let now = polled.engine.deadline().expect("a poll is due");
  let mut out = Output::default();
  polled.engine.tick(now, &mut out);
  assert!(out.errors.is_empty(), "{:?}", out.errors);
  let next = polled.engine.deadline().expect("a poll is due");
  next.saturating_duration_since(now)
}

/// A round that takes longer than the poll interval is followed by a pause as long as itself:
/// polling takes half of the time at most.
#[test]
fn a_slow_round_is_followed_by_a_pause_as_long() {
  // A listing costs 300 ms, and the reads of the young files cost nothing.
  let pause = pause_after_a_poll(20, Duration::from_millis(300), Duration::ZERO);
  // The round took at least one listing, and the pause is as long as the round.
  assert!(pause >= Duration::from_millis(600), "{pause:?}");
  assert!(pause < Duration::from_secs(2), "{pause:?}");
}

/// The reads of young files count in the round: a round that read twenty files just written
/// is followed by a pause as long as the reads took.
#[test]
fn the_reads_of_young_files_count_in_the_round() {
  let pause = pause_after_a_poll(20, Duration::ZERO, Duration::from_millis(10));
  // Twenty reads of 10 ms: the round took 200 ms, and so does the pause.
  assert_eq!(pause, Duration::from_millis(200 + 200), "{pause:?}");
}

/// The engine after its backend stopped: it works as a watcher that only polls, at the interval
/// of the settings and with no delete delay.
#[test]
fn a_stopped_backend_is_polled_as_a_watcher_that_only_polls() {
  let dir = tempfile::tempdir().unwrap();
  let hour = Duration::from_secs(3600);
  let options = Options {
    poll_interval: hour,
    delete_delay: hour,
    loose_spelling: cfg!(any(target_vendor = "apple", windows)),
    ..Options::default()
  };
  let backend = PollBackend::new(Sink::new(|_| {})).unwrap();
  let mut engine = Engine::new(backend, StdFs, options.clone());
  engine.fall_back_to(Options { poll_interval: INTERVAL, delete_delay: Duration::ZERO, ..options });
  let (start, mut out) = (Instant::now(), Output::default());
  engine.watch(vec![pathwatch::path::normalize(dir.path()).unwrap()], start, &mut out);
  let first_poll = engine.deadline().expect("only the poll is due, in an hour");
  assert!(first_poll >= start + hour && first_poll < start + hour + Duration::from_secs(1));

  engine.signal(Signal::Failed(io::Error::other("gone")), start);
  // The loss is reported, and everything is looked at once: that is no poll.
  let looked = engine.deadline().expect("the look is due");
  engine.tick(looked, &mut out);
  assert_eq!(out.errors.len(), 1);
  assert_eq!(out.errors[0].kind(), ErrorKind::Io);
  assert!(out.events.is_empty(), "{:?}", out.events);

  fs::write(dir.path().join("a.txt"), "1").unwrap();
  // No backend tells of the file: only a poll finds it.
  while out.events.is_empty() {
    let now = engine.deadline().expect("the poll is always due");
    assert!(now < start + Duration::from_secs(1), "no poll came in a second");
    engine.tick(now, &mut out);
  }
  assert_eq!(out.events.len(), 1);
  assert_eq!(
    (out.events[0].kind, &out.events[0].path),
    (EventKind::Create, &dir.path().join("a.txt"))
  );

  // The delete delay of an hour is gone with the backend: the poll that finds the file gone
  // tells it.
  out.events.clear();
  fs::remove_file(dir.path().join("a.txt")).unwrap();
  while out.events.is_empty() {
    let now = engine.deadline().expect("the poll is always due");
    assert!(now < start + Duration::from_secs(2), "no Delete came in two seconds");
    engine.tick(now, &mut out);
  }
  assert_eq!(
    (out.events[0].kind, &out.events[0].path),
    (EventKind::Delete, &dir.path().join("a.txt"))
  );
}

/// The hash reads up to the limit and no further.
#[test]
fn a_file_longer_than_the_limit_has_no_hash() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("f.txt");
  fs::write(&path, [7; 1000]).unwrap();
  assert!(StdFs.hash(&path, 1000).unwrap().is_some());
  assert_eq!(StdFs.hash(&path, 999).unwrap(), None);
  // The hash does not depend on the limit.
  assert_eq!(StdFs.hash(&path, 1000).unwrap(), StdFs.hash(&path, u64::MAX).unwrap());

  // The limit ends the read, not the end of the file: a file that grew since the look is not
  // read whole.
  let big = dir.path().join("big.bin");
  File::create(&big).unwrap().set_len(96 * 1024 * 1024).unwrap();
  let started = Instant::now();
  assert_eq!(StdFs.hash(&big, 16 * 1024 * 1024).unwrap(), None);
  assert!(started.elapsed() < Duration::from_secs(5));
}

/// Nothing but a regular file is opened: not a directory, and not a link that takes the place
/// of a file after the look.
#[test]
fn a_link_is_not_followed() {
  let dir = tempfile::tempdir().unwrap();
  assert!(!matches!(StdFs.hash(dir.path(), u64::MAX), Ok(Some(_))));
  let target = dir.path().join("target.txt");
  fs::write(&target, "1").unwrap();
  assert!(StdFs.hash(&target, u64::MAX).unwrap().is_some());
  let link = dir.path().join("link.txt");
  #[cfg(unix)]
  std::os::unix::fs::symlink(&target, &link).unwrap();
  #[cfg(windows)]
  {
    // A junction stands for another name too.
    let junction = dir.path().join("junction");
    let made = std::process::Command::new("cmd")
      .args(["/C", "mklink", "/J"])
      .arg(&junction)
      .arg(dir.path())
      .output();
    assert!(made.is_ok_and(|output| output.status.success()), "mklink /J failed");
    assert!(!matches!(StdFs.hash(&junction, u64::MAX), Ok(Some(_))), "the junction was followed");
    fs::remove_dir(&junction).unwrap();
    // Making a symbolic link needs a privilege that this user may not have.
    if std::os::windows::fs::symlink_file(&target, &link).is_err() {
      return;
    }
  }
  assert!(!matches!(StdFs.hash(&link, u64::MAX), Ok(Some(_))), "the link was followed");
}

/// A FIFO that takes the place of a file after the look: the open does not wait for a writer.
#[cfg(unix)]
#[test]
fn a_fifo_does_not_block() {
  let dir = tempfile::tempdir().unwrap();
  let fifo = dir.path().join("fifo");
  let made = std::process::Command::new("mkfifo").arg(&fifo).status();
  assert!(made.is_ok_and(|status| status.success()), "mkfifo failed");
  let started = Instant::now();
  assert_eq!(StdFs.hash(&fifo, u64::MAX).unwrap(), None);
  assert!(started.elapsed() < Duration::from_secs(5));
}
