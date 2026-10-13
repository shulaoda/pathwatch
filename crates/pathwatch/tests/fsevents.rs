//! The FSEvents backend alone. A flood runs alone (`Turn`), and no test starts when more than
//! 50,000 files lie next to the test program.
#![cfg(target_os = "macos")]

use std::{
  collections::{HashMap, HashSet},
  ffi::{OsStr, OsString},
  fs::{self, File, Permissions},
  io,
  os::unix::{
    ffi::OsStrExt,
    fs::{PermissionsExt, symlink},
  },
  path::{Path, PathBuf},
  sync::{
    Arc, Condvar, Mutex, MutexGuard, Once, OnceLock, PoisonError,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc::{self, Receiver, TryRecvError},
  },
  thread::{self, JoinHandle},
  time::{Duration, Instant, SystemTime},
};

use pathwatch::{
  backend::{Backend, Coverage, DirKey, Hint, Signal, Sink, Want, is_watch_limit},
  engine::{Engine, Options, Output},
  fs::{Entry, Fs, Meta, StdFs},
  fsevents::{
    FsEventsBackend, MAX_NEW_STREAMS, MAX_PATHS_PER_STREAM, MAX_STREAM_PATHS, MAX_STREAMS, Raw,
    flag, is_too_high,
  },
};
use tempfile::TempDir;

/// Only a failing test waits this long: after a flood anywhere on the disk, FSEvents holds
/// records back for up to 45 s.
const TIMEOUT: Duration = Duration::from_secs(90);

/// A flood runs alone: FSEvents answers it with late records and loss reports for every
/// stream, and a test next to it would fail for a reason that is not its own.
struct Turn {
  alone: bool,
}

/// How many tests run side by side, and whether a flood runs.
static TURNS: Mutex<(usize, bool)> = Mutex::new((0, false));
static TURN_IS_OVER: Condvar = Condvar::new();

impl Turn {
  fn take(alone: bool) -> Self {
    assert_few_files_next_to_the_test_program();
    allow_more_open_files();
    let mut turns = Self::turns();
    while turns.1 || (alone && turns.0 > 0) {
      turns = TURN_IS_OVER.wait(turns).unwrap_or_else(PoisonError::into_inner);
    }
    if alone {
      turns.1 = true;
    } else {
      turns.0 += 1;
    }
    Self { alone }
  }

  fn turns() -> MutexGuard<'static, (usize, bool)> {
    TURNS.lock().unwrap_or_else(PoisonError::into_inner)
  }
}

impl Drop for Turn {
  fn drop(&mut self) {
    let mut turns = Self::turns();
    if self.alone {
      turns.1 = false;
    } else {
      turns.0 -= 1;
    }
    TURN_IS_OVER.notify_all();
  }
}

/// A stream takes 0.1 s and more to start for every 100,000 files next to the program.
fn assert_few_files_next_to_the_test_program() {
  static FILES: OnceLock<usize> = OnceLock::new();
  let files = *FILES.get_or_init(|| {
    let program = std::env::current_exe().unwrap();
    fs::read_dir(program.parent().unwrap()).map_or(0, Iterator::count)
  });
  assert!(
    files <= 50_000,
    "{files} files lie next to the test program, and FSEvents is slow for such a program: a \
     stream takes 0.1 s and more to start for every 100,000 of them. `cargo clean` removes \
     the old object files of the build directory."
  );
}

/// A shell gives a program 256 open files: too few for the guards of thousands of stream paths.
fn allow_more_open_files() {
  #[repr(C)]
  struct Limit {
    now: u64,
    most: u64,
  }
  unsafe extern "C" {
    fn getrlimit(resource: i32, limit: *mut Limit) -> i32;
    fn setrlimit(resource: i32, limit: *const Limit) -> i32;
  }
  const OPEN_FILES: i32 = 8;
  /// `OPEN_MAX`: macOS gives every program this many when it asks.
  const ENOUGH: u64 = 10_240;
  static ASKED: Once = Once::new();
  ASKED.call_once(|| {
    let mut limit = Limit { now: 0, most: 0 };
    // SAFETY: both calls get a pointer to a `struct rlimit` that lives until they return.
    unsafe {
      if getrlimit(OPEN_FILES, &raw mut limit) == 0 && limit.now < ENOUGH {
        limit.now = ENOUGH.min(limit.most);
        setrlimit(OPEN_FILES, &raw const limit);
      }
    }
  });
}

const NO_PATHS: [&Path; 0] = [];

/// A hint, owned. The backend never sends `Hint::Written`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Seen {
  Changed(DirKey, PathBuf),
  Lost,
}

fn changed(dir: DirKey, rel: &str) -> Seen {
  Seen::Changed(dir, PathBuf::from(rel))
}

/// The paths below the directory of `key` that `hints` name.
fn rels(hints: &[Seen], key: DirKey) -> HashSet<&str> {
  let rels = hints.iter().filter_map(|hint| match hint {
    Seen::Changed(dir, rel) if *dir == key => rel.to_str(),
    _ => None,
  });
  rels.collect()
}

/// An empty `rel` is a loss report for the directory of the key.
fn is_loss(rel: &Path) -> bool {
  rel.as_os_str().is_empty()
}

fn raw(records: &[(&Path, u32)]) -> Raw {
  let mut raw = Raw::default();
  for (path, flags) in records {
    raw.push(path.as_os_str().as_bytes(), *flags);
  }
  raw
}

struct Harness {
  backend: FsEventsBackend,
  /// Each signal with the time at which the callback handed it over.
  signals: Receiver<(Instant, Signal<Raw>)>,
  /// The playground. It lies behind the link /var -> /private/var, so this spelling is never
  /// the one of the records.
  dir: TempDir,
  /// The directory that each key was watched with.
  watched: Vec<(DirKey, PathBuf)>,
  sentinels: u32,
  /// Every record that came in.
  records: Vec<(PathBuf, u32)>,
  /// Given back last, when the streams are stopped and the playground is removed.
  _turn: Turn,
}

impl Harness {
  fn new() -> Self {
    Self::start(Turn::take(false), Duration::ZERO)
  }

  fn alone() -> Self {
    Self::start(Turn::take(true), Duration::ZERO)
  }

  /// A callback that takes `delay` for each call makes FSEvents drop records.
  fn alone_with_slow_callback(delay: Duration) -> Self {
    Self::start(Turn::take(true), delay)
  }

  fn start(turn: Turn, delay: Duration) -> Self {
    let (sender, signals) = mpsc::channel();
    let sink = Sink::new(move |signal| {
      let _ = sender.send((Instant::now(), signal));
      if !delay.is_zero() {
        thread::sleep(delay);
      }
    });
    let backend = FsEventsBackend::new(sink).unwrap();
    let dir = tempfile::tempdir().unwrap();
    assert_ne!(dir.path(), fs::canonicalize(dir.path()).unwrap(), "no symlink on the way");
    let (watched, records) = (Vec::new(), Vec::new());
    Self { backend, signals, dir, watched, sentinels: 0, records, _turn: turn }
  }

  /// `""` is the playground itself.
  fn path(&self, rel: &str) -> PathBuf {
    self.dir.path().join(rel)
  }

  /// The same place as `path`, spelled the way the records spell it.
  fn real(&self, rel: &str) -> PathBuf {
    fs::canonicalize(self.dir.path()).unwrap().join(rel)
  }

  /// `watch` without `commit`.
  fn watch(&mut self, index: u32, dir: &Path, want: Want) -> DirKey {
    let key = DirKey::new(index, 0);
    let coverage = self.backend.watch(key, dir, want);
    assert!(matches!(coverage, Coverage::WatchedTree), "{coverage:?}");
    self.watched.retain(|(known, _)| *known != key);
    self.watched.push((key, dir.to_owned()));
    key
  }

  /// Says which keys that got `NotWatched` a stream reports now.
  fn commit(&mut self) -> Vec<DirKey> {
    let committed = self.backend.commit();
    assert!(committed.failed.is_empty(), "{:?}", committed.failed);
    committed.now_watched
  }

  /// `watch`, `commit` and a sync: a new stream also delivers what happened shortly before
  /// its start.
  fn cover(&mut self, index: u32, rel: &str) -> DirKey {
    let key = self.watch(index, &self.path(rel), Want::Contents);
    self.commit();
    self.sync(key);
    key
  }

  fn translate(&mut self, raw: Raw) -> Vec<Seen> {
    self.records.extend(raw.records().map(|(path, flags)| (path.to_owned(), flags)));
    let mut seen = Vec::new();
    self.backend.translate(raw, &mut |hint| {
      seen.push(match hint {
        Hint::Changed { dir, rel } => Seen::Changed(dir, rel.to_owned()),
        Hint::Written { .. } => panic!("FSEvents flags cannot prove a write"),
        Hint::Lost => Seen::Lost,
      });
    });
    seen
  }

  /// The hints of the next callback call, and when it was made. `None` after `deadline`.
  fn next(&mut self, deadline: Instant) -> Option<(Instant, Vec<Seen>)> {
    let left = deadline.saturating_duration_since(Instant::now());
    match self.signals.recv_timeout(left) {
      Ok((at, Signal::Raw(raw))) => Some((at, self.translate(raw))),
      Ok((_, Signal::Failed(error))) => panic!("the backend failed: {error}"),
      Err(_) => None,
    }
  }

