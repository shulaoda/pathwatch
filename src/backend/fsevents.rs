use std::{
  collections::BTreeMap,
  ffi::{OsStr, OsString},
  fs::{self, File, OpenOptions},
  io, mem,
  ops::{Bound, Range},
  os::unix::{
    ffi::OsStrExt,
    fs::{MetadataExt, OpenOptionsExt},
  },
  path::{Path, PathBuf},
  slice,
  sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
  },
};

use rustc_hash::{FxHashMap, FxHashSet};

use self::ffi::{Notices, Queue, Stream};
use crate::{
  backend::{
    Backend, Committed, Coverage, Hint, NodeKey, Sink, Want, copy_error, give_room_back,
    may_watch_a_tree, watch_limit,
  },
  fs::{FileType, is_missing, name_and_kind},
};

pub const MAX_STREAMS: usize = 64;
pub const MAX_NEW_STREAMS: usize = 8;
pub const MAX_STREAM_PATHS: usize = 64;
pub const MAX_PATHS_PER_STREAM: usize = 4096;
const EXPECTED_PATH_BYTES: usize = 128;
const DATA_VOLUME: &str = "/System/Volumes/Data";

const ERROR_PATH_NOT_UTF8: &str = "FSEvents cannot watch a path that is not UTF-8";
const ERROR_QUEUE_CREATE: &str = "dispatch_queue_create failed";
const ERROR_STREAM_CREATE: &str = "FSEventStreamCreate failed";
const ERROR_STREAM_START: &str = "FSEvents did not start a stream: the process may run its 512 \
                                  streams already, or FSEvents is not available here";
const ERROR_SOURCE_CREATE: &str = "dispatch_source_create failed";
const ERROR_NO_GUARD: &str = "no open file is left for the directories above a watched path \
                              (raise the limit of open files, ulimit -n)";

pub mod flag {
  pub const MUST_SCAN_SUB_DIRS: u32 = 0x1;
  pub const USER_DROPPED: u32 = 0x2;
  pub const KERNEL_DROPPED: u32 = 0x4;
  pub const MOUNT: u32 = 0x40;
  pub const UNMOUNT: u32 = 0x80;
  pub const CREATED: u32 = 0x100;
  pub const REMOVED: u32 = 0x200;
  pub const RENAMED: u32 = 0x800;
  pub const IS_DIR: u32 = 0x2_0000;

  pub const RECORDS_LOST: u32 = MUST_SCAN_SUB_DIRS | USER_DROPPED | KERNEL_DROPPED;
  pub const DIR_REPLACED: u32 = RENAMED | MOUNT | UNMOUNT;
  pub const ENTRY_CAME_OR_WENT: u32 = CREATED | REMOVED | DIR_REPLACED;
}

#[derive(Debug)]
pub struct FsEventsBackend {
  covers: Covers,
  sink: Arc<Sink<Raw>>,
  queue: Queue,
  running: Vec<Running>,
  paths: Dirs<()>,
  pending: Dirs<()>,
  path_limit: usize,
  guard: Guard,
  guard_is_stale: bool,
  named: FxHashMap<NodeKey, PathBuf>,
  last_parent: Option<(PathBuf, PathBuf)>,
}

#[derive(Debug, Default)]
pub struct Raw {
  paths: Vec<u8>,
  records: Vec<(usize, u32)>,
  pub lost: Vec<NodeKey>,
}

impl Raw {
  fn lost(keys: Vec<NodeKey>) -> Self {
    Self { lost: keys, ..Self::default() }
  }

  pub fn push(&mut self, path: &[u8], flags: u32) {
    self.paths.extend_from_slice(path);
    self.records.push((self.paths.len(), flags));
  }

  pub fn records(&self) -> impl Iterator<Item = (&Path, u32)> {
    let mut start = 0;
    self.records.iter().map(move |&(end, flags)| {
      let path = &self.paths[start..end];
      start = end;
      (Path::new(OsStr::from_bytes(path)), flags)
    })
  }
}

impl FsEventsBackend {
  pub fn new(sink: Sink<Raw>) -> io::Result<Self> {
    let sink = Arc::new(sink);
    let queue = Queue::new()?;
    let notices = Notices::new(&queue, &sink)?;
    Ok(Self {
      covers: Covers::default(),
      sink,
      queue,
      running: Vec::new(),
      paths: Dirs::default(),
      pending: Dirs::default(),
      path_limit: MAX_STREAM_PATHS,
      guard: Guard { notices, dirs: FxHashMap::default(), last_id: ffi::VOLUMES },
      guard_is_stale: false,
      named: FxHashMap::default(),
      last_parent: None,
    })
  }
}

impl Backend for FsEventsBackend {
  type Raw = Raw;

  fn watch(&mut self, key: NodeKey, dir: &Path, want: Want) -> Coverage {
    let real = match self.real_path(dir) {
      Ok(real) => real,
      Err(error) => {
        self.unwatch(key);
        return Coverage::Failed(error);
      }
    };
    if let Some(old) = self.covers.dir_of.get(&key)
      && *old != real
    {
      if self.paths.holds(old) {
        self.sink.raw(Raw::lost(vec![key]));
      }
      self.unwatch(key);
    }
    let has_stream = self.paths.holds(&real) || self.pending.holds(&real);
    if !has_stream {
      if want == Want::Names {
        if self.named.get(&key) != Some(&real) {
          self.unwatch(key);
          self.named.insert(key, real);
          self.guard_is_stale = true;
        }
        return Coverage::Watched;
      }
      if !may_watch_a_tree(real.strip_prefix(DATA_VOLUME).unwrap_or(&real)) {
        self.unwatch(key);
        return Coverage::NotWatched;
      }
      self.pending.insert(real.clone(), ());
    }
    if self.named.remove(&key).is_some() {
      self.guard_is_stale = true;
    }
    self.covers.insert(key, real);
    Coverage::WatchedTree
  }

  fn unwatch(&mut self, key: NodeKey) {
    if self.named.remove(&key).is_some() {
      give_room_back(&mut self.named);
      self.guard_is_stale = true;
    }
    if let Some(dir) = self.covers.remove(key)
      && self.pending.contains(&dir)
    {
      self.pending.remove(&dir);
      for (below, _) in self.covers.by_dir.below_one(&dir) {
        if !self.paths.holds(below) {
          self.pending.insert(below.to_owned(), ());
        }
      }
    }
  }

