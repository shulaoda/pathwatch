//! The FSEvents backend of macOS: streams, the covers that the keys stand for, and the guard of
//! the directories above them. The design is in `references/design.md` 7.2.

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
  path::{Component, Path, PathBuf},
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
    Backend, Committed, Coverage, DirKey, Hint, Sink, Want, give_room_back, same, watch_limit,
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
  /// Another directory may stand at the path now.
  pub const DIR_REPLACED: u32 = RENAMED | MOUNT | UNMOUNT;
  pub const ENTRY_CAME_OR_WENT: u32 = CREATED | REMOVED | DIR_REPLACED;
}

#[derive(Debug)]
pub struct FsEventsBackend {
  covers: Covers,
  streams: Streams,
  /// The keys asked for names of a directory that no stream reports, at its real path: its
  /// guard tells about it.
  named: FxHashMap<DirKey, PathBuf>,
  last_parent: Option<RealParent>,
}

#[derive(Debug)]
struct RealParent {
  asked: PathBuf,
  real: PathBuf,
}

impl FsEventsBackend {
  pub fn new(sink: Sink<Raw>) -> io::Result<Self> {
    Ok(Self {
      covers: Covers::default(),
      streams: Streams::new(sink)?,
      named: FxHashMap::default(),
      last_parent: None,
    })
  }
}

impl Backend for FsEventsBackend {
  type Raw = Raw;

  fn watch(&mut self, key: DirKey, dir: &Path, want: Want) -> Coverage {
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
      // The key was at another real path, and nothing else said so. A stream that was to start
      // for it there is not needed.
      if self.streams.paths.holds(old) {
        self.streams.sink.raw(Raw { lost: vec![key], ..Raw::default() });
      }
      self.unwatch(key);
    }
    let has_stream = self.streams.paths.holds(&real) || self.streams.pending.holds(&real);
    if !has_stream {
      // A directory on the way: names come and go in it only when one of them is renamed or
      // removed, which the guard of that one tells, as its own does for this one.
      if want == Want::Names {
        if self.named.get(&key) != Some(&real) {
          self.unwatch(key);
          self.named.insert(key, real);
          self.streams.guard_is_stale = true;
        }
        return Coverage::Watched;
      }
      if !may_hold_a_stream(&real) {
        self.unwatch(key);
        return Coverage::NotWatched;
      }
      self.streams.pending.insert(real.clone(), ());
    }
    if self.named.remove(&key).is_some() {
      self.streams.guard_is_stale = true;
    }
    self.covers.insert(key, real);
    Coverage::WatchedTree
  }

  fn unwatch(&mut self, key: DirKey) {
    if self.named.remove(&key).is_some() {
      give_room_back(&mut self.named);
      self.streams.guard_is_stale = true;
    }
    // The stream that was to start here is not needed: a cover below it, which counted on
    // that stream, asks for one of its own.
    if let Some(dir) = self.covers.remove(key)
      && self.streams.pending.contains(&dir)
    {
      self.streams.pending.remove(&dir);
      for (below, _) in self.covers.by_dir.below(&[&dir]) {
        if !self.streams.paths.holds(below) {
          self.streams.pending.insert(below.to_owned(), ());
        }
      }
    }
  }

  fn commit(&mut self) -> Committed {
    // Notices whose wake-up was dropped with a full queue: the engine, which checks everything
    // then, hears of their keys here.
    let told = self.told();
    if !told.is_empty() {
      self.streams.sink.raw(Raw { lost: told, ..Raw::default() });
    }
    self.streams.stop_idle(&self.covers);
    self.last_parent = None;
    let mut committed = Committed::default();
    if self.streams.pending.is_empty() && !self.streams.guard_is_stale {
      return committed;
    }
    let Started { fresh, unguarded, failed } =
      self.streams.start_pending(&self.covers, &self.named);
    let mut gone = Vec::new();
    if !unguarded.is_empty() || failed.is_some() {
      // The keys at and below a directory with no guard, and the covers with no stream, go back
      // to the engine: an error is reported and polled, a directory that was away is checked.
      let why = |dir: &Path| unguarded.holders(dir).next().map(|(why, _)| why.as_ref());
      let streamed = &self.streams.paths;
      let named = self.named.iter().map(|(&key, dir)| (key, why(dir)));
      let covered = self.covers.dir_of.iter().filter(|(_, dir)| !streamed.holds(dir));
      let covered = covered.map(|(&key, dir)| (key, why(dir).or(failed.as_ref().map(Some))));
      let taken: Vec<_> = named.chain(covered).filter_map(|(key, why)| Some((key, why?))).collect();
      for (key, why) in taken {
        self.named.remove(&key);
        self.covers.remove(key);
        match why {
          Some(error) => committed.failed.push((key, same(error))),
          None => gone.push(key),
        }
      }
    }
    let mut moved = self.streams.moved_since_asked(&fresh, &self.covers);
    moved.lost.extend(gone);
    if !moved.records.is_empty() || !moved.lost.is_empty() {
      self.streams.sink.raw(moved);
    }
    self.streams.fit_guard(&self.named);
    committed
  }

  fn translate(&mut self, raw: Raw, hint: &mut dyn FnMut(Hint<'_>)) {
    for key in raw.lost.iter().copied().chain(self.told()) {
      hint(Hint::Changed { dir: key, rel: Path::new("") });
    }
    let covers = &self.covers;
    let mut lost_places = Vec::new();
    let mut moved_dirs = Vec::new();
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
      // The record of a volume that came or went is for a directory, though it does not say so.
      let is_dir = flags & (flag::IS_DIR | flag::MOUNT | flag::UNMOUNT) != 0;
      if is_dir && flags & flag::ENTRY_CAME_OR_WENT == 0 {
        continue;
      }
      if is_dir && flags & flag::DIR_REPLACED != 0 {
        moved_dirs.push(path);
      }
      for (keys, rel) in covers.by_dir.holders(path) {
        for &key in keys {
          hint(Hint::Changed { dir: key, rel });
        }
      }
    }
    let moved = covers.by_dir.below(&moved_dirs).flat_map(|(_, keys)| keys).copied();
    for key in covers.keys_around(&lost_places).chain(moved) {
      hint(Hint::Changed { dir: key, rel: Path::new("") });
    }
  }

  fn raw_bytes(raw: &Self::Raw) -> usize {
    raw.paths.capacity()
      + raw.records.capacity() * size_of::<(usize, u32)>()
      + raw.lost.capacity() * size_of::<DirKey>()
  }
}

