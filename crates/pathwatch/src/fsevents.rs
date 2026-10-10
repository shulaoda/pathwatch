use std::{
  collections::{BTreeMap, VecDeque},
  ffi::{OsStr, OsString},
  fs, io, mem,
  ops::{Bound, Range},
  os::unix::{ffi::OsStrExt, fs::MetadataExt},
  path::{Component, Path, PathBuf},
  slice,
  sync::Arc,
  time::{Duration, Instant},
};

use rustc_hash::{FxHashMap, FxHashSet};

use self::ffi::{Guard, Queue, Stream};
use crate::backend::{Backend, Coverage, DirKey, Hint, Sink, Want};

pub const MAX_STREAMS: usize = 64;
pub const MAX_NEW_STREAMS: usize = 8;
pub const MAX_STREAM_PATHS: usize = 64;
pub const MAX_PATHS_PER_STREAM: usize = 4096;
const MAX_RETIRING_STREAMS: usize = MAX_STREAMS + MAX_NEW_STREAMS;
const RETIRING_TIMEOUT: Duration = Duration::from_millis(500);
const EXPECTED_PATH_BYTES: usize = 128;
const DATA_VOLUME: &str = "/System/Volumes/Data";

const ERROR_PATH_NOT_UTF8: &str = "FSEvents cannot watch a path that is not UTF-8";
const ERROR_QUEUE_CREATE: &str = "dispatch_queue_create failed";
const ERROR_STREAM_CREATE: &str = "FSEventStreamCreate failed";
const ERROR_STREAM_START: &str =
  "FSEvents cannot start one more stream (one process can run 512 streams)";
const ERROR_NAME_ON_DISK: &str = "getattrlist gave no name";
const ERROR_NO_GUARD: &str = "no open file is left for the directories above a watched path \
                              (raise the limit of open files, ulimit -n)";

pub mod flag {
  pub const MUST_SCAN_SUB_DIRS: u32 = 0x1;
  pub const USER_DROPPED: u32 = 0x2;
  pub const KERNEL_DROPPED: u32 = 0x4;
  pub const HISTORY_DONE: u32 = 0x10;
  pub const CREATED: u32 = 0x100;
  pub const REMOVED: u32 = 0x200;
  pub const RENAMED: u32 = 0x800;
  pub const IS_DIR: u32 = 0x2_0000;

  pub const RECORDS_LOST: u32 = MUST_SCAN_SUB_DIRS | USER_DROPPED | KERNEL_DROPPED;
  pub const ENTRY_CAME_OR_WENT: u32 = CREATED | REMOVED | RENAMED;
}

#[derive(Debug)]
pub struct FsEventsBackend {
  covers: Covers,
  streams: Streams,
  not_watched: FxHashMap<DirKey, PathBuf>,
  last_parent: Option<RealParent>,
}

impl FsEventsBackend {
  pub fn new(sink: Sink<Raw>) -> io::Result<Self> {
    let sink = Arc::new(sink);
    Ok(Self {
      covers: Covers::default(),
      streams: Streams {
        guard: Guard::new(&sink)?,
        sink,
        queue: Queue::new()?,
        running: Vec::new(),
        paths: Dirs::default(),
        pending: Dirs::default(),
        path_limit: MAX_STREAM_PATHS,
        retiring: VecDeque::new(),
      },
      not_watched: FxHashMap::default(),
      last_parent: None,
    })
  }

  pub fn guarded(&self) -> Vec<&Path> {
    self.streams.guard.dirs()
  }

  pub fn stream_count(&self) -> usize {
    self.streams.running.len()
  }

  pub fn retiring_count(&self) -> usize {
    self.streams.retiring.iter().map(|old| old.streams.len()).sum()
  }

  pub fn covers(&self) -> Vec<&Path> {
    self.covers.by_dir.paths()
  }

  pub fn cover_of_key(&self) -> &FxHashMap<DirKey, PathBuf> {
    &self.covers.dir_of
  }

  pub fn streamed(&self) -> Vec<&Path> {
    self.streams.paths.paths()
  }

  pub fn pending(&self) -> Vec<&Path> {
    self.streams.pending.paths()
  }