  fn commit(&mut self) -> Committed {
    let mut lost = self.take_moved();
    self.stop_idle();
    self.last_parent = None;
    let mut committed = Committed::default();
    if self.pending.is_empty() && !self.guard_is_stale {
      return committed;
    }
    let Started { fresh, unguarded, failed } = self.start_pending();
    for (key, error) in self.keys_not_live(&unguarded, failed.as_ref()) {
      self.named.remove(&key);
      self.covers.remove(key);
      if is_missing(error) {
        lost.push(key);
      } else {
        committed.failed.push((key, copy_error(error)));
      }
    }
    lost.extend(self.moved_since_asked(&fresh));
    if !lost.is_empty() {
      self.sink.raw(Raw::lost(lost));
    }
    self.fit_guard();
    committed
  }

  fn translate(&mut self, raw: Raw, hint: &mut dyn FnMut(Hint<'_>)) {
    for key in raw.lost.iter().copied().chain(self.take_moved()) {
      hint(Hint::Changed { dir: key, rel: Path::new("") });
    }
    let covers = &self.covers.by_dir;
    let mut lost_places = Vec::new();
    let mut replaced_dirs = Vec::new();
    for (path, flags) in raw.records() {
      if flags & flag::RECORDS_LOST != 0 {
        let names_a_place = flags & flag::MUST_SCAN_SUB_DIRS != 0 && path != Path::new("/");
        if names_a_place {
          lost_places.push(path);
        } else {
          hint(Hint::Lost);
        }
        continue;
      }
      let is_dir = flags & (flag::IS_DIR | flag::MOUNT | flag::UNMOUNT) != 0;
      if is_dir && flags & flag::ENTRY_CAME_OR_WENT == 0 {
        continue;
      }
      if is_dir && flags & flag::DIR_REPLACED != 0 {
        replaced_dirs.push(path);
      }
      for (keys, rel) in covers.holders(path) {
        for &key in keys {
          hint(Hint::Changed { dir: key, rel });
        }
      }
    }
    let holding = lost_places.iter().flat_map(|place| covers.holders(place)).map(|(keys, _)| keys);
    let below =
      covers.below(&lost_places).chain(covers.below(&replaced_dirs)).map(|(_, keys)| keys);
    for &key in holding.chain(below).flatten() {
      hint(Hint::Changed { dir: key, rel: Path::new("") });
    }
  }

  fn raw_bytes(raw: &Self::Raw) -> usize {
    raw.paths.capacity()
      + raw.records.capacity() * size_of::<(usize, u32)>()
      + raw.lost.capacity() * size_of::<NodeKey>()
  }
}

impl Drop for FsEventsBackend {
  fn drop(&mut self) {
    self.guard.notices.cancel();
    let streams = mem::take(&mut self.running);
    self.queue.sync(move || drop(streams));
  }
}

impl FsEventsBackend {
  fn real_path(&mut self, dir: &Path) -> io::Result<PathBuf> {
    let real = match (dir.parent(), dir.file_name()) {
      (Some(parent), Some(name)) => {
        let last = match self.last_parent.take() {
          Some((last, real)) if last == parent => (last, real),
          _ => (parent.to_owned(), short_path(fs::canonicalize(parent)?)),
        };
        let (_, real_parent) = self.last_parent.insert(last);
        match name_and_kind(dir)? {
          (on_disk, kind) if kind != FileType::Symlink && on_disk.eq_ignore_ascii_case(name) => {
            real_parent.join(on_disk)
          }
          _ => fs::canonicalize(dir)?,
        }
      }
      _ => fs::canonicalize(dir)?,
    };
    let real = short_path(real);
    if real.to_str().is_none() {
      return Err(io::Error::new(io::ErrorKind::InvalidData, ERROR_PATH_NOT_UTF8));
    }
    Ok(real)
  }

  fn take_moved(&mut self) -> Vec<NodeKey> {
    let moved = self.guard.take_fired();
    if moved.is_empty() {
      return Vec::new();
    }
    self.guard_is_stale = true;
    self.distrust(&moved);
    let is_below = |dir: &Path| moved.iter().any(|moved| dir.starts_with(moved));
    let named = self.named.iter().filter(|(_, dir)| is_below(dir)).map(|(&key, _)| key);
    named.chain(self.covers.keys_at_or_below(&moved)).collect()
  }

  fn stop_idle(&mut self) {
    let needed =
      |path: &PathBuf| self.paths.contains(path) && self.covers.keys_under(path).next().is_some();
    let idle: Vec<Running> =
      self.running.extract_if(.., |running| !running.paths.iter().any(needed)).collect();
    if idle.is_empty() {
      return;
    }
    for path in idle.iter().flat_map(|running| &running.paths) {
      self.paths.remove(path);
    }
    self.guard_is_stale = true;
    self.queue.run(move || drop(idle));
  }

  fn start_pending(&mut self) -> Started {
    let new = top_most(self.pending.drain().collect());
    let replace = !new.is_empty()
      && (self.running.len() >= MAX_STREAMS
        || self.paths.len() + new.len() > self.path_limit
        || self.paths.below(&new).next().is_some());
    let (mut paths, keep) = if replace {
      let covered = self.covers.by_dir.keys().map(Path::to_owned).collect();
      let placed = place(covered, MAX_STREAM_PATHS - MAX_NEW_STREAMS);
      let keep = self.streams_to_keep(&placed);
      (placed, keep)
    } else {
      (new, Vec::new())
    };
    let needed = paths.len();
    let kept_streams = self.running.iter().zip(&keep).filter(|(_, keep)| **keep);
    let already_streamed: FxHashSet<&Path> =
      kept_streams.flat_map(|(running, _)| &running.paths).map(PathBuf::as_path).collect();
    paths.retain(|path| !already_streamed.contains(path.as_path()));
    let unguarded = self
      .guard
      .open(paths.iter().map(PathBuf::as_path), self.named.values().map(PathBuf::as_path));
    paths.retain(|path| !unguarded.holds(path));
    if paths.is_empty() {
      return Started { fresh: Vec::new(), unguarded, failed: None };
    }
    let streams = match self.start_streams(&paths) {
      Ok(streams) => streams,
      Err(error) => return Started { fresh: Vec::new(), unguarded, failed: Some(error) },
    };
    let fresh = paths.iter().filter(|path| !self.paths.contains(path)).cloned().collect();
    if replace {
      self.stop_replaced(keep);
      self.path_limit = if needed > MAX_STREAM_PATHS { 2 * needed } else { MAX_STREAM_PATHS };
    }
    self.running.extend(streams);
    for path in paths {
      self.paths.insert(path, ());
    }
    Started { fresh, unguarded, failed: None }
  }