impl FsEventsBackend {
  fn real_path(&mut self, dir: &Path) -> io::Result<PathBuf> {
    let real = match (dir.parent(), dir.file_name()) {
      (Some(parent), Some(name)) => {
        let last = match self.last_parent.take() {
          Some(last) if last.asked == parent => last,
          _ => RealParent { asked: parent.to_owned(), real: short_way(fs::canonicalize(parent)?) },
        };
        let real_parent = &self.last_parent.insert(last).real;
        // A link, or a name that differs by more than ASCII case, goes the slow way, by the OS.
        match name_and_kind(dir)? {
          (on_disk, kind) if kind != FileType::Symlink && on_disk.eq_ignore_ascii_case(name) => {
            real_parent.join(on_disk)
          }
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

  /// The keys at and below the directories whose guards told since the last look: each checks
  /// its directory, and each cover is read again, as its stream heard nothing while its path
  /// was another one, also when the path is back by now.
  fn told(&mut self) -> Vec<DirKey> {
    let moved = self.streams.moved();
    if moved.is_empty() {
      return Vec::new();
    }
    let is_below = |dir: &Path| moved.iter().any(|moved| dir.starts_with(moved));
    let named = self.named.iter().filter(|(_, dir)| is_below(dir)).map(|(&key, _)| key);
    named.chain(self.covers.keys_at_or_below(&moved)).collect()
  }
}

/// What the tests look at.
impl FsEventsBackend {
  pub fn stream_count(&self) -> usize {
    self.streams.running.len()
  }

  pub fn covers(&self) -> Vec<&Path> {
    self.covers.by_dir.paths()
  }

  pub fn cover_of_key(&self) -> &FxHashMap<DirKey, PathBuf> {
    &self.covers.dir_of
  }

  pub fn guarded(&self) -> Vec<&Path> {
    let mut dirs: Vec<&Path> = self.streams.guard.dirs.keys().map(PathBuf::as_path).collect();
    dirs.sort_unstable();
    dirs
  }

  pub fn streamed(&self) -> Vec<&Path> {
    self.streams.paths.paths()
  }

  pub fn pending(&self) -> Vec<&Path> {
    self.streams.pending.paths()
  }
}

#[derive(Debug, Default)]
pub struct Raw {
  paths: Vec<u8>,
  records: Vec<(usize, u32)>,
  lost: Vec<DirKey>,
}

impl Raw {
  pub fn push(&mut self, path: &[u8], flags: u32) {
    self.paths.extend_from_slice(path);
    self.records.push((self.paths.len(), flags));
  }

  /// The keys that the backend reports lost by itself, with no record.
  pub fn lost(&self) -> &[DirKey] {
    &self.lost
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

  /// Forgets the cover of `key`. Gives its directory when no key is left at it.
  fn remove(&mut self, key: DirKey) -> Option<PathBuf> {
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

  /// The keys of the covers at and below a directory of `dirs`.
  fn keys_at_or_below<'a>(
    &'a self,
    dirs: &'a [impl AsRef<Path>],
  ) -> impl Iterator<Item = DirKey> + 'a {
    let at = dirs.iter().filter_map(|dir| self.by_dir.get(dir.as_ref()));
    at.chain(self.by_dir.below(dirs).map(|(_, keys)| keys)).flatten().copied()
  }

  /// The keys of the covers that hold a directory of `dirs`, and of the covers below one: every
  /// key that can see what is at or below them.
  fn keys_around<'a>(&'a self, dirs: &'a [impl AsRef<Path>]) -> impl Iterator<Item = DirKey> + 'a {
    let holding = dirs.iter().flat_map(|dir| self.by_dir.holders(dir.as_ref()));
    let below = self.by_dir.below(dirs).map(|(_, keys)| keys);
    holding.map(|(keys, _)| keys).chain(below).flatten().copied()
  }
}

#[derive(Debug)]
struct Streams {
  sink: Arc<Sink<Raw>>,
  queue: Queue,
  running: Vec<Running>,
  /// The stream paths that are trusted. `distrust` takes a path out while its stream runs on,
  /// so these are not always all the paths of `running`.
  paths: Dirs<()>,
  pending: Dirs<()>,
  path_limit: usize,
  guard: Guard,
  /// The stream paths or the named directories changed since the guard was last made to fit.
  guard_is_stale: bool,
}

struct Started {
  /// The paths that got a stream and had none before.
  fresh: Vec<PathBuf>,
  /// The directories that got no guard, each with the reason, or none when it was not there: no
  /// stream started below them.
  unguarded: Dirs<Option<io::Error>>,
  /// Why no stream started, when none could: the old ones stay as they are.
  failed: Option<io::Error>,
}

/// A stream and the paths it was started for.
#[derive(Debug)]
struct Running {
  /// Stops when dropped.
  _stream: Stream,
  paths: Vec<PathBuf>,
}

impl Streams {
  fn new(sink: Sink<Raw>) -> io::Result<Self> {
    let sink = Arc::new(sink);
    let queue = Queue::new()?;
    let notices = Notices::new(&queue, &sink)?;
    Ok(Self {
      sink,
      queue,
      running: Vec::new(),
      paths: Dirs::default(),
      pending: Dirs::default(),
      path_limit: MAX_STREAM_PATHS,
      guard: Guard { notices, dirs: FxHashMap::default(), last_id: ffi::VOLUMES },
      guard_is_stale: false,
    })
  }

  /// Starts the streams for the pending paths, after the guard of the directories at and above
  /// them, and of the `named` ones: a directory renamed between the guard and the start is told
  /// too. A pending path below a directory that got no guard gets no stream.
  fn start_pending(&mut self, covers: &Covers, named: &FxHashMap<DirKey, PathBuf>) -> Started {
    let new = top_most(self.pending.drain().collect());
    let replace = !new.is_empty()
      && (self.running.len() >= MAX_STREAMS
        || self.paths.len() + new.len() > self.path_limit
        || self.paths.below(&new).next().is_some());
    let (placed, keep) = if replace {
      // With room for the paths of the next few commits: a few new directories must not
      // replace every stream again.
      let covered = covers.by_dir.keys().map(Path::to_owned).collect();
      let placed = place(covered, MAX_STREAM_PATHS - MAX_NEW_STREAMS);
      let keep = self.keep(&placed, covers);
      (placed, keep)
    } else {
      (new, Vec::new())
    };
    let kept = self.running.iter().zip(&keep).filter(|(_, keep)| **keep);
    let kept: FxHashSet<&Path> =
      kept.flat_map(|(running, _)| &running.paths).map(PathBuf::as_path).collect();
    let mut paths: Vec<PathBuf> =
      placed.iter().filter(|path| !kept.contains(path.as_path())).cloned().collect();
    let unguarded =
      self.guard.open(paths.iter().map(PathBuf::as_path), named.values().map(PathBuf::as_path));
    paths.retain(|path| !unguarded.holds(path));
    if paths.is_empty() {
      return Started { fresh: Vec::new(), unguarded, failed: None };
    }
    let stream_count =
      MAX_NEW_STREAMS.min(paths.len()).max(paths.len().div_ceil(MAX_PATHS_PER_STREAM));
    let start = |first: usize| -> io::Result<Running> {
      let every_nth = || paths.iter().skip(first).step_by(stream_count);
      let stream = Stream::start(&self.queue, &self.sink, every_nth())?;
      Ok(Running { _stream: stream, paths: every_nth().cloned().collect() })
    };
    let streams = match (0..stream_count).map(start).collect::<io::Result<Vec<Running>>>() {
      Ok(streams) => streams,
      Err(error) => return Started { fresh: Vec::new(), unguarded, failed: Some(error) },
    };
    let fresh = paths.iter().filter(|path| !self.paths.contains(path)).cloned().collect();
    // The new streams run before the old ones stop: nothing is missed in between.
    if replace {
      let mut keep = keep.into_iter();
      let replaced: Vec<Running> =
        self.running.extract_if(.., |_| keep.next() == Some(false)).collect();
      if !replaced.is_empty() {
        self.stop(replaced, covers);
      }
      self.paths = Dirs::default();
      for path in self.running.iter().flat_map(|running| &running.paths) {
        self.paths.insert(path.clone(), ());
      }
      // Paths that must stay apart raise the limit: not every commit replaces the streams.
      self.path_limit =
        if placed.len() > MAX_STREAM_PATHS { 2 * placed.len() } else { MAX_STREAM_PATHS };
    }
    self.running.extend(streams);
    for path in paths {
      self.paths.insert(path, ());
    }
    Started { fresh, unguarded, failed: None }
  }

  /// Which running streams a replacement keeps: each whose paths are all placed again and still
  /// trusted, so that it goes on with no gap, the newest of two on one path. When the streams
  /// are too many, only the ones with the most covers, so that the next commits have room.
  fn keep(&self, placed: &[PathBuf], covers: &Covers) -> Vec<bool> {
    let mut placed: FxHashSet<&Path> = placed.iter().map(PathBuf::as_path).collect();
    let mut kept = Vec::new();
    for (index, running) in self.running.iter().enumerate().rev() {
      let stays = |path: &PathBuf| placed.contains(path.as_path()) && self.paths.contains(path);
      if running.paths.iter().all(stays) {
        for path in &running.paths {
          placed.remove(path.as_path());
        }
        let covers_at = |path: &PathBuf| covers.by_dir.below(slice::from_ref(path)).count() + 1;
        kept.push((running.paths.iter().map(covers_at).sum::<usize>(), index));
      }
    }
    let room = if self.running.len() >= MAX_STREAMS { MAX_STREAMS / 2 } else { MAX_STREAMS };
    kept.sort_unstable_by(|a, b| b.cmp(a));
    kept.truncate(room - MAX_NEW_STREAMS);
    let mut keep = vec![false; self.running.len()];
    for (_, index) in kept {
      keep[index] = true;
    }
    keep
  }

  /// The directories whose guards told since the last look: renamed, also only in letter case,
  /// removed, or a volume came or went at or above them. Their guards close, and the stream
  /// paths at and below them are trusted no more (`references/design.md` 7.2).
  fn moved(&mut self) -> Vec<PathBuf> {
    let told = self.guard.notices.take();
    let mut moved: Vec<PathBuf> = told.iter().filter_map(|&id| self.guard.take(id)).collect();
    if told.contains(&ffi::VOLUMES) {
      moved.extend(self.guard.take_covered());
    }
    if !moved.is_empty() {
      self.guard_is_stale = true;
      self.distrust(&moved);
    }
    top_most(moved)
  }

  /// Closes the guards that no stream path and none of `named` needs. Only after the start: the
  /// old stream paths keep theirs until new streams stand, or for good if none could start.
  fn fit_guard(&mut self, named: &FxHashMap<DirKey, PathBuf>) {
    self.guard.fit(self.paths.keys(), named.values().map(PathBuf::as_path));
    self.guard_is_stale = false;
  }

  /// The stream paths at and below `dirs` are trusted no more.
  fn distrust(&mut self, dirs: &[PathBuf]) {
    let below: Vec<PathBuf> = self.paths.below(dirs).map(|(path, ())| path.to_owned()).collect();
    for path in dirs.iter().chain(&below) {
      self.paths.remove(path);
    }
  }

  /// The check after the start (`references/design.md` 7.2): a stream path, or a name between it
  /// and a cover, can have changed between `watch` and the start of the stream.
  fn moved_since_asked(&mut self, started: &[PathBuf], covers: &Covers) -> Raw {
    let mut moved = Raw::default();
    for path in started {
      let is_there = fs::canonicalize(path).is_ok_and(|real| real == *path);
      if !is_there {
        self.distrust(slice::from_ref(path));
        moved.push(path.as_os_str().as_bytes(), flag::MUST_SCAN_SUB_DIRS);
        continue;
      }
      let mut spelled = path.as_path();
      for (cover, keys) in covers.by_dir.below(slice::from_ref(path)) {
        let is_spelled = |dir: &Path| {
          name_and_kind(dir).is_ok_and(|(name, kind)| {
            kind != FileType::Symlink && Some(name.as_os_str()) == dir.file_name()
          })
        };
        if cover.ancestors().take_while(|dir| !spelled.starts_with(dir)).all(is_spelled) {
          spelled = cover;
        } else {
          moved.lost.extend(keys);
        }
      }
    }
    moved
  }

  /// Stops the streams that no cover needs any more: their records would reach no key, so
  /// nothing is lost with them.
  fn stop_idle(&mut self, covers: &Covers) {
    let needed = |path: &PathBuf| {
      self.paths.contains(path)
        && (covers.by_dir.contains(path)
          || covers.by_dir.below(slice::from_ref(path)).next().is_some())
    };
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

  /// Stops the replaced streams, with a loss report for the covers at and below their paths: a
  /// stream that stops takes what it has not delivered. A cover above them has a stream of its
  /// own: trusted stream paths never lie one below another.
  fn stop(&self, stopped: Vec<Running>, covers: &Covers) {
    let paths = top_most(stopped.iter().flat_map(|old| &old.paths).cloned().collect());
    let lost = covers.keys_at_or_below(&paths).collect();
    let sink = Arc::clone(&self.sink);
    self.queue.run(move || {
      drop(stopped);
      sink.raw(Raw { lost, ..Raw::default() });
    });
  }
}

impl Drop for Streams {
  fn drop(&mut self) {
    // No notice is read once this returns, and the streams stop on their queue after the stops
    // that wait there: no report of theirs comes after this.
    self.guard.notices.cancel();
    let streams = mem::take(&mut self.running);
    self.queue.sync(move || drop(streams));
  }
}

/// The open files of the guards of every watcher in the process: at most half of what the
/// process may open.
static GUARD_FILES: AtomicUsize = AtomicUsize::new(0);

/// An open file for each directory at and above a stream path, and for each directory that a key
/// asks names of: the kernel tells at once when one of them is renamed or removed, which no
/// stream tells (`references/design.md` 7.2).
#[derive(Debug)]
struct Guard {
  notices: Notices,
  dirs: FxHashMap<PathBuf, Guarded>,
  last_id: usize,
}

#[derive(Debug)]
struct Guarded {
  file: File,
  _ticket: Ticket,
  id: usize,
}

/// One of the open files that the guards may hold.
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

impl Guard {
  /// Opens a guard for each directory at and above `paths`, and for each of `named`, that has
  /// none. Gives the ones that got none, each with the reason, or with none when it was not
  /// there: it may be back by now, and nothing would tell its next rename.
  ///
  /// From the top down: what is below a directory that got none fails with it, and gets no
  /// guard. So the room that is left goes to whole ways down, and a guard that runs out of room
  /// at `/` does not fail every key.
  fn open<'a>(
    &mut self,
    paths: impl Iterator<Item = &'a Path>,
    named: impl Iterator<Item = &'a Path>,
  ) -> Dirs<Option<io::Error>> {
    let mut unguarded = Dirs::default();
    for dir in wanted(paths, named) {
      if self.dirs.contains_key(dir) || unguarded.holds(dir) {
        continue;
      }
      match self.open_one(dir) {
        Ok(guarded) => {
          self.dirs.insert(dir.to_owned(), guarded);
        }
        Err(error) => unguarded.insert(dir.to_owned(), (!is_missing(&error)).then_some(error)),
      }
    }
    unguarded
  }

  /// Closes the guards that no directory at or above `paths`, and none of `named`, needs.
  fn fit<'a>(
    &mut self,
    paths: impl Iterator<Item = &'a Path>,
    named: impl Iterator<Item = &'a Path>,
  ) {
    let wanted = wanted(paths, named);
    self.dirs.retain(|dir, _| wanted.binary_search(&dir.as_path()).is_ok());
  }

  fn open_one(&mut self, dir: &Path) -> io::Result<Guarded> {
    let ticket = Ticket::take()?;
    let mut options = OpenOptions::new();
    let file = options.read(true).custom_flags(ffi::O_EVTONLY | ffi::O_SEARCH).open(dir);
    let file = file.map_err(|error| match error.raw_os_error() {
      Some(ffi::EMFILE | ffi::ENFILE) => watch_limit(ERROR_NO_GUARD),
      _ => error,
    })?;
    self.last_id += 1;
    self.notices.watch(&file, self.last_id)?;
    Ok(Guarded { file, _ticket: ticket, id: self.last_id })
  }

  /// The directory of the guard that told `id`, if that guard still stands. It is closed, and so
  /// is each guard below it, whose directory left with it: other directories may be at their
  /// paths now.
  fn take(&mut self, id: usize) -> Option<PathBuf> {
    let dir = self.dirs.iter().find(|(_, guarded)| guarded.id == id)?.0.clone();
    self.dirs.retain(|guarded, _| !guarded.starts_with(&dir));
    Some(dir)
  }

  /// The directories of the guards that stand on another directory than the one at their path
  /// now: a volume came or went at or above it, which tells no guard. They are closed like the
  /// guards that told.
  fn take_covered(&mut self) -> Vec<PathBuf> {
    let id = |found: fs::Metadata| (found.dev(), found.ino());
    let is_covered = |(dir, guarded): &(&PathBuf, &Guarded)| {
      guarded.file.metadata().ok().map(id) != fs::metadata(dir).ok().map(id)
    };
    let covered: Vec<PathBuf> =
      self.dirs.iter().filter(is_covered).map(|(dir, _)| dir.clone()).collect();
    self.dirs.retain(|dir, _| !covered.iter().any(|covered| dir.starts_with(covered)));
    covered
  }
}

/// The directories at and above `paths`, and `named`, sorted: each before the ones below it.
fn wanted<'a>(
  paths: impl Iterator<Item = &'a Path>,
  named: impl Iterator<Item = &'a Path>,
) -> Vec<&'a Path> {
  let mut wanted: Vec<&Path> = paths.flat_map(Path::ancestors).chain(named).collect();
  wanted.sort_unstable();
  wanted.dedup();
  wanted
}

/// `realpath` spells `/Users` as `/System/Volumes/Data/Users`, and FSEvents reports the short
/// path. The short path is taken when it is the same directory and a real path itself.
fn short_way(real: PathBuf) -> PathBuf {
  let Ok(below) = real.strip_prefix(DATA_VOLUME) else { return real };
  let short = Path::new("/").join(below);
  let id = |path: &Path| fs::metadata(path).map(|found| (found.dev(), found.ino())).ok();
  let is_same = fs::canonicalize(&short).is_ok_and(|own| own == short)
    && id(&short).is_some_and(|short| Some(short) == id(&real));
  if is_same { short } else { real }
}

/// Whether a stream path there would report far more than the covers: `/`, a directory in
/// it, one more name down, and the temporary directory of the user with the names above it.
pub fn is_too_high(dir: &Path) -> bool {
  let names: Vec<&[u8]> = names_of(dir).collect();
  match names.as_slice() {
    [] | [_] | [_, _] => true,
    [b"private", b"var", b"folders", below @ ..] => below.len() <= 3,
    _ => false,
  }
}

/// Whether a stream may stand on the directory that a cover asks for: not on `/`, `/Users`
/// or a home directory, where every program of the user writes. The same three that no
/// handle of Windows is put on (`windows::place::may_hold_a_handle`).
fn may_hold_a_stream(dir: &Path) -> bool {
  let mut names = names_of(dir);
  match (names.next(), names.next(), names.next()) {
    (None, ..) => false,
    (Some(first), _, None) => first != b"Users",
    _ => true,
  }
}

fn names_of(dir: &Path) -> impl Iterator<Item = &[u8]> {
  let dir = dir.strip_prefix(DATA_VOLUME).unwrap_or(dir);
  dir.components().filter_map(|part| match part {
    Component::Normal(name) => Some(name.as_bytes()),
    _ => None,
  })
}

/// At most `max` stream paths for the covers: paths go up to directories that covers have in
/// common, the deepest first, but never to one that is too high.
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

/// Directories by path, in path order: the ones that hold a path and the ones below a path
/// are each one search away.
#[derive(Debug)]
struct Dirs<T> {
  by_path: BTreeMap<OsString, T>,
  /// The shortest and the longest path: a lookup skips the ancestors that no path can be.
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