  fn real_path(&mut self, dir: &Path, want: Want) -> io::Result<PathBuf> {
    let real = match (want, dir.parent(), dir.file_name()) {
      (Want::Contents, Some(parent), Some(name)) => {
        let last = match self.last_parent.take() {
          Some(last) if last.asked == parent => last,
          _ => RealParent { asked: parent.to_owned(), real: short_way(fs::canonicalize(parent)?) },
        };
        let real_parent = &self.last_parent.insert(last).real;
        match ffi::name_on_disk(dir)? {
          Some(on_disk) if on_disk.eq_ignore_ascii_case(name) => real_parent.join(on_disk),
          _ => fs::canonicalize(dir)?,
        }
      }
      _ => fs::canonicalize(dir)?,
    };
    let real = short_way(real);
    if real.to_str().is_none() {
      return Err(io::Error::new(io::ErrorKind::InvalidData, ERROR_PATH_NOT_UTF8));
    }
    Ok(real)
  }
}

impl Backend for FsEventsBackend {
  type Raw = Raw;

  fn watch(&mut self, key: DirKey, dir: &Path, want: Want) -> Coverage {
    let real = match self.real_path(dir, want) {
      Ok(real) => real,
      Err(error) => {
        self.unwatch(key);
        return Coverage::Failed(error);
      }
    };
    let has_stream = self.streams.paths.holds(&real) || self.streams.pending.holds(&real);
    if !has_stream {
      if want == Want::Names {
        self.unwatch(key);
        self.not_watched.insert(key, real);
        return Coverage::NotWatched;
      }
      self.streams.pending.insert(real.clone(), ());
    }
    let old = self.covers.dir_of.get(&key);
    if old.is_some_and(|old| *old != real && self.streams.paths.holds(old)) {
      self.streams.sink.raw(Raw { lost: vec![key], ..Raw::default() });
    }
    self.not_watched.remove(&key);
    self.covers.insert(key, real);
    Coverage::WatchedTree
  }

  fn unwatch(&mut self, key: DirKey) {
    self.not_watched.remove(&key);
    self.covers.remove(key);
  }

  fn commit(&mut self) -> io::Result<Vec<DirKey>> {
    self.streams.stop_retired();
    self.last_parent = None;
    if self.streams.pending.is_empty() {
      return Ok(Vec::new());
    }
    let started = self.streams.start_pending(&self.covers);
    let streamed = &self.streams.paths;
    let started = match started {
      Ok(started) => started,
      Err(error) => {
        self.covers.retain(|dir| streamed.holds(dir));
        return Err(error);
      }
    };
    let moved = self.streams.moved_since_asked(&started, &self.covers);
    if !moved.records.is_empty() || !moved.lost.is_empty() {
      self.streams.sink.raw(moved);
    }
    let streamed = &self.streams.paths;
    let now_streamed = self.not_watched.iter().filter(|(_, dir)| streamed.holds(dir));
    Ok(now_streamed.map(|(&key, _)| key).collect())
  }

  fn translate(&mut self, raw: Raw, hint: &mut dyn FnMut(Hint<'_>)) {
    self.streams.stop_retired();
    let unguarded: Vec<PathBuf> =
      raw.moved.iter().filter_map(|&dir| self.streams.guard.dir_of(dir)).collect();
    self.streams.distrust(&unguarded);
    for &key in &raw.lost {
      hint(Hint::Changed { dir: key, rel: Path::new("") });
    }
    let covers = &self.covers.by_dir;
    let mut lost_places: Vec<&Path> = unguarded.iter().map(PathBuf::as_path).collect();
    let mut moved_dirs = Vec::new();
    for (path, flags) in raw.records() {
      if flags & flag::HISTORY_DONE != 0 {
        continue;
      }
      if flags & flag::RECORDS_LOST != 0 {
        let names_a_place = flags & flag::MUST_SCAN_SUB_DIRS != 0 && path != Path::new("/");
        if names_a_place {
          lost_places.push(path);
        } else {
          hint(Hint::Lost);
        }
        continue;
      }
      let is_dir = flags & flag::IS_DIR != 0;
      if is_dir && flags & flag::ENTRY_CAME_OR_WENT == 0 {
        continue;
      }
      if is_dir && flags & flag::RENAMED != 0 {
        moved_dirs.push(path);
      }
      for (keys, rel) in covers.holders(path) {
        for &key in keys {
          hint(Hint::Changed { dir: key, rel });
        }
      }
    }
    let holders = lost_places.iter().flat_map(|path| covers.holders(path)).map(|(keys, _)| keys);
    moved_dirs.extend_from_slice(&lost_places);
    let below = covers.below(&moved_dirs);
    for keys in holders.chain(below.into_iter().map(|(_, keys)| keys)) {
      for &key in keys {
        hint(Hint::Changed { dir: key, rel: Path::new("") });
      }
    }
  }

  fn raw_bytes(raw: &Self::Raw) -> usize {
    let records = size_of_val(raw.records.as_slice());
    raw.paths.len() + records + size_of_val(raw.moved.as_slice()) + size_of_val(raw.lost.as_slice())
  }
}

#[derive(Debug)]
struct RealParent {
  asked: PathBuf,
  real: PathBuf,
}

#[derive(Debug, Default)]
pub struct Raw {
  paths: Vec<u8>,
  records: Vec<(usize, u32)>,
  moved: Vec<usize>,
  lost: Vec<DirKey>,
}

impl Raw {
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

#[derive(Debug, Default)]
struct Covers {
  by_dir: Dirs<Vec<DirKey>>,
  dir_of: FxHashMap<DirKey, PathBuf>,
}

impl Covers {
  fn insert(&mut self, key: DirKey, dir: PathBuf) {
    if self.dir_of.get(&key) == Some(&dir) {
      return;
    }
    self.remove(key);
    match self.by_dir.get_mut(&dir) {
      Some(keys) => keys.push(key),
      None => self.by_dir.insert(dir.clone(), vec![key]),
    }
    self.dir_of.insert(key, dir);
  }