  /// All hints up to the last one of `expected`, in order, without those of `sync` files.
  fn hints_until(&mut self, expected: &[Seen]) -> Vec<Seen> {
    let deadline = Instant::now() + TIMEOUT;
    let mut seen = Vec::new();
    while !expected.iter().all(|hint| seen.contains(hint)) {
      let Some((_, hints)) = self.next(deadline) else {
        panic!("no {expected:?}, only {seen:?}");
      };
      seen.extend(hints);
    }
    seen.retain(|hint| !matches!(hint, Seen::Changed(_, rel) if is_sentinel(rel)));
    seen
  }

  /// All hints up to a record for `path`, which may give no hint.
  fn hints_until_record(&mut self, path: &Path) -> Vec<Seen> {
    let deadline = Instant::now() + TIMEOUT;
    let known = self.records.len();
    let mut seen = Vec::new();
    while !self.records[known..].iter().any(|(record, _)| record == path) {
      let Some((_, hints)) = self.next(deadline) else {
        panic!("no record for {}, only {seen:?}", path.display());
      };
      seen.extend(hints);
    }
    seen
  }

  /// Whether FSEvents itself dropped records so far: on a loaded machine it does so for any
  /// stream, and the loss report that follows is not the backend's doing.
  fn os_dropped(&self) -> bool {
    let dropped = flag::USER_DROPPED | flag::KERNEL_DROPPED;
    self.records.iter().any(|(_, flags)| flags & dropped != 0)
  }

  /// Every hint for what the test did so far below the stream path of `key`: a stream delivers
  /// its records in order, so all of them come before the record of a new file in the
  /// directory. Two streams keep no order.
  fn sync(&mut self, key: DirKey) -> Vec<Seen> {
    self.sentinels += 1;
    let name = format!(".sync-{}", self.sentinels);
    let (_, dir) = self.watched.iter().find(|(known, _)| *known == key).unwrap();
    File::create(dir.join(&name)).unwrap();
    self.hints_until(&[changed(key, &name)])
  }

  /// Receives until every file of `files` (path below `key`, time before its write) is reported.
  fn wait_for_files(
    &mut self,
    key: DirKey,
    reported: &mut Reported,
    mut files: Vec<(PathBuf, Instant)>,
  ) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
      files.retain(|(rel, made)| !reported.has(rel, *made));
      let Some((first, _)) = files.first() else { return };
      let Some((at, hints)) = self.next(deadline) else {
        panic!("{} files were not reported, the first is {}", files.len(), first.display());
      };
      reported.add(key, at, hints);
      // Take what is there already before the long list is checked again.
      while let Some((at, hints)) = self.next(Instant::now()) {
        reported.add(key, at, hints);
      }
    }
  }
}

/// What the hints for one key say about the files below its directory.
#[derive(Default)]
struct Reported {
  files: HashSet<PathBuf>,
  /// The last loss report: one for the directory of the key, or `Lost`.
  last_lost: Option<Instant>,
  /// How many of the loss reports were `Lost`.
  lost_everywhere: usize,
}

impl Reported {
  fn add(&mut self, key: DirKey, at: Instant, hints: Vec<Seen>) {
    for hint in hints {
      match hint {
        Seen::Changed(dir, rel) if dir == key && is_loss(&rel) => self.last_lost = Some(at),
        Seen::Changed(dir, rel) if dir == key => {
          self.files.insert(rel);
        }
        Seen::Lost => {
          self.last_lost = Some(at);
          self.lost_everywhere += 1;
        }
        Seen::Changed(..) => {}
      }
    }
  }

  /// Found by a hint, or by the scan that a loss report after the write asks for.
  fn has(&self, rel: &Path, made: Instant) -> bool {
    self.files.contains(rel) || self.last_lost.is_some_and(|at| at >= made)
  }
}

/// A thread that writes files until it is told to stop, or until the test fails.
struct Writer {
  stop: Arc<AtomicBool>,
  made: Arc<AtomicUsize>,
  /// From this many files on, the writer makes one file a millisecond.
  fast_until: Arc<AtomicUsize>,
  thread: Option<JoinHandle<Vec<Instant>>>,
}

/// Writes the files "0", "1", ... into `dir`, and after `names` files the first ones again.
fn spawn_writer(dir: PathBuf, names: usize, pause: Duration) -> Writer {
  let stop = Arc::new(AtomicBool::new(false));
  let made = Arc::new(AtomicUsize::new(0));
  let fast_until = Arc::new(AtomicUsize::new(usize::MAX));
  let thread = thread::spawn({
    let (stop, made, fast_until) = (Arc::clone(&stop), Arc::clone(&made), Arc::clone(&fast_until));
    move || {
      let mut started = Vec::new();
      while !stop.load(Ordering::Relaxed) {
        let name = (started.len() % names).to_string();
        started.push(Instant::now());
        fs::write(dir.join(name), "1").unwrap();
        made.fetch_add(1, Ordering::Relaxed);
        if started.len() >= fast_until.load(Ordering::Relaxed) {
          thread::sleep(Duration::from_millis(1));
        } else if !pause.is_zero() {
          thread::sleep(pause);
        }
      }
      started
    }
  });
  Writer { stop, made, fast_until, thread: Some(thread) }
}

impl Writer {
  fn made(&self) -> usize {
    self.made.load(Ordering::Relaxed)
  }

  /// After `files` more files at full speed, one file a millisecond: a slow step of the test
  /// does not pile files up.
  fn fast_for(&self, files: usize) {
    self.fast_until.store(self.made() + files, Ordering::Relaxed);
  }

  /// The time right before each write.
  fn finish(mut self) -> Vec<Instant> {
    self.stop.store(true, Ordering::Relaxed);
    self.thread.take().unwrap().join().unwrap()
  }
}

impl Drop for Writer {
  fn drop(&mut self) {
    // A writer that runs on after its test has failed slows FSEvents down for all other tests.
    self.stop.store(true, Ordering::Relaxed);
    if let Some(thread) = self.thread.take() {
      let _ = thread.join();
    }
  }
}

fn is_sentinel(rel: &Path) -> bool {
  rel.file_name().is_some_and(|name| name.as_bytes().starts_with(b".sync-"))
}

fn chmod(path: &Path, mode: u32) {
  fs::set_permissions(path, Permissions::from_mode(mode)).unwrap();
}

fn touch(path: &Path) {
  File::open(path).unwrap().set_modified(SystemTime::now()).unwrap();
}

/// 10,000 files from four threads, spread over 50 directories.
fn burst(h: &mut Harness) -> Reported {
  for dir in 0..50 {
    fs::create_dir(h.path(&dir.to_string())).unwrap();
  }
  let key = h.cover(1, "");
  let writers: Vec<JoinHandle<Vec<(PathBuf, Instant)>>> = (0..4)
    .map(|writer| {
      let root = h.path("");
      thread::spawn(move || {
        let made = (0..2500).map(|index| {
          let rel = PathBuf::from(format!("{}/{writer}-{index}", (index * 4 + writer) % 50));
          let before = Instant::now();
          File::create(root.join(&rel)).unwrap();
          (rel, before)
        });
        made.collect()
      })
    })
    .collect();
  let files = writers.into_iter().flat_map(|writer| writer.join().unwrap()).collect();
  let mut reported = Reported::default();
  h.wait_for_files(key, &mut reported, files);
  reported
}

#[test]
fn file_changes_in_a_watched_directory() {
  let mut h = Harness::new();
  fs::write(h.path("written"), "1").unwrap();
  fs::write(h.path("deleted"), "1").unwrap();
  fs::write(h.path("replaced"), "1").unwrap();
  let key = h.cover(1, "");

  // The sync takes the records of each change out, so the next hint is a new one.
  fs::write(h.path("written"), "2").unwrap();
  h.hints_until(&[changed(key, "written")]);
  h.sync(key);

  File::create(h.path("created")).unwrap();
  h.hints_until(&[changed(key, "created")]);
  h.sync(key);

  fs::remove_file(h.path("deleted")).unwrap();
  h.hints_until(&[changed(key, "deleted")]);
  h.sync(key);

  fs::write(h.path("replaced.tmp"), "2").unwrap();
  fs::rename(h.path("replaced.tmp"), h.path("replaced")).unwrap();
  h.hints_until(&[changed(key, "replaced.tmp"), changed(key, "replaced")]);
}

#[test]
fn file_three_levels_below_the_cover() {
  let mut h = Harness::new();
  fs::create_dir_all(h.path("one/two/three")).unwrap();
  let key = h.cover(1, "");

  File::create(h.path("one/two/three/file")).unwrap();
  let mut hints = h.hints_until(&[changed(key, "one/two/three/file")]);
  hints.extend(h.sync(key));
  // No record for a directory on the way: a new entry does not count as a change of it.
  assert_eq!(rels(&hints, key), HashSet::from(["one/two/three/file"]));
}

#[test]
fn directory_gives_one_hint_and_none_for_what_is_inside() {
  let mut h = Harness::new();
  let outside = tempfile::tempdir().unwrap();
  for tree in [h.path("old"), outside.path().join("tree")] {
    fs::create_dir_all(tree.join("deep")).unwrap();
    fs::write(tree.join("child"), "1").unwrap();
    fs::write(tree.join("deep/child"), "1").unwrap();
  }
  let key = h.cover(1, "");

  fs::create_dir(h.path("made")).unwrap();
  let mut hints = h.hints_until(&[changed(key, "made")]);
  hints.extend(h.sync(key));
  assert_eq!(rels(&hints, key), HashSet::from(["made"]));

  fs::rename(h.path("old"), h.path("new")).unwrap();
  let mut hints = h.hints_until(&[changed(key, "old"), changed(key, "new")]);
  hints.extend(h.sync(key));
  assert_eq!(rels(&hints, key), HashSet::from(["old", "new"]));

  fs::rename(outside.path().join("tree"), h.path("in")).unwrap();
  let mut hints = h.hints_until(&[changed(key, "in")]);
  hints.extend(h.sync(key));
  assert_eq!(rels(&hints, key), HashSet::from(["in"]));

  fs::rename(h.path("new"), outside.path().join("out")).unwrap();
  let mut hints = h.hints_until(&[changed(key, "new")]);
  hints.extend(h.sync(key));
  assert_eq!(rels(&hints, key), HashSet::from(["new"]));
}