  fn below<'a>(
    &'a self,
    paths: &'a [impl AsRef<Path>],
  ) -> impl Iterator<Item = (&'a Path, &'a T)> + 'a {
    paths.iter().flat_map(move |path| {
      let mut prefix = path.as_ref().components().as_path().as_os_str().to_owned();
      if !prefix.as_bytes().ends_with(b"/") {
        prefix.push("/");
      }
      let after = (Bound::Excluded(prefix.as_os_str()), Bound::Unbounded);
      let dirs = self.by_path.range::<OsStr, _>(after);
      let dirs = dirs.take_while(move |(dir, _)| dir.as_bytes().starts_with(prefix.as_bytes()));
      dirs.map(|(dir, value)| (Path::new(dir), value))
    })
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

  use super::{
    ERROR_QUEUE_CREATE, ERROR_SOURCE_CREATE, ERROR_STREAM_CREATE, ERROR_STREAM_START,
    EXPECTED_PATH_BYTES, Raw,
  };
  use crate::backend::{Sink, watch_limit};

  pub(super) const O_EVTONLY: i32 = 0x8000;
  pub(super) const O_SEARCH: i32 = 0x4010_0000;
  pub(super) const ENFILE: i32 = 23;
  pub(super) const EMFILE: i32 = 24;
  /// The notice of a volume that came or went: the ids of the guards come after it.
  pub(super) const VOLUMES: usize = 0;