  fn remove(&mut self, key: DirKey) {
    let Some(dir) = self.dir_of.remove(&key) else { return };
    let Some(keys) = self.by_dir.get_mut(&dir) else { return };
    keys.retain(|known| *known != key);
    if keys.is_empty() {
      self.by_dir.remove(&dir);
    }
  }

  fn retain(&mut self, keep: impl Fn(&Path) -> bool) {
    let Self { by_dir, dir_of } = self;
    by_dir.retain(|dir, keys| {
      let kept = keep(dir);
      if !kept {
        for key in keys.iter() {
          dir_of.remove(key);
        }
      }
      kept
    });
  }
}

#[derive(Debug)]
struct Streams {
  sink: Arc<Sink<Raw>>,
  queue: Queue,
  running: Vec<Stream>,
  paths: Dirs<()>,
  pending: Dirs<()>,
  path_limit: usize,
  retiring: VecDeque<Retiring>,
  guard: Guard,
}

#[derive(Debug)]
struct Retiring {
  streams: Vec<Stream>,
  deadline: Instant,
}

impl Streams {
  fn start_pending(&mut self, covers: &Covers) -> io::Result<Vec<PathBuf>> {
    let new = top_most(self.pending.drain().collect());
    let replace = self.running.len() >= MAX_STREAMS
      || self.paths.len() + new.len() > self.path_limit
      || !self.paths.below(&new).is_empty();
    let paths = if replace {
      place(covers.dir_of.values().cloned().collect(), MAX_STREAM_PATHS)
    } else {
      new
    };
    let stream_count =
      MAX_NEW_STREAMS.min(paths.len()).max(paths.len().div_ceil(MAX_PATHS_PER_STREAM));
    let fresh: Vec<PathBuf> =
      paths.iter().filter(|path| !self.paths.contains(path)).cloned().collect();
    let mut asked = 0;
    let no_guard = paths.iter().find_map(|path| {
      asked += 1;
      self.guard.hold(path).err().filter(|error| !is_gone(error))
    });
    let start = |first| {
      let every_nth = paths.iter().skip(first).step_by(stream_count);
      Stream::start(&self.queue, &self.sink, every_nth)
    };
    let streams = no_guard
      .map_or_else(|| (0..stream_count).map(start).collect::<io::Result<Vec<Stream>>>(), Err);
    let streams = match streams {
      Ok(streams) => streams,
      Err(error) => {
        for path in &paths[..asked] {
          self.guard.release(path);
        }
        return Err(error);
      }
    };
    if replace {
      let replaced = mem::take(&mut self.running);
      if !replaced.is_empty() {
        let deadline = Instant::now() + RETIRING_TIMEOUT;
        self.retiring.push_back(Retiring { streams: replaced, deadline });
      }
      self.stop_retired();
      for path in mem::take(&mut self.paths).drain() {
        self.guard.release(&path);
      }
      self.path_limit =
        if paths.len() > MAX_STREAM_PATHS { 2 * paths.len() } else { MAX_STREAM_PATHS };
    }
    self.running.extend(streams);
    for path in paths {
      self.paths.insert(path, ());
    }
    Ok(fresh)
  }