  fn start_streams(&self, paths: &[PathBuf]) -> io::Result<Vec<Running>> {
    let count = MAX_NEW_STREAMS.min(paths.len()).max(paths.len().div_ceil(MAX_PATHS_PER_STREAM));
    let start = |first: usize| -> io::Result<Running> {
      let every_nth = || paths.iter().skip(first).step_by(count);
      let stream = Stream::start(&self.queue, &self.sink, every_nth())?;
      Ok(Running { _stream: stream, paths: every_nth().cloned().collect() })
    };
    (0..count).map(start).collect()
  }

  fn stop_replaced(&mut self, keep: Vec<bool>) {
    let mut keep = keep.into_iter();
    let replaced: Vec<Running> =
      self.running.extract_if(.., |_| keep.next() == Some(false)).collect();
    if !replaced.is_empty() {
      self.stop(replaced);
    }
    self.paths = Dirs::default();
    for path in self.running.iter().flat_map(|running| &running.paths) {
      self.paths.insert(path.clone(), ());
    }
  }

  fn streams_to_keep(&self, placed: &[PathBuf]) -> Vec<bool> {
    let mut placed: FxHashSet<&Path> = placed.iter().map(PathBuf::as_path).collect();
    let mut by_covers = Vec::new();
    for (index, running) in self.running.iter().enumerate().rev() {
      let stays = |path: &PathBuf| placed.contains(path.as_path()) && self.paths.contains(path);
      if running.paths.iter().all(stays) {
        for path in &running.paths {
          placed.remove(path.as_path());
        }
        let covers_at = |path: &PathBuf| self.covers.by_dir.below_one(path).count() + 1;
        by_covers.push((running.paths.iter().map(covers_at).sum::<usize>(), index));
      }
    }
    let room = if self.running.len() >= MAX_STREAMS { MAX_STREAMS / 2 } else { MAX_STREAMS };
    by_covers.sort_unstable_by(|a, b| b.cmp(a));
    by_covers.truncate(room - MAX_NEW_STREAMS);
    let mut keep = vec![false; self.running.len()];
    for (_, index) in by_covers {
      keep[index] = true;
    }
    keep
  }

  fn keys_not_live<'e>(
    &self,
    unguarded: &'e Dirs<io::Error>,
    failed: Option<&'e io::Error>,
  ) -> Vec<(NodeKey, &'e io::Error)> {
    if unguarded.is_empty() && failed.is_none() {
      return Vec::new();
    }
    let why = |dir: &Path| unguarded.holders(dir).next().map(|(error, _)| error);
    let named = self.named.iter().map(|(&key, dir)| (key, why(dir)));
    let covered = self.covers.dir_of.iter().filter(|(_, dir)| !self.paths.holds(dir));
    let covered = covered.map(|(&key, dir)| (key, why(dir).or(failed)));
    named.chain(covered).filter_map(|(key, why)| Some((key, why?))).collect()
  }

  fn moved_since_asked(&mut self, started: &[PathBuf]) -> Vec<NodeKey> {
    let mut moved = Vec::new();
    for path in started {
      let is_still_real = fs::canonicalize(path).is_ok_and(|real| real == *path);
      if !is_still_real {
        self.distrust(slice::from_ref(path));
        moved.extend(self.covers.keys_under(path));
        continue;
      }
      let mut checked = path.as_path();
      for (cover, keys) in self.covers.by_dir.below_one(path) {
        if cover.ancestors().take_while(|dir| !checked.starts_with(dir)).all(is_spelled) {
          checked = cover;
        } else {
          moved.extend(keys);
        }
      }
    }
    moved
  }

  fn fit_guard(&mut self) {
    let wanted = wanted(self.paths.keys(), self.named.values().map(PathBuf::as_path));
    self.guard.dirs.retain(|dir, _| wanted.binary_search(&dir.as_path()).is_ok());
    give_room_back(&mut self.guard.dirs);
    self.guard_is_stale = false;
  }

  fn distrust(&mut self, dirs: &[PathBuf]) {
    let below: Vec<PathBuf> = self.paths.below(dirs).map(|(path, ())| path.to_owned()).collect();
    for path in dirs.iter().chain(&below) {
      self.paths.remove(path);
    }
  }

  fn stop(&self, stopped: Vec<Running>) {
    let paths = top_most(stopped.iter().flat_map(|old| &old.paths).cloned().collect());
    let lost = self.covers.keys_at_or_below(&paths).collect();
    let sink = Arc::clone(&self.sink);
    self.queue.run(move || {
      drop(stopped);
      sink.raw(Raw::lost(lost));
    });
  }
}

fn short_path(real: PathBuf) -> PathBuf {
  let Ok(below) = real.strip_prefix(DATA_VOLUME) else { return real };
  let short = Path::new("/").join(below);
  let id = |path: &Path| fs::metadata(path).map(|found| (found.dev(), found.ino())).ok();
  let is_same = fs::canonicalize(&short).is_ok_and(|own| own == short)
    && id(&short).is_some_and(|short| Some(short) == id(&real));
  if is_same { short } else { real }
}

fn is_spelled(dir: &Path) -> bool {
  name_and_kind(dir).is_ok_and(|(name, kind)| {
    kind != FileType::Symlink && Some(name.as_os_str()) == dir.file_name()
  })
}

struct Started {
  fresh: Vec<PathBuf>,
  unguarded: Dirs<io::Error>,
  failed: Option<io::Error>,
}

#[derive(Debug)]
struct Running {
  _stream: Stream,
  paths: Vec<PathBuf>,
}

pub fn is_too_high(dir: &Path) -> bool {
  let dir = dir.strip_prefix(DATA_VOLUME).unwrap_or(dir);
  let mut names = dir.iter().filter(|&name| name != "/").map(OsStrExt::as_bytes);
  match [names.next(), names.next(), names.next()] {
    [_, _, None] => true,
    [Some(b"private"), Some(b"var"), Some(b"folders")] => names.count() <= 3,
    _ => false,
  }
}