#[test]
fn chmod_and_touch_of_a_directory_give_no_hint() {
  let mut h = Harness::alone();
  fs::create_dir(h.path("sub")).unwrap();
  let key = h.cover(1, "");
  let dirs = [h.real(""), h.real("sub")];
  let unwatched = tempfile::tempdir().unwrap();

  let deadline = Instant::now() + TIMEOUT;
  let mut mode = 0o700;
  loop {
    // FSEvents merges the flags of all changes of a path for as long as it remembers the path,
    // so a chmod right after the mkdir still says `Created`. Many other changes make it forget.
    for index in 0..2000 {
      let file = unwatched.path().join(index.to_string());
      File::create(&file).unwrap();
      fs::remove_file(&file).unwrap();
    }
    h.records.clear();
    mode ^= 0o050;
    for dir in &dirs {
      chmod(dir, mode);
      touch(dir);
    }
    let hints = h.sync(key);

    let flags_of = |dir: &PathBuf| -> Vec<u32> {
      h.records.iter().filter(|(path, _)| path == dir).map(|(_, flags)| *flags).collect()
    };
    let all_flags: Vec<u32> = dirs.iter().flat_map(flags_of).collect();
    assert!(dirs.iter().all(|dir| !flags_of(dir).is_empty()), "no record for a directory");
    assert!(all_flags.iter().all(|flags| flags & flag::IS_DIR != 0), "{all_flags:x?}");
    if all_flags.iter().all(|flags| flags & (flag::CREATED | flag::REMOVED | flag::RENAMED) == 0) {
      assert_eq!(rels(&hints, key), HashSet::new());
      return;
    }
    assert!(Instant::now() < deadline, "the records still say that the directories are new");
  }
}

#[test]
fn chmod_of_a_file_gives_a_hint() {
  let mut h = Harness::new();
  fs::write(h.path("file"), "1").unwrap();
  let key = h.cover(1, "");

  // The flags of a write and of a chmod come merged: the backend cannot tell them apart.
  chmod(&h.path("file"), 0o600);
  h.hints_until(&[changed(key, "file")]);
}

/// A cover below it that counted on that stream gets one of its own.
#[test]
fn a_cover_unwatched_before_the_commit_gets_no_stream() {
  let mut h = Harness::new();
  fs::create_dir_all(h.path("dir/below")).unwrap();
  let dir = h.watch(1, &h.path("dir"), Want::Contents);
  let below = h.watch(2, &h.path("dir/below"), Want::Contents);
  assert_eq!(h.backend.pending(), [h.real("dir")]);

  h.backend.unwatch(dir);
  assert_eq!(h.backend.pending(), [h.real("dir/below")]);
  h.commit();
  assert_eq!(h.backend.streamed(), [h.real("dir/below")]);
  File::create(h.path("dir/below/file")).unwrap();
  h.hints_until(&[changed(below, "file")]);
}

/// Its guards go with it. A cover below one of its paths keeps it.
#[test]
fn a_stream_with_no_cover_left_is_stopped() {
  let mut h = Harness::new();
  fs::create_dir_all(h.path("one/below")).unwrap();
  fs::create_dir(h.path("two")).unwrap();
  let one = h.cover(1, "one");
  let two = h.cover(2, "two");
  let below = h.watch(3, &h.path("one/below"), Want::Contents);
  h.commit();
  assert_eq!(h.backend.stream_count(), 2);
  let guarded = h.backend.guarded().len();

  h.backend.unwatch(one);
  h.commit();
  assert_eq!(h.backend.stream_count(), 2);
  File::create(h.path("one/below/file")).unwrap();
  h.hints_until(&[changed(below, "file")]);

  h.backend.unwatch(below);
  h.commit();
  assert_eq!(h.backend.stream_count(), 1);
  assert_eq!(h.backend.streamed(), [h.real("two")]);
  assert_eq!(h.backend.guarded().len(), guarded - 1);
  File::create(h.path("two/file")).unwrap();
  h.hints_until(&[changed(two, "file")]);
}

#[test]
fn ancestor_is_not_watched_unless_a_stream_reports_it() {
  let mut h = Harness::new();
  fs::create_dir_all(h.path("cover/below")).unwrap();
  fs::create_dir(h.path("other")).unwrap();

  let alone = DirKey::new(9, 0);
  let coverage = h.backend.watch(alone, &h.path("other"), Want::Names);
  assert!(matches!(coverage, Coverage::NotWatched), "{coverage:?}");
  h.commit();
  assert_eq!(h.backend.stream_count(), 0);
  assert_eq!(h.backend.covers(), NO_PATHS);

  let cover = h.cover(1, "cover");
  let below = h.watch(2, &h.path("cover/below"), Want::Names);
  assert_eq!(h.backend.pending(), NO_PATHS);
  h.commit();
  assert_eq!(h.backend.stream_count(), 1);

  // Each key gets the path below its own directory.
  File::create(h.path("cover/below/file")).unwrap();
  h.hints_until(&[changed(cover, "below/file"), changed(below, "file")]);

  let coverage = h.backend.watch(alone, &h.path("other"), Want::Names);
  assert!(matches!(coverage, Coverage::NotWatched), "{coverage:?}");
}

#[test]
fn watch_is_live_when_commit_returns() {
  let mut h = Harness::new();
  for index in 0..50 {
    let dir = h.path(&index.to_string());
    fs::create_dir(&dir).unwrap();
    let key = h.watch(index, &dir, Want::Contents);
    h.commit();
    File::create(dir.join("file")).unwrap();
    h.hints_until(&[changed(key, "file")]);
  }
}

#[test]
fn rebuild_opens_no_gap_for_an_old_cover() {
  let mut h = Harness::alone();
  fs::create_dir(h.path("old")).unwrap();
  let old = h.cover(1, "old");
  let writer = spawn_writer(h.path("old"), usize::MAX, Duration::ZERO);
  let mut reported = Reported::default();

  let deadline = Instant::now() + TIMEOUT;
  let mut index = 100;
  for group in 0..5 {
    // Four directories get a stream each, then the directory above them: every stream is
    // replaced.
    let above = format!("group-{group}");
    let below = (0..4).map(|dir| format!("{above}/{dir}"));
    for dir in below.chain([above.clone()]) {
      writer.fast_for(500);
      fs::create_dir_all(h.path(&dir)).unwrap();
      h.watch(index, &h.path(&dir), Want::Contents);
      index += 1;
      let streams = h.backend.stream_count();
      h.commit();
      if dir == above {
        assert_eq!(h.backend.stream_count(), group + 2);
        // A replaced stream stays for the whole overlap time.
        assert!(h.backend.retiring_count() >= streams);
      }
      // The writer goes on for a while before the set of streams changes again.
      let goal = writer.made() + 80;
      while writer.made() < goal {
        if let Some((at, hints)) = h.next(Instant::now() + Duration::from_millis(1)) {
          reported.add(old, at, hints);
        }
        assert!(Instant::now() < deadline);
      }
    }
  }

  let files = writer.finish().into_iter().enumerate();
  let files: Vec<_> = files.map(|(index, made)| (PathBuf::from(index.to_string()), made)).collect();
  assert!(files.len() >= 2000);
  h.wait_for_files(old, &mut reported, files);
}

#[test]
fn cover_above_a_cover() {
  let mut h = Harness::new();
  fs::create_dir_all(h.path("up/low")).unwrap();
  let low = h.cover(1, "up/low");
  let up = h.watch(2, &h.path("up"), Want::Contents);
  h.commit();
  // The upper stream reports the lower directory too: no stream path lies below another.
  assert_eq!(h.backend.stream_count(), 1);
  assert_eq!(h.backend.streamed(), [h.real("up")]);

  // The replaced stream may still run and deliver the record too, so each hint can come twice.
  File::create(h.path("up/low/a")).unwrap();
  let mut hints = h.hints_until(&[changed(low, "a"), changed(up, "low/a")]);
  File::create(h.path("up/b")).unwrap();
  hints.extend(h.hints_until(&[changed(up, "b")]));
  hints.extend(h.sync(up));
  assert!(!rels(&hints, low).contains("b"));

  // A stopped stream takes with it what it has not delivered, so a loss report comes with
  // its stop.
  let deadline = Instant::now() + TIMEOUT;
  while h.backend.retiring_count() > 0 {
    hints.extend(h.sync(up));
    assert!(Instant::now() < deadline, "{} streams never stop", h.backend.retiring_count());
  }
  hints.extend(h.sync(up));
  assert!(hints.contains(&Seen::Lost), "{hints:?}");
  File::create(h.path("up/low/d")).unwrap();
  h.hints_until(&[changed(low, "d"), changed(up, "low/d")]);
}

#[test]
fn spellings_of_one_directory_get_one_hint_each() {
  let mut h = Harness::new();
  fs::create_dir(h.path("dir")).unwrap();
  symlink(h.path("dir"), h.path("link")).unwrap();
  let given = h.watch(1, &h.path("dir"), Want::Contents);
  let real = h.watch(2, &h.real("dir"), Want::Contents);
  let link = h.watch(3, &h.path("link"), Want::Contents);
  h.commit();
  assert_eq!(h.backend.stream_count(), 1);
  assert_eq!(h.backend.covers().len(), 1);

  fs::create_dir(h.path("dir/sub")).unwrap();
  File::create(h.path("link/sub/file")).unwrap();
  h.hints_until(&[given, real, link].map(|key| changed(key, "sub/file")));
}