  fn distrust(&mut self, dirs: &[PathBuf]) {
    if dirs.is_empty() {
      return;
    }
    let dirs: FxHashSet<&Path> = dirs.iter().map(PathBuf::as_path).collect();
    let below = |path: &&Path| path.ancestors().any(|above| dirs.contains(above));
    let moved: Vec<PathBuf> = self.paths.keys().filter(below).map(Path::to_owned).collect();
    for path in moved {
      self.paths.remove(&path);
      self.guard.release(&path);
    }
  }

  fn moved_since_asked(&mut self, started: &[PathBuf], covers: &Covers) -> Raw {
    let mut moved = Raw::default();
    for path in started {
      let is_there = fs::canonicalize(path).is_ok_and(|real| real == *path);
      if !is_there || !self.guard.holds(path) {
        self.distrust(slice::from_ref(path));
        moved.push(path.as_os_str().as_bytes(), flag::MUST_SCAN_SUB_DIRS);
        continue;
      }
      let mut spelled = path.as_path();
      for (cover, keys) in covers.by_dir.below(slice::from_ref(path)) {
        let is_spelled =
          |dir: &Path| ffi::name_on_disk(dir).is_ok_and(|name| name.as_deref() == dir.file_name());
        if cover.ancestors().take_while(|dir| !spelled.starts_with(dir)).all(is_spelled) {
          spelled = cover;
        } else {
          moved.lost.extend(keys);
        }
      }
    }
    moved
  }