fn place(covers: Vec<PathBuf>, max: usize) -> Vec<PathBuf> {
  let paths = top_most(covers);
  let too_many = paths.len().saturating_sub(max);
  if too_many == 0 {
    return paths;
  }
  let joins: Vec<usize> = paths
    .windows(2)
    .map(|pair| {
      let shared = pair[0].components().zip(pair[1].components()).take_while(|(a, b)| a == b);
      let depth = shared.count();
      let dir: PathBuf = pair[0].components().take(depth).collect();
      if is_too_high(&dir) { 0 } else { depth }
    })
    .collect();
  let mut deepest_first: Vec<usize> = joins.iter().copied().filter(|&depth| depth > 0).collect();
  deepest_first.sort_unstable_by(|a, b| b.cmp(a));
  let level = deepest_first.get(too_many - 1).copied().unwrap_or(1);
  let mut remaining =
    paths.len() - deepest_first.iter().take_while(|&&depth| depth > level).count();
  let mut placed = Vec::new();
  let mut common_depth = usize::MAX;
  let mut merges_at_level = None;
  for (path, join) in paths.into_iter().zip(joins.into_iter().chain([0])) {
    if join < level {
      merges_at_level = None;
    }
    if join > level || join == level && *merges_at_level.get_or_insert(remaining > max) {
      remaining -= usize::from(join == level);
      common_depth = common_depth.min(join);
    } else {
      placed.push(path.components().take(common_depth).collect());
      common_depth = usize::MAX;
    }
  }
  placed
}

fn top_most(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
  paths.sort_unstable();
  paths.dedup_by(|path, kept| path.starts_with(kept));
  paths
}

#[derive(Debug)]
struct Guard {
  notices: Notices,
  dirs: FxHashMap<PathBuf, Guarded>,
  last_id: usize,
}

impl Guard {
  fn open<'a>(
    &mut self,
    paths: impl Iterator<Item = &'a Path>,
    named: impl Iterator<Item = &'a Path>,
  ) -> Dirs<io::Error> {
    let mut unguarded = Dirs::default();
    for dir in wanted(paths, named) {
      if self.dirs.contains_key(dir) || unguarded.holds(dir) {
        continue;
      }
      match self.open_one(dir) {
        Ok(guarded) => {
          self.dirs.insert(dir.to_owned(), guarded);
        }
        Err(error) => unguarded.insert(dir.to_owned(), error),
      }
    }
    unguarded
  }

  fn open_one(&mut self, dir: &Path) -> io::Result<Guarded> {
    let ticket = Ticket::take()?;
    let mut options = OpenOptions::new();
    let file = options.read(true).custom_flags(libc::O_EVTONLY | libc::O_SEARCH).open(dir);
    let file = file.map_err(|error| match error.raw_os_error() {
      Some(libc::EMFILE | libc::ENFILE) => watch_limit(ERROR_NO_GUARD),
      _ => error,
    })?;
    self.last_id += 1;
    self.notices.watch(&file, self.last_id)?;
    Ok(Guarded { file, _ticket: ticket, id: self.last_id })
  }

  fn take_fired(&mut self) -> Vec<PathBuf> {
    let fired = self.notices.take();
    if fired.is_empty() {
      return Vec::new();
    }
    let volumes = fired.contains(&ffi::VOLUMES);
    let id = |found: fs::Metadata| (found.dev(), found.ino());
    let has_moved = |(dir, guarded): &(&PathBuf, &Guarded)| {
      fired.contains(&guarded.id)
        || (volumes && guarded.file.metadata().ok().map(id) != fs::metadata(dir).ok().map(id))
    };
    let moved = top_most(self.dirs.iter().filter(has_moved).map(|(dir, _)| dir.clone()).collect());
    self.dirs.retain(|dir, _| !moved.iter().any(|moved| dir.starts_with(moved)));
    moved
  }
}

#[derive(Debug)]
struct Guarded {
  file: File,
  _ticket: Ticket,
  id: usize,
}

fn wanted<'a>(
  paths: impl Iterator<Item = &'a Path>,
  named: impl Iterator<Item = &'a Path>,
) -> Vec<&'a Path> {
  let mut wanted: Vec<&Path> = paths.flat_map(Path::ancestors).chain(named).collect();
  wanted.sort_unstable();
  wanted.dedup();
  wanted
}

static GUARD_FILES: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
struct Ticket;

impl Ticket {
  fn take() -> io::Result<Self> {
    if GUARD_FILES.fetch_add(1, Ordering::Relaxed) < ffi::open_file_limit() / 2 {
      return Ok(Self);
    }
    GUARD_FILES.fetch_sub(1, Ordering::Relaxed);
    Err(watch_limit(ERROR_NO_GUARD))
  }
}

impl Drop for Ticket {
  fn drop(&mut self) {
    GUARD_FILES.fetch_sub(1, Ordering::Relaxed);
  }
}

#[derive(Debug, Default)]
struct Covers {
  by_dir: Dirs<Vec<NodeKey>>,
  dir_of: FxHashMap<NodeKey, PathBuf>,
}

impl Covers {
  fn insert(&mut self, key: NodeKey, dir: PathBuf) {
    if self.dir_of.get(&key) == Some(&dir) {
      return;
    }
    match self.by_dir.get_mut(&dir) {
      Some(keys) => keys.push(key),
      None => self.by_dir.insert(dir.clone(), vec![key]),
    }
    self.dir_of.insert(key, dir);
  }

  fn remove(&mut self, key: NodeKey) -> Option<PathBuf> {
    let dir = self.dir_of.remove(&key)?;
    give_room_back(&mut self.dir_of);
    let keys = self.by_dir.get_mut(&dir)?;
    keys.retain(|known| *known != key);
    if !keys.is_empty() {
      return None;
    }
    self.by_dir.remove(&dir);
    Some(dir)
  }

  fn keys_under<'a>(&'a self, dir: &'a Path) -> impl Iterator<Item = NodeKey> + 'a {
    let at = self.by_dir.get(dir).into_iter();
    at.chain(self.by_dir.below_one(dir).map(|(_, keys)| keys)).flatten().copied()
  }

  fn keys_at_or_below<'a>(&'a self, dirs: &'a [PathBuf]) -> impl Iterator<Item = NodeKey> + 'a {
    dirs.iter().flat_map(move |dir| self.keys_under(dir))
  }
}

#[derive(Debug)]
struct Dirs<T> {
  by_path: BTreeMap<OsString, T>,
  path_lengths: Range<usize>,
}

impl<T> Default for Dirs<T> {
  fn default() -> Self {
    Self { by_path: BTreeMap::new(), path_lengths: Range { start: usize::MAX, end: 0 } }
  }
}