#[test]
fn cover_directory_is_deleted_and_comes_back() {
  let mut h = Harness::new();
  fs::create_dir(h.path("dir")).unwrap();
  let key = h.cover(1, "dir");

  fs::remove_dir_all(h.path("dir")).unwrap();
  h.hints_until(&[changed(key, "")]);

  // The stream matches by path, so it goes on with the new directory.
  fs::create_dir(h.path("dir")).unwrap();
  File::create(h.path("dir/file")).unwrap();
  h.hints_until(&[changed(key, "file")]);
  assert_eq!(h.backend.stream_count(), 1);
}

#[test]
fn unwatch_stops_the_hints_of_that_key_only() {
  let mut h = Harness::new();
  fs::create_dir(h.path("dir")).unwrap();
  fs::create_dir(h.path("other")).unwrap();
  let given = h.watch(1, &h.path("dir"), Want::Contents);
  let real = h.watch(2, &h.real("dir"), Want::Contents);
  let other = h.watch(3, &h.path("other"), Want::Contents);
  h.commit();
  h.sync(other);

  h.backend.unwatch(given);
  File::create(h.path("dir/a")).unwrap();
  let hints = h.hints_until_record(&h.real("dir/a"));
  assert_eq!(rels(&hints, real), HashSet::from(["a"]));
  assert_eq!(rels(&hints, given), HashSet::new());

  // No key is left for the cover: its records are dropped. The path stays until a commit.
  h.backend.unwatch(real);
  File::create(h.path("dir/b")).unwrap();
  let hints = h.hints_until_record(&h.real("dir/b"));
  assert_eq!(rels(&hints, real), HashSet::new());
  assert_eq!(rels(&hints, given), HashSet::new());
  assert!(h.backend.streamed().contains(&h.real("dir").as_path()));
  assert_eq!(h.backend.covers().len(), 1);

  h.backend.unwatch(given);
}

#[test]
fn key_follows_its_path_to_another_directory() {
  let mut h = Harness::new();
  fs::create_dir(h.path("one")).unwrap();
  fs::create_dir(h.path("two")).unwrap();
  symlink(h.path("one"), h.path("link")).unwrap();
  let key = h.cover(1, "link");
  assert_eq!(h.backend.covers(), [h.real("one")]);

  fs::remove_file(h.path("link")).unwrap();
  symlink(h.path("two"), h.path("link")).unwrap();
  h.watch(1, &h.path("link"), Want::Contents);
  h.commit();
  assert_eq!(h.backend.covers(), [h.real("two")]);
  // The stream of the old directory serves no key any more: it is stopped.
  assert_eq!(h.backend.stream_count(), 1);
  assert_eq!(h.backend.streamed(), [h.real("two")]);
  // The key was at another real path, and nothing else said so: it is reported lost.
  h.hints_until(&[changed(key, "")]);

  File::create(h.path("one/stale")).unwrap();
  File::create(h.path("two/fresh")).unwrap();
  let mut hints = h.hints_until(&[changed(key, "fresh")]);
  hints.extend(h.sync(key));
  assert!(!rels(&hints, key).contains("stale"));
}

/// Only where the short path is the same directory and a real path of its own.
#[test]
fn the_short_way_out_of_the_data_volume() {
  let mut h = Harness::new();
  fs::create_dir(h.path("dir")).unwrap();
  let data = Path::new("/System/Volumes/Data");
  let long = data.join(h.real("dir").strip_prefix("/").unwrap());
  if !long.exists() {
    // The playground is on another volume.
    return;
  }
  let key = h.watch(1, &long, Want::Contents);
  h.commit();
  assert_eq!(h.backend.covers(), [h.real("dir")]);
  File::create(h.path("dir/file")).unwrap();
  h.hints_until(&[changed(key, "file")]);

  // `/Users` has a parent with no short way. No stream stands on it, and the real path is
  // kept with the keys that got no watch.
  let users = DirKey::new(2, 0);
  let coverage = h.backend.watch(users, &data.join("Users"), Want::Contents);
  assert!(matches!(coverage, Coverage::NotWatched), "{coverage:?}");
  assert_eq!(h.backend.not_watched()[&users], Path::new("/Users"));
  h.backend.unwatch(users);
  // `/home` is a link into the data volume: the same directory, but no real path.
  if let Ok(real) = fs::canonicalize("/home")
    && real.starts_with(data)
  {
    let home = h.watch(3, Path::new("/home"), Want::Contents);
    assert_eq!(h.backend.cover_of_key()[&home], real);
    h.backend.unwatch(home);
  }
}

/// With the last key gone, the streams that a replacement left running need no overlap: the
/// next commit stops them, and nothing stays open.
#[test]
fn the_last_unwatch_stops_the_replaced_streams_too() {
  let mut h = Harness::new();
  fs::create_dir_all(h.path("up/low")).unwrap();
  let low = h.cover(1, "up/low");
  let up = h.watch(2, &h.path("up"), Want::Contents);
  h.commit();
  assert_eq!(h.backend.retiring_count(), 1);

  h.backend.unwatch(low);
  h.backend.unwatch(up);
  h.commit();
  assert_eq!((h.backend.stream_count(), h.backend.retiring_count()), (0, 0));
  assert_eq!(h.backend.guarded(), NO_PATHS);
}

/// A guard that cannot be opened (here: the process has no open file left) fails its own path
/// and no other: the other paths get their streams, and the keys of that path are named.
#[test]
fn a_guard_that_cannot_be_opened_fails_its_path_only() {
  let mut h = Harness::alone();
  fs::create_dir_all(h.path("a/x")).unwrap();
  fs::create_dir_all(h.path("a/z")).unwrap();
  fs::create_dir_all(h.path("b/y")).unwrap();
  h.cover(1, "a/x");
  let z = h.watch(2, &h.path("a/z"), Want::Contents);
  let y = h.watch(3, &h.path("b/y"), Want::Contents);
  // Every open file of the process but two: the guard of `a/z` takes one and `b` the other,
  // so `b/y` gets none.
  let mut hogs = Vec::new();
  while let Ok(file) = File::open("/dev/null") {
    hogs.push(file);
  }
  hogs.truncate(hogs.len() - 2);
  let committed = h.backend.commit();
  drop(hogs);
  assert_eq!(committed.failed.len(), 1, "{committed:?}");
  let (failed, error) = &committed.failed[0];
  assert!(*failed == y && is_watch_limit(error), "{committed:?}");
  assert_eq!(h.backend.streamed(), [h.real("a/x"), h.real("a/z")]);
  File::create(h.path("a/z/file")).unwrap();
  h.hints_until(&[changed(z, "file")]);

  // With room again, the key asks again and gets its stream.
  h.watch(3, &h.path("b/y"), Want::Contents);
  h.commit();
  File::create(h.path("b/y/file")).unwrap();
  h.hints_until(&[changed(y, "file")]);
}

/// A stream never stands on `/`, `/Users` or a home directory: such a cover gets no watch and
/// is polled, as on Windows, and the covers below are reported as before.
#[test]
fn no_stream_stands_on_the_root_or_a_home() {
  let mut h = Harness::new();
  fs::create_dir(h.path("dir")).unwrap();
  let key = h.cover(1, "dir");
  let home = PathBuf::from(std::env::var_os("HOME").unwrap());
  for (index, dir) in [(2, Path::new("/")), (3, Path::new("/Users")), (4, home.as_path())] {
    let key = DirKey::new(index, 0);
    let coverage = h.backend.watch(key, dir, Want::Contents);
    assert!(matches!(coverage, Coverage::NotWatched), "{}: {coverage:?}", dir.display());
    assert!(h.backend.not_watched().contains_key(&key), "{}", dir.display());
  }
  assert_eq!(h.commit(), []);
  assert_eq!(h.backend.streamed(), [h.real("dir")]);

  File::create(h.path("dir/file")).unwrap();
  h.hints_until(&[changed(key, "file")]);
}

#[test]
fn cover_renamed_in_letter_case_is_heard_after_the_next_watch() {
  let mut h = Harness::new();
  fs::create_dir(h.path("Dir")).unwrap();
  if !h.path("DIR").exists() {
    // The volume tells letter case apart, so there is nothing special to test.
    return;
  }
  let key = h.cover(1, "Dir");

  // The stream goes deaf: it matches the old spelling. The rename itself is reported, and
  // that makes the engine ask again.
  fs::rename(h.path("Dir"), h.path("DIR")).unwrap();
  h.hints_until(&[changed(key, "")]);
  h.watch(1, &h.path("Dir"), Want::Contents);
  assert_eq!(h.backend.pending(), [h.real("DIR")]);
  h.commit();

  File::create(h.path("Dir/file")).unwrap();
  h.hints_until(&[changed(key, "file")]);
}

