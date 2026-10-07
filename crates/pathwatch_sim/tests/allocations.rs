//! How often the engine asks for memory, as budgets that fail when they are passed. The usual
//! work of a watcher that runs for days must ask for none, or for the paths of its events only.
//!
//! The disk and the backend of these stories ask for no memory themselves, so every request
//! that is counted is one of the engine. Each test counts on its own thread.

#![cfg(unix)]

use std::{
  alloc::{GlobalAlloc, Layout, System},
  cell::Cell,
  ffi::OsStr,
  io,
  path::{Path, PathBuf},
  sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
  },
  time::{Duration, Instant, SystemTime},
};

use pathwatch::{
  EventKind,
  backend::{Backend, Coverage, DirKey, Hint, Signal, Want},
  engine::{Engine, Options, Output},
  fs::{DirId, FileType, Fs, Meta},
};

/// The system allocator, with a counter for each thread.
struct Counting;

thread_local! {
  /// How often this thread asked for memory.
  static REQUESTS: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: every call goes to the system allocator with the arguments it came with. The counter
// does not touch the memory.
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    REQUESTS.set(REQUESTS.get() + 1);
    // SAFETY: the caller keeps the rules of `GlobalAlloc::alloc`.
    unsafe { System.alloc(layout) }
  }

  unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
    // SAFETY: the caller keeps the rules of `GlobalAlloc::dealloc`.
    unsafe { System.dealloc(block, layout) }
  }

  unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    REQUESTS.set(REQUESTS.get() + 1);
    // SAFETY: the caller keeps the rules of `GlobalAlloc::realloc`.
    unsafe { System.realloc(block, layout, new_size) }
  }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// How often `work` asks for memory.
fn requests(work: impl FnOnce()) -> u64 {
  let before = REQUESTS.get();
  work();
  REQUESTS.get() - before
}

/// A disk that asks for no memory: a name with a dot is a file, and every other name is a
/// directory. All files have the same size, and a test changes it to change them all.
#[derive(Clone)]
struct PlainDisk {
  size: Arc<AtomicU64>,
}

/// The time of this disk. Its files are older than that, so none is young, and none is read.
const NOW: Duration = Duration::from_secs(1000);

impl Fs for PlainDisk {
  fn lstat(&self, path: &Path) -> io::Result<Meta> {
    let is_file = path.extension().is_some();
    Ok(Meta {
      kind: if is_file { FileType::File } else { FileType::Dir },
      modified: SystemTime::UNIX_EPOCH,
      changed: SystemTime::UNIX_EPOCH,
      size: self.size.load(Ordering::Relaxed),
      file_id: 1,
      dir_id: DirId::default(),
    })
  }

  fn stat(&self, path: &Path) -> io::Result<Meta> {
    self.lstat(path)
  }

  fn read_dir(&self, _: &Path, _: &mut dyn FnMut(&OsStr, Option<FileType>)) -> io::Result<()> {
    Ok(())
  }

  fn hash(&self, _: &Path) -> io::Result<u64> {
    Ok(0)
  }

  fn link_target(&self, _: &Path) -> io::Result<PathBuf> {
    Err(io::ErrorKind::InvalidInput.into())
  }

  fn real_path(&self, path: &Path) -> io::Result<PathBuf> {
    Ok(path.to_path_buf())
  }

  fn now(&self) -> SystemTime {
    SystemTime::UNIX_EPOCH + NOW
  }
}

/// A backend that watches every directory, and whose raw notification is a hint already: a
/// file of a directory was written.
struct Direct {
  /// The key of each directory that the engine asked a watch for.
  keys: Arc<Mutex<Vec<(PathBuf, DirKey)>>>,
}