impl<T> Dirs<T> {
  fn len(&self) -> usize {
    self.by_path.len()
  }

  fn is_empty(&self) -> bool {
    self.by_path.is_empty()
  }

  fn insert(&mut self, dir: PathBuf, value: T) {
    let len = dir.as_os_str().len();
    self.path_lengths = self.path_lengths.start.min(len)..self.path_lengths.end.max(len + 1);
    self.by_path.insert(dir.into_os_string(), value);
  }

  fn contains(&self, dir: &Path) -> bool {
    self.by_path.contains_key(dir.as_os_str())
  }

  fn get(&self, dir: &Path) -> Option<&T> {
    self.by_path.get(dir.as_os_str())
  }

  fn get_mut(&mut self, dir: &Path) -> Option<&mut T> {
    self.by_path.get_mut(dir.as_os_str())
  }

  fn remove(&mut self, dir: &Path) {
    self.by_path.remove(dir.as_os_str());
    if self.by_path.is_empty() {
      self.path_lengths = Range { start: usize::MAX, end: 0 };
    }
  }

  fn drain(&mut self) -> impl Iterator<Item = PathBuf> {
    mem::take(self).by_path.into_keys().map(PathBuf::from)
  }

  fn keys(&self) -> impl Iterator<Item = &Path> {
    self.by_path.keys().map(Path::new)
  }

  #[cfg(feature = "test-hooks")]
  fn paths(&self) -> Vec<&Path> {
    let mut paths: Vec<&Path> = self.keys().collect();
    paths.sort_unstable();
    paths
  }

  fn holds(&self, path: &Path) -> bool {
    self.holders(path).next().is_some()
  }

  fn holders<'a, 'p>(&'a self, path: &'p Path) -> impl Iterator<Item = (&'a T, &'p Path)> {
    let path = path.components().as_path();
    let bytes = path.as_os_str().as_bytes();
    path
      .ancestors()
      .skip_while(|dir| dir.as_os_str().len() >= self.path_lengths.end)
      .take_while(|dir| dir.as_os_str().len() >= self.path_lengths.start)
      .filter_map(move |dir| {
        let value = self.by_path.get(dir.as_os_str())?;
        let rest = &bytes[dir.as_os_str().len()..];
        let rest = rest.strip_prefix(b"/").unwrap_or(rest);
        Some((value, Path::new(OsStr::from_bytes(rest))))
      })
  }

  fn below_one<'a>(&'a self, path: &'a Path) -> impl Iterator<Item = (&'a Path, &'a T)> + 'a {
    let mut prefix = path.components().as_path().as_os_str().to_owned();
    if !prefix.as_bytes().ends_with(b"/") {
      prefix.push("/");
    }
    let after = (Bound::Excluded(prefix.as_os_str()), Bound::Unbounded);
    let dirs = self.by_path.range::<OsStr, _>(after);
    let dirs = dirs.take_while(move |(dir, _)| dir.as_bytes().starts_with(prefix.as_bytes()));
    dirs.map(|(dir, value)| (Path::new(dir), value))
  }

  fn below<'a>(
    &'a self,
    paths: &'a [impl AsRef<Path>],
  ) -> impl Iterator<Item = (&'a Path, &'a T)> + 'a {
    paths.iter().flat_map(move |path| self.below_one(path.as_ref()))
  }
}

#[cfg(feature = "test-hooks")]
impl FsEventsBackend {
  pub fn stream_count(&self) -> usize {
    self.running.len()
  }

  pub fn covers(&self) -> Vec<&Path> {
    self.covers.by_dir.paths()
  }

  pub fn cover_of_key(&self) -> &FxHashMap<NodeKey, PathBuf> {
    &self.covers.dir_of
  }

  pub fn guarded(&self) -> Vec<&Path> {
    self.guard.dirs.keys().map(PathBuf::as_path).collect()
  }

  pub fn streamed(&self) -> Vec<&Path> {
    self.paths.paths()
  }

  pub fn pending(&self) -> Vec<&Path> {
    self.pending.paths()
  }
}

mod ffi {
  use std::{
    ffi::{CStr, c_char, c_void},
    fs::File,
    io, mem,
    os::{
      fd::{AsRawFd, FromRawFd, OwnedFd},
      unix::ffi::OsStrExt,
    },
    path::PathBuf,
    ptr::{self, NonNull},
    sync::{Arc, Mutex, PoisonError},
  };

  use libc::{EV_ADD, EV_CLEAR, EVFILT_FS, EVFILT_VNODE, NOTE_DELETE, NOTE_RENAME, NOTE_REVOKE};

  use super::{
    ERROR_QUEUE_CREATE, ERROR_SOURCE_CREATE, ERROR_STREAM_CREATE, ERROR_STREAM_START,
    EXPECTED_PATH_BYTES, Raw,
  };
  use crate::backend::{Sink, watch_limit};

  pub(super) const VOLUMES: usize = 0;

  const CREATE_NO_DEFER: u32 = 0x2;
  const CREATE_FILE_EVENTS: u32 = 0x10;
  const SINCE_NOW: u64 = u64::MAX;
  const ENCODING_UTF8: u32 = 0x0800_0100;
  const VQ_MOUNT: u32 = 0x8;
  const VQ_UNMOUNT: u32 = 0x10;
  const NO_EVENT: libc::kevent =
    libc::kevent { ident: 0, filter: 0, flags: 0, fflags: 0, data: 0, udata: ptr::null_mut() };

  type CFTypeRef = *const c_void;
  type FSEventStreamRef = *mut c_void;
  type DispatchQueue = *mut c_void;

  type FSEventStreamCallback = extern "C" fn(
    stream: FSEventStreamRef,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    ids: *const u64,
  );

  #[repr(C)]
  struct FSEventStreamContext {
    version: isize,
    info: *mut c_void,
    retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<extern "C" fn(*const c_void)>,
    copy_description: Option<extern "C" fn(*const c_void) -> CFTypeRef>,
  }

  pub(super) fn open_file_limit() -> usize {
    // SAFETY: the call takes no memory of ours.
    usize::try_from(unsafe { getdtablesize() }).unwrap_or(0)
  }

  #[derive(Debug)]
  pub(super) struct Notices {
    kqueue: i32,
    source: NonNull<c_void>,
    fired: Arc<Mutex<Vec<usize>>>,
  }

  struct Reader {
    kqueue: OwnedFd,
    sink: Arc<Sink<Raw>>,
    fired: Arc<Mutex<Vec<usize>>>,
  }