/// The guard tells: a loss report for each cover below, and the path is trusted no more.
#[test]
fn a_directory_above_a_cover_is_renamed() {
  let mut h = Harness::new();
  fs::create_dir_all(h.path("above/dir")).unwrap();
  fs::create_dir_all(h.path("above/other")).unwrap();
  let key = h.cover(1, "above/dir");
  let other = h.cover(2, "above/other");
  // One open file for each directory on the way, and the two covers share the way.
  let playground = h.real("").components().count() - 1;
  assert_eq!(h.backend.guarded().len(), playground + 3);

  fs::rename(h.path("above"), h.path("away")).unwrap();
  h.hints_until(&[changed(key, ""), changed(other, "")]);
  assert_eq!(h.backend.streamed(), NO_PATHS);
  assert_eq!(h.backend.guarded(), NO_PATHS);

  fs::rename(h.path("away"), h.path("above")).unwrap();
  h.watch(1, &h.path("above/dir"), Want::Contents);
  assert_eq!(h.backend.pending(), [h.real("above/dir")]);
  h.commit();
  File::create(h.path("above/dir/file")).unwrap();
  h.hints_until(&[changed(key, "file")]);
  assert_eq!(h.backend.guarded().len(), playground + 2);
}

/// The kernel tells about each rename, however short the directory was away.
#[test]
fn a_directory_above_a_cover_is_away_and_back_at_once() {
  let mut h = Harness::new();
  fs::create_dir_all(h.path("above/dir")).unwrap();
  let key = h.cover(1, "above/dir");
  for round in 0..20 {
    fs::rename(h.path("above"), h.path("away")).unwrap();
    File::create(h.path("away/dir").join(format!("file-{round}"))).unwrap();
    fs::rename(h.path("away"), h.path("above")).unwrap();
    h.hints_until(&[changed(key, "")]);
    h.watch(1, &h.path("above/dir"), Want::Contents);
    h.commit();
    h.sync(key);
  }
}

/// Guarded all the same, like a cover spelled through the data volume: neither is taken for
/// a directory that moved.
#[test]
fn a_cover_below_a_directory_that_cannot_be_read() {
  let mut h = Harness::new();
  fs::create_dir_all(h.path("locked/dir")).unwrap();
  fs::set_permissions(h.path("locked"), Permissions::from_mode(0o311)).unwrap();
  let key = h.cover(1, "locked/dir");
  assert!(h.backend.guarded().contains(&h.real("locked").as_path()));
  File::create(h.path("locked/dir/file")).unwrap();
  let hints = h.hints_until(&[changed(key, "file")]);
  assert!(h.os_dropped() || !hints.contains(&changed(key, "")), "{hints:?}");
  assert_eq!(h.backend.streamed(), [h.real("locked/dir")]);
  fs::set_permissions(h.path("locked"), Permissions::from_mode(0o755)).unwrap();

  fs::create_dir(h.path("plain")).unwrap();
  let through_the_data_volume =
    Path::new("/System/Volumes/Data").join(h.real("plain").strip_prefix("/").unwrap());
  if !through_the_data_volume.is_dir() {
    return;
  }
  let plain = h.watch(2, &through_the_data_volume, Want::Contents);
  h.commit();
  h.sync(plain);
  assert_eq!(h.backend.streamed().len(), 2);
  File::create(h.path("plain/file")).unwrap();
  let deadline = Instant::now() + Duration::from_secs(2);
  let mut hints = Vec::new();
  while let Some((_, more)) = h.next(deadline) {
    hints.extend(more);
  }
  assert!(h.os_dropped() || !hints.contains(&changed(plain, "")), "{hints:?}");
  assert_eq!(h.backend.streamed().len(), 2);
}

/// The real path is asked at `watch` and the stream starts at `commit`: the backend asks again
/// after the start.
#[test]
fn a_stream_path_that_moved_before_its_stream_started_is_lost() {
  let mut h = Harness::new();
  fs::create_dir_all(h.path("above/dir")).unwrap();
  fs::create_dir(h.path("other")).unwrap();
  let dir = h.watch(1, &h.path("above/dir"), Want::Contents);
  let other = h.watch(2, &h.path("other"), Want::Contents);
  fs::rename(h.path("above"), h.path("away")).unwrap();
  h.commit();
  let hints = h.hints_until(&[changed(dir, "")]);
  assert_eq!(hints.len(), 1, "{hints:?}");
  assert_eq!(h.backend.streamed(), [h.real("other")]);
  h.sync(other);
}

/// No stream was there to report the rename: the backend asks for each name below a new stream
/// path after the start.
#[test]
fn a_directory_below_a_stream_path_is_spelled_anew_before_its_stream_started() {
  let mut h = Harness::new();
  fs::create_dir_all(h.path("top/mid/low")).unwrap();
  fs::create_dir_all(h.path("top/other")).unwrap();
  if !h.path("top/MID").exists() {
    // The volume tells letter case apart, so there is nothing special to test.
    return;
  }
  h.watch(1, &h.path("top"), Want::Contents);
  let low = h.watch(2, &h.path("top/mid/low"), Want::Contents);
  let other = h.watch(3, &h.path("top/other"), Want::Contents);
  fs::rename(h.path("top/mid"), h.path("top/MID")).unwrap();
  thread::sleep(Duration::from_millis(100));
  h.commit();
  let hints = h.hints_until(&[changed(low, "")]);
  assert!(!hints.contains(&changed(other, "")), "{hints:?}");

  h.watch(2, &h.path("top/mid/low"), Want::Contents);
  h.commit();
  assert_eq!(h.backend.cover_of_key()[&low], h.real("top/MID/low"));
  File::create(h.path("top/mid/low/file")).unwrap();
  h.hints_until(&[changed(low, "file")]);
}

/// The engine meets this behind a link, where no node of its tree is the renamed directory.
#[test]
fn a_directory_between_a_stream_path_and_a_cover_is_renamed() {
  let mut h = Harness::new();
  fs::create_dir_all(h.path("top/mid/low")).unwrap();
  fs::create_dir_all(h.path("top/other")).unwrap();
  let top = h.cover(1, "top");
  let low = h.watch(2, &h.path("top/mid/low"), Want::Names);
  let other = h.watch(3, &h.path("top/other"), Want::Names);

  fs::rename(h.path("top/mid"), h.path("top/moved")).unwrap();
  let hints = h.hints_until(&[changed(top, "mid"), changed(top, "moved"), changed(low, "")]);
  assert!(!hints.contains(&changed(other, "")), "{hints:?}");
}

#[test]
fn missing_directory_fails_and_leaves_the_key_unwatched() {
  let mut h = Harness::new();
  fs::create_dir(h.path("dir")).unwrap();
  let key = h.cover(1, "dir");

  fs::remove_dir_all(h.path("dir")).unwrap();
  for want in [Want::Names, Want::Contents] {
    match h.backend.watch(key, &h.path("dir"), want) {
      Coverage::Failed(error) => assert_eq!(error.kind(), std::io::ErrorKind::NotFound),
      coverage => panic!("{coverage:?}"),
    }
  }
  assert_eq!(h.backend.covers(), NO_PATHS);
  assert!(h.backend.cover_of_key().is_empty());
  assert_eq!(h.backend.pending(), NO_PATHS);
}

#[test]
fn lost_records_are_hints_for_the_covers_of_their_place() {
  let mut h = Harness::new();
  fs::create_dir_all(h.path("up/cover/below")).unwrap();
  fs::create_dir(h.path("apart")).unwrap();
  let cover = h.watch(1, &h.path("up/cover"), Want::Contents);
  let twin = h.watch(2, &h.real("up/cover"), Want::Contents);
  let below = h.watch(3, &h.path("up/cover/below"), Want::Names);
  let apart = h.watch(4, &h.path("apart"), Want::Contents);
  let lost = flag::MUST_SCAN_SUB_DIRS | flag::USER_DROPPED;
  let all_of = |key| changed(key, "");

  // Each key of the cover and of the covers below, with an empty `rel`: a name would make
  // the engine check that one name only.
  let hints = h.translate(raw(&[(&h.real("up/cover"), lost)]));
  assert_eq!(hints, [all_of(cover), all_of(twin), all_of(below)]);
  assert_eq!(h.translate(raw(&[(&h.real("apart"), lost)])), [all_of(apart)]);

  let hints = h.translate(raw(&[(&h.real("up"), flag::MUST_SCAN_SUB_DIRS)]));
  assert_eq!(hints, [all_of(cover), all_of(twin), all_of(below)]);
  let hints = h.translate(raw(&[(&h.real(""), lost)]));
  assert_eq!(hints, [all_of(apart), all_of(cover), all_of(twin), all_of(below)]);

  // Every cover that holds the place, the longest first.
  let hints = h.translate(raw(&[(&h.real("up/cover/below/deep"), lost)]));
  assert_eq!(hints, [all_of(below), all_of(cover), all_of(twin)]);
  let hints = h.translate(raw(&[(&h.real("up/cover/other/deep"), lost)]));
  assert_eq!(hints, [all_of(cover), all_of(twin)]);

  // The same first letters are not the same directory.
  for place in ["other", "up/other", "up/cov", "up/cover2", "apart2/deep"] {
    assert_eq!(h.translate(raw(&[(&h.real(place), lost)])), [], "{place}");
  }

  // `/` is everything.
  for flags in [lost, flag::MUST_SCAN_SUB_DIRS, flag::MUST_SCAN_SUB_DIRS | flag::KERNEL_DROPPED] {
    assert_eq!(h.translate(raw(&[(Path::new("/"), flags)])), [Seen::Lost], "{flags:#x}");
  }

  // A drop without `MustScanSubDirs` names no place.
  for flags in [flag::USER_DROPPED, flag::KERNEL_DROPPED, flag::USER_DROPPED | flag::KERNEL_DROPPED]
  {
    assert_eq!(h.translate(raw(&[(&h.real("up/cover"), flags)])), [Seen::Lost], "{flags:#x}");
    assert_eq!(h.translate(raw(&[(&h.real("other"), flags)])), [Seen::Lost], "{flags:#x}");
  }

  // The hints of the loss come after those of the other records of the call.
  let file = flag::CREATED | 0x1_0000;
  let hints = h.translate(raw(&[
    (&h.real("up/cover/a"), file),
    (&h.real("up/cover"), lost),
    (&h.real("up/cover/b"), file),
  ]));
  let expected = [
    changed(cover, "a"),
    changed(twin, "a"),
    changed(cover, "b"),
    changed(twin, "b"),
    all_of(cover),
    all_of(twin),
    all_of(below),
  ];
  assert_eq!(hints, expected);

  // A path that no cover has any more: no key is left to see the place.
  for key in [cover, twin, below] {
    h.backend.unwatch(key);
  }
  assert_eq!(h.translate(raw(&[(&h.real("up/cover"), lost)])), []);
  assert_eq!(h.translate(raw(&[(&h.real("up"), lost)])), []);
  assert_eq!(h.translate(raw(&[(&h.real(""), lost)])), [all_of(apart)]);
  assert_eq!(h.translate(raw(&[(Path::new("/"), lost)])), [Seen::Lost]);
}

