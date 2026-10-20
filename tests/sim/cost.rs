//! What the engine costs, as numbers. No test here fails on a number: each one prints the time
//! of the engine, the time inside the simulation and the allocations, and a person compares two
//! runs on the same machine. A small engine time can be off by a fifth (see `stopwatch`).
//! Run it with `cargo test --release -p pathwatch --test sim cost:: -- --ignored --nocapture`.

use std::{
  collections::HashMap,
  ffi::{OsStr, OsString},
  io,
  path::{Path, PathBuf},
  sync::{
    Arc, Mutex, MutexGuard, OnceLock, PoisonError,
    atomic::{AtomicU64, Ordering},
  },
  time::{Duration, Instant, SystemTime},
};

use crate::fixtures::{Mode, SimBackend, SimFs};
use pathwatch::{
  EventKind,
  backend::{Backend, Committed, Coverage, DirKey, Hint, Signal, Want},
  engine::{Engine, Ignored, Options, Output},
  fs::{Entry, Fs, Meta},
  poll::PollBackend,
};

// ---- counting memory ----

use crate::counting::{allocations, live};

// ---- telling the engine from the simulation ----

/// What the calls into the simulation cost so far.
#[derive(Default)]
struct Outside {
  nanos: AtomicU64,
  calls: AtomicU64,
  allocations: AtomicU64,
}

fn nanos(time: Duration) -> u64 {
  u64::try_from(time.as_nanos()).expect("a test does not run for 500 years")
}

/// Runs one call into the simulation with the stopwatch on.
fn timed<T>(outside: &Outside, call: impl FnOnce() -> T) -> T {
  let allocated = allocations();
  let start = Instant::now();
  let result = call();
  outside.nanos.fetch_add(nanos(start.elapsed()), Ordering::Relaxed);
  outside.allocations.fetch_add(allocations() - allocated, Ordering::Relaxed);
  outside.calls.fetch_add(1, Ordering::Relaxed);
  result
}

/// The time that a stopwatch takes for itself and does not count: about the same for one
/// [`timed`] call and for one `Run::call`. It is measured once, and taken off the time of the
/// engine.
fn stopwatch() -> u64 {
  static COST: OnceLock<u64> = OnceLock::new();
  *COST.get_or_init(|| {
    const CALLS: u64 = 1_000_000;
    let outside = Outside::default();
    let start = Instant::now();
    for _ in 0..CALLS {
      timed(&outside, || ());
    }
    nanos(start.elapsed()).saturating_sub(outside.nanos.load(Ordering::Relaxed)) / CALLS
  })
}

/// A part of the simulation, the disk or the backend, with a stopwatch on every call.
///
/// Where the simulation calls back for each result (`read_dir`, `translate`), the results are
/// collected first and handed over after the stopwatch has stopped. So the work of the engine
/// for each result is not counted as time of the simulation.
struct Timed<T> {
  inner: T,
  outside: Arc<Outside>,
}

impl Fs for Timed<SimFs> {
  fn lstat(&self, path: &Path) -> io::Result<Meta> {
    timed(&self.outside, || self.inner.lstat(path))
  }

  fn stat(&self, path: &Path) -> io::Result<Meta> {
    timed(&self.outside, || self.inner.stat(path))
  }

  fn read_dir(&self, dir: &Path, each: &mut dyn FnMut(&OsStr, Entry)) -> io::Result<()> {
    let mut entries: Vec<(OsString, Entry)> = Vec::new();
    let result = timed(&self.outside, || {
      self.inner.read_dir(dir, &mut |name, entry| entries.push((name.to_owned(), entry)))
    });
    for (name, entry) in &entries {
      each(name, *entry);
    }
    result
  }

  fn hash(&self, path: &Path, limit: u64) -> io::Result<Option<u64>> {
    timed(&self.outside, || self.inner.hash(path, limit))
  }

  fn link_target(&self, path: &Path) -> io::Result<PathBuf> {
    timed(&self.outside, || self.inner.link_target(path))
  }