  impl Notices {
    pub(super) fn new(queue: &Queue, sink: &Arc<Sink<Raw>>) -> io::Result<Self> {
      // SAFETY: the call takes no memory of ours.
      let fd = unsafe { libc::kqueue() };
      let ident = usize::try_from(fd).map_err(|_| io::Error::last_os_error())?;
      // SAFETY: `fd` was just opened, and nothing else owns it.
      let queue_fd = unsafe { OwnedFd::from_raw_fd(fd) };
      let fired = Arc::default();
      let reader =
        Box::new(Reader { kqueue: queue_fd, sink: Arc::clone(sink), fired: Arc::clone(&fired) });
      // SAFETY: the type is the one of libdispatch, and the descriptor and the queue are open.
      let source =
        unsafe { dispatch_source_create(&raw const SOURCE_TYPE_READ, ident, 0, queue.0.as_ptr()) };
      let source = NonNull::new(source).ok_or_else(|| io::Error::other(ERROR_SOURCE_CREATE))?;
      // SAFETY: the source is new and not resumed yet. It owns `reader` from here on: its cancel
      // handler frees it, once no call of its event handler runs or will run.
      unsafe {
        dispatch_set_context(source.as_ptr(), Box::into_raw(reader).cast());
        dispatch_source_set_event_handler_f(source.as_ptr(), read);
        dispatch_source_set_cancel_handler_f(source.as_ptr(), free);
        dispatch_resume(source.as_ptr());
      }
      let notices = Self { kqueue: fd, source, fired };
      notices.add(0, EVFILT_FS, VQ_MOUNT | VQ_UNMOUNT, VOLUMES)?;
      Ok(notices)
    }

    pub(super) fn take(&self) -> Vec<usize> {
      mem::take(&mut self.fired.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub(super) fn cancel(&self) {
      // SAFETY: the source lives until `drop` releases it, and a second cancel does nothing.
      unsafe { dispatch_source_cancel(self.source.as_ptr()) };
    }

    pub(super) fn watch(&self, file: &File, id: usize) -> io::Result<()> {
      let fd = usize::try_from(file.as_raw_fd()).map_err(io::Error::other)?;
      self.add(fd, EVFILT_VNODE, NOTE_DELETE | NOTE_RENAME | NOTE_REVOKE, id)
    }

    fn add(&self, ident: usize, filter: i16, fflags: u32, id: usize) -> io::Result<()> {
      let udata = ptr::without_provenance_mut(id);
      let change = libc::kevent { ident, filter, flags: EV_ADD | EV_CLEAR, fflags, data: 0, udata };
      // SAFETY: `change` is one whole `kevent`; the call reads it, writes none, and does not wait.
      let done =
        unsafe { libc::kevent(self.kqueue, &raw const change, 1, ptr::null_mut(), 0, ptr::null()) };
      if done < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }
  }

  impl Drop for Notices {
    fn drop(&mut self) {
      // SAFETY: the source came from `dispatch_source_create`, and this is its one release. Its
      // cancel handler closes the kernel queue after the last call of `read`.
      unsafe {
        dispatch_source_cancel(self.source.as_ptr());
        dispatch_release(self.source.as_ptr());
      }
    }
  }

  extern "C" fn read(context: *mut c_void) {
    // SAFETY: the context is the `Reader` of the source, which only the cancel handler frees,
    // and never while this runs.
    let reader = unsafe { &*context.cast::<Reader>() };
    let mut events = [NO_EVENT; 64];
    let room = i32::try_from(events.len()).unwrap_or(0);
    let now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    let kqueue = reader.kqueue.as_raw_fd();
    // SAFETY: `events` holds `room` `kevent`, which the call may write; it does not wait.
    let count =
      unsafe { libc::kevent(kqueue, ptr::null(), 0, events.as_mut_ptr(), room, &raw const now) };
    let Ok(count) = usize::try_from(count) else { return };
    if count > 0 {
      let mut fired = reader.fired.lock().unwrap_or_else(PoisonError::into_inner);
      for id in events[..count].iter().map(|event| event.udata.addr()) {
        if !fired.contains(&id) {
          fired.push(id);
        }
      }
      drop(fired);
      reader.sink.raw(Raw::default());
    }
  }

  extern "C" fn free(context: *mut c_void) {
    // SAFETY: the context came from `Box::into_raw` in `Notices::new`, and the cancel handler
    // runs once.
    drop(unsafe { Box::from_raw(context.cast::<Reader>()) });
  }

  #[derive(Debug)]
  pub(super) struct Queue(NonNull<c_void>);

  impl Queue {
    pub(super) fn new() -> io::Result<Self> {
      // SAFETY: the label ends with a NUL, and a null attribute asks for a serial queue.
      let queue = unsafe { dispatch_queue_create(c"pathwatch.fsevents".as_ptr(), ptr::null()) };
      NonNull::new(queue).map(Self).ok_or_else(|| io::Error::other(ERROR_QUEUE_CREATE))
    }

    pub(super) fn sync<F: FnOnce() + Send>(&self, work: F) {
      // SAFETY: the queue is open, and `call` takes the box back once, before this returns.
      unsafe { dispatch_sync_f(self.0.as_ptr(), Box::into_raw(Box::new(work)).cast(), call::<F>) };
    }

    pub(super) fn run<F: FnOnce() + Send + 'static>(&self, work: F) {
      let work = Box::into_raw(Box::new(work));
      // SAFETY: the queue is open, and libdispatch keeps it until `call` has run.
      unsafe { dispatch_async_f(self.0.as_ptr(), work.cast(), call::<F>) };
    }
  }

  impl Drop for Queue {
    fn drop(&mut self) {
      // SAFETY: the queue came from `dispatch_queue_create`, and this is its one release.
      unsafe { dispatch_release(self.0.as_ptr()) };
    }
  }

  extern "C" fn call<F: FnOnce()>(context: *mut c_void) {
    // SAFETY: `context` came from `Box::into_raw` of an `F` in `sync` or `run`, and the queue
    // calls this once for it.
    let work = unsafe { Box::from_raw(context.cast::<F>()) };
    work();
  }

  #[derive(Debug)]
  pub(super) struct Stream(NonNull<c_void>);

  // SAFETY: the functions of a stream may be called from any thread: FSEvents keeps the stream,
  // and its `info`, until a callback that runs has returned.
  unsafe impl Send for Stream {}

