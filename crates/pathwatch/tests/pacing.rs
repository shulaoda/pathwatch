//! The rest after a poll, with the poll backend on a real directory and a disk whose clock the
//! test moves: nine times the looks of the round, the reads of young files once, and five
//! seconds at most.

use std::{
  cell::Cell,
  ffi::{OsStr, OsString},
  fs, io,
  path::{Path, PathBuf},
  time::{Duration, Instant, SystemTime},
};

use pathwatch::{
  backend::Sink,
  engine::{Engine, Options, Output},
  fs::{Entry, Fs, Meta, StdFs},
  poll::PollBackend,
};

/// The real disk, with a clock that moves by a price for each listing and for each read of a
/// content, and by nothing else.
struct Priced {
  listing: Duration,
  read: Duration,
  ticks: Cell<Duration>,
}

impl Priced {
  fn pay(&self, price: Duration) {
    self.ticks.set(self.ticks.get() + price);
  }
}

impl Fs for Priced {
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

const INTERVAL: Duration = Duration::from_millis(100);

/// A watched directory of `files` files written just now, polled every 100 ms. Gives the time
/// from the start of the first poll to the next one: what the round took on the clock of the
/// disk, and the pause after it.
fn pause_after_a_poll(files: usize, listing: Duration, read: Duration) -> Duration {
  let temp = tempfile::tempdir().unwrap();
  // `canonicalize` gives the `\\?\` form on Windows, which the engine takes for the plain one.
  let dir = temp.path().canonicalize().unwrap();
  let dir = pathwatch::path::normalize(&dir).unwrap().into_owned();
  for n in 0..files {
    fs::write(dir.join(format!("f{n}.txt")), "1").unwrap();
  }
  let options = Options {
    poll_interval: INTERVAL,
    loose_spelling: cfg!(any(target_vendor = "apple", windows)),
    ..Options::default()
  };
  let backend = PollBackend::new(Sink::new(|_| {})).unwrap();
  let disk = Priced { listing, read, ticks: Cell::new(Duration::ZERO) };
  let mut engine = Engine::new(backend, disk, options);
  let (mut now, mut out) = (Instant::now(), Output::default());
  engine.watch(vec![dir.clone()], now, &mut out);
  assert!(out.events.is_empty() && out.errors.is_empty(), "{out:?}");

  // The first poll: the interval after the flush of `watch`, which paid for its reads too.
  now = engine.deadline().expect("a poll is due");
  engine.tick(now, &mut out);
  assert!(out.errors.is_empty(), "{:?}", out.errors);
  let next = engine.deadline().expect("a poll is due");
  next.saturating_duration_since(now)
}

/// The looks of a round come again at every round: the rest is nine times what they took.
#[test]
fn the_looks_of_a_round_rest_nine_times_as_long() {
  // A listing costs 300 ms, and the reads of the young files cost nothing.
  let pause = pause_after_a_poll(20, Duration::from_millis(300), Duration::ZERO);
  // The round took at least one listing, and the rest is nine times the looks.
  assert!(pause >= Duration::from_secs(3), "{pause:?}");
  assert!(pause < Duration::from_secs(15), "{pause:?}");
}

/// The reads of young files are paid once: a round that read twenty files just written rests
/// for the time of the reads, not nine times as long.
#[test]
fn the_reads_of_young_files_are_paid_once() {
  let pause = pause_after_a_poll(20, Duration::ZERO, Duration::from_millis(10));
  // Twenty reads of 10 ms: the round took 200 ms, and the rest is 200 ms, not 1.8 s.
  assert_eq!(pause, Duration::from_millis(200 + 200), "{pause:?}");
}

/// The rest is five seconds at most: a burst of young files does not put the next poll off for
/// minutes.
#[test]
fn the_rest_is_five_seconds_at_most() {
  let pause = pause_after_a_poll(20, Duration::ZERO, Duration::from_secs(1));
  assert_eq!(pause, Duration::from_secs(20 + 5), "{pause:?}");
}