  fn real_path(&self, path: &Path) -> io::Result<PathBuf> {
    timed(&self.outside, || self.inner.real_path(path))
  }

  fn name_on_disk(&self, path: &Path) -> io::Result<OsString> {
    timed(&self.outside, || self.inner.name_on_disk(path))
  }

  fn now(&self) -> SystemTime {
    timed(&self.outside, || self.inner.now())
  }
}

/// A hint that owns its path.
enum Kept {
  Changed(DirKey, PathBuf),
  Written(DirKey, PathBuf),
  Lost,
  Mount(PathBuf),
}

impl Kept {
  fn of(hint: Hint<'_>) -> Self {
    match hint {
      Hint::Changed { dir, rel } => Kept::Changed(dir, rel.to_path_buf()),
      Hint::Written { dir, rel, .. } => Kept::Written(dir, rel.to_path_buf()),
      Hint::Lost => Kept::Lost,
      Hint::Mount { path } => Kept::Mount(path.to_path_buf()),
    }
  }

  fn hint(&self) -> Hint<'_> {
    match self {
      Kept::Changed(dir, rel) => Hint::Changed { dir: *dir, rel },
      Kept::Written(dir, rel) => Hint::Written { dir: *dir, rel, new: false },
      Kept::Lost => Hint::Lost,
      Kept::Mount(path) => Hint::Mount { path },
    }
  }
}

impl<B: Backend> Backend for Timed<B> {
  type Raw = B::Raw;

  fn watch(&mut self, key: DirKey, dir: &Path, want: Want) -> Coverage {
    timed(&self.outside, || self.inner.watch(key, dir, want))
  }

  fn unwatch(&mut self, key: DirKey) {
    timed(&self.outside, || self.inner.unwatch(key));
  }

  fn commit(&mut self) -> Committed {
    timed(&self.outside, || self.inner.commit())
  }

  fn translate(&mut self, raw: Self::Raw, hint: &mut dyn FnMut(Hint<'_>)) {
    let mut hints = Vec::new();
    timed(&self.outside, || self.inner.translate(raw, &mut |hint| hints.push(Kept::of(hint))));
    for kept in &hints {
      hint(kept.hint());
    }
  }
}

/// A backend for the test of hints. It watches nothing: the test makes the hints by hand, with
/// the keys that the engine gave for its directories, and one raw notification holds them all.
/// So `translate` costs nothing, and the test measures the engine alone, without a stopwatch
/// for each hint.
struct ByHand {
  /// One watch reaches every directory below it, as with FSEvents. If not, each directory has
  /// a watch of its own, as with inotify.
  trees: bool,
  /// The key of each directory that the engine asked a watch for.
  keys: Arc<Mutex<HashMap<PathBuf, DirKey>>>,
}

impl Backend for ByHand {
  type Raw = Arc<Vec<Kept>>;

  fn watch(&mut self, key: DirKey, dir: &Path, want: Want) -> Coverage {
    if self.trees && want == Want::Names {
      return Coverage::NotWatched;
    }
    lock(&self.keys).insert(dir.to_path_buf(), key);
    if self.trees { Coverage::WatchedTree } else { Coverage::Watched }
  }

  fn unwatch(&mut self, _key: DirKey) {}