  impl Stream {
    pub(super) fn start<'a>(
      queue: &Queue,
      sink: &Arc<Sink<Raw>>,
      paths: impl Iterator<Item = &'a PathBuf>,
    ) -> io::Result<Self> {
      let array = path_array(paths).ok_or_else(|| io::Error::other(ERROR_STREAM_CREATE))?;
      let context = FSEventStreamContext {
        version: 0,
        info: Arc::as_ptr(sink).cast_mut().cast(),
        retain: Some(retain),
        release: Some(release),
        copy_description: None,
      };
      // SAFETY: `context` and `array` are whole values that the call copies, and `info` is an
      // `Arc<Sink<Raw>>` pointer that `retain` and `release` count.
      let stream = unsafe {
        FSEventStreamCreate(
          ptr::null(),
          callback,
          &raw const context,
          array.0.as_ptr(),
          SINCE_NOW,
          0.0,
          CREATE_FILE_EVENTS | CREATE_NO_DEFER,
        )
      };
      let stream = NonNull::new(stream).ok_or_else(|| io::Error::other(ERROR_STREAM_CREATE))?;
      // SAFETY: the stream was just created, and it is put on an open queue before it starts.
      let started = unsafe {
        FSEventStreamSetDispatchQueue(stream.as_ptr(), queue.0.as_ptr());
        FSEventStreamStart(stream.as_ptr()) != 0
      };
      if started {
        return Ok(Self(stream));
      }
      // SAFETY: the stream did not start. It is taken off its queue and released once, here.
      unsafe {
        FSEventStreamInvalidate(stream.as_ptr());
        FSEventStreamRelease(stream.as_ptr());
      }
      Err(watch_limit(ERROR_STREAM_START))
    }
  }

  impl Drop for Stream {
    fn drop(&mut self) {
      // SAFETY: the stream runs on a queue, and this is its one stop, invalidate and release.
      unsafe {
        FSEventStreamStop(self.0.as_ptr());
        FSEventStreamInvalidate(self.0.as_ptr());
        FSEventStreamRelease(self.0.as_ptr());
      }
    }
  }

  extern "C" fn callback(
    _stream: FSEventStreamRef,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    _ids: *const u64,
  ) {
    // SAFETY: `info` is the `Arc<Sink<Raw>>` pointer of the stream, alive while the stream is.
    let sink = unsafe { &*info.cast::<Sink<Raw>>() };
    let paths = paths.cast::<*const c_char>();
    let mut raw = Raw {
      paths: Vec::with_capacity(count * EXPECTED_PATH_BYTES),
      records: Vec::with_capacity(count),
      ..Raw::default()
    };
    for index in 0..count {
      // SAFETY: FSEvents gives `count` C strings and `count` flags, and `index` is below `count`.
      let (path, flags) = unsafe { (CStr::from_ptr(*paths.add(index)), *flags.add(index)) };
      raw.push(path.to_bytes(), flags);
    }
    sink.raw(raw);
  }

  extern "C" fn retain(info: *const c_void) -> *const c_void {
    // SAFETY: `info` came from `Arc::as_ptr` on the `Arc<Sink<Raw>>` that `FsEventsBackend` holds.
    unsafe { Arc::increment_strong_count(info.cast::<Sink<Raw>>()) };
    info
  }

  extern "C" fn release(info: *const c_void) {
    // SAFETY: FSEvents calls this once for each call of `retain`, and gives back its count.
    unsafe { Arc::decrement_strong_count(info.cast::<Sink<Raw>>()) };
  }

  struct Cf(NonNull<c_void>);

  impl Drop for Cf {
    fn drop(&mut self) {
      // SAFETY: the object came from a `Create` call, and this is its one release.
      unsafe { CFRelease(self.0.as_ptr()) };
    }
  }

  fn path_array<'a>(paths: impl Iterator<Item = &'a PathBuf>) -> Option<Cf> {
    let strings = paths
      .map(|path| {
        let bytes = path.as_os_str().as_bytes();
        // SAFETY: the call reads `bytes.len()` bytes of `bytes` and keeps a copy.
        let string = unsafe {
          CFStringCreateWithBytes(
            ptr::null(),
            bytes.as_ptr(),
            bytes.len().cast_signed(),
            ENCODING_UTF8,
            0,
          )
        };
        NonNull::new(string.cast_mut()).map(Cf)
      })
      .collect::<Option<Vec<Cf>>>()?;
    let values: Vec<CFTypeRef> =
      strings.iter().map(|string| string.0.as_ptr().cast_const()).collect();
    // SAFETY: `values` holds `values.len()` object pointers, and the array retains each of them.
    let array = unsafe {
      CFArrayCreate(
        ptr::null(),
        values.as_ptr(),
        values.len().cast_signed(),
        &raw const TYPE_ARRAY_CALLBACKS,
      )
    };
    NonNull::new(array.cast_mut()).map(Cf)
  }

  #[link(name = "CoreServices", kind = "framework")]
  unsafe extern "C" {
    fn FSEventStreamCreate(
      allocator: CFTypeRef,
      callback: FSEventStreamCallback,
      context: *const FSEventStreamContext,
      paths: CFTypeRef,
      since: u64,
      latency: f64,
      flags: u32,
    ) -> FSEventStreamRef;
    fn FSEventStreamSetDispatchQueue(stream: FSEventStreamRef, queue: DispatchQueue);
    fn FSEventStreamStart(stream: FSEventStreamRef) -> u8;
    fn FSEventStreamStop(stream: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream: FSEventStreamRef);
    fn FSEventStreamRelease(stream: FSEventStreamRef);
  }

  #[link(name = "CoreFoundation", kind = "framework")]
  unsafe extern "C" {
    #[link_name = "kCFTypeArrayCallBacks"]
    static TYPE_ARRAY_CALLBACKS: c_void;
    fn CFStringCreateWithBytes(
      allocator: CFTypeRef,
      bytes: *const u8,
      len: isize,
      encoding: u32,
      is_external: u8,
    ) -> CFTypeRef;
    fn CFArrayCreate(
      allocator: CFTypeRef,
      values: *const CFTypeRef,
      count: isize,
      callbacks: *const c_void,
    ) -> CFTypeRef;
    fn CFRelease(object: CFTypeRef);
  }

  unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> DispatchQueue;
    fn dispatch_release(object: *mut c_void);
    fn dispatch_sync_f(
      queue: DispatchQueue,
      context: *mut c_void,
      work: extern "C" fn(*mut c_void),
    );
    fn dispatch_async_f(
      queue: DispatchQueue,
      context: *mut c_void,
      work: extern "C" fn(*mut c_void),
    );
    #[link_name = "_dispatch_source_type_read"]
    static SOURCE_TYPE_READ: c_void;
    fn dispatch_source_create(
      kind: *const c_void,
      handle: usize,
      mask: usize,
      queue: DispatchQueue,
    ) -> *mut c_void;
    fn dispatch_set_context(object: *mut c_void, context: *mut c_void);
    fn dispatch_source_set_event_handler_f(
      source: *mut c_void,
      handler: extern "C" fn(*mut c_void),
    );
    fn dispatch_source_set_cancel_handler_f(
      source: *mut c_void,
      handler: extern "C" fn(*mut c_void),
    );
    fn dispatch_resume(object: *mut c_void);
    fn dispatch_source_cancel(source: *mut c_void);
    fn getdtablesize() -> i32;
  }
}