#[test]
fn record_goes_to_every_cover_that_holds_its_path() {
  let mut h = Harness::new();
  // In the order of the list of covers: `a.b` sorts after everything below `a`.
  let dirs = ["a", "a/b", "a/b/c/d", "a/bb", "a.b", "z"];
  let mut keys = HashMap::new();
  // Not in that order, so that the list has to sort them.
  for (index, dir) in [3, 0, 5, 2, 4, 1].map(|at| dirs[at]).into_iter().enumerate() {
    fs::create_dir_all(h.path(dir)).unwrap();
    keys.insert(dir, h.watch(u32::try_from(index).unwrap(), &h.path(dir), Want::Contents));
  }
  assert_eq!(h.backend.covers(), dirs.map(|dir| h.real(dir)));

  let file = flag::CREATED | 0x1_0000;
  // The record, and its hints: cover and path below it, the longest cover first.
  let cases: &[(&str, &[(&str, &str)])] = &[
    ("a/b/c/d/e/file", &[("a/b/c/d", "e/file"), ("a/b", "c/d/e/file"), ("a", "b/c/d/e/file")]),
    ("a/b/c/d", &[("a/b/c/d", ""), ("a/b", "c/d"), ("a", "b/c/d")]),
    // The cover right before these paths in the list does not hold them.
    ("a/b/c/file", &[("a/b", "c/file"), ("a", "b/c/file")]),
    ("a/b/c", &[("a/b", "c"), ("a", "b/c")]),
    ("a/b/x", &[("a/b", "x"), ("a", "b/x")]),
    ("a/b0/file", &[("a", "b0/file")]),
    ("a/bb/file", &[("a/bb", "file"), ("a", "bb/file")]),
    ("a/bbb", &[("a", "bbb")]),
    ("a/c/d/file", &[("a", "c/d/file")]),
    ("a", &[("a", "")]),
    ("a.b/file", &[("a.b", "file")]),
    ("a.b/b/c/d/file", &[("a.b", "b/c/d/file")]),
    ("z/a/b", &[("z", "a/b")]),
    // No cover holds these.
    ("0/file", &[]),
    ("a.a/file", &[]),
    ("a.c", &[]),
    ("ab/b/file", &[]),
    ("y/a/b", &[]),
    ("zz/file", &[]),
    ("", &[]),
  ];
  for (record, expected) in cases {
    let expected: Vec<Seen> = expected.iter().map(|(dir, rel)| changed(keys[dir], rel)).collect();
    assert_eq!(h.translate(raw(&[(&h.real(record), file)])), expected, "{record}");
  }

  h.backend.unwatch(keys["a/b"]);
  let hints = h.translate(raw(&[(&h.real("a/b/c/d/file"), file)]));
  assert_eq!(hints, [changed(keys["a/b/c/d"], "file"), changed(keys["a"], "b/c/d/file")]);
  let hints = h.translate(raw(&[(&h.real("a/b/file"), file)]));
  assert_eq!(hints, [changed(keys["a"], "b/file")]);
  h.backend.unwatch(keys["a"]);
  assert_eq!(h.translate(raw(&[(&h.real("a/b/file"), file)])), []);
  let hints = h.translate(raw(&[(&h.real("a/b/c/d/file"), file)]));
  assert_eq!(hints, [changed(keys["a/b/c/d"], "file")]);
}

#[test]
fn records_outside_every_cover_are_dropped() {
  let mut h = Harness::new();
  fs::create_dir(h.path("a")).unwrap();
  let key = h.watch(1, &h.path("a"), Want::Contents);
  let file = flag::CREATED | 0x1_0000;

  assert_eq!(h.translate(raw(&[(&h.real("other/file"), file)])), []);
  assert_eq!(h.translate(raw(&[(&h.real(""), file)])), []);
  // The same first letters are not the same directory.
  assert_eq!(h.translate(raw(&[(&h.real("ab"), file)])), []);
  assert_eq!(h.translate(raw(&[(&h.real("ab/file"), file)])), []);
  assert_eq!(h.translate(raw(&[(&h.real("a/file"), file)])), [changed(key, "file")]);

  h.backend.unwatch(key);
  assert_eq!(h.translate(raw(&[(&h.real("a/file"), file)])), []);
}

#[test]
fn flags_decide_only_about_directories() {
  let mut h = Harness::new();
  let key = h.watch(1, &h.path(""), Want::Contents);
  let (is_file, inode_meta, modified, change_owner) = (0x1_0000, 0x400, 0x1000, 0x4000);

  for flags in [0, modified, inode_meta, change_owner, flag::REMOVED, flag::RENAMED | modified] {
    let hints = h.translate(raw(&[(&h.real("name"), flags | is_file)]));
    assert_eq!(hints, [changed(key, "name")], "{flags:#x}");
  }
  for flags in [flag::CREATED, flag::REMOVED, flag::RENAMED, flag::CREATED | change_owner] {
    let hints = h.translate(raw(&[(&h.real("name"), flags | flag::IS_DIR)]));
    assert_eq!(hints, [changed(key, "name")], "{flags:#x}");
    let hints = h.translate(raw(&[(&h.real(""), flags | flag::IS_DIR)]));
    assert_eq!(hints, [changed(key, "")], "{flags:#x}");
  }
  for flags in [0, inode_meta, change_owner, inode_meta | change_owner | 0x8000] {
    assert_eq!(h.translate(raw(&[(&h.real("name"), flags | flag::IS_DIR)])), [], "{flags:#x}");
    assert_eq!(h.translate(raw(&[(&h.real(""), flags | flag::IS_DIR)])), [], "{flags:#x}");
  }
  assert_eq!(h.translate(raw(&[(&h.real(""), flag::HISTORY_DONE)])), []);

  // One callback call, several records.
  let hints = h.translate(raw(&[
    (&h.real("a"), flag::CREATED | is_file),
    (&h.real("dir"), inode_meta | flag::IS_DIR),
    (&h.real("dir/b"), modified | is_file),
  ]));
  assert_eq!(hints, [changed(key, "a"), changed(key, "dir/b")]);
}

/// Each directory lies right in the temporary directory, which is too high to share a path on.
#[test]
fn more_covers_than_one_stream_can_hold() {
  let mut h = Harness::alone();
  let top = fs::canonicalize(std::env::temp_dir()).unwrap();
  assert!(is_too_high(&top), "the test needs a temporary directory that is too high: {top:?}");
  let count = u32::try_from(MAX_PATHS_PER_STREAM).unwrap() + 4;
  let dirs: Vec<TempDir> = (0..count).map(|_| tempfile::tempdir_in(&top).unwrap()).collect();
  for (index, dir) in (0..count).zip(&dirs) {
    h.watch(index, dir.path(), Want::Contents);
  }
  h.commit();
  assert_eq!(h.backend.stream_count(), MAX_NEW_STREAMS);
  // The paths stay apart, as the covers are.
  assert_eq!(h.backend.streamed().len(), dirs.len());

  // A stream with this many paths drops records easily, and says so for each path.
  let mut waiting: HashMap<DirKey, Instant> = HashMap::new();
  for (index, dir) in (0..count).zip(&dirs) {
    waiting.insert(DirKey::new(index, 0), Instant::now());
    File::create(dir.path().join("file")).unwrap();
  }
  let deadline = Instant::now() + TIMEOUT;
  while !waiting.is_empty() {
    let Some((at, hints)) = h.next(deadline) else {
      panic!("{} directories without a hint", waiting.len());
    };
    for hint in hints {
      match hint {
        Seen::Changed(dir, rel) if rel == Path::new("file") => {
          waiting.remove(&dir);
        }
        Seen::Changed(dir, rel) if is_loss(&rel) => {
          waiting.retain(|key, made| *key != dir || *made > at);
        }
        Seen::Lost => waiting.retain(|_, made| *made > at),
        Seen::Changed(..) => {}
      }
    }
  }

  // One record for each path in one callback call: the hints come in that order.
  let lost: Vec<(&Path, u32)> =
    dirs.iter().map(|dir| (dir.path(), flag::MUST_SCAN_SUB_DIRS | flag::USER_DROPPED)).collect();
  let hints = h.translate(raw(&lost));
  let expected: Vec<Seen> = (0..count).map(|index| changed(DirKey::new(index, 0), "")).collect();
  assert_eq!(hints, expected);
}

