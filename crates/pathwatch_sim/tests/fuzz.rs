//! The fuzzer: random changes of the disk mixed with deliveries, clock steps, `watch` and
//! `unwatch` calls and failures. A run is a list of [`Op`]s made from a seed alone, so it can be
//! replayed and cut down. It plays the handler by rules that are written down here a second
//! time, not by the engine's. `README.md` has the commands for the big runs and for one seed.

#![cfg(unix)]

use std::{
  collections::{BTreeMap, BTreeSet},
  fmt::Write as _,
  io, mem,
  panic::{self, AssertUnwindSafe},
  path::{Path, PathBuf},
  sync::{
    Arc, Mutex, MutexGuard, Once, PoisonError,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
  },
  time::{Duration, Instant},
};

use pathwatch::{
  EntryKind, Event, EventKind,
  backend::Signal,
  engine::{Engine, Ignored, Options, Output},
  fs::{FileType, Fs},
  poll::PollBackend,
};
use pathwatch_sim::{
  Deliver, Mode, SimBackend, SimFs, can_be_watched, check_fingerprints, followed,
};

// ---- random numbers ----

/// xorshift64. The same seed gives the same run on every machine.
struct Random(u64);

impl Random {
  fn new(seed: u64) -> Self {
    // One round of splitmix64, so that seeds 1, 2, 3 do not start with similar bits.
    let mut mixed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    Self((mixed ^ (mixed >> 31)) | 1)
  }

  fn next(&mut self) -> u64 {
    self.0 ^= self.0 << 13;
    self.0 ^= self.0 >> 7;
    self.0 ^= self.0 << 17;
    self.0
  }

  fn below(&mut self, limit: u64) -> u64 {
    self.next() % limit
  }

  fn chance(&mut self, percent: u64) -> bool {
    self.below(100) < percent
  }

  fn pick<T: Copy>(&mut self, from: &[T]) -> T {
    from[usize::try_from(self.below(from.len() as u64)).unwrap()]
  }

  fn small(&mut self, limit: u64) -> u8 {
    u8::try_from(self.below(limit)).unwrap()
  }

  /// `count` names out of a small alphabet, so that paths collide often.
  fn names(&mut self, count: u64) -> String {
    let mut path = String::new();
    for _ in 0..count {
      path.push('/');
      path.push_str(self.pick(&["a", "a", "b", "b", "c", "c", "skip"]));
    }
    path
  }

  /// A path of `shortest` to four names.
  fn path(&mut self, shortest: u64) -> String {
    let count = shortest + self.below(5 - shortest);
    self.names(count)
  }

  /// Where a change of the disk happens: more often than not at, below or next to a watched
  /// path, because a change far away from all of them tests nothing.
  fn place(&mut self) -> Place {
    match self.below(100) {
      0..15 => Place::Path(self.path(1)),
      15..45 => {
        let count = self.pick(&[0, 0, 0, 1, 1, 1, 1, 1, 2, 2]);
        let up = self.pick(&[0, 0, 0, 0, 0, 1, 1, 1, 2, 2]);
        Place::Near { number: self.small(8), up, rel: self.names(count) }
      }
      45..80 => Place::Known(self.small(32)),
      80..90 => Place::File(self.small(32)),
      _ => {
        let count = self.below(2);
        Place::Dir { which: self.small(32), rel: self.names(count) }
      }
    }
  }
}

/// A path for a change of the disk.
#[derive(Clone, Debug)]
enum Place {
  Path(String),
  /// From the watched path with this number, `up` names up, then `rel` down. What that is depends
  /// on the paths that are watched when the change runs.
  Near {
    number: u8,
    up: u8,
    rel: String,
  },
  /// The file number `which` that a handler should know about.
  Known(u8),
  /// The file or link number `which` on the disk.
  File(u8),
  /// The link number `which` on the disk.
  Link(u8),
  /// `rel` in the directory number `which` on the disk.
  Dir {
    which: u8,
    rel: String,
  },
}

impl Place {
  fn resolve(&self, fs: &SimFs, filter: u8, watched: &BTreeSet<PathBuf>) -> String {
    let pick = |from: Vec<PathBuf>, which: u8| {
      let path = from.get(usize::from(which) % from.len().max(1));
      path.map_or_else(|| "/a".to_owned(), |path| path.display().to_string())
    };
    match self {
      Self::Path(path) => path.clone(),
      Self::Known(which) => {
        let known: Vec<PathBuf> = reportable(fs, filter, watched).into_iter().collect();
        if known.is_empty() { pick(fs.files(), *which) } else { pick(known, *which) }
      }
      Self::File(which) => pick(fs.files(), *which),
      Self::Link(which) => {
        let links = fs.files().into_iter().filter(|path| fs.read_link(path).is_some());
        pick(links.collect(), *which)
      }
      Self::Dir { which, rel } => format!("{}{rel}", pick(fs.dirs(), *which)),
      Self::Near { number, up, rel } => {
        let Some(path) = watched.iter().nth(usize::from(*number) % watched.len().max(1)) else {
          return if rel.is_empty() { "/a".to_owned() } else { rel.clone() };
        };
        let above = path.ancestors().nth(usize::from(*up)).unwrap_or(Path::new("/"));
        let path = format!("{}{rel}", above.display().to_string().trim_end_matches('/'));
        if path.is_empty() { "/a".to_owned() } else { path }
      }
    }
  }
}

// ---- what a run is made of ----

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
  Sim(Mode),
  Poll,
}

/// How the simulated disk behaves.
#[derive(Clone, Debug)]
#[expect(clippy::struct_excessive_bools, reason = "each one is a switch of the disk")]
struct DiskConfig {
  /// How coarse modification times are. Never above two seconds: beyond that the design does
  /// not promise to see a second write of the same size. And a step that leaves a part of a
  /// second in the time is never above 20 ms where an OS watch covers the file: the engine
  /// takes such a time for a fine one. With the poll backend it can be.
  time_step: Duration,
  birth_times: bool,
  reuse_inode_numbers: bool,
  listing_kinds: bool,
  listing_meta: bool,
}

#[derive(Clone, Debug)]
struct Config {
  backend: Target,
  /// Bit 0: a directory named `skip` is ignored, with everything below it. Bit 1: files named
  /// `c` are ignored. So the filter ignores everything below a directory that it ignores, and
  /// it still answers in two ways for the two kinds.
  filter: u8,
  debounce: Duration,
  delete_delay: Duration,
  poll_interval: Duration,
  /// The engine takes every disk for one that knows a name under two spellings (macOS,
  /// Windows). On a disk that is not such a disk, the rule for it only costs looks: it must
  /// do no harm. Without it, the engine finds out for each directory.
  loose_spelling: bool,
  /// The disk is such a disk, whatever the engine takes it for. Then some changes name an
  /// entry in upper case: the entry is the same, and the OS tells about a name that is gone
  /// in the spelling of the change. Some watched paths have a name in upper case too: a
  /// watched path is the path that its caller gave, and an entry that a change makes at it
  /// gets that name.
  ignore_case: bool,
  /// The directory `/b` is another disk, with the other rule (`SimFs::set_mount`).
  mount: bool,
  disk: DiskConfig,
  /// The shortest watched path, in names. With 3, the directories at the top only lead to
  /// watched paths, and a backend that watches whole trees does not watch them.
  shortest_path: u64,
  steps: usize,
  /// How many times everything is settled and checked before the end of the run.
  checks: usize,
}