  fn stop_retired(&mut self) {
    let now = Instant::now();
    let mut still_running: usize = self.retiring.iter().map(|old| old.streams.len()).sum();
    let mut stopped = Vec::new();
    while let Some(oldest) = self.retiring.front() {
      if now < oldest.deadline && still_running <= MAX_RETIRING_STREAMS {
        break;
      }
      still_running -= oldest.streams.len();
      stopped.extend(self.retiring.pop_front());
    }
    if !stopped.is_empty() {
      self.queue.sync(move || drop(stopped));
      let mut may_have_lost = Raw::default();
      may_have_lost.push(b"/", flag::MUST_SCAN_SUB_DIRS);
      self.sink.raw(may_have_lost);
    }
  }
}

impl Drop for Streams {
  fn drop(&mut self) {
    self.running.clear();
    self.retiring.clear();
    self.queue.sync(|| {});
  }
}

fn short_way(real: PathBuf) -> PathBuf {
  let Ok(below) = real.strip_prefix(DATA_VOLUME) else { return real };
  let short = Path::new("/").join(below);
  let id = |path: &Path| fs::metadata(path).map(|found| (found.dev(), found.ino())).ok();
  let is_same = fs::canonicalize(&short).is_ok_and(|own| own == short)
    && id(&short).is_some_and(|short| Some(short) == id(&real));
  if is_same { short } else { real }
}

fn is_gone(error: &io::Error) -> bool {
  matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
}

pub fn is_too_high(dir: &Path) -> bool {
  let dir = dir.strip_prefix(DATA_VOLUME).unwrap_or(dir);
  let names = dir.components().filter_map(|part| match part {
    Component::Normal(name) => Some(name.as_bytes()),
    _ => None,
  });
  let names: Vec<&[u8]> = names.collect();
  match names.as_slice() {
    [] | [_] | [_, _] => true,
    [b"private", b"var", b"folders", below @ ..] => below.len() <= 3,
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
  let mut left = paths.len() - deepest_first.iter().take_while(|&&depth| depth > level).count();
  let mut placed = Vec::new();
  let mut kept = usize::MAX;
  let mut takes_level = None;
  for (path, join) in paths.into_iter().zip(joins.into_iter().chain([0])) {
    if join < level {
      takes_level = None;
    }
    if join > level || join == level && *takes_level.get_or_insert(left > max) {
      left -= usize::from(join == level);
      kept = kept.min(join);
    } else {
      placed.push(path.components().take(kept).collect());
      kept = usize::MAX;
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

  fn get_mut(&mut self, dir: &Path) -> Option<&mut T> {
    self.by_path.get_mut(dir.as_os_str())
  }

  fn remove(&mut self, dir: &Path) {
    self.by_path.remove(dir.as_os_str());
    self.reset_lengths_if_empty();
  }

  fn retain(&mut self, mut keep: impl FnMut(&Path, &mut T) -> bool) {
    self.by_path.retain(|dir, value| keep(Path::new(dir), value));
    self.reset_lengths_if_empty();
  }

  fn drain(&mut self) -> impl Iterator<Item = PathBuf> {
    mem::take(self).by_path.into_keys().map(PathBuf::from)
  }

  fn reset_lengths_if_empty(&mut self) {
    if self.by_path.is_empty() {
      self.path_lengths = Range { start: usize::MAX, end: 0 };
    }
  }

  fn keys(&self) -> impl Iterator<Item = &Path> {
    self.by_path.keys().map(Path::new)
  }

  fn paths(&self) -> Vec<&Path> {
    let mut paths: Vec<&Path> = self.keys().collect();
    paths.sort_unstable();
    paths
  }

  fn holds(&self, path: &Path) -> bool {
    self.holders(path).next().is_some()
  }

  fn holders<'a>(&'a self, path: &'a Path) -> impl Iterator<Item = (&'a T, &'a Path)> {
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

  fn below(&self, paths: &[impl AsRef<Path>]) -> Vec<(&Path, &T)> {
    let mut below = Vec::new();
    for path in paths {
      let mut prefix = path.as_ref().components().as_path().as_os_str().to_owned();
      if !prefix.as_bytes().ends_with(b"/") {
        prefix.push("/");
      }
      let after = (Bound::Excluded(prefix.as_os_str()), Bound::Unbounded);
      let dirs = self.by_path.range::<OsStr, _>(after);
      let dirs = dirs.take_while(|(dir, _)| dir.as_bytes().starts_with(prefix.as_bytes()));
      below.extend(dirs.map(|(dir, value)| (Path::new(dir), value)));
    }
    below
  }
}

#[expect(clippy::undocumented_unsafe_blocks, reason = "this file has no comments")]
mod ffi {
  use std::{
    ffi::{CStr, CString, OsStr, OsString, c_char, c_void},
    fs::{File, OpenOptions},
    io, mem,
    os::{
      fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
      unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    },
    path::{Path, PathBuf},
    ptr::{self, NonNull},
    sync::{
      Arc,
      atomic::{AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
  };

  use rustc_hash::FxHashMap;

  use super::{
    ERROR_NAME_ON_DISK, ERROR_NO_GUARD, ERROR_PATH_NOT_UTF8, ERROR_QUEUE_CREATE,
    ERROR_STREAM_CREATE, ERROR_STREAM_START, EXPECTED_PATH_BYTES, Raw,
  };
  use crate::backend::{Sink, watch_limit};

  #[derive(Debug)]
  pub(super) struct Queue(NonNull<c_void>);

  unsafe impl Send for Queue {}

  impl Queue {
    pub(super) fn new() -> io::Result<Self> {
      let queue = unsafe { dispatch_queue_create(c"pathwatch.fsevents".as_ptr(), ptr::null()) };
      NonNull::new(queue).map(Self).ok_or_else(|| io::Error::other(ERROR_QUEUE_CREATE))
    }

    pub(super) fn sync<F: FnOnce() + Send>(&self, work: F) {
      extern "C" fn run<F: FnOnce() + Send>(context: *mut c_void) {
        let work = unsafe { &mut *context.cast::<Option<F>>() };
        if let Some(work) = work.take() {
          work();
        }
      }
      let mut work = Some(work);
      unsafe { dispatch_sync_f(self.0.as_ptr(), (&raw mut work).cast(), run::<F>) };
    }
  }

  impl Drop for Queue {
    fn drop(&mut self) {
      unsafe { dispatch_release(self.0.as_ptr()) };
    }
  }

  #[derive(Debug)]
  pub(super) struct Stream(NonNull<c_void>);

  unsafe impl Send for Stream {}

  impl Stream {
    pub(super) fn start<'a>(
      queue: &Queue,
      sink: &Arc<Sink<Raw>>,
      paths: impl Iterator<Item = &'a PathBuf>,
    ) -> io::Result<Self> {
      let array = path_array(paths)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, ERROR_PATH_NOT_UTF8))?;
      let context = FSEventStreamContext {
        version: 0,
        info: Arc::as_ptr(sink).cast_mut().cast(),
        retain: Some(retain),
        release: Some(release),
        copy_description: None,
      };
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
      let started = unsafe {
        FSEventStreamSetDispatchQueue(stream.as_ptr(), queue.0.as_ptr());
        FSEventStreamStart(stream.as_ptr()) != 0
      };
      if started {
        return Ok(Self(stream));
      }
      unsafe {
        FSEventStreamInvalidate(stream.as_ptr());
        FSEventStreamRelease(stream.as_ptr());
      }
      Err(watch_limit(ERROR_STREAM_START))
    }
  }

  impl Drop for Stream {
    fn drop(&mut self) {
      unsafe {
        FSEventStreamStop(self.0.as_ptr());
        FSEventStreamInvalidate(self.0.as_ptr());
        FSEventStreamRelease(self.0.as_ptr());
      }
    }
  }

  static GUARD_FDS: AtomicUsize = AtomicUsize::new(0);

  #[derive(Debug)]
  pub(super) struct Guard {
    queue: OwnedFd,
    dirs: FxHashMap<PathBuf, Guarded>,
    last_id: usize,
    waiter: Option<JoinHandle<()>>,
  }

  #[derive(Debug)]
  struct Guarded {
    _file: File,
    id: usize,
    holders: usize,
  }

  impl Guard {
    pub(super) fn new(sink: &Arc<Sink<Raw>>) -> io::Result<Self> {
      let queue = unsafe { kqueue() };
      if queue < 0 {
        return Err(io::Error::last_os_error());
      }
      let queue = unsafe { OwnedFd::from_raw_fd(queue) };
      change(&queue, 0, EVFILT_USER, EV_ADD | EV_CLEAR, 0, 0)?;
      let (fd, sink) = (queue.as_raw_fd(), Arc::clone(sink));
      let waiter = thread::Builder::new().name("pathwatch-guard".into());
      let waiter = waiter.spawn(move || wait_for_moves(fd, &sink))?;
      Ok(Self { queue, dirs: FxHashMap::default(), last_id: 0, waiter: Some(waiter) })
    }

    pub(super) fn dirs(&self) -> Vec<&Path> {
      let mut dirs: Vec<&Path> = self.dirs.keys().map(PathBuf::as_path).collect();
      dirs.sort_unstable();
      dirs
    }

    pub(super) fn hold(&mut self, path: &Path) -> io::Result<()> {
      let chain = || path.ancestors().filter(|dir| dir.parent().is_some());
      let missing: Vec<&Path> = chain().filter(|dir| !self.dirs.contains_key(*dir)).collect();
      let room = usize::try_from(unsafe { getdtablesize() }).unwrap_or(0) / 2;
      let first_id = self.last_id + 1;
      let open = |(index, dir): (usize, &&Path)| {
        let file = OpenOptions::new().read(true).custom_flags(O_EVTONLY | O_SEARCH).open(dir)?;
        let (fd, id) = (file.as_raw_fd(), first_id + index);
        let moves = NOTE_DELETE | NOTE_RENAME | NOTE_REVOKE;
        change(&self.queue, fd, EVFILT_VNODE, EV_ADD | EV_CLEAR, moves, id)?;
        Ok(Guarded { _file: file, id, holders: 0 })
      };
      let taken = GUARD_FDS.fetch_add(missing.len(), Ordering::Relaxed) + missing.len();
      let opened = if taken > room && !missing.is_empty() {
        Err(watch_limit(ERROR_NO_GUARD))
      } else {
        missing.iter().enumerate().map(open).collect::<io::Result<Vec<Guarded>>>()
      };
      let opened = opened.inspect_err(|_| {
        GUARD_FDS.fetch_sub(missing.len(), Ordering::Relaxed);
      })?;
      self.last_id += missing.len();
      for (dir, guarded) in missing.into_iter().zip(opened) {
        self.dirs.insert(dir.to_owned(), guarded);
      }
      for dir in chain() {
        if let Some(guarded) = self.dirs.get_mut(dir) {
          guarded.holders += 1;
        }
      }
      Ok(())
    }

    pub(super) fn release(&mut self, path: &Path) {
      if !self.dirs.contains_key(path) {
        return;
      }
      for dir in path.ancestors() {
        let Some(guarded) = self.dirs.get_mut(dir) else { continue };
        guarded.holders -= 1;
        if guarded.holders == 0 {
          self.dirs.remove(dir);
          GUARD_FDS.fetch_sub(1, Ordering::Relaxed);
        }
      }
    }

    pub(super) fn dir_of(&self, id: usize) -> Option<PathBuf> {
      self.dirs.iter().find(|(_, guarded)| guarded.id == id).map(|(dir, _)| dir.clone())
    }

    pub(super) fn holds(&self, path: &Path) -> bool {
      path.parent().is_none() || self.dirs.contains_key(path)
    }
  }

  impl Drop for Guard {
    fn drop(&mut self) {
      let asked = change(&self.queue, 0, EVFILT_USER, 0, NOTE_TRIGGER, 0);
      if let Some(waiter) = self.waiter.take()
        && asked.is_ok()
      {
        let _ = waiter.join();
      }
      GUARD_FDS.fetch_sub(self.dirs.len(), Ordering::Relaxed);
    }
  }

  fn change(
    queue: &OwnedFd,
    ident: RawFd,
    filter: i16,
    flags: u16,
    notes: u32,
    id: usize,
  ) -> io::Result<()> {
    let ident = usize::try_from(ident).map_err(io::Error::other)?;
    let udata = ptr::without_provenance_mut(id);
    let change = KEvent { ident, filter, flags, fflags: notes, data: 0, udata };
    let nothing = ptr::null_mut();
    let done = unsafe { kevent(queue.as_raw_fd(), &raw const change, 1, nothing, 0, ptr::null()) };
    if done < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
  }

  fn wait_for_moves(queue: RawFd, sink: &Sink<Raw>) {
    loop {
      let none =
        KEvent { ident: 0, filter: 0, flags: 0, fflags: 0, data: 0, udata: ptr::null_mut() };
      let mut events = [none; 32];
      let count = unsafe { kevent(queue, ptr::null(), 0, events.as_mut_ptr(), 32, ptr::null()) };
      let Ok(count) = usize::try_from(count) else {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
          continue;
        }
        return sink.failed(error);
      };
      let events = &events[..count];
      let dirs = events.iter().filter(|event| event.filter == EVFILT_VNODE);
      let moved: Vec<usize> = dirs.map(|event| event.udata.addr()).collect();
      if !moved.is_empty() {
        sink.raw(Raw { moved, ..Raw::default() });
      }
      if events.iter().any(|event| event.filter == EVFILT_USER) {
        return;
      }
    }
  }

  pub(super) fn name_on_disk(path: &Path) -> io::Result<Option<OsString>> {
    let c_path = CString::new(path.as_os_str().as_bytes())?;
    let wanted = AttrList {
      bitmap_count: ATTR_BIT_MAP_COUNT,
      reserved: 0,
      common: ATTR_CMN_NAME | ATTR_CMN_OBJTYPE,
      volume: 0,
      dir: 0,
      file: 0,
      fork: 0,
    };
    let mut found = NameAndType {
      length: 0,
      name: AttrReference { offset: 0, length: 0 },
      object_type: 0,
      name_bytes: [0; MAX_NAME_BYTES],
    };
    let result = unsafe {
      getattrlist(
        c_path.as_ptr(),
        (&raw const wanted).cast_mut().cast(),
        (&raw mut found).cast(),
        size_of::<NameAndType>(),
        FSOPT_NOFOLLOW,
      )
    };
    if result != 0 {
      return Err(io::Error::last_os_error());
    }
    if found.object_type == VLNK {
      return Ok(None);
    }
    let start = usize::try_from(found.name.offset).ok().and_then(|offset_from_reference| {
      (mem::offset_of!(NameAndType, name) + offset_from_reference)
        .checked_sub(mem::offset_of!(NameAndType, name_bytes))
    });
    let len_without_nul =
      usize::try_from(found.name.length).ok().and_then(|len| len.checked_sub(1));
    let name =
      start.zip(len_without_nul).and_then(|(start, len)| found.name_bytes.get(start..start + len));
    let name = name.ok_or_else(|| io::Error::other(ERROR_NAME_ON_DISK))?;
    Ok(Some(OsStr::from_bytes(name).to_owned()))
  }

  extern "C" fn callback(
    _stream: FSEventStreamRef,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    _ids: *const u64,
  ) {
    let sink = unsafe { &*info.cast::<Sink<Raw>>() };
    let paths = paths.cast::<*const c_char>();
    let mut raw = Raw {
      paths: Vec::with_capacity(count * EXPECTED_PATH_BYTES),
      records: Vec::with_capacity(count),
      moved: Vec::new(),
      lost: Vec::new(),
    };
    for index in 0..count {
      let (path, flags) = unsafe { (CStr::from_ptr(*paths.add(index)), *flags.add(index)) };
      raw.push(path.to_bytes(), flags);
    }
    sink.raw(raw);
  }

  extern "C" fn retain(info: *const c_void) -> *const c_void {
    unsafe { Arc::increment_strong_count(info.cast::<Sink<Raw>>()) };
    info
  }

  extern "C" fn release(info: *const c_void) {
    unsafe { Arc::decrement_strong_count(info.cast::<Sink<Raw>>()) };
  }

  struct Cf(NonNull<c_void>);

  impl Drop for Cf {
    fn drop(&mut self) {
      unsafe { CFRelease(self.0.as_ptr()) };
    }
  }

  fn path_array<'a>(paths: impl Iterator<Item = &'a PathBuf>) -> Option<Cf> {
    let strings = paths
      .map(|path| {
        let bytes = path.as_os_str().as_bytes();
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

  const CREATE_NO_DEFER: u32 = 0x2;
  const CREATE_FILE_EVENTS: u32 = 0x10;
  const SINCE_NOW: u64 = u64::MAX;
  const ENCODING_UTF8: u32 = 0x0800_0100;

  const EVFILT_VNODE: i16 = -4;
  const EVFILT_USER: i16 = -10;
  const EV_ADD: u16 = 0x1;
  const EV_CLEAR: u16 = 0x20;
  const NOTE_DELETE: u32 = 0x1;
  const NOTE_RENAME: u32 = 0x20;
  const NOTE_REVOKE: u32 = 0x40;
  const NOTE_TRIGGER: u32 = 0x0100_0000;
  const O_EVTONLY: i32 = 0x8000;
  const O_SEARCH: i32 = 0x4010_0000;

  const ATTR_BIT_MAP_COUNT: u16 = 5;
  const ATTR_CMN_NAME: u32 = 0x1;
  const ATTR_CMN_OBJTYPE: u32 = 0x8;
  const FSOPT_NOFOLLOW: u32 = 0x1;
  const VLNK: u32 = 5;
  const MAX_NAME_BYTES: usize = 768;

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

  #[repr(C)]
  struct ArrayCallbacks {
    _opaque: [u8; 0],
  }

  #[repr(C)]
  #[derive(Clone, Copy)]
  struct KEvent {
    ident: usize,
    filter: i16,
    flags: u16,
    fflags: u32,
    data: isize,
    udata: *mut c_void,
  }

  #[repr(C)]
  struct AttrList {
    bitmap_count: u16,
    reserved: u16,
    common: u32,
    volume: u32,
    dir: u32,
    file: u32,
    fork: u32,
  }

  #[repr(C)]
  struct NameAndType {
    length: u32,
    name: AttrReference,
    object_type: u32,
    name_bytes: [u8; MAX_NAME_BYTES],
  }

  #[repr(C)]
  struct AttrReference {
    offset: i32,
    length: u32,
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
    static TYPE_ARRAY_CALLBACKS: ArrayCallbacks;
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
      callbacks: *const ArrayCallbacks,
    ) -> CFTypeRef;
    fn CFRelease(object: CFTypeRef);
  }

  unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> DispatchQueue;
    fn dispatch_release(object: DispatchQueue);
    fn dispatch_sync_f(
      queue: DispatchQueue,
      context: *mut c_void,
      work: extern "C" fn(*mut c_void),
    );
    fn getattrlist(
      path: *const c_char,
      wanted: *mut c_void,
      buffer: *mut c_void,
      size: usize,
      options: u32,
    ) -> i32;
    fn getdtablesize() -> i32;
    fn kqueue() -> i32;
    fn kevent(
      queue: i32,
      changes: *const KEvent,
      change_count: i32,
      events: *mut KEvent,
      event_count: i32,
      timeout: *const c_void,
    ) -> i32;
  }
}

#[cfg(test)]
mod tests {
  use std::path::{Path, PathBuf};

  use super::{Dirs, is_too_high, place};

  fn paths(paths: &[&str]) -> Vec<PathBuf> {
    paths.iter().map(PathBuf::from).collect()
  }

  fn placed(covers: &[&str], max: usize) -> Vec<PathBuf> {
    place(paths(covers), max)
  }

  #[test]
  fn more_covers_than_stream_paths_share_the_lowest_common_directories() {
    let covers =
      ["/Users/me/p/a/1", "/Users/me/p/a/2", "/Users/me/p/b/1", "/Users/me/p/b/2", "/Users/me/q"];
    assert_eq!(placed(&covers, 5), paths(&covers));
    assert_eq!(
      placed(&covers, 4),
      paths(&["/Users/me/p/a", "/Users/me/p/b/1", "/Users/me/p/b/2", "/Users/me/q"])
    );
    assert_eq!(placed(&covers, 3), paths(&["/Users/me/p/a", "/Users/me/p/b", "/Users/me/q"]));
    assert_eq!(placed(&covers, 2), paths(&["/Users/me/p", "/Users/me/q"]));
    assert_eq!(placed(&covers, 1), paths(&["/Users/me/p", "/Users/me/q"]));
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

        let mut found: Vec<&Path> =
          covers.below(&[path]).into_iter().map(|(_, dir)| *dir).collect();
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