/// Each key still gets its hints below its own directory.
#[test]
fn many_covers_share_stream_paths_on_common_directories() {
  let mut h = Harness::alone();
  let per_group = u32::try_from(MAX_STREAM_PATHS).unwrap() / 2;
  let mut keys = Vec::new();
  for group in 0..4 {
    for dir in 0..per_group {
      let rel = format!("g{group}/d{dir}");
      fs::create_dir_all(h.path(&rel)).unwrap();
      keys.push((h.watch(group * per_group + dir, &h.path(&rel), Want::Contents), rel));
    }
  }
  fs::create_dir(h.path("g0/apart")).unwrap();

  // No stream reports a group directory yet.
  let group = DirKey::new(1000, 0);
  let coverage = h.backend.watch(group, &h.path("g0"), Want::Names);
  assert!(matches!(coverage, Coverage::NotWatched), "{coverage:?}");
  assert_eq!(h.commit(), [group]);
  assert_eq!(h.backend.stream_count(), MAX_NEW_STREAMS);
  // The groups go up in path order until the paths fit: the last one keeps its own.
  let mut expected: Vec<PathBuf> =
    (0..per_group).map(|dir| h.real(&format!("g3/d{dir}"))).collect();
  expected.extend(["g0", "g1", "g2"].map(|group| h.real(group)));
  expected.sort();
  assert_eq!(h.backend.streamed(), expected);
  assert!(expected.len() <= MAX_STREAM_PATHS);
  assert_eq!(h.backend.covers().len(), keys.len());

  // A stream reports the group directory now, and still none reports the playground.
  let group = h.watch(1000, &h.path("g0"), Want::Names);
  let coverage = h.backend.watch(DirKey::new(1001, 0), &h.path(""), Want::Names);
  assert!(matches!(coverage, Coverage::NotWatched), "{coverage:?}");
  assert_eq!(h.commit(), []);
  assert_eq!(h.backend.stream_count(), MAX_NEW_STREAMS);

  // Each key gets the path below its own directory, also where a path is shared.
  for (_, rel) in &keys {
    File::create(h.path(rel).join("file")).unwrap();
  }
  let expected: Vec<Seen> = keys.iter().map(|(key, _)| changed(*key, "file")).collect();
  let hints = h.hints_until(&expected);
  assert!(hints.contains(&changed(group, "d0/file")));

  // A shared path reports what lies next to the covers too: only a key that holds the place
  // gets a hint.
  File::create(h.path("g0/apart/file")).unwrap();
  h.hints_until(&[changed(group, "apart/file")]);
  let file = flag::CREATED | 0x1_0000;
  let hints = h.translate(raw(&[(&h.real("g0/apart/file"), file)]));
  assert_eq!(hints, [changed(group, "apart/file")]);
  assert_eq!(h.translate(raw(&[(&h.real("g1/apart/file"), file)])), []);

  // One more directory below a shared path needs no new stream.
  let new = h.watch(2000, &h.path("g0/apart"), Want::Contents);
  assert_eq!(h.backend.pending(), NO_PATHS);
  assert_eq!(h.commit(), []);
  assert_eq!(h.backend.stream_count(), MAX_NEW_STREAMS);
  File::create(h.path("g0/apart/new")).unwrap();
  h.hints_until(&[changed(new, "new"), changed(group, "apart/new")]);
}

#[test]
fn real_path_is_the_one_of_the_os_for_every_directory_of_one_parent() {
  let mut h = Harness::new();
  let names = ["plain", "Mixed Case", "ünï", "e\u{301}", "link"];
  for name in &names[..4] {
    fs::create_dir(h.path(name)).unwrap();
  }
  symlink(h.path("plain"), h.path("link")).unwrap();
  // Asked in another letter case too: on the usual disk it is the same directory.
  let loose = h.path("MIXED case").exists();
  let asked = names.iter().map(|name| (*name).to_owned());
  let asked: Vec<String> = asked.chain(loose.then(|| "MIXED case".to_owned())).collect();
  for (index, name) in (0..).zip(&asked) {
    let key = h.watch(index, &h.path(name), Want::Contents);
    let real = fs::canonicalize(h.path(name)).unwrap();
    assert_eq!(h.backend.cover_of_key()[&key], real, "{name}");
  }
}

