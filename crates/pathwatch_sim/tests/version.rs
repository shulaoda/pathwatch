//! Two things the engine takes from the disk on its word: the version of a file in place of its
//! hash where an OS watch reports writes (NTFS), and a listing that can lag behind a write.

#![cfg(unix)]

use std::{
  collections::BTreeMap,
  ffi::{OsStr, OsString},
  io,
  path::{Path, PathBuf},
  sync::{
    Arc, Mutex, MutexGuard, PoisonError,
    atomic::{AtomicUsize, Ordering},
  },
  time::{Duration, Instant, SystemTime},
};

use pathwatch::{
  backend::Signal,
  engine::{Engine, Options, Output},
  fs::{Entry, Fs, Meta},
  poll::PollBackend,
};
use pathwatch_sim::{Deliver, Mode, NONE, SimBackend, SimFs, show};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
  mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The simulated disk with two things of its own: a version for each file that the test sets,
/// and a listing that gives a file as it was when the test took a picture of it.
#[derive(Clone)]
struct Disk {
  fs: SimFs,
  versions: Arc<Mutex<BTreeMap<PathBuf, u64>>>,
  version_calls: Arc<AtomicUsize>,
  lags: bool,
  pictures: Arc<Mutex<BTreeMap<PathBuf, Meta>>>,
}

impl Disk {
  fn new(lags: bool) -> Self {
    Self {
      fs: SimFs::new(),
      versions: Arc::default(),
      version_calls: Arc::default(),
      lags,
      pictures: Arc::default(),
    }
  }

  fn set_version(&self, path: &str, version: u64) {
    lock(&self.versions).insert(PathBuf::from(path), version);
  }

  fn version_calls(&self) -> usize {
    self.version_calls.load(Ordering::Relaxed)
  }

  /// From now on a listing gives this file as it is now.
  fn picture(&self, path: &str) {
    let meta = self.fs.lstat(Path::new(path)).unwrap();
    lock(&self.pictures).insert(PathBuf::from(path), meta);
  }
}

impl Fs for Disk {
  fn lstat(&self, path: &Path) -> io::Result<Meta> {
    self.fs.lstat(path)
  }

  fn stat(&self, path: &Path) -> io::Result<Meta> {
    self.fs.stat(path)
  }

  fn read_dir(&self, dir: &Path, each: &mut dyn FnMut(&OsStr, Entry)) -> io::Result<()> {
    let pictures = lock(&self.pictures);
    self.fs.read_dir(dir, &mut |name, entry| {
      let entry = match (entry, pictures.get(&dir.join(name))) {
        (Entry::File { .. }, Some(meta)) => {
          Entry::File { modified: meta.modified, size: meta.size, file_id: meta.file_id }
        }
        (entry, _) => entry,
      };
      each(name, entry);
    })
  }

  fn hash(&self, path: &Path, limit: u64) -> io::Result<Option<u64>> {
    self.fs.hash(path, limit)
  }

  fn link_target(&self, path: &Path) -> io::Result<PathBuf> {
    self.fs.link_target(path)
  }

  fn real_path(&self, path: &Path) -> io::Result<PathBuf> {
    self.fs.real_path(path)
  }

  fn name_on_disk(&self, path: &Path) -> io::Result<OsString> {
    self.fs.name_on_disk(path)
  }

  fn now(&self) -> SystemTime {
    self.fs.now()
  }

  fn listing_lags(&self) -> bool {
    self.lags
  }

  fn version(&self, path: &Path) -> Option<u64> {
    self.version_calls.fetch_add(1, Ordering::Relaxed);
    lock(&self.versions).get(path).copied()
  }
}

struct Run<B: Deliver> {
  engine: Engine<B, Disk>,
  disk: Disk,
  now: Instant,
}

impl<B: Deliver> Run<B> {
  fn new(backend: B, disk: &Disk) -> Self {
    let engine = Engine::new(backend, disk.clone(), Options::default());
    Self { engine, disk: disk.clone(), now: Instant::now() }
  }

  fn watch(&mut self, path: &str) -> Vec<String> {
    let mut out = Output::default();
    self.engine.watch(vec![PathBuf::from(path)], self.now, &mut out);
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    show(&out.events)
  }