impl Config {
  fn new(seed: u64, backend: Target, steps: usize) -> Self {
    let mut random = Random::new(seed ^ 0xC0F1_6000);
    let millis = Duration::from_millis;
    let mut config = Self {
      backend,
      filter: random.pick(&[0, 1, 1, 2, 3]),
      debounce: millis(random.pick(&[10, 10, 0, 50])),
      delete_delay: millis(random.pick(&[100, 100, 0, 400])),
      poll_interval: millis(if backend == Target::Poll {
        random.pick(&[100, 100, 1000])
      } else {
        1000
      }),
      disk: DiskConfig {
        time_step: random.pick(&[
          Duration::from_nanos(1),
          millis(1),
          millis(if backend == Target::Poll { 100 } else { 10 }),
          millis(1000),
          millis(2000),
        ]),
        birth_times: random.chance(40),
        reuse_inode_numbers: !random.chance(15),
        listing_kinds: !random.chance(20),
        listing_meta: random.chance(40),
      },
      shortest_path: random.pick(&[1, 2, 2, 3, 3]),
      steps,
      checks: if random.chance(50) { 2 } else { 0 },
      loose_spelling: random.chance(25),
      ignore_case: random.chance(25),
      mount: false,
    };
    config.mount = random.chance(15);
    config
  }

  /// Whether a part of the disk takes two spellings of a name for one.
  fn folds(&self) -> bool {
    self.ignore_case || self.mount
  }

  fn options(&self) -> Options {
    let filter = self.filter;
    let ignored: Ignored = Arc::new(move |path, kind| ignored(filter, path, kind));
    Options {
      ignored: (filter != 0).then_some(ignored),
      debounce: self.debounce,
      delete_delay: self.delete_delay,
      poll_interval: self.poll_interval,
      loose_spelling: self.loose_spelling,
    }
  }
}

fn ignored(filter: u8, path: &Path, kind: EntryKind) -> bool {
  let name = |name: &str| path.file_name().is_some_and(|last| last == name);
  let below_skip = path.parent().is_some_and(|dir| dir.iter().any(|name| name == "skip"));
  (filter & 1 != 0 && (below_skip || (kind == EntryKind::Dir && name("skip"))))
    || (filter & 2 != 0 && kind == EntryKind::File && name("c"))
}

/// A change of the disk. The paths are fixed when the run is made; what happens depends on
/// what is there when the change runs, and it may fail. The disk says what is true afterwards.
#[derive(Clone, Debug)]
enum Disk {
  /// Creates the file or writes it in place. `same_size`: eight bytes, like every other such
  /// write, so that the size never tells.
  /// `parents`: the directories above it are made first.
  Write {
    path: Place,
    same_size: bool,
    parents: bool,
  },
  /// `mkdir -p`.
  Mkdir {
    path: Place,
  },
  Remove {
    path: Place,
  },
  /// `rm -r`.
  RemoveAll {
    path: Place,
  },
  Rename {
    from: Place,
    to: Place,
  },
  /// A file becomes a directory with a file in it; a directory becomes a file.
  Flip {
    path: Place,
  },
  /// Two entries change places, by three renames.
  Swap {
    one: Place,
    other: Place,
  },
  /// Makes a link, or points an old link at `link` somewhere else.
  Symlink {
    target: Place,
    link: Place,
  },
  /// Moves a path above the watched path with this number out of the way. `level` picks which
  /// one.
  Away {
    number: u8,
    level: u8,
  },
  /// Writes a file inside the directory that `Away` moved last.
  WriteAway {
    rel: String,
  },
  /// Moves the last `Away` back.
  Back,
  /// Puts a link on the way to the watched path with this number: a directory there moves to a
  /// store and the link points to it; a link there is pointed at `target` or at another store.
  Relink {
    number: u8,
    level: u8,
    target: String,
    store: Option<u8>,
  },
}