  /// How many files the process may have open (`getdtablesize`).
  pub(super) fn open_file_limit() -> usize {
    // SAFETY: the call takes no memory of ours.
    usize::try_from(unsafe { getdtablesize() }).unwrap_or(0)
  }

  /// The kernel queue of the guard: a notice for each guarded directory that is renamed or
  /// removed, or whose volume goes, and for each volume that comes or goes. It is read on the
  /// dispatch queue of the streams, so it needs no thread of its own.
  #[derive(Debug)]
  pub(super) struct Notices {
    /// Open until `cancel` or `drop`: the `Reader` owns it, and only a cancel frees that.
    queue: i32,
    source: NonNull<c_void>,
    /// The ids that told and were not taken yet. The signal that wakes the engine for them can
    /// be dropped with a full queue; this list cannot.
    told: Arc<Mutex<Vec<usize>>>,
  }

  // SAFETY: `kevent` may be called on the queue from any thread, and a dispatch source may be
  // cancelled from any thread.
  unsafe impl Send for Notices {}

  /// What the dispatch source reads with: it owns the kernel queue, which its cancel handler
  /// closes.
  struct Reader {
    queue: OwnedFd,
    sink: Arc<Sink<Raw>>,
    told: Arc<Mutex<Vec<usize>>>,
  }