  /// Hands over what the OS has to say and runs the engine for this long. Gives the events.
  fn run(&mut self, time: Duration) -> Vec<String> {
    let end = self.now + time;
    let mut events = Vec::new();
    loop {
      for raw in B::take_raw(&self.disk.fs, usize::MAX) {
        self.engine.signal(Signal::Raw(raw), self.now);
      }
      let Some(deadline) = self.engine.deadline().filter(|at| *at <= end) else { break };
      self.disk.fs.advance(deadline.saturating_duration_since(self.now));
      self.now = deadline;
      let mut out = Output::default();
      self.engine.tick(self.now, &mut out);
      assert!(out.errors.is_empty(), "{:?}", out.errors);
      events.extend(out.events);
    }
    self.disk.fs.advance(end.saturating_duration_since(self.now));
    self.now = end;
    show(&events)
  }
}

/// Where an OS watch reports writes, the version stands in for the hash: a young file is not
/// read, and a look without the word of the OS compares the versions.
#[test]
fn the_version_stands_in_for_the_hash() {
  let disk = Disk::new(false);
  // Every write of the same size keeps the fingerprint.
  disk.fs.set_time_step(Duration::from_secs(2));
  disk.fs.mkdir("/p").unwrap();
  disk.fs.write("/p/a", "1111").unwrap();
  disk.set_version("/p/a", 1);
  let mut run = Run::new(SimBackend::attach(&disk.fs, Mode::Inode), &disk);
  assert_eq!(run.watch("/p"), NONE);
  assert_eq!((disk.fs.calls().hash, disk.version_calls()), (0, 1));

  // The OS loses its word of the write: the version tells, without a read. The looks stay
  // inside the step of the clock.
  let look = Duration::from_millis(500);
  disk.fs.write("/p/a", "2222").unwrap();
  disk.set_version("/p/a", 2);
  disk.fs.lose_raw(usize::MAX);
  assert_eq!(run.run(look), ["U /p/a"]);
  assert_eq!(disk.fs.calls().hash, 0);

  // The disk gave the file no new number: that says nothing was written.
  disk.fs.write("/p/a", "3333").unwrap();
  disk.fs.lose_raw(usize::MAX);
  assert_eq!(run.run(look), NONE);

  // The word of the OS that the file was written counts, whatever the number says.
  disk.fs.write("/p/a", "4444").unwrap();
  assert_eq!(run.run(look), ["U /p/a"]);
  assert_eq!(disk.fs.calls().hash, 0);
}

/// Under the poll the version is not asked: it also moves with what is no write. The hash
/// finds the write.
#[test]
fn the_poll_asks_no_version() {
  let disk = Disk::new(false);
  disk.fs.set_time_step(Duration::from_secs(2));
  disk.fs.mkdir("/p").unwrap();
  disk.fs.write("/p/a", "1111").unwrap();
  disk.set_version("/p/a", 1);
  let mut run = Run::new(PollBackend, &disk);
  assert_eq!(run.watch("/p"), NONE);
  assert_eq!((disk.fs.calls().hash, disk.version_calls()), (1, 0));

  disk.fs.write("/p/a", "2222").unwrap();
  disk.set_version("/p/a", 2);
  assert_eq!(run.run(Duration::from_secs(3)), ["U /p/a"]);
  assert_eq!(disk.version_calls(), 0);
}

/// A listing that can lag behind a write is trusted for a file only where an OS watch reports
/// the write: under the poll the file is looked at, and the stale listing does no harm.
#[test]
fn a_listing_that_lags_is_not_trusted_under_the_poll() {
  let lstats = |lags: bool| {
    let disk = Disk::new(lags);
    disk.fs.set_listing_meta(true);
    disk.fs.mkdir("/p").unwrap();
    let mut run = Run::new(PollBackend, &disk);
    assert_eq!(run.watch("/p"), NONE);

    // The listing gives the file as it was before its second write.
    disk.fs.write("/p/a", "1").unwrap();
    disk.picture("/p/a");
    disk.fs.write("/p/a", "22").unwrap();
    let before = disk.fs.calls().lstat;
    assert_eq!(run.run(Duration::from_secs(1)), ["C /p/a"], "lags {lags}");
    let looks = disk.fs.calls().lstat - before;
    if lags {
      assert_eq!(run.run(Duration::from_secs(3)), NONE);
    }
    looks
  };
  // One look more: the file itself, in place of what the listing said of it.
  assert_eq!(lstats(true), lstats(false) + 1);
}