#[derive(Clone, Debug)]
enum ToWatch {
  Path(String),
  /// A file or a directory that is on the disk.
  Existing(Place),
  /// A path below the watched path with this number.
  Below {
    number: u8,
    rel: String,
  },
  /// A path above it, `up` names shorter.
  Above {
    number: u8,
    up: u8,
  },
  /// A path that cannot be watched.
  Bad(&'static str),
}

/// A path for `unwatch`.
#[derive(Clone, Debug)]
enum ToUnwatch {
  /// The watched path with this number.
  Watched(u8),
  /// Any path. Most of the time it is not watched.
  Path(String),
  /// A path that cannot be watched.
  Bad(&'static str),
}

/// What happens to the notifications that wait.
#[derive(Clone, Debug)]
enum Queue {
  Deliver {
    count: u8,
  },
  DeliverAll,
  /// Some notifications come twice, the second time in the wrong order.
  DeliverTwice {
    count: u8,
  },
  /// Some notifications are taken out now and handed over by a later `DeliverHeld`.
  Hold {
    count: u8,
  },
  DeliverHeld,
  /// The queue of the runtime drops notifications and says so.
  Drop {
    count: u8,
  },
  /// The OS drops notifications and says so in its own way.
  Lose {
    count: u8,
  },
}

#[derive(Clone, Debug)]
enum Op {
  Disk(Disk),
  /// The change runs in the middle of an engine call: before the disk or backend call that
  /// comes `after` such calls from now.
  Trap {
    after: u8,
    change: Disk,
  },
  /// `settled`: everything is delivered and checked before the paths are added.
  Watch {
    paths: Vec<ToWatch>,
    settled: bool,
  },
  Unwatch {
    paths: Vec<ToUnwatch>,
  },
  /// A later call of the engine takes this long: the clock of the disk moves in the middle of
  /// it.
  Slow {
    after: u8,
    millis: u8,
  },
  Queue(Queue),
  Advance {
    millis: u32,
  },
  Tick,
  /// Moves the clock to the next deadline and ticks.
  Step,
  /// Takes away the permission to list a directory or to read a file.
  Deny {
    path: Place,
  },
  /// Gives the permission back for one of the paths that lost it.
  Allow {
    which: u8,
  },
  WatchLimit(Option<u8>),
  /// Every read of the disk fails: as if unplugged (`gone`), or with an error that says nothing.
  /// `after`: not now, but in the middle of an engine call, before the disk or backend call
  /// that comes `after` such calls from now. Only an error that says nothing comes that way. A
  /// drive that is unplugged between the `watch` and the `commit` of a stream takes no stream
  /// with it, so nothing tells when it is back: that is the known hole of the prefix backend
  /// once more (a directory above a stream that is away and back before the next poll).
  Offline {
    gone: bool,
    after: Option<u8>,
  },
  Online,
  BackendDies,
  /// Delivers everything, lets every delay and two polls pass, and checks the end state.
  Settle,
}

fn random_disk(random: &mut Random) -> Disk {
  let path = random.place();
  match random.below(100) {
    0..32 => Disk::Write { path, same_size: true, parents: random.chance(70) },
    32..40 => Disk::Write { path, same_size: false, parents: random.chance(70) },
    40..50 => Disk::Mkdir { path },
    50..55 => Disk::Remove { path },
    55..61 => Disk::RemoveAll { path },
    61..74 => Disk::Rename { from: path, to: random.place() },
    74..77 => Disk::Flip { path },
    77..80 => Disk::Swap { one: path, other: random.place() },
    80..85 => {
      // Half of the time an old link is pointed somewhere else.
      let link = if random.chance(50) { Place::Link(random.small(8)) } else { random.place() };
      Disk::Symlink { target: path, link }
    }
    85..89 => Disk::Away { number: random.small(8), level: random.small(4) },
    89..93 => Disk::WriteAway { rel: random.path(1) },
    93..97 => Disk::Back,
    _ => Disk::Relink {
      number: random.small(8),
      level: random.small(4),
      target: random.path(1),
      store: random.chance(50).then(|| random.small(4)),
    },
  }
}

fn random_to_watch(random: &mut Random, shortest: u64) -> ToWatch {
  match random.below(100) {
    0..35 => ToWatch::Path(random.path(shortest)),
    35..50 => ToWatch::Existing(Place::File(random.small(32))),
    50..60 => ToWatch::Existing(Place::Dir { which: random.small(32), rel: String::new() }),
    60..80 => ToWatch::Below { number: random.small(8), rel: random.path(1) },
    80..95 => ToWatch::Above { number: random.small(8), up: 1 + random.small(2) },
    _ => ToWatch::Bad(random.pick(&["relative/a", "/a/../b", "/", ""])),
  }
}

fn random_to_unwatch(random: &mut Random) -> ToUnwatch {
  match random.below(100) {
    0..80 => ToUnwatch::Watched(random.small(8)),
    80..95 => ToUnwatch::Path(random.path(1)),
    _ => ToUnwatch::Bad(random.pick(&["relative/a", "/a/../b", "/", ""])),
  }
}

fn random_ops(seed: u64, config: &Config) -> Vec<Op> {
  let mut random = Random::new(seed);
  let mut ops = Vec::with_capacity(config.steps);
  let check_every = config.steps / (config.checks + 1);
  // Something to watch from the start: a few directories and files, and a few watched paths.
  for _ in 0..random.below(10) {
    let path = Place::Path(random.path(1));
    ops.push(Op::Disk(if random.chance(50) {
      Disk::Mkdir { path }
    } else {
      Disk::Write { path, same_size: true, parents: true }
    }));
  }
  let first = 1 + random.below(3);
  let paths = (0..first).map(|_| ToWatch::Path(random.path(config.shortest_path))).collect();
  ops.push(Op::Watch { paths, settled: false });
  for step in ops.len()..config.steps {
    if step % check_every == 0 {
      ops.push(Op::Settle);
    }
    ops.push(match random.below(1000) {
      0..515 => Op::Disk(random_disk(&mut random)),
      515..540 => {
        let count = 1 + random.below(2);
        Op::Unwatch { paths: (0..count).map(|_| random_to_unwatch(&mut random)).collect() }
      }
      540..580 => {
        // Only a change that needs nothing from the run can wait inside the disk.
        let change = loop {
          let change = random_disk(&mut random);
          if !matches!(
            change,
            Disk::Away { .. } | Disk::WriteAway { .. } | Disk::Back | Disk::Relink { .. }
          ) {
            break change;
          }
        };
        Op::Trap { after: random.small(24), change }
      }
      580..640 => {
        let count = 1 + random.below(3);
        let paths =
          (0..count).map(|_| random_to_watch(&mut random, config.shortest_path)).collect();
        Op::Watch { paths, settled: random.chance(30) }
      }
      640..648 => Op::Slow { after: random.small(24), millis: 1 + random.small(30) },
      648..720 => Op::Queue(Queue::Deliver { count: 1 + random.small(4) }),
      720..745 => Op::Queue(Queue::DeliverAll),
      745..760 => Op::Queue(Queue::DeliverTwice { count: 1 + random.small(3) }),
      760..772 => Op::Queue(Queue::Hold { count: 1 + random.small(3) }),
      772..784 => Op::Queue(Queue::DeliverHeld),
      784..799 => Op::Queue(Queue::Drop { count: 1 + random.small(5) }),
      799..814 => Op::Queue(Queue::Lose { count: 1 + random.small(5) }),
      814..864 => Op::Advance { millis: u32::try_from(random.below(40)).unwrap() },
      864..880 => Op::Advance { millis: u32::try_from(random.below(1500)).unwrap() },
      880..925 => Op::Tick,
      925..975 => Op::Step,
      975..979 => Op::Deny { path: random.place() },
      979..986 => Op::Allow { which: random.small(4) },
      986..990 => Op::WatchLimit(random.pick(&[None, None, Some(0), Some(2), Some(5)])),
      990..992 => match random.below(3) {
        0 => Op::Offline { gone: true, after: None },
        1 => Op::Offline { gone: false, after: None },
        _ => Op::Offline { gone: false, after: Some(random.small(24)) },
      },
      992..999 => Op::Online,
      _ => Op::BackendDies,
    });
  }
  ops
}

// ---- the rules of the design, written down a second time ----
//
// The walk of the disk and the look at one file are in `pathwatch_sim` (`caught_up.rs`), where
// the driver of the other tests uses them too.

/// The files a handler must know when nothing is in flight, read from the disk and the watched
/// paths alone.
fn reportable(fs: &SimFs, filter: u8, watched: &BTreeSet<PathBuf>) -> BTreeSet<PathBuf> {
  pathwatch_sim::reportable(fs, &|path, kind| ignored(filter, path, kind), watched)
}

/// Whether the handler keeps a file at `path` that it knows when the watched paths are
/// `watched`: the path is a watched path, or it is in a watched directory with no ignored
/// directory on the way down. This is read from the text of the paths alone: `unwatch` looks at
/// no disk to decide what it forgets.
fn covered(filter: u8, watched: &BTreeSet<PathBuf>, path: &Path) -> bool {
  watched.contains(path) || path.parent().is_some_and(|dir| tracked(filter, watched, dir))
}

/// Whether `path`, or a directory on the way down to it from the watched directory above, was
/// registered while the watched paths were `before`. On a disk that takes two spellings of a
/// name for one, such a name can be another spelling of an entry of that directory, and then
/// the directory does not cover it. No text says that: it is what the engine saw on the disk.
fn through_registered(
  before: &BTreeSet<PathBuf>,
  watched: &BTreeSet<PathBuf>,
  path: &Path,
) -> bool {
  let mut on_the_way = path.ancestors().take_while(|at| !watched.contains(*at));
  on_the_way.any(|at| before.iter().any(|old| old.starts_with(at)))
}

/// Whether a directory at `dir` would track every entry.
fn tracked(filter: u8, watched: &BTreeSet<PathBuf>, dir: &Path) -> bool {
  let inside = watched.contains(dir) || dir.parent().is_some_and(|up| tracked(filter, watched, up));
  inside && !ignored(filter, dir, EntryKind::Dir)
}

/// What a handler reads at a path when it gets an event for it.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Content {
  Bytes(String),
  /// A link that is not followed: a file entry of its own.
  Link(PathBuf),
  /// No file: the path is gone, or it is a directory.
  Nothing,
  /// The disk changed while the engine read it, so the handler may have read the state
  /// before the change or the one after it.
  Either,
}

fn content(fs: &SimFs, followed: &BTreeSet<PathBuf>, path: &Path) -> Content {
  if !followed.contains(path)
    && let Some(target) = fs.read_link(path)
  {
    return Content::Link(target);
  }
  fs.read(path)
    .map_or(Content::Nothing, |bytes| Content::Bytes(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Whether the engine takes `path` for a directory.
fn is_directory(fs: &SimFs, followed: &BTreeSet<PathBuf>, path: &Path) -> bool {
  let meta = fs.quiet(|| if followed.contains(path) { fs.stat(path) } else { fs.lstat(path) });
  meta.is_ok_and(|meta| meta.kind == FileType::Dir)
}

/// For every path on the way to a watched path: the directory it leads to now, and whether a
/// poll has to look before the disk changes again, for the prefix backend, when that changes.
/// A stream reports what happens at and below a watched directory, and below the directory
/// that holds a watched path with a file or nothing at it, by real path. It ends when a
/// directory above that one is renamed or removed, and says so. Nothing tells about a path
/// that is reached through a link.
fn ways(
  fs: &SimFs,
  watched: &BTreeSet<PathBuf>,
) -> BTreeMap<PathBuf, (Option<(u64, PathBuf)>, bool)> {
  let mut ways = BTreeMap::new();
  for path in followed(watched) {
    if path.parent().is_none() {
      continue;
    }
    let linked = path.ancestors().any(|above| fs.read_link(above).is_some());
    let object = fs.dir_object(&path);
    ways.insert(path, (object, linked));
  }
  ways
}

// ---- the run ----

/// What the run shares with a change that waits inside the disk.
struct Shared {
  filter: u8,
  trace: Mutex<Vec<String>>,
  watched: Mutex<BTreeSet<PathBuf>>,
  /// Gives every write its own content and every helper directory its own name.
  serial: AtomicU64,
  /// Set by a change that only a poll can see with the prefix backend.
  moved_unseen: AtomicBool,
  watch_moves: bool,
  /// `Config::folds`.
  folds: bool,
  /// How many operations of the run were started.
  reached: AtomicUsize,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
  mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
  fn say(&self, line: String) {
    lock(&self.trace).push(line);
  }

  fn serial(&self) -> u64 {
    self.serial.fetch_add(1, Ordering::Relaxed) + 1
  }

  /// Runs a change of the disk and notes when it moved something that only a poll can see.
  fn change(&self, fs: &SimFs, what: &str, change: impl FnOnce() -> String) {
    let before = self.watch_moves.then(|| ways(fs, &lock(&self.watched)));
    let line = change();
    if let Some(before) = before {
      let after = ways(fs, &lock(&self.watched));
      let moved = after.iter().any(|(path, (object, polled))| {
        before.get(path).is_some_and(|(was, was_polled)| was != object && (*polled || *was_polled))
      });
      if moved {
        self.moved_unseen.store(true, Ordering::Relaxed);
      }
    }
    self.say(format!("{what}{line}"));
  }
}

fn outcome(result: &io::Result<()>) -> &'static str {
  if result.is_ok() { "" } else { "  (failed)" }
}

/// Runs a change that needs nothing from the run. Says what it did.
fn change_disk(fs: &SimFs, change: &Disk, shared: &Shared) -> String {
  let serial = shared.serial();
  let at = |place: &Place| place.resolve(fs, shared.filter, &lock(&shared.watched));
  // Every third change names its entry in upper case, on a disk that takes that for the same
  // name. Only an entry that is there: a new entry would get the name.
  let in_upper_case = |path: String| {
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", &path));
    let is_there =
      fs.read(&path).is_some() || fs.read_link(&path).is_some() || fs.dir_object(&path).is_some();
    if shared.folds && serial.is_multiple_of(3) && is_there {
      format!("{dir}/{}", name.to_ascii_uppercase())
    } else {
      path
    }
  };
  match change {
    Disk::Write { path, same_size, parents } => {
      let path = in_upper_case(at(path));
      if *parents && let Some(parent) = Path::new(&path).parent() {
        let _ = fs.mkdir_all(parent);
      }
      let content = if *same_size {
        format!("{serial:08}")
      } else {
        format!("{serial}{}", "x".repeat(usize::try_from(serial % 7).unwrap()))
      };
      let result = fs.write(&path, &content);
      format!("write {path} {content:?}{}", outcome(&result))
    }
    Disk::Mkdir { path } => {
      let path = at(path);
      format!("mkdir -p {path}{}", outcome(&fs.mkdir_all(&path)))
    }
    Disk::Remove { path } => {
      let path = in_upper_case(at(path));
      format!("rm {path}{}", outcome(&fs.remove(&path)))
    }
    Disk::RemoveAll { path } => {
      let path = in_upper_case(at(path));
      format!("rm -r {path}{}", outcome(&fs.remove_all(&path)))
    }
    Disk::Rename { from, to } => {
      let (from, to) = (in_upper_case(at(from)), at(to));
      format!("mv {from} {to}{}", outcome(&fs.rename(&from, &to)))
    }
    Disk::Flip { path } => {
      let path = at(path);
      let kind = fs.quiet(|| fs.lstat(Path::new(&path))).ok().map(|meta| meta.kind);
      let result = match kind {
        Some(FileType::Dir) => {
          fs.remove_all(&path).and_then(|()| fs.write(&path, format!("{serial:08}")))
        }
        Some(_) => fs
          .remove(&path)
          .and_then(|()| fs.mkdir(&path))
          .and_then(|()| fs.write(format!("{path}/a"), format!("{serial:08}"))),
        None => Err(io::ErrorKind::NotFound.into()),
      };
      format!("flip {path} (was {kind:?}){}", outcome(&result))
    }
    Disk::Swap { one, other } => {
      let (one, other) = (at(one), at(other));
      let spare = format!("/swap{serial}");
      let result = fs.rename(&one, &spare).and_then(|()| {
        // When the second step fails, the first one is taken back.
        fs.rename(&other, &one).or_else(|error| fs.rename(&spare, &one).and(Err(error)))
      });
      let result = result.and_then(|()| fs.rename(&spare, &other));
      format!("swap {one} {other}{}", outcome(&result))
    }
    Disk::Symlink { target, link } => {
      let (target, link) = (at(target), at(link));
      // No file is thrown away for the link: only an old link makes room.
      if fs.read_link(&link).is_some() {
        let _ = fs.remove(&link);
      }
      format!("ln -sfn {target} {link}{}", outcome(&fs.symlink(&target, &link)))
    }
    Disk::Away { .. } | Disk::WriteAway { .. } | Disk::Back | Disk::Relink { .. } => {
      unreachable!("this change needs the run")
    }
  }
}

struct Run<B: Deliver> {
  config: Config,
  engine: Engine<B, SimFs>,
  fs: SimFs,
  now: Instant,
  shared: Arc<Shared>,
  watched: BTreeSet<PathBuf>,
  followed: BTreeSet<PathBuf>,
  /// What the handler knows: every file it was told about, with the content the file had when
  /// the handler heard of it last.
  known: BTreeMap<PathBuf, Content>,
  held: Vec<B::Raw>,
  /// The paths that `Away` moved, with the place each one is at now.
  away: Vec<(PathBuf, PathBuf)>,
  /// The directories that `Relink` moved behind a link.
  stores: Vec<PathBuf>,
  /// The paths that `Deny` took the permission from.
  denied: Vec<String>,
  offline: bool,
  /// The prefix backend was not told about a move. A poll has to see it before the disk
  /// changes again: `docs/design.md` lists "away and back before the next poll" as a
  /// known hole.
  poll_first: bool,
}

/// Stops the run. The tag in front names the rule that broke, so that a cut-down run can be
/// held to the same failure.
macro_rules! fail {
  ($tag:literal, $($message:tt)*) => {
    panic!("[{}] {}", $tag, format!($($message)*))
  };
}

impl<B: Deliver<Raw: Clone>> Run<B> {
  fn new(config: &Config, fs: SimFs, backend: B, shared: Arc<Shared>) -> Self {
    fs.set_time_step(config.disk.time_step);
    fs.set_birth_times(config.disk.birth_times);
    fs.set_reuse_inode_numbers(config.disk.reuse_inode_numbers);
    fs.set_listing_kinds(config.disk.listing_kinds);
    fs.set_listing_meta(config.disk.listing_meta);
    fs.set_ignore_case(config.ignore_case);
    if config.mount {
      fs.set_mount("b");
    }
    Self {
      config: config.clone(),
      engine: Engine::new(backend, fs.clone(), config.options()),
      fs,
      now: Instant::now(),
      shared,
      watched: BTreeSet::new(),
      followed: BTreeSet::new(),
      known: BTreeMap::new(),
      held: Vec::new(),
      away: Vec::new(),
      stores: Vec::new(),
      denied: Vec::new(),
      offline: false,
      poll_first: false,
    }
  }

  fn say(&self, line: String) {
    self.shared.say(line);
  }

  fn watched_path(&self, number: u8) -> Option<&PathBuf> {
    self.watched.iter().nth(usize::from(number) % self.watched.len().max(1))
  }

  /// The path `level` names long on the way to the watched path with this number.
  fn on_the_way(&self, number: u8, level: u8) -> Option<PathBuf> {
    let watched = self.watched_path(number)?;
    let names = watched.components().count() - 1;
    let keep = 1 + usize::from(level) % names;
    Some(watched.components().take(1 + keep).collect())
  }

  // -- the handler --

  /// What the handler reads at `path` when the engine tells it about the file.
  ///
  /// With a change of the disk in the middle of the call, the engine may have read it before
  /// it. If the file then goes back to exactly what the engine saw (the same inode, time and
  /// size), no event follows, and none can: an equal fingerprint is what makes a repeated hint
  /// cost nothing. So after such a call the content is not held against the engine.
  fn content_told(&self, path: &Path, trapped: bool) -> Content {
    if trapped { Content::Either } else { content(&self.fs, &self.followed, path) }
  }

  fn hear(&mut self, out: &Output, trapped: bool) {
    // The order of a batch is not promised: it is sorted here, and no rule below depends on it.
    let mut events: Vec<&Event> = out.events.iter().collect();
    events.sort_by(|a, b| a.path.cmp(&b.path));
    let paths: BTreeSet<&Path> = events.iter().map(|event| event.path.as_path()).collect();
    if paths.len() != events.len() {
      fail!("batch", "a path twice: {:?}", out.events);
    }
    let mut line = String::new();
    for event in events {
      let path = &event.path;
      let content = self.content_told(path, trapped);
      let _ = write!(line, " {:?} {}", event.kind, path.display());
      match event.kind {
        EventKind::Create => {
          if self.known.insert(path.clone(), content).is_some() {
            fail!("create", "Create for a file the handler knows: {}", path.display());
          }
        }
        EventKind::Update => {
          if self.known.insert(path.clone(), content).is_none() {
            fail!("update", "Update for a file the handler does not know: {}", path.display());
          }
        }
        EventKind::Delete => {
          if self.known.remove(path).is_none() {
            fail!("delete", "Delete for a file the handler does not know: {}", path.display());
          }
        }
      }
      // The disk did not change since the engine read it, so the event must fit it.
      if !trapped && event.kind != EventKind::Delete && is_directory(&self.fs, &self.followed, path)
      {
        fail!("directory", "{:?} for a directory: {}", event.kind, path.display());
      }
    }
    if !line.is_empty() {
      self.say(format!("    ->{line}"));
    }
    // A path is a file or a directory for the handler, never both: when the kind changes, the
    // `Delete`s are in the same batch as the `Create`s.
    for (above, below) in self.known.keys().zip(self.known.keys().skip(1)) {
      if below.starts_with(above) {
        fail!("kind", "the handler knows {} and {}", above.display(), below.display());
      }
    }
    for error in &out.errors {
      self.say(format!("    error: {error}"));
    }
  }

  fn check_view(&self) {
    let view = self.engine.view();
    if !view.iter().eq(self.known.keys()) {
      let known: BTreeSet<&PathBuf> = self.known.keys().collect();
      let view: BTreeSet<&PathBuf> = view.iter().collect();
      fail!(
        "view",
        "the handler and the engine know different files.\n  only the handler: {:?}\n  only the \
         engine: {:?}",
        known.difference(&view).collect::<Vec<_>>(),
        view.difference(&known).collect::<Vec<_>>()
      );
    }
  }

  fn check_invariants(&self) {
    if let Err(broken) = self.engine.check_invariants() {
      fail!("invariant", "{broken}");
    }
  }

  // -- calls of the engine --

  fn watch(&mut self, paths: &[ToWatch], settled: bool) {
    let mut texts = Vec::new();
    let mut new_watched = self.watched.clone();
    for path in paths {
      let (text, valid) = match path {
        ToWatch::Path(path) => (path.clone(), true),
        ToWatch::Existing(place) => {
          (place.resolve(&self.fs, self.config.filter, &self.watched), true)
        }
        ToWatch::Below { number, rel } => match self.watched_path(*number) {
          Some(path) => (format!("{}{rel}", path.display()), true),
          None => (rel.clone(), true),
        },
        ToWatch::Above { number, up } => {
          let above =
            self.watched_path(*number).and_then(|path| path.ancestors().nth(usize::from(*up)));
          match above {
            Some(above) if above.parent().is_some() => (above.display().to_string(), true),
            _ => continue,
          }
        }
        ToWatch::Bad(path) => ((*path).to_owned(), false),
      };
      let respell = valid && self.config.folds();
      let text = if respell { self.in_the_way_of_a_caller(&text) } else { text };
      // A path that the filter ignores for both kinds does not become a watched path.
      let filter = self.config.filter;
      if valid && can_be_watched(&|path, kind| ignored(filter, path, kind), Path::new(&text)) {
        // The registered form of these paths is the text itself.
        new_watched.insert(PathBuf::from(&text));
      }
      texts.push(text);
    }
    self.say(format!("watch {texts:?}"));

    // Silence is only for what no older watched path had to report.
    let before = reportable(&self.fs, self.config.filter, &self.watched);
    let after = reportable(&self.fs, self.config.filter, &new_watched);
    self.followed = followed(&new_watched);
    self.watched.clone_from(&new_watched);
    *lock(&self.shared.watched) = new_watched;

    let traps = self.fs.traps_run();
    let mut out = Output::default();
    self.engine.watch(texts.iter().map(PathBuf::from).collect(), self.now, &mut out);
    let trapped = self.fs.traps_run() != traps;

    // What `watch` leaves unsaid: with a change of the disk in the middle of the call, a file
    // that came or went in that time may be part of it or not, so the engine is believed.
    let silent: Vec<PathBuf> = if trapped {
      self.engine.view().into_iter().collect()
    } else {
      after.difference(&before).cloned().collect()
    };
    if settled && !trapped {
      // The tree was in line with the disk, so the engine knew what was reportable before.
      for event in &out.events {
        if event.kind == EventKind::Create && silent.contains(&event.path) {
          fail!("baseline", "Create for a file of the baseline: {}", event.path.display());
        }
      }
    }
    // When the tree was behind the disk, the engine may count a file of the baseline as a
    // file of an older watched directory and say `Create`. That is one event too many, not a
    // missed one. The other way round is checked by `check_view`: no file is known in silence
    // but these.
    self.hear(&out, trapped);
    for path in silent {
      if !self.known.contains_key(&path) {
        let content = self.content_told(&path, trapped);
        self.known.insert(path, content);
      }
    }
    self.check_view();
  }

  /// A watched path as a caller can spell it on a disk that takes two spellings of a name for
  /// one: of three paths, one has its last name in upper case, and one the name before that.
  fn in_the_way_of_a_caller(&self, path: &str) -> String {
    let mut names: Vec<String> = path.split('/').map(str::to_owned).collect();
    let pick = match self.shared.serial() % 3 {
      0 => names.len().checked_sub(1),
      1 => names.len().checked_sub(2),
      _ => None,
    };
    // Not the empty name in front of the first `/`.
    if let Some(name) = pick.filter(|&at| at > 0).and_then(|at| names.get_mut(at)) {
      name.make_ascii_uppercase();
    }
    names.join("/")
  }

  fn unwatch(&mut self, paths: &[ToUnwatch]) {
    let mut texts = Vec::new();
    let mut new_watched = self.watched.clone();
    for path in paths {
      let text = match path {
        ToUnwatch::Watched(number) => match self.watched_path(*number) {
          Some(path) => path.display().to_string(),
          None => continue,
        },
        ToUnwatch::Path(path) => path.clone(),
        ToUnwatch::Bad(path) => (*path).to_owned(),
      };
      new_watched.remove(Path::new(&text));
      texts.push(text);
    }
    self.say(format!("unwatch {texts:?}"));
    self.followed = followed(&new_watched);
    let before = mem::replace(&mut self.watched, new_watched.clone());
    *lock(&self.shared.watched) = new_watched;

    let traps = self.fs.traps_run();
    let mut out = Output::default();
    self.engine.unwatch(texts.iter().map(PathBuf::from).collect(), self.now, &mut out);
    let trapped = self.fs.traps_run() != traps;

    // The handler forgets what no watched path covers any more, and no event tells about it:
    // a `Delete` for such a file fails in `hear`. Everything else stays known, also a file
    // whose `Delete` waits.
    let (filter, watched) = (self.config.filter, &self.watched);
    self.known.retain(|path, _| covered(filter, watched, path));
    if self.config.folds() {
      // Where only the disk says whether a watched directory covers a name, the engine is
      // believed: it kept the answer of its last look, and `Engine::check_spellings` holds
      // that answer to the disk. A `Create` in the same call is for a file that was forgotten.
      let view = self.engine.view();
      let event = |path: &Path| out.events.iter().find(|event| event.path == path);
      self.known.retain(|path, _| {
        !through_registered(&before, watched, path)
          || event(path).map_or(view.contains(path), |event| event.kind != EventKind::Create)
      });
    }
    self.hear(&out, trapped);
    self.check_view();
  }

  fn tick(&mut self) {
    let poll = self.engine.poll_due(self.now);
    let traps = self.fs.traps_run();
    let mut out = Output::default();
    self.engine.tick(self.now, &mut out);
    let trapped = self.fs.traps_run() != traps;
    self.hear(&out, trapped);
    self.check_view();
    if poll && !trapped && !self.offline {
      self.poll_first = false;
    }
  }

  fn advance(&mut self, by: Duration) {
    self.now += by;
    self.fs.advance(by);
  }

  /// Moves the clock to the next deadline and ticks. `false`: there is none.
  fn step(&mut self) -> bool {
    let Some(deadline) = self.engine.deadline() else { return false };
    self.advance(deadline.saturating_duration_since(self.now));
    self.tick();
    true
  }

  fn signal(&mut self, raw: B::Raw) {
    self.engine.signal(Signal::Raw(raw), self.now);
  }

  fn deliver(&mut self, count: usize) {
    for raw in B::take_raw(&self.fs, count) {
      self.signal(raw);
    }
  }

  fn set_online(&mut self) {
    self.offline = false;
    self.fs.set_offline(None);
  }

  // -- changes of the disk --

  /// Lets a poll see a move that the prefix backend was not told about, before the disk
  /// changes again.
  fn let_the_poll_see(&mut self) {
    if !self.poll_first {
      return;
    }
    self.say("  (a poll first)".to_owned());
    self.fs.clear_traps();
    self.set_online();
    while self.poll_first {
      if !self.step() {
        self.poll_first = false;
      }
    }
  }

  fn permit(&mut self, path: String, denied: bool) {
    self.let_the_poll_see();
    let fs = self.fs.clone();
    let mut done = false;
    self.shared.change(&fs, "", || {
      done = fs.set_denied(&path, denied).is_ok();
      format!("deny {path} {denied}{}", if done { "" } else { "  (failed)" })
    });
    if done {
      if denied {
        self.denied.push(path);
      }
      // Nothing tells about a change of permissions: the next scan finds it.
      self.engine.overflow(self.now);
    }
  }

  fn disk(&mut self, change: &Disk) {
    let fs = self.fs.clone();
    let shared = Arc::clone(&self.shared);
    match change {
      Disk::Away { number, level } => {
        let Some(path) = self.on_the_way(*number, *level) else { return };
        let place = PathBuf::from(format!("/away{}", shared.serial()));
        shared.change(&fs, "", || {
          let result = fs.rename(&path, &place);
          if result.is_ok() {
            self.away.push((path.clone(), place.clone()));
          }
          format!("mv {} {}{}", path.display(), place.display(), outcome(&result))
        });
      }
      Disk::WriteAway { rel } => {
        let Some((_, place)) = self.away.last() else { return };
        let path = format!("{}{rel}", place.display());
        let content = format!("{:08}", shared.serial());
        shared.change(&fs, "", || {
          format!("write {path} {content:?}{}", outcome(&fs.write(&path, &content)))
        });
      }
      Disk::Back => {
        let Some((path, place)) = self.away.pop() else { return };
        shared.change(&fs, "", || {
          let result = fs.rename(&place, &path);
          format!("mv {} {}{}", place.display(), path.display(), outcome(&result))
        });
      }
      Disk::Relink { number, level, target, store } => {
        let Some(path) = self.on_the_way(*number, *level) else { return };
        let old_store = store
          .filter(|_| !self.stores.is_empty())
          .map(|store| self.stores[usize::from(store) % self.stores.len()].clone());
        let is_dir = fs.quiet(|| fs.lstat(&path)).is_ok_and(|meta| meta.kind == FileType::Dir);
        let new_store = PathBuf::from(format!("/store{}", shared.serial()));
        shared.change(&fs, "", || {
          if is_dir {
            let result = fs.rename(&path, &new_store).and_then(|()| fs.symlink(&new_store, &path));
            if result.is_ok() {
              self.stores.push(new_store.clone());
            }
            format!(
              "mv {0} {1}; ln -s {1} {0}{2}",
              path.display(),
              new_store.display(),
              outcome(&result)
            )
          } else {
            let target = old_store.unwrap_or_else(|| PathBuf::from(target));
            // No file is thrown away for the link: only an old link makes room.
            if fs.read_link(&path).is_some() {
              let _ = fs.remove(&path);
            }
            let result = fs.symlink(&target, &path);
            format!("ln -sfn {} {}{}", target.display(), path.display(), outcome(&result))
          }
        });
      }
      stateless => shared.change(&fs, "", || change_disk(&fs, stateless, &shared)),
    }
  }

  fn queue(&mut self, how: &Queue) {
    match how {
      Queue::Deliver { count } => {
        self.say(format!("deliver {count} of {}", self.fs.pending_raw()));
        self.deliver(usize::from(*count));
      }
      Queue::DeliverAll => {
        self.say(format!("deliver all {}", self.fs.pending_raw()));
        self.deliver(usize::MAX);
      }
      Queue::DeliverTwice { count } => {
        let raws = B::take_raw(&self.fs, usize::from(*count));
        self.say(format!("deliver {} twice", raws.len()));
        for raw in raws.iter().chain(raws.iter().rev()).cloned() {
          self.signal(raw);
        }
      }
      Queue::Hold { count } => {
        let mut raws = B::take_raw(&self.fs, usize::from(*count));
        self.say(format!("hold back {}", raws.len()));
        self.held.append(&mut raws);
      }
      Queue::DeliverHeld => {
        self.say(format!("deliver the {} held back", self.held.len()));
        for raw in std::mem::take(&mut self.held) {
          self.signal(raw);
        }
      }
      Queue::Drop { count } => {
        let dropped = B::take_raw(&self.fs, usize::from(*count)).len();
        self.say(format!("the queue drops {dropped}"));
        if dropped > 0 {
          self.engine.overflow(self.now);
        }
      }
      Queue::Lose { count } => {
        self.say(format!("the OS loses {}", self.fs.lose_raw(usize::from(*count))));
      }
    }
  }

  fn run(&mut self, op: &Op) {
    match op {
      Op::Disk(change) => {
        self.let_the_poll_see();
        self.disk(change);
      }
      Op::Trap { after, change } => {
        self.let_the_poll_see();
        let (shared, change) = (Arc::clone(&self.shared), change.clone());
        self.say(format!("in the middle of a later call, {after} calls from now: {change:?}"));
        self.fs.at_call(u64::from(*after), move |fs| {
          shared.change(fs, "    (in the middle) ", || change_disk(fs, &change, &shared));
        });
      }
      Op::Slow { after, millis } => {
        self.say(format!("a later call takes {millis} ms, {after} calls from now"));
        let by = Duration::from_millis(u64::from(*millis));
        self.fs.at_call(u64::from(*after), move |fs| fs.advance(by));
      }
      Op::Watch { paths, settled } => {
        if *settled {
          self.settle_and_check();
        }
        self.watch(paths, *settled);
      }
      Op::Unwatch { paths } => self.unwatch(paths),
      Op::Queue(how) => self.queue(how),
      Op::Advance { millis } => {
        self.say(format!("{millis} ms pass, tick"));
        self.advance(Duration::from_millis(u64::from(*millis)));
        self.tick();
      }
      Op::Tick => {
        self.say("tick".to_owned());
        self.tick();
      }
      Op::Step => {
        self.say("to the next deadline, tick".to_owned());
        self.step();
      }
      Op::Deny { path } => {
        let path = path.resolve(&self.fs, self.config.filter, &self.watched);
        self.permit(path, true);
      }
      Op::Allow { which } => {
        if !self.denied.is_empty() {
          let path = self.denied.remove(usize::from(*which) % self.denied.len());
          self.permit(path, false);
        }
      }
      Op::WatchLimit(limit) => {
        self.say(format!("watch limit {limit:?}"));
        self.fs.set_watch_limit(limit.map(usize::from));
      }
      Op::Offline { gone, after } => {
        self.offline = true;
        let kind = if *gone { io::ErrorKind::NotFound } else { io::ErrorKind::TimedOut };
        match after {
          None => {
            self.say(format!("the disk is offline (gone: {gone})"));
            self.fs.set_offline(Some(kind));
          }
          Some(after) => {
            self.say(format!(
              "in the middle of a later call, {after} calls from now: the disk is offline"
            ));
            self.fs.at_call(u64::from(*after), move |fs| fs.set_offline(Some(kind)));
          }
        }
      }
      Op::Online => {
        self.say("the disk is online".to_owned());
        self.set_online();
      }
      Op::BackendDies => {
        self.say("the backend dies".to_owned());
        self.engine.signal(Signal::Failed(io::Error::other("the backend died")), self.now);
      }
      Op::Settle => self.settle_and_check(),
    }
    if self.shared.moved_unseen.swap(false, Ordering::Relaxed) {
      self.poll_first = true;
      self.fs.clear_traps();
    }
    self.check_invariants();
  }

  fn settle_and_check(&mut self) {
    self.say("settle".to_owned());
    self.fs.clear_traps();
    self.set_online();
    for raw in std::mem::take(&mut self.held) {
      self.signal(raw);
    }
    let config = &self.config;
    // The last two seconds are for loss reports that wait for the end of a rest after a slow
    // scan, and for a young file that cannot be compared: where only the poll looks at it, the
    // `Update` that it may owe comes with the first poll that finds it two seconds old.
    let end = self.now
      + config.debounce
      + config.delete_delay
      + config.poll_interval * 2
      + Duration::from_secs(2);
    for ticks in 0.. {
      if ticks == 100_000 {
        fail!("spin", "a deadline of the engine never passes: it has work that no tick does");
      }
      self.deliver(usize::MAX);
      match self.engine.deadline() {
        Some(deadline) if deadline <= end => {
          self.advance(deadline.saturating_duration_since(self.now));
          self.tick();
        }
        _ => break,
      }
    }
    self.advance(end.saturating_duration_since(self.now));
    self.poll_first = false;
    self.check_invariants();

    let expected = reportable(&self.fs, self.config.filter, &self.watched);
    if !expected.iter().eq(self.known.keys()) {
      let known: BTreeSet<&PathBuf> = self.known.keys().collect();
      let disk: BTreeSet<&PathBuf> = expected.iter().collect();
      fail!(
        "files",
        "the handler knows other files than the disk has.\n  only the handler: {:?}\n  only the \
         disk: {:?}",
        known.difference(&disk).collect::<Vec<_>>(),
        disk.difference(&known).collect::<Vec<_>>()
      );
    }
    for (path, told) in &self.known {
      let now = content(&self.fs, &self.followed, path);
      if *told != now && *told != Content::Either {
        fail!(
          "content",
          "{} changed after its last event: the handler has {told:?}, the disk {now:?}",
          path.display()
        );
      }
    }
    // Also after a change in the middle of a call, where the content above is not held against
    // the engine: whatever the engine read then, it must have looked again since.
    if let Err(stale) = check_fingerprints(&self.fs, &self.watched, &self.engine.fingerprints()) {
      fail!("stale", "{stale}");
    }
    if let Err(wrong) = self.fs.quiet(|| self.engine.check_spellings()) {
      fail!("spelling", "{wrong}");
    }
    if let Err(left) = self.engine.check_idle() {
      fail!("idle", "the engine is not done: {left}");
    }
  }

  fn play(&mut self, ops: &[Op]) {
    for (index, op) in ops.iter().enumerate() {
      self.shared.reached.store(index + 1, Ordering::Relaxed);
      self.run(op);
    }
    self.settle_and_check();
  }
}

// ---- running, cutting down, reporting ----

struct Failure {
  /// The rule that broke: the tag of `fail!`, or `panic` for anything else.
  tag: String,
  message: String,
  trace: Vec<String>,
  /// How many operations were started when it broke. The rest of the run plays no part.
  reached: usize,
}

thread_local! {
  static QUIET_PANICS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A failing run panics, and cutting it down panics hundreds of times. Those panics are caught
/// and reported once, so the hook stays silent for this thread while a run is on.
fn silence_caught_panics() {
  static HOOK: Once = Once::new();
  HOOK.call_once(|| {
    let default = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
      if !QUIET_PANICS.get() {
        default(info);
      }
    }));
  });
}

fn play(config: &Config, ops: &[Op]) -> Result<(), Failure> {
  silence_caught_panics();
  let shared = Arc::new(Shared {
    filter: config.filter,
    trace: Mutex::new(Vec::new()),
    watched: Mutex::new(BTreeSet::new()),
    serial: AtomicU64::new(0),
    moved_unseen: AtomicBool::new(false),
    watch_moves: config.backend == Target::Sim(Mode::Prefix),
    folds: config.folds(),
    reached: AtomicUsize::new(0),
  });
  QUIET_PANICS.set(true);
  let result = panic::catch_unwind(AssertUnwindSafe(|| {
    let fs = SimFs::new();
    match config.backend {
      Target::Sim(mode) => {
        let backend = SimBackend::attach(&fs, mode);
        Run::new(config, fs, backend, Arc::clone(&shared)).play(ops);
      }
      Target::Poll => {
        Run::new(config, fs, PollBackend, Arc::clone(&shared)).play(ops);
      }
    }
  }));
  QUIET_PANICS.set(false);
  result.map_err(|panic| {
    let message = panic
      .downcast_ref::<String>()
      .cloned()
      .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
      .unwrap_or_else(|| "a panic without a message".to_owned());
    let tag = match message.strip_prefix('[').and_then(|rest| rest.split_once(']')) {
      Some((tag, _)) => tag.to_owned(),
      None => "panic".to_owned(),
    };
    let trace = lock(&shared.trace).clone();
    Failure { tag, message, trace, reached: shared.reached.load(Ordering::Relaxed) }
  })
}

/// Takes operations out of a failing run for as long as it still breaks the same rule.
fn cut_down(config: &Config, mut ops: Vec<Op>, tag: &str) -> Vec<Op> {
  let still_fails = |ops: &[Op]| play(config, ops).is_err_and(|failure| failure.tag == tag);
  // Big pieces first, then single operations, each time from the end to the start.
  let mut piece = ops.len().div_ceil(2);
  loop {
    let before = ops.len();
    let mut end = ops.len();
    while end > 0 {
      let start = end.saturating_sub(piece);
      let mut shorter = ops.clone();
      shorter.drain(start..end);
      if still_fails(&shorter) {
        ops = shorter;
      }
      end = start;
    }
    if piece > 1 {
      piece = piece.div_ceil(2);
    } else if ops.len() == before {
      return ops;
    }
  }
}

fn report(seed: u64, config: &Config, mut ops: Vec<Op>, failure: &Failure) -> String {
  ops.truncate(failure.reached);
  let short = cut_down(config, ops, &failure.tag);
  let again = play(config, &short);
  let mut text = format!(
    "seed {seed} on {:?} failed: {}\n{config:?}\n\nThe run cut down to {} operations:\n",
    config.backend,
    failure.message,
    short.len()
  );
  for op in &short {
    let _ = writeln!(text, "  {op:?}");
  }
  match again {
    Err(again) => {
      let _ = writeln!(text, "\nWhat happened in it:\n  {}", again.trace.join("\n  "));
      let _ = writeln!(text, "\n{}", again.message);
    }
    Ok(()) => {
      let _ = writeln!(text, "\nThe whole run:\n  {}", failure.trace.join("\n  "));
    }
  }
  text
}

fn number(name: &str) -> Option<u64> {
  std::env::var(name).ok().and_then(|value| value.parse().ok())
}

/// Runs `seeds` seeds from `first` on. The first failure is cut down and reported.
fn fuzz(backend: Target, first: u64, seeds: u64, steps: usize) {
  for seed in first..first + seeds {
    let config = Config::new(seed, backend, steps);
    let ops = random_ops(seed, &config);
    if let Err(failure) = play(&config, &ops) {
      panic!("{}", report(seed, &config, ops, &failure));
    }
  }
}

const BACKENDS: [Target; 4] =
  [Target::Sim(Mode::Inode), Target::Sim(Mode::Prefix), Target::Sim(Mode::Handle), Target::Poll];

/// The quick run of every `cargo test`.
mod quick {
  use super::*;