  impl Notices {
    pub(super) fn new(queue: &Queue, sink: &Arc<Sink<Raw>>) -> io::Result<Self> {
      // SAFETY: the call takes no memory of ours.
      let fd = unsafe { kqueue() };
      let ident = usize::try_from(fd).map_err(|_| io::Error::last_os_error())?;
      // SAFETY: `fd` was just opened, and nothing else owns it.
      let queue_fd = unsafe { OwnedFd::from_raw_fd(fd) };
      let told = Arc::default();
      let reader =
        Box::new(Reader { queue: queue_fd, sink: Arc::clone(sink), told: Arc::clone(&told) });
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
      let notices = Self { queue: fd, source, told };
      notices.add(KEvent {
        ident: 0,
        filter: EVFILT_FS,
        flags: EV_ADD | EV_CLEAR,
        fflags: VQ_MOUNT | VQ_UNMOUNT,
        data: 0,
        udata: ptr::without_provenance_mut(VOLUMES),
      })?;
      Ok(notices)
    }

    /// The ids that told since the last call.
    pub(super) fn take(&self) -> Vec<usize> {
      mem::take(&mut self.told.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Stops the reading: no call of `read` runs once the work on the queue so far is done.
    pub(super) fn cancel(&self) {
      // SAFETY: the source lives until `drop` releases it, and a second cancel does nothing.
      unsafe { dispatch_source_cancel(self.source.as_ptr()) };
    }

    /// Asks for the notice `id` when the directory of `file` is renamed or removed, or its
    /// volume goes. The ask goes with the file when it is closed.
    pub(super) fn watch(&self, file: &File, id: usize) -> io::Result<()> {
      self.add(KEvent {
        ident: usize::try_from(file.as_raw_fd()).map_err(io::Error::other)?,
        filter: EVFILT_VNODE,
        flags: EV_ADD | EV_CLEAR,
        fflags: NOTE_DELETE | NOTE_RENAME | NOTE_REVOKE,
        data: 0,
        udata: ptr::without_provenance_mut(id),
      })
    }

    fn add(&self, change: KEvent) -> io::Result<()> {
      // SAFETY: `change` is one whole `kevent`; the call reads it, writes none, and does not wait.
      let done =
        unsafe { kevent(self.queue, &raw const change, 1, ptr::null_mut(), 0, ptr::null()) };
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

  /// The event handler: the kernel queue has notices. One call takes up to 64; the source calls
  /// again while more wait.
  extern "C" fn read(context: *mut c_void) {
    // SAFETY: the context is the `Reader` of the source, which only the cancel handler frees,
    // and never while this runs.
    let reader = unsafe { &*context.cast::<Reader>() };
    let mut events = [NO_EVENT; 64];
    let room = i32::try_from(events.len()).unwrap_or(0);
    let now = Timespec { seconds: 0, nanos: 0 };
    let queue = reader.queue.as_raw_fd();
    // SAFETY: `events` holds `room` `kevent`, which the call may write; it does not wait.
    let count = unsafe { kevent(queue, ptr::null(), 0, events.as_mut_ptr(), room, &raw const now) };
    let Ok(count) = usize::try_from(count) else { return };
    if count > 0 {
      // Each id once: a directory renamed away and back again and again while the engine is
      // busy tells again and again, and the list waits for the engine.
      let mut told = reader.told.lock().unwrap_or_else(PoisonError::into_inner);
      for id in events[..count].iter().map(|event| event.udata.addr()) {
        if !told.contains(&id) {
          told.push(id);
        }
      }
      drop(told);
      // Only a wake-up: `translate` and `commit` take the ids.
      reader.sink.raw(Raw::default());
    }
  }

  /// The cancel handler: frees the `Reader`, which closes the kernel queue.
  extern "C" fn free(context: *mut c_void) {
    // SAFETY: the context came from `Box::into_raw` in `Notices::new`, and the cancel handler
    // runs once.
    drop(unsafe { Box::from_raw(context.cast::<Reader>()) });
  }

  #[derive(Debug)]
  pub(super) struct Queue(NonNull<c_void>);

  // SAFETY: a dispatch queue may be used from any thread, and `Drop` releases it once.
  unsafe impl Send for Queue {}

  impl Queue {
    pub(super) fn new() -> io::Result<Self> {
      // SAFETY: the label ends with a NUL, and a null attribute asks for a serial queue.
      let queue = unsafe { dispatch_queue_create(c"pathwatch.fsevents".as_ptr(), ptr::null()) };
      NonNull::new(queue).map(Self).ok_or_else(|| io::Error::other(ERROR_QUEUE_CREATE))
    }

    pub(super) fn sync<F: FnOnce() + Send>(&self, work: F) {
      extern "C" fn run<F: FnOnce() + Send>(context: *mut c_void) {
        // SAFETY: `context` is the `&mut Option<F>` of `sync` below, which waits until this ran:
        // the value is alive, and nothing else touches it meanwhile.
        let work = unsafe { &mut *context.cast::<Option<F>>() };
        if let Some(work) = work.take() {
          work();
        }
      }
      let mut work = Some(work);
      // SAFETY: the queue is open, and the call returns only when `run` has run, so `work`
      // lives long enough.
      unsafe { dispatch_sync_f(self.0.as_ptr(), (&raw mut work).cast(), run::<F>) };
    }

    /// Runs `work` on the queue, after what waits there, and does not wait for it: the queue
    /// can be busy for a long time with a callback. A `sync` after it waits for it too.
    pub(super) fn run<F: FnOnce() + Send + 'static>(&self, work: F) {
      extern "C" fn run<F: FnOnce() + Send>(context: *mut c_void) {
        // SAFETY: `context` came from `Box::into_raw` in `run` below, and the queue runs it once.
        let work = unsafe { Box::from_raw(context.cast::<F>()) };
        work();
      }
      let work = Box::into_raw(Box::new(work));
      // SAFETY: the queue is open, and libdispatch keeps it until the work has run.
      unsafe { dispatch_async_f(self.0.as_ptr(), work.cast(), run::<F>) };
    }
  }

  impl Drop for Queue {
    fn drop(&mut self) {
      // SAFETY: the queue came from `dispatch_queue_create`, and this is its one release.
      unsafe { dispatch_release(self.0.as_ptr()) };
    }
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
      // The paths are UTF-8 (`real_path`): only an allocation that failed gives no array.
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
    // SAFETY: `info` came from `Arc::as_ptr` on the `Arc<Sink<Raw>>` that `Streams` holds.
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

  const CREATE_NO_DEFER: u32 = 0x2;
  const CREATE_FILE_EVENTS: u32 = 0x10;
  const SINCE_NOW: u64 = u64::MAX;
  const ENCODING_UTF8: u32 = 0x0800_0100;
  const EVFILT_VNODE: i16 = -4;
  const EVFILT_FS: i16 = -9;
  const EV_ADD: u16 = 0x1;
  const EV_CLEAR: u16 = 0x20;
  const NOTE_DELETE: u32 = 0x1;
  const NOTE_RENAME: u32 = 0x20;
  const NOTE_REVOKE: u32 = 0x40;
  const VQ_MOUNT: u32 = 0x8;
  const VQ_UNMOUNT: u32 = 0x10;
  const NO_EVENT: KEvent =
    KEvent { ident: 0, filter: 0, flags: 0, fflags: 0, data: 0, udata: ptr::null_mut() };

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
  struct Timespec {
    seconds: i64,
    nanos: i64,
  }

  #[repr(C)]
  struct SourceType {
    _opaque: [u8; 0],
  }

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
    static SOURCE_TYPE_READ: SourceType;
    fn dispatch_source_create(
      kind: *const SourceType,
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
    fn kqueue() -> i32;
    fn kevent(
      queue: i32,
      changes: *const KEvent,
      change_count: i32,
      events: *mut KEvent,
      event_count: i32,
      timeout: *const Timespec,
    ) -> i32;
  }
}

#[cfg(test)]
mod tests {
  use std::path::{Path, PathBuf};

  use super::{DATA_VOLUME, Dirs, is_too_high, may_hold_a_stream, place, short_way};

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

  /// The directory itself is asked, not only its parent: `/Users` has a parent with no short way.
  #[test]
  fn the_short_way_of_a_directory_whose_parent_has_none() {
    let long = Path::new(DATA_VOLUME).join("Users");
    if long.is_dir() {
      assert_eq!(short_way(long), Path::new("/Users"));
    }
  }

  #[test]
  fn where_a_stream_may_stand_for_a_cover() {
    for dir in ["/", "/Users", "/Users/me", "/System/Volumes/Data", "/System/Volumes/Data/Users/me"]
    {
      assert!(!may_hold_a_stream(Path::new(dir)), "{dir}");
    }
    for dir in ["/Users/me/project", "/Volumes/disk", "/opt/homebrew", "/private/tmp", "/home"] {
      assert!(may_hold_a_stream(Path::new(dir)), "{dir}");
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