impl Backend for Direct {
  type Raw = (DirKey, &'static str);

  fn watch(&mut self, key: DirKey, dir: &Path, _: Want) -> Coverage {
    self.keys.lock().unwrap().push((dir.to_path_buf(), key));
    Coverage::Watched
  }

  fn unwatch(&mut self, _: DirKey) {}

  fn commit(&mut self) -> io::Result<()> {
    Ok(())
  }

  fn translate(&mut self, (dir, name): Self::Raw, hint: &mut dyn FnMut(Hint<'_>)) {
    hint(Hint::Written { dir, rel: Path::new(name) });
  }
}

/// An engine that watches five files of `/p/src`, each by its own path.
struct Story {
  engine: Engine<Direct, PlainDisk>,
  disk: PlainDisk,
  /// The key of `/p/src`.
  src: DirKey,
  now: Instant,
}

const FILES: [&str; 5] = ["a.js", "b.js", "c.js", "d.js", "e.js"];

fn paths() -> Vec<PathBuf> {
  FILES.iter().map(|file| Path::new("/p/src").join(file)).collect()
}

fn story() -> Story {
  let disk = PlainDisk { size: Arc::new(AtomicU64::new(1)) };
  let keys = Arc::default();
  let backend = Direct { keys: Arc::clone(&keys) };
  let mut engine = Engine::new(backend, disk.clone(), Options::default());
  let now = Instant::now();
  let mut out = Output::default();
  engine.watch(paths(), now, &mut out);
  assert!(out.events.is_empty() && out.errors.is_empty(), "the baseline is silent");
  let src = keys.lock().unwrap().iter().find(|(dir, _)| dir == Path::new("/p/src")).unwrap().1;
  Story { engine, disk, src, now }
}

impl Story {
  /// Moves the clock to the next deadline of the engine and runs what is due.
  fn step(&mut self, out: &mut Output) {
    self.now = self.engine.deadline().expect("something is due");
    self.engine.tick(self.now, out);
  }
}

/// Paths that are watched already, in their clean form: `watch` asks for no memory. A path is
/// not copied to be cleaned, and nothing new goes into the tree.
#[test]
fn watching_the_same_paths_again_asks_for_nothing() {
  let mut s = story();
  let (again, mut out) = (paths(), Output::default());
  assert_eq!(requests(|| s.engine.watch(again, s.now, &mut out)), 0);
  // A path that is not clean is copied once.
  let (unclean, mut out) = (vec![PathBuf::from("/p//src/./a.js")], Output::default());
  assert_eq!(requests(|| s.engine.watch(unclean, s.now, &mut out)), 1);
}

/// One changed file: the hint and its flush ask for memory once, for the path of the event.
/// The lists of the flush are used again. The first flush makes them.
#[test]
fn a_change_asks_for_the_path_of_its_event_only() {
  let mut s = story();
  let mut out = Output { events: Vec::with_capacity(16), errors: Vec::new() };
  for (round, size) in (2..6).enumerate() {
    s.disk.size.store(size, Ordering::Relaxed);
    let asked = requests(|| {
      for file in &FILES[..3] {
        s.engine.signal(Signal::Raw((s.src, file)), s.now);
      }
      s.step(&mut out);
    });
    assert_eq!(out.events.len(), 3, "{:?}", out.events);
    assert!(out.events.iter().all(|event| event.kind == EventKind::Update));
    out.events.clear();
    if round > 0 {
      assert_eq!(asked, 3, "round {round}");
    }
  }
}

/// A poll with nothing to find, next to OS watches for every directory: it asks for no memory.
#[test]
fn a_poll_asks_for_nothing() {
  let mut s = story();
  let mut out = Output::default();
  // The first poll makes its lists.
  s.step(&mut out);
  for _ in 0..3 {
    assert_eq!(requests(|| s.step(&mut out)), 0);
  }
  assert!(out.events.is_empty() && out.errors.is_empty(), "nothing changed");
}

/// `unwatch` of one path, in its clean form: it asks for no memory.
#[test]
fn unwatch_of_a_path_asks_for_nothing() {
  let mut s = story();
  let mut out = Output::default();
  // The first call makes the lists of a flush.
  s.engine.unwatch(vec![PathBuf::from("/p/src/a.js")], s.now, &mut out);
  let path = vec![PathBuf::from("/p/src/b.js")];
  assert_eq!(requests(|| s.engine.unwatch(path, s.now, &mut out)), 0);
  assert!(out.events.is_empty() && out.errors.is_empty(), "unwatch is silent");
}