  const SEEDS: u64 = 250;
  const STEPS: usize = 160;

  #[test]
  fn inode() {
    fuzz(Target::Sim(Mode::Inode), 1, SEEDS, STEPS);
  }

  #[test]
  fn prefix() {
    fuzz(Target::Sim(Mode::Prefix), 1, SEEDS, STEPS);
  }

  #[test]
  fn handle() {
    fuzz(Target::Sim(Mode::Handle), 1, SEEDS, STEPS);
  }

  #[test]
  fn poll() {
    fuzz(Target::Poll, 1, SEEDS, STEPS);
  }
}

/// The big runs. `PATHWATCH_FUZZ_FIRST` and `PATHWATCH_FUZZ_SEEDS` choose the seeds.
mod big {
  use super::*;

  fn seeds() -> (u64, u64) {
    (number("PATHWATCH_FUZZ_FIRST").unwrap_or(1000), number("PATHWATCH_FUZZ_SEEDS").unwrap_or(3000))
  }

  fn many(backend: Target) {
    let (first, seeds) = seeds();
    fuzz(backend, first, seeds, 160);
  }

  #[test]
  #[ignore = "a long run"]
  fn inode() {
    many(Target::Sim(Mode::Inode));
  }

  #[test]
  #[ignore = "a long run"]
  fn prefix() {
    many(Target::Sim(Mode::Prefix));
  }