#[cfg(test)]
mod tests {
  use std::path::{Path, PathBuf};

  use super::{DATA_VOLUME, Dirs, is_too_high, place, short_path};

  fn paths(paths: &[&str]) -> Vec<PathBuf> {
    paths.iter().map(PathBuf::from).collect()
  }

  #[test]
  fn more_covers_than_stream_paths_share_the_lowest_common_directories() {
    let covers =
      ["/Users/me/p/a/1", "/Users/me/p/a/2", "/Users/me/p/b/1", "/Users/me/p/b/2", "/Users/me/q"];
    let placed = |max| place(paths(&covers), max);
    assert_eq!(placed(5), paths(&covers));
    assert_eq!(
      placed(4),
      paths(&["/Users/me/p/a", "/Users/me/p/b/1", "/Users/me/p/b/2", "/Users/me/q"])
    );
    assert_eq!(placed(3), paths(&["/Users/me/p/a", "/Users/me/p/b", "/Users/me/q"]));
    assert_eq!(placed(2), paths(&["/Users/me/p", "/Users/me/q"]));
    assert_eq!(placed(1), paths(&["/Users/me/p", "/Users/me/q"]));
  }

  #[test]
  fn what_is_too_high_for_a_stream_path() {
    let too_high = [
      "/",
      "/Users",
      "/Users/me",
      "/Volumes",
      "/Volumes/disk",
      "/private",
      "/private/tmp",
      "/private/var",
      "/opt/homebrew",
      "/private/var/folders",
      "/private/var/folders/zz",
      "/private/var/folders/zz/abc",
      "/private/var/folders/zz/abc/T",
      "/System/Volumes/Data",
      "/System/Volumes/Data/Users/me",
      "/System/Volumes/Data/private/var/folders/zz/abc/T",
    ];
    for dir in too_high {
      assert!(is_too_high(Path::new(dir)), "{dir}");
    }
    let fine = [
      "/Users/me/project",
      "/Users/me/project/packages/a/src",
      "/Volumes/disk/project",
      "/private/tmp/project",
      "/private/var/folders/zz/abc/T/project",
      "/private/var/folders/zz/abc/T/project/src",
      "/private/var/other/project",
      "/System/Volumes/Data/Users/me/project",
      "/System/Volumes/Data/private/var/folders/zz/abc/T/project",
      "/System/Library/Frameworks",
    ];
    for dir in fine {
      assert!(!is_too_high(Path::new(dir)), "{dir}");
    }
  }

  #[test]
  fn the_short_path_of_a_directory_whose_parent_has_none() {
    let long = Path::new(DATA_VOLUME).join("Users");
    if long.is_dir() {
      assert_eq!(short_path(long), Path::new("/Users"));
    }
  }

  fn all_paths(depth: usize) -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from("/")];
    let mut level = paths.clone();
    for _ in 0..depth {
      let longer = |path: &PathBuf| ["a", "a.b", "ab", "b"].map(|name| path.join(name));
      level = level.iter().flat_map(longer).collect();
      paths.extend(level.iter().cloned());
    }
    paths
  }

  #[test]
  fn the_table_finds_what_a_look_at_every_cover_finds() {
    let mut paths = all_paths(4);
    paths.sort();
    for step in [1, 2, 3, 5, 7, 11, 23, 47, 101, 211, 400] {
      let picked = |index: &usize| ((index * 2_654_435_761) % 1013).is_multiple_of(step);
      let picked: Vec<&PathBuf> = (0..paths.len()).filter(picked).map(|at| &paths[at]).collect();
      let mut covers = Dirs::default();
      for path in &picked {
        covers.insert((*path).clone(), path.as_path());
      }
      for path in &paths {
        let found: Vec<(&Path, &Path)> =
          covers.holders(path).map(|(cover, rel)| (*cover, rel)).collect();
        let expected: Vec<(&Path, &Path)> = path
          .ancestors()
          .filter(|dir| picked.iter().any(|cover| cover.as_path() == *dir))
          .map(|dir| (dir, path.strip_prefix(dir).unwrap()))
          .collect();
        assert_eq!(found, expected, "the covers that hold {}, step {step}", path.display());
        assert_eq!(covers.holds(path), !expected.is_empty());

        let mut found: Vec<&Path> = covers.below(&[path]).map(|(_, dir)| *dir).collect();
        found.sort_unstable();
        let lies_below = |cover: &&&PathBuf| **cover != path && cover.starts_with(path);
        let expected: Vec<&Path> =
          picked.iter().filter(lies_below).map(|cover| cover.as_path()).collect();
        assert_eq!(found, expected, "the covers below {}, step {step}", path.display());
      }
    }
  }

  #[test]
  fn a_path_with_a_slash_at_its_end_is_held_like_the_path_without() {
    let mut covers = Dirs::default();
    covers.insert(PathBuf::from("/x/a"), 1);
    covers.insert(PathBuf::from("/x/a/b"), 2);
    let holders = |path| covers.holders(Path::new(path)).collect::<Vec<_>>();
    assert_eq!(holders("/x/a/"), [(&1, Path::new(""))]);
    assert_eq!(holders("/x/a/b/"), [(&2, Path::new("")), (&1, Path::new("b"))]);
    assert_eq!(holders("/x/a/b/c"), [(&2, Path::new("c")), (&1, Path::new("b/c"))]);
    assert_eq!(holders("/x/ab"), []);
    assert_eq!(holders("/x"), []);
  }
}