  fn translate(&mut self, raw: Self::Raw, hint: &mut dyn FnMut(Hint<'_>)) {
    for kept in raw.iter() {
      hint(kept.hint());
    }
  }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
  mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---- one measurement ----

/// What some calls of the engine cost.
struct Cost {
  /// The time of the calls, with everything in them.
  total: Duration,
  /// The part of it inside the simulation.
  simulation: Duration,
  /// How often a stopwatch ran: once for each call into the simulation, and once around the
  /// calls of the engine that a test made in one go.
  stopwatches: u64,
  /// How often the engine asked for memory.
  allocations: u64,
}

impl Cost {
  /// The time of the engine itself.
  fn engine(&self) -> Duration {
    let stopwatch = Duration::from_nanos(self.stopwatches * stopwatch());
    self.total.saturating_sub(self.simulation).saturating_sub(stopwatch)
  }

  fn show(&self, what: &str) {
    let ms = |time: Duration| time.as_secs_f64() * 1000.0;
    say(&format!(
      "{what:<64}{:>10.3} ms engine{:>10.3} ms simulation{:>9} allocations",
      ms(self.engine()),
      ms(self.simulation),
      self.allocations
    ));
  }

  /// The same for one of `count` equal things.
  fn show_each(&self, what: &str, count: u32) {
    let each = |time: Duration| time.as_secs_f64() * 1_000_000.0 / f64::from(count);
    let allocations = u32::try_from(self.allocations).expect("fewer than 2^32 allocations");
    say(&format!(
      "{what:<64}{:>10.3} us engine{:>10.3} us simulation{:>9.1} allocations",
      each(self.engine()),
      each(self.simulation),
      f64::from(allocations) / f64::from(count)
    ));
  }
}

#[expect(clippy::print_stdout, reason = "these tests are here to print what they measure")]
fn say(line: &str) {
  println!("{line}");
}

/// An engine on a simulated disk. Every call of the engine is measured.
struct Run<B: Backend> {
  engine: Engine<Timed<B>, Timed<SimFs>>,
  /// The disk, for the changes that the test makes.
  fs: SimFs,
  now: Instant,
  outside: Arc<Outside>,
  /// The time and the allocations of the engine calls since the last [`Run::take_cost`], and
  /// how often the stopwatch ran around them.
  total: Duration,
  allocations: u64,
  stopwatches: u64,
}

impl Run<SimBackend> {
  fn sim(fs: SimFs, mode: Mode, options: Options) -> Self {
    let backend = SimBackend::attach(&fs, mode);
    Self::new(fs, backend, options)
  }

  /// Hands every waiting raw notification to the engine.
  fn deliver(&mut self) {
    let raws = self.fs.take_raw(usize::MAX);
    self.call(|engine, now| {
      for raw in raws {
        engine.signal(Signal::Raw(raw), now);
      }
    });
  }
}

impl<B: Backend> Run<B> {
  fn new(fs: SimFs, backend: B, options: Options) -> Self {
    let outside = Arc::new(Outside::default());
    let timed_fs = Timed { inner: fs.clone(), outside: Arc::clone(&outside) };
    let timed_backend = Timed { inner: backend, outside: Arc::clone(&outside) };
    Self {
      engine: Engine::new(timed_backend, timed_fs, options),
      fs,
      now: Instant::now(),
      outside,
      total: Duration::ZERO,
      allocations: 0,
      stopwatches: 0,
    }
  }

  /// Runs some calls of the engine with the stopwatch on.
  fn call<T>(&mut self, call: impl FnOnce(&mut Engine<Timed<B>, Timed<SimFs>>, Instant) -> T) -> T {
    let allocated = allocations();
    let start = Instant::now();
    let result = call(&mut self.engine, self.now);
    self.total += start.elapsed();
    self.allocations += allocations() - allocated;
    self.stopwatches += 1;
    result
  }

  fn watch(&mut self, paths: Vec<PathBuf>) -> Output {
    let mut out = Output::default();
    self.call(|engine, now| engine.watch(paths, now, &mut out));
    out
  }

  fn unwatch(&mut self, paths: Vec<PathBuf>) -> Output {
    let mut out = Output::default();
    self.call(|engine, now| engine.unwatch(paths, now, &mut out));
    out
  }

  /// Moves both clocks to the next deadline of the engine and runs what is due.
  fn step(&mut self) -> Output {
    let deadline = self.engine.deadline().expect("something is due");
    let wait = deadline.saturating_duration_since(self.now);
    self.now += wait;
    self.fs.advance(wait);
    let mut out = Output::default();
    self.call(|engine, now| engine.tick(now, &mut out));
    out
  }

  /// Whether only the poll is left to do. The tests that call this set the poll far away.
  fn is_idle(&self) -> bool {
    self.engine.deadline().is_none_or(|at| at > self.now + Duration::from_secs(60))
  }

  /// What the calls of the engine cost since the last call of this function.
  fn take_cost(&mut self) -> Cost {
    let take = |counter: &AtomicU64| counter.swap(0, Ordering::Relaxed);
    let outside_allocations = take(&self.outside.allocations);
    Cost {
      total: std::mem::take(&mut self.total),
      simulation: Duration::from_nanos(take(&self.outside.nanos)),
      stopwatches: take(&self.outside.calls) + std::mem::take(&mut self.stopwatches),
      allocations: std::mem::take(&mut self.allocations) - outside_allocations,
    }
  }
}

// ---- the disks ----

const FILES: usize = 20_000;
const MODES: [Mode; 2] = [Mode::Inode, Mode::Prefix];

/// The tests measure time, so they run one after the other.
fn serial() -> MutexGuard<'static, ()> {
  static SERIAL: Mutex<()> = Mutex::new(());
  lock(&SERIAL)
}

/// A fixed random order (xorshift64), the same in every run.
fn shuffle<T>(items: &mut [T]) {
  let mut state = 0x9E37_79B9_7F4A_7C15_u64;
  for last in (1..items.len()).rev() {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    let bound = u64::try_from(last).expect("a length fits") + 1;
    items.swap(last, usize::try_from(state % bound).expect("it is below a length"));
  }
}

const PROJECT: &str = "/home/user/project";

/// The 2,000 directories of the project, eight names deep.
fn project_dirs() -> impl Iterator<Item = String> {
  (0..20).flat_map(|package| {
    (0..100).map(move |dir| format!("{PROJECT}/packages/pkg{package:02}/src/dir{dir:02}"))
  })
}

/// The sources of a big project: 20,000 files in 2,000 directories. The list is in a random
/// order, as a bundler finds its modules.
fn project(fs: &SimFs) -> Vec<PathBuf> {
  let mut files = Vec::with_capacity(FILES);
  for dir in project_dirs() {
    fs.mkdir_all(&dir).unwrap();
    for file in 0..10 {
      let path = PathBuf::from(format!("{dir}/file{file}.js"));
      fs.write(&path, "export default 1;\n").unwrap();
      files.push(path);
    }
  }
  assert_eq!(files.len(), FILES);
  shuffle(&mut files);
  grow_old(fs);
  files
}

const ENTRIES: usize = 50_000;

/// One directory with 50,000 files, `/big/dir`. Gives the files.
fn big_directory(fs: &SimFs) -> Vec<PathBuf> {
  fs.mkdir_all("/big/dir").unwrap();
  let files: Vec<PathBuf> =
    (0..ENTRIES).map(|file| format!("/big/dir/file{file:05}.js").into()).collect();
  for file in &files {
    fs.write(file, "export default 1;\n").unwrap();
  }
  grow_old(fs);
  files
}

/// Lets time pass, so that no file is young and the engine reads none.
fn grow_old(fs: &SimFs) {
  fs.advance(Duration::from_secs(10));
}

/// A poll that never comes, for the tests that measure something else.
fn no_poll() -> Options {
  Options { poll_interval: Duration::from_secs(3600), ..Options::default() }
}

// ---- the tests ----

/// Measures five times and keeps, for each cost, the run in which the engine was fastest: the
/// run that other work on this machine disturbed least.
fn best<const N: usize>(mut measure: impl FnMut() -> [Cost; N]) -> [Cost; N] {
  let mut best = measure();
  for _ in 1..5 {
    for (best, next) in best.iter_mut().zip(measure()) {
      if next.engine() < best.engine() {
        *best = next;
      }
    }
  }
  best
}

/// Adds the project in one batch and shows the cost, and the memory that is kept afterwards.
fn watch_project<B: Backend>(name: &str, start: impl Fn(SimFs) -> Run<B>) {
  let (mut kept, mut looks, mut watches) = (0, 0, 0);
  let [cost] = best(|| {
    let fs = SimFs::new();
    let files = project(&fs);
    let mut run = start(fs);
    let before = live();
    let out = run.watch(files);
    kept = live().saturating_sub(before);
    (looks, watches) = (run.fs.calls().lstat, run.fs.calls().watch);
    assert!(out.events.is_empty() && out.errors.is_empty(), "the baseline is silent");
    [run.take_cost()]
  });
  cost.show(&format!("a. watch 20,000 files in 2,000 directories ({name})"));
  say(&format!("   {} bytes are kept for each watched file", kept / FILES));
  say(&format!("   {looks} looks at the disk (lstat), {watches} watch calls"));
}

#[test]
#[ignore = "it measures and prints"]
fn a_watch_20000_files_in_one_batch() {
  let _serial = serial();
  // With the poll backend the kept memory is the memory of the engine alone. With the others
  // the simulated backend keeps its watches too.
  watch_project("Poll", |fs| Run::new(fs, PollBackend, no_poll()));
  for mode in MODES {
    watch_project(&format!("{mode:?}"), |fs| Run::sim(fs, mode, no_poll()));
  }
  // The same with a filter that ignores nothing and counts how often it is asked: once for a
  // watched file, and once for a directory that is the watch target of some.
  let asked = Arc::new(AtomicU64::new(0));
  let count = Arc::clone(&asked);
  let ignored: Ignored = Arc::new(move |_, _| {
    count.fetch_add(1, Ordering::Relaxed);
    false
  });
  let options = Options { ignored: Some(ignored), ..no_poll() };
  watch_project("Poll, with a filter", |fs| Run::new(fs, PollBackend, options.clone()));
  // `best` measures five times.
  say(&format!("   the filter is asked {} times", asked.load(Ordering::Relaxed) / 5));
}

#[test]
#[ignore = "it measures and prints"]
fn b_one_change_of_one_watched_file() {
  const CHANGES: u32 = 2_000;
  let _serial = serial();
  for mode in MODES {
    let [cost] = best(|| {
      let fs = SimFs::new();
      let files = project(&fs);
      let mut run = Run::sim(fs, mode, no_poll());
      run.watch(files.clone());
      run.take_cost();
      for (number, file) in (0..CHANGES).zip(&files) {
        run.fs.write(file, format!("export default {number};\n")).unwrap();
        run.deliver();
        let out = run.step();
        assert_eq!(out.events.len(), 1, "{:?}", out.events);
        assert_eq!(out.events[0].kind, EventKind::Update);
      }
      [run.take_cost()]
    });
    cost.show_each(&format!("b. one change of one watched file ({mode:?})"), CHANGES);
  }
}

#[test]
#[ignore = "it measures and prints"]
fn c_hints_for_names_that_nobody_watches() {
  const HINTS: u32 = 100_000;
  let _serial = serial();
  for (shape, trees) in [("a watch for each directory", false), ("one watch for the tree", true)] {
    let [cost] = best(|| {
      let fs = SimFs::new();
      let mut files = project(&fs);
      // A watched file at the top of the project. A backend that watches trees then has one
      // watch there, and every hint comes with a long path below it.
      fs.write(format!("{PROJECT}/package.json"), "{}").unwrap();
      files.push(format!("{PROJECT}/package.json").into());
      let keys = Arc::default();
      let mut engine = Engine::new(ByHand { trees, keys: Arc::clone(&keys) }, fs, no_poll());
      let now = Instant::now();
      engine.watch(files, now, &mut Output::default());

      // 50 names that nobody watches in each of the 2,000 directories.
      let keys = lock(&keys);
      let mut hints = Vec::new();
      for dir in project_dirs() {
        for other in 0..50 {
          let name = format!("other{other}.tmp");
          hints.push(if trees {
            let below = Path::new(&dir).strip_prefix(PROJECT).unwrap();
            Kept::Changed(keys[Path::new(PROJECT)], below.join(name))
          } else {
            Kept::Written(keys[Path::new(&dir)], name.into())
          });
        }
      }
      assert_eq!(hints.len(), HINTS as usize);
      let hints = Arc::new(hints);

      let allocated = allocations();
      let start = Instant::now();
      engine.signal(Signal::Raw(Arc::clone(&hints)), now);
      let total = start.elapsed();
      let allocations = allocations() - allocated;
      assert_eq!(engine.deadline(), Some(now + no_poll().poll_interval), "no hint made work");
      [Cost { total, simulation: Duration::ZERO, stopwatches: 0, allocations }]
    });
    cost.show_each(&format!("c. one of 100,000 hints for a name nobody watches ({shape})"), HINTS);
  }
}

#[test]
#[ignore = "it measures and prints"]
fn d_scan_a_directory_of_50000_files() {
  let _serial = serial();
  let mut kept = 0;
  let [first, second] = best(|| {
    let fs = SimFs::new();
    big_directory(&fs);
    let mut run = Run::sim(fs, Mode::Inode, no_poll());

    let before = live();
    let out = run.watch(vec!["/big/dir".into()]);
    kept = live().saturating_sub(before);
    assert!(out.events.is_empty() && out.errors.is_empty(), "the baseline is silent");
    let first = run.take_cost();

    // Lost notifications: everything is scanned once more.
    run.call(Engine::overflow);
    let out = run.step();
    assert!(out.events.is_empty() && run.is_idle(), "nothing changed");
    [first, run.take_cost()]
  });
  first.show("d. first scan of a directory of 50,000 files");
  say(&format!("   {} bytes are kept for each file", kept / ENTRIES));
  second.show("d. second scan of it, nothing changed");
}

#[test]
#[ignore = "it measures and prints"]
fn e_a_tree_of_50000_files_vanishes_and_comes_back() {
  let _serial = serial();
  for mode in MODES {
    let mut events = Vec::new();
    let [cost] = best(|| {
      let fs = SimFs::new();
      for dir in 0..500 {
        let dir = format!("/proj/tree/dir{dir:03}");
        fs.mkdir_all(&dir).unwrap();
        for file in 0..100 {
          fs.write(format!("{dir}/file{file:02}.js"), "export default 1;\n").unwrap();
        }
      }
      grow_old(&fs);
      let mut run = Run::sim(fs, mode, no_poll());
      run.watch(vec!["/proj/tree".into()]);
      run.take_cost();

      // Away: the flush finds the directory missing and holds its node. The deletes wait.
      run.fs.rename("/proj/tree", "/proj/away").unwrap();
      run.deliver();
      events = run.step().events;
      assert!(events.is_empty(), "the deletes wait");
      // Back, inside the delete delay.
      run.fs.rename("/proj/away", "/proj/tree").unwrap();
      run.deliver();
      while !run.is_idle() {
        events.append(&mut run.step().events);
      }
      [run.take_cost()]
    });
    cost.show(&format!("e. a tree of 50,000 files vanishes and comes back ({mode:?})"));
    say(&format!("   {} events", events.len()));
  }
}

/// Watches the project and runs some polls. Shows the cost of one.
fn poll_rounds<B: Backend>(what: &str, start: impl Fn(SimFs) -> Run<B>) {
  const ROUNDS: u32 = 20;
  let [cost] = best(|| {
    let fs = SimFs::new();
    let files = project(&fs);
    let mut run = start(fs);
    run.watch(files);
    run.take_cost();
    for _ in 0..ROUNDS {
      let out = run.step();
      assert!(out.events.is_empty() && out.errors.is_empty(), "nothing changed");
    }
    [run.take_cost()]
  });
  cost.show_each(what, ROUNDS);
}

#[test]
#[ignore = "it measures and prints"]
fn f_one_polling_round_at_20000_paths() {
  let _serial = serial();
  let fast = Options { poll_interval: Duration::from_millis(100), ..Options::default() };
  poll_rounds("f. one polling round at 20,000 paths (Poll)", |fs| {
    Run::new(fs, PollBackend, fast.clone())
  });
  // With a backend of the OS the poll looks only at what no watch covers.
  for mode in MODES {
    let what = format!("f. one poll at 20,000 paths, next to OS watches ({mode:?})");
    poll_rounds(&what, |fs| Run::sim(fs, mode, Options::default()));
  }
}

#[test]
#[ignore = "it measures and prints"]
fn g_remove_single_files_from_a_directory_of_50000() {
  const REMOVED: u32 = 2_000;
  let _serial = serial();
  let [cost] = best(|| {
    let fs = SimFs::new();
    let mut files = big_directory(&fs);
    // From anywhere in the directory, not from one end of a list.
    shuffle(&mut files);
    let mut run = Run::sim(fs, Mode::Inode, no_poll());
    run.watch(vec!["/big/dir".into()]);
    run.take_cost();
    for (_, file) in (0..REMOVED).zip(&files) {
      run.fs.remove(file).unwrap();
      run.deliver();
      // The flush, and the second look when the delete delay is over.
      let mut events = Vec::new();
      while !run.is_idle() {
        events.append(&mut run.step().events);
      }
      assert_eq!(events.len(), 1, "{events:?}");
      assert_eq!(events[0].kind, EventKind::Delete);
    }
    [run.take_cost()]
  });
  cost.show_each("g. remove one file from a directory of 50,000 files", REMOVED);
}

#[test]
#[ignore = "it measures and prints"]
fn h_unwatch_20000_files() {
  let _serial = serial();
  for mode in MODES {
    let mut kept = 0;
    let [batch, single] = best(|| {
      // All in one call.
      let fs = SimFs::new();
      let files = project(&fs);
      let mut run = Run::sim(fs, mode, no_poll());
      let before = live();
      run.watch(files.clone());
      run.take_cost();
      let out = run.unwatch(files);
      assert!(out.events.is_empty() && out.errors.is_empty(), "unwatch is silent");
      kept = live().saturating_sub(before);
      let batch = run.take_cost();

      // One call for each file.
      let fs = SimFs::new();
      let files = project(&fs);
      let mut run = Run::sim(fs, mode, no_poll());
      run.watch(files.clone());
      run.take_cost();
      for file in files {
        run.unwatch(vec![file]);
      }
      [batch, run.take_cost()]
    });
    batch.show(&format!("h. unwatch 20,000 files in one call ({mode:?})"));
    say(&format!("   {kept} bytes are still kept afterwards, for the whole project"));
    single.show_each(&format!("h. unwatch one of 20,000 files ({mode:?})"), 20_000);
  }
}

#[test]
#[ignore = "it measures and prints"]
fn i_a_name_nobody_watches_on_a_disk_with_loose_spelling() {
  const HINTS: u32 = 100_000;
  let _serial = serial();
  // The setting of case c with one watch for the tree, on a disk that takes two spellings of
  // a name for one. A hint for a name that is not registered is looked up once more, under
  // every case of its letters: the hint costs more, but it leaves no work, and the disk is not
  // looked at. A hint for another spelling of a registered name makes that one name be checked.
  let (mut unrelated, mut related) = (0, 0);
  let [hint, flush] = best(|| {
    let fs = SimFs::new();
    let mut files = project(&fs);
    fs.write(format!("{PROJECT}/package.json"), "{}").unwrap();
    files.push(format!("{PROJECT}/package.json").into());
    let options = Options { loose_spelling: true, ..no_poll() };
    let mut run = Run::sim(fs, Mode::Prefix, options);
    run.watch(files);
    for dir in project_dirs() {
      for other in 0..50 {
        run.fs.write(format!("{dir}/other{other}.tmp"), "1").unwrap();
      }
    }
    run.take_cost();
    let before = run.fs.calls().lstat;
    run.deliver();
    let hint = run.take_cost();
    assert!(run.is_idle(), "a hint for a name nobody watches left work");
    unrelated = run.fs.calls().lstat - before;

    run.fs.write(format!("{PROJECT}/PACKAGE.JSON"), "{}").unwrap();
    run.deliver();
    run.take_cost();
    let out = run.step();
    assert!(out.events.is_empty(), "{:?}", out.events);
    related = run.fs.calls().lstat - before;
    [hint, run.take_cost()]
  });
  hint.show_each("i. one of 100,000 hints for a name nobody watches (loose spelling)", HINTS);
  say(&format!("   {unrelated} looks at the disk (lstat) for all of them, and no flush"));
  flush.show("i. the flush after a hint for another spelling of a watched file");
  say(&format!("   {related} looks at the disk (lstat) in that flush"));
}

#[test]
#[ignore = "it measures and prints"]
fn j_a_new_file_in_a_watched_directory() {
  const NEW: u32 = 20;
  let _serial = serial();
  // On a disk that takes two spellings of a name for one, a new name can be another spelling
  // of an entry that the tree has. So the look that finds a new entry asks the disk how it
  // spells that one name. It makes no listing, however many entries the directory has.
  for entries in [200, ENTRIES] {
    for loose_spelling in [false, true] {
      let (mut looks, mut questions, mut listings) = (0, 0, 0);
      let [cost] = best(|| {
        let fs = SimFs::new();
        fs.mkdir_all("/big/dir").unwrap();
        for file in 0..entries {
          fs.write(format!("/big/dir/file{file:05}.js"), "export default 1;\n").unwrap();
        }
        grow_old(&fs);
        let options = Options { loose_spelling, ..no_poll() };
        let mut run = Run::sim(fs, Mode::Inode, options);
        run.watch(vec!["/big/dir".into()]);
        run.take_cost();
        let before = run.fs.calls();
        for new in 0..NEW {
          run.fs.write(format!("/big/dir/new{new}.js"), "1").unwrap();
          run.deliver();
          let mut events = Vec::new();
          while !run.is_idle() {
            events.append(&mut run.step().events);
          }
          assert_eq!(events.len(), 1, "{events:?}");
          assert_eq!(events[0].kind, EventKind::Create);
        }
        let after = run.fs.calls();
        looks = (after.lstat - before.lstat) / u64::from(NEW);
        questions = (after.name_on_disk - before.name_on_disk) / u64::from(NEW);
        listings = (after.read_dir - before.read_dir) / u64::from(NEW);
        [run.take_cost()]
      });
      let spelling = if loose_spelling { "loose" } else { "strict" };
      cost.show_each(&format!("j. one new file among {entries} files ({spelling} spelling)"), NEW);
      say(&format!(
        "   {looks} looks at the disk (lstat), {questions} questions for a name and {listings} \
         listings for each new file"
      ));
    }
  }
}

#[test]
#[ignore = "it measures and prints"]
fn k_unwatch_a_tree_above_20000_watched_files() {
  let _serial = serial();
  // The files are watched one by one, and the project above them as a tree. Then the tree is
  // taken back. Where one watch covers a whole tree, every directory on the way gives such a
  // watch back, and everything below is scanned deep, once.
  for mode in MODES {
    let (mut looks, mut watches, mut unwatches) = (0, 0, 0);
    let [cost] = best(|| {
      let fs = SimFs::new();
      let files = project(&fs);
      let mut run = Run::sim(fs, mode, no_poll());
      run.watch(files);
      run.watch(vec![PROJECT.into()]);
      run.take_cost();
      let before = run.fs.calls();
      let out = run.unwatch(vec![PROJECT.into()]);
      assert!(out.events.is_empty() && out.errors.is_empty(), "unwatch is silent");
      let after = run.fs.calls();
      looks = after.lstat - before.lstat;
      (watches, unwatches) = (after.watch - before.watch, after.unwatch - before.unwatch);
      [run.take_cost()]
    });
    cost.show(&format!("k. unwatch a tree above 20,000 watched files ({mode:?})"));
    say(&format!(
      "   {looks} looks at the disk (lstat), {watches} watch calls, {unwatches} unwatch calls"
    ));
  }
}