  #[test]
  #[ignore = "a long run"]
  fn handle() {
    many(Target::Sim(Mode::Handle));
  }

  #[test]
  #[ignore = "a long run"]
  fn poll() {
    many(Target::Poll);
  }

  /// Few seeds, many steps: the tree grows old, and every rare thing happens many times.
  #[test]
  #[ignore = "a long run"]
  fn long() {
    let (first, seeds) = seeds();
    for backend in BACKENDS {
      fuzz(backend, first, (seeds / 500).max(2), 5000);
    }
  }
}

/// `PATHWATCH_FUZZ_SEED=<seed>`: one seed on every backend, with the cut-down story of each
/// failure. `PATHWATCH_FUZZ_STEPS` sets the length of the run: 160 as in the quick and the big
/// runs, and a failure of `long` needs 5000.
#[test]
#[ignore = "a tool to study one seed"]
fn replay() {
  let seed = number("PATHWATCH_FUZZ_SEED").expect("set PATHWATCH_FUZZ_SEED");
  let steps = number("PATHWATCH_FUZZ_STEPS").map_or(160, |steps| usize::try_from(steps).unwrap());
  let mut failures = Vec::new();
  for backend in BACKENDS {
    let config = Config::new(seed, backend, steps);
    let ops = random_ops(seed, &config);
    if let Err(failure) = play(&config, &ops) {
      failures.push(report(seed, &config, ops, &failure));
    }
  }
  assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