#[test]
fn drop_while_a_writer_is_active() {
  let _alone = Turn::take(true);
  let dir = tempfile::tempdir().unwrap();
  let covers = u32::try_from(MAX_NEW_STREAMS).unwrap();
  for index in 0..covers {
    fs::create_dir(dir.path().join(format!("dir-{index}"))).unwrap();
  }
  let writer = spawn_writer(dir.path().join("dir-0"), 256, Duration::from_micros(200));

  for round in 0..20 {
    let calls = Arc::new(AtomicUsize::new(0));
    let (sender, signals) = mpsc::channel();
    // A slow callback is very likely in the middle of a call when the backend is dropped.
    let sink = Sink::new({
      let calls = Arc::clone(&calls);
      move |signal| {
        thread::sleep(Duration::from_millis(2));
        calls.fetch_add(1, Ordering::SeqCst);
        let _ = sender.send(signal);
      }
    });
    let mut backend = FsEventsBackend::new(sink).unwrap();
    let rebuild = round % 2 == 1;
    for index in 0..if rebuild { covers } else { 1 } {
      let dir = dir.path().join(format!("dir-{index}"));
      let coverage = backend.watch(DirKey::new(index, 0), &dir, Want::Contents);
      assert!(matches!(coverage, Coverage::WatchedTree), "{coverage:?}");
    }
    assert!(backend.commit().failed.is_empty());
    // Every second round also drops the streams that a rebuild has just replaced.
    if rebuild {
      let coverage = backend.watch(DirKey::new(covers, 0), dir.path(), Want::Contents);
      assert!(matches!(coverage, Coverage::WatchedTree), "{coverage:?}");
      assert!(backend.commit().failed.is_empty());
    }
    assert_eq!(backend.retiring_count(), if rebuild { MAX_NEW_STREAMS } else { 0 });
    assert!(matches!(signals.recv_timeout(TIMEOUT), Ok(Signal::Raw(_))), "no callback yet");

    drop(backend);
    let calls_at_drop = calls.load(Ordering::SeqCst);
    // A record takes about 10 ms from the write to the callback, and these writes take longer.
    // So a callback that the drop did not stop would show up now.
    let goal = writer.made() + 100;
    while writer.made() < goal {
      thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(calls.load(Ordering::SeqCst), calls_at_drop, "a callback after the drop");
    // The streams give up the sink some time after their release: then the channel ends.
    let deadline = Instant::now() + TIMEOUT;
    loop {
      match signals.try_recv() {
        Ok(_) => {}
        Err(TryRecvError::Disconnected) => break,
        Err(TryRecvError::Empty) => {
          assert!(Instant::now() < deadline, "the streams keep the sink");
          thread::sleep(Duration::from_millis(1));
        }
      }
    }
  }
  writer.finish();
}

#[test]
fn burst_is_reported() {
  let mut h = Harness::alone();
  burst(&mut h);
}

#[test]
fn burst_with_lost_records_is_reported_as_lost() {
  // The sink of the watcher never blocks. This one does, to make FSEvents drop records for real.
  let mut h = Harness::alone_with_slow_callback(Duration::from_millis(20));
  let reported = burst(&mut h);
  assert!(reported.last_lost.is_some(), "FSEvents dropped nothing");

  // A stream that drops records says so for its path: the loss is for the cover, not for all.
  let any_loss = flag::MUST_SCAN_SUB_DIRS | flag::USER_DROPPED | flag::KERNEL_DROPPED;
  let drops: Vec<&(PathBuf, u32)> =
    h.records.iter().filter(|(_, flags)| flags & any_loss != 0).collect();
  let root = h.real("");
  let names_the_cover =
    |(path, flags): &&(PathBuf, u32)| *path == root && flags & flag::MUST_SCAN_SUB_DIRS != 0;
  assert!(drops.iter().any(names_the_cover), "{drops:?}");
  if drops.iter().all(names_the_cover) {
    assert_eq!(reported.lost_everywhere, 0);
  }
}

/// `cargo test -p pathwatch --test fsevents -- --ignored failed_start`
#[test]
#[ignore = "it takes all 512 streams of the process, so no other test can run next to it"]
fn failed_start_is_reported_and_the_old_streams_stay() {
  let mut h = Harness::new();
  for index in 0..=MAX_STREAMS {
    fs::create_dir(h.path(&format!("dir-{index}"))).unwrap();
  }
  let keys: Vec<DirKey> = (0..MAX_STREAMS)
    .map(|index| {
      let key =
        h.watch(u32::try_from(index).unwrap(), &h.path(&format!("dir-{index}")), Want::Contents);
      h.commit();
      key
    })
    .collect();
  assert_eq!(h.backend.stream_count(), MAX_STREAMS);

  // Other backends take the streams that are left, one each.
  let unwatched = tempfile::tempdir().unwrap();
  let mut hogs = Vec::new();
  loop {
    let mut hog = FsEventsBackend::new(Sink::new(|_signal| {})).unwrap();
    let coverage = hog.watch(DirKey::new(0, 0), unwatched.path(), Want::Contents);
    assert!(matches!(coverage, Coverage::WatchedTree), "{coverage:?}");
    if !hog.commit().failed.is_empty() {
      break;
    }
    hogs.push(hog);
    assert!(hogs.len() < 1000, "the process has no stream limit");
  }
  assert_eq!(hogs.len() + MAX_STREAMS, 512);

  // The commit fails for the two keys it was for, and names them: the backend forgets them.
  let last = h.path(&format!("dir-{MAX_STREAMS}"));
  fs::create_dir(last.join("below")).unwrap();
  let (new, below) = (DirKey::new(100, 0), DirKey::new(101, 0));
  assert!(matches!(h.backend.watch(new, &last, Want::Contents), Coverage::WatchedTree));
  let coverage = h.backend.watch(below, &last.join("below"), Want::Names);
  assert!(matches!(coverage, Coverage::WatchedTree));
  let committed = h.backend.commit();
  let failed: HashSet<DirKey> = committed.failed.iter().map(|(key, _)| *key).collect();
  assert_eq!(failed, HashSet::from([new, below]));
  assert!(committed.failed.iter().all(|(_, error)| is_watch_limit(error)), "{committed:?}");
  let known: HashSet<DirKey> = h.backend.cover_of_key().keys().copied().collect();
  assert_eq!(known, keys.iter().copied().collect());
  assert_eq!(h.backend.stream_count(), MAX_STREAMS);
  assert_eq!(h.backend.retiring_count(), 0);
  assert_eq!(h.backend.covers().len(), MAX_STREAMS);
  assert!(!h.backend.streamed().contains(&fs::canonicalize(&last).unwrap().as_path()));
  assert_eq!(h.backend.pending(), NO_PATHS);

  File::create(h.path("dir-0/file")).unwrap();
  h.hints_until(&[changed(keys[0], "file")]);

  // FSEvents takes a moment to give the place of a stopped stream free.
  hogs.pop();
  let deadline = Instant::now() + TIMEOUT;
  let new = loop {
    let new = h.watch(100, &last, Want::Contents);
    if h.backend.commit().failed.is_empty() {
      break new;
    }
    assert!(Instant::now() < deadline, "no stream can start");
  };
  // More covers than stream paths now: they share one path, the directory above them.
  assert_eq!(h.backend.stream_count(), 1);
  assert_eq!(h.backend.retiring_count(), MAX_STREAMS);
  File::create(last.join("file")).unwrap();
  File::create(h.path("dir-0/again")).unwrap();
  h.hints_until(&[changed(new, "file"), changed(keys[0], "again")]);
}

/// The real disk, with a count of the `lstat` calls.
struct CountingFs {
  disk: StdFs,
  lstats: Arc<AtomicUsize>,
}

impl Fs for CountingFs {
  fn lstat(&self, path: &Path) -> io::Result<Meta> {
    self.lstats.fetch_add(1, Ordering::Relaxed);
    self.disk.lstat(path)
  }

  fn stat(&self, path: &Path) -> io::Result<Meta> {
    self.disk.stat(path)
  }

  fn read_dir(&self, dir: &Path, each: &mut dyn FnMut(&OsStr, Entry)) -> io::Result<()> {
    self.disk.read_dir(dir, each)
  }

  fn hash(&self, path: &Path, limit: u64) -> io::Result<Option<u64>> {
    self.disk.hash(path, limit)
  }

  fn link_target(&self, path: &Path) -> io::Result<PathBuf> {
    self.disk.link_target(path)
  }

  fn real_path(&self, path: &Path) -> io::Result<PathBuf> {
    self.disk.real_path(path)
  }

  fn name_on_disk(&self, path: &Path) -> io::Result<OsString> {
    self.disk.name_on_disk(path)
  }

  fn now(&self) -> SystemTime {
    self.disk.now()
  }
}

/// An engine on the real disk with the FSEvents backend, as the watcher runs it.
struct RealEngine {
  engine: Engine<FsEventsBackend, CountingFs>,
  signals: Receiver<Signal<Raw>>,
  lstats: Arc<AtomicUsize>,
  out: Output,
}

impl RealEngine {
  fn new() -> Self {
    let (sender, signals) = mpsc::channel();
    let sink = Sink::new(move |signal| {
      let _ = sender.send(signal);
    });
    let backend = FsEventsBackend::new(sink).unwrap();
    let lstats = Arc::new(AtomicUsize::new(0));
    let fs = CountingFs { disk: StdFs, lstats: Arc::clone(&lstats) };
    let options =
      Options { debounce: Duration::from_millis(50), loose_spelling: true, ..Options::default() };
    Self { engine: Engine::new(backend, fs, options), signals, lstats, out: Output::default() }
  }

  fn watch(&mut self, paths: Vec<PathBuf>) {
    self.engine.watch(paths, Instant::now(), &mut self.out);
  }

  /// Runs the engine for `time` as the watcher thread does, and counts its `lstat` calls.
  fn run(&mut self, time: Duration) -> usize {
    let before = self.lstats.load(Ordering::Relaxed);
    let end = Instant::now() + time;
    while Instant::now() < end {
      let next = self.engine.deadline().map_or(end, |deadline| deadline.min(end));
      if let Ok(signal) = self.signals.recv_timeout(next.saturating_duration_since(Instant::now()))
      {
        self.engine.signal(signal, Instant::now());
      }
      self.engine.tick(Instant::now(), &mut self.out);
    }
    self.lstats.load(Ordering::Relaxed) - before
  }
}

#[expect(clippy::print_stdout, reason = "the cost test is here to print what it measures")]
fn say(line: &str) {
  println!("{line}");
}

/// 20,000 files in 2,000 directories, each watched by its own path: it prints what that costs
/// and fails when a budget is passed.
#[test]
#[ignore = "a measurement of a few seconds: run it alone, in release mode"]
fn the_cost_of_single_files() {
  let dir = tempfile::tempdir().unwrap();
  let mut paths = Vec::new();
  for group in 0..20 {
    for sub in 0..100 {
      let sub = dir.path().join(format!("project/g{group:02}/d{sub:03}"));
      fs::create_dir_all(&sub).unwrap();
      for file in 0..10 {
        paths.push(sub.join(format!("m{file}.js")));
        fs::write(paths.last().unwrap(), "export {}\n").unwrap();
      }
    }
  }
  // A new stream also delivers what happened shortly before its start.
  thread::sleep(Duration::from_secs(2));
  // Polled: the directories above the project, and the project with its 20 groups.
  let poll_budget = 50;

  let mut all = RealEngine::new();
  let started = Instant::now();
  all.watch(paths.clone());
  let took = started.elapsed();
  let setup_lstats = all.lstats.load(Ordering::Relaxed);
  say(&format!("one `watch` call for all files: {took:.1?}, {setup_lstats} lstat"));
  assert_eq!(all.out.errors.len(), 0, "{:?}", all.out.errors);
  let idle = all.run(Duration::from_secs(3)) / 3;
  say(&format!("  then idle: {idle} lstat a second"));
  assert!(idle <= poll_budget, "{idle} lstat in an idle second");
  drop(all);

  let mut each = RealEngine::new();
  let started = Instant::now();
  for path in &paths {
    each.watch(vec![path.clone()]);
  }
  let took = started.elapsed();
  say(&format!("one `watch` call for each file: {took:.1?}, {} errors", each.out.errors.len()));
  assert_eq!(each.out.errors.len(), 0, "{:?}", each.out.errors);
  // The streams were replaced many times, and each stop makes the engine read everything.
  let settling = each.run(Duration::from_secs(3));
  say(&format!("  then the replaced streams stop: {settling} lstat in 3 s, and no event"));
  assert_eq!(each.out.events.len(), 0, "{:?}", each.out.events);
  let idle = each.run(Duration::from_secs(3)) / 3;
  say(&format!("  then idle: {idle} lstat a second"));
  assert!(idle <= poll_budget, "{idle} lstat in an idle second");

  // One `lstat` for each written file, and no check of a directory on the way.
  let changed: Vec<&PathBuf> = paths.iter().step_by(10).collect();
  for path in &changed {
    fs::write(path, "export default 1\n").unwrap();
  }
  let burst = each.run(Duration::from_secs(3));
  let events = each.out.events.len();
  say(&format!(
    "a burst of {} files: {burst} lstat (with 3 polls), {events} events",
    changed.len()
  ));
  assert_eq!(events, changed.len());
  assert!(burst <= changed.len() + 4 * poll_budget, "{burst} lstat for the burst");
  drop(each);

  // The backend alone: what one record costs with 2,000 covers.
  let mut h = Harness::new();
  let real = fs::canonicalize(dir.path()).unwrap();
  let dirs: Vec<&Path> = paths.iter().step_by(10).map(|path| path.parent().unwrap()).collect();
  let started = Instant::now();
  for dir in &dirs {
    fs::canonicalize(dir).unwrap();
  }
  let each = started.elapsed() / u32::try_from(dirs.len()).unwrap();
  say(&format!("the real path of a directory from the OS: {each:.1?}"));
  let started = Instant::now();
  for (index, dir) in (0..).zip(&dirs) {
    let coverage = h.backend.watch(DirKey::new(index, 0), dir, Want::Contents);
    assert!(matches!(coverage, Coverage::WatchedTree), "{coverage:?}");
  }
  let each = started.elapsed() / u32::try_from(dirs.len()).unwrap();
  say(&format!("backend `watch` of a directory, with its real path: {each:.1?}"));
  let mut held = Raw::default();
  let mut apart = Raw::default();
  for path in &paths {
    let path = real.join(path.strip_prefix(dir.path()).unwrap());
    held.push(path.as_os_str().as_bytes(), flag::CREATED | 0x1_0000);
    let next_to = path.parent().unwrap().parent().unwrap().join("dist/chunk.js");
    apart.push(next_to.as_os_str().as_bytes(), flag::CREATED | 0x1_0000);
  }
  for (name, raw, expected) in [("a cover holds", held, paths.len()), ("no cover holds", apart, 0)]
  {
    let mut hints = 0;
    let started = Instant::now();
    h.backend.translate(raw, &mut |_| hints += 1);
    let each = started.elapsed() / u32::try_from(paths.len()).unwrap();
    say(&format!("`translate` of a record that {name}: {each:.0?}"));
    assert_eq!(hints, expected);
  }
}
