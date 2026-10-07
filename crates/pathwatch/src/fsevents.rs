//! The FSEvents backend (macOS).
//!
//! A stream watches a list of paths, each with the whole tree below it. It reports a change as a
//! record: the real path of what changed, and flags. The C functions are declared by hand, at
//! the end of the file.

use std::{
  ffi::{CStr, CString, OsStr, OsString, c_char, c_void},
  fs, io, mem,
  os::unix::ffi::OsStrExt,
  path::{Component, Path, PathBuf},
  ptr::{self, NonNull},
  sync::Arc,
  time::{Duration, Instant},
};

use rustc_hash::FxHashMap;

use crate::{
  backend::{Backend, Coverage, DirKey, Hint, Sink, Want, watch_limit},
  placement::place,
};

/// With this many streams running, a `commit` does not add one more: it starts new streams for
/// all covers in their place.
pub const MAX_STREAMS: usize = 8;

/// The most stream paths before they go up to common directories.
///
/// FSEvents hands records over late when a process has many stream paths and other programs
/// write a lot. A probe, with 50,000 other files written: 0.1 s late with 1 or with 100 paths,
/// 0.9 s with 500, 3.5 s with 2,000. So up to 100 paths cost nothing. Fewer than needed is not
/// better: a path on a higher directory reports more that nobody watches. 64 leaves room for
/// the paths that come between two placements (at most `MAX_STREAMS` more), and it is the
/// limit of the Windows backend too.
pub const MAX_STREAM_PATHS: usize = 64;

/// The longest time a replaced stream runs next to the new ones.
const MAX_OVERLAP: Duration = Duration::from_millis(500);

/// The most replaced streams that run on. A process can run 512 streams, and commits that come
/// fast, each with a new directory, would use them up.
pub const MAX_RETIRING: usize = 64;

/// The most paths that one stream takes. `FSEventStreamStart` fails for a stream with more.
pub const MAX_PATHS: usize = 4096;

/// Flags of a record (`kFSEventStreamEventFlag...`). The OS merges the flags of a path over many
/// seconds, so they do not say what happened: the backend never sends `Hint::Written`.
pub mod flag {
  pub const MUST_SCAN_SUB_DIRS: u32 = 0x1;
  pub const USER_DROPPED: u32 = 0x2;
  pub const KERNEL_DROPPED: u32 = 0x4;
  pub const HISTORY_DONE: u32 = 0x10;
  pub const CREATED: u32 = 0x100;
  pub const REMOVED: u32 = 0x200;
  pub const RENAMED: u32 = 0x800;
  pub const IS_DIR: u32 = 0x2_0000;
}

/// Watches directories with FSEvents.
///
/// A **cover** is a directory that the engine watches and that a stream reports. A **stream
/// path** is a directory that a stream was started for: the stream reports everything below it.
///
/// - A cover that no stream reports yet becomes a stream path at the next `commit`. Streams are
///   only added, so a cover is never without one.
/// - With more than [`MAX_STREAM_PATHS`] stream paths, they go up to directories that the
///   covers have in common ([`place`]), but never to a directory that is too high (see
///   `is_too_high`). A cover below a running stream path needs no new stream.
/// - A stream path can report much that no cover holds. A record goes to the keys of the covers
///   that hold its path, and to nobody else.
/// - With [`MAX_STREAMS`] streams, new streams for all covers take over, and the old ones run on
///   until they have delivered what they still hold. At most [`MAX_RETIRING`] run on.
///
/// Limits of the OS:
///
/// - A write to a file that stays open, or a write through `mmap`, is not reported before the
///   file is closed.
/// - A process can run 512 streams. When no stream can start, `commit` fails with a watch limit
///   error and the engine polls the new directories.
#[derive(Debug)]
pub struct FsEventsBackend {
  /// Behind an `Arc`: every stream holds a count of it for its callback.
  sink: Arc<Sink<Raw>>,
  queue: Queue,
  /// The covers, each with the keys that the engine watches it under.
  covers: Dirs<Vec<DirKey>>,
  /// The cover of each key.
  cover_of_key: FxHashMap<DirKey, PathBuf>,
  /// The paths of the running streams.
  streamed: Dirs<()>,
  /// The paths for the stream that the next `commit` starts.
  pending: Dirs<()>,
  streams: Vec<Stream>,
  /// Replaced streams that may still hold records from before the new streams started.
  retiring: Vec<Retiring>,
  /// How many stream paths the last placement left.
  placed: usize,
  /// The directories that got `NotWatched`, each under its key.
  not_watched: FxHashMap<DirKey, PathBuf>,
  /// The last `commit` started a stream that reports one of them. See
  /// [`Backend::covers_more`].
  covers_more: bool,
  /// The directory above the one that `watch` was asked for last, until the next `commit`. See
  /// `real_path`.
  parent: Option<RealParent>,
}

/// A replaced stream that still runs.
#[derive(Debug)]
struct Retiring {
  stream: Stream,
  /// The newest event id of the system right after the new streams were started. A stream that
  /// has delivered this id has nothing left that the new streams do not get too.
  caught_up: u64,
  /// A stream that gets no event never reaches `caught_up`.
  deadline: Instant,
}

/// The records of one callback call: the raw notification of this backend.
#[derive(Debug, Default)]
pub struct Raw {
  /// The paths of all records, one after the other.
  paths: Vec<u8>,
  /// For each record: where its path ends in `paths`, and its flags.
  records: Vec<(usize, u32)>,
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

impl FsEventsBackend {
  /// Makes the dispatch queue only. The first stream starts at the first `commit` that has a
  /// directory for it.
  pub fn new(sink: Sink<Raw>) -> io::Result<Self> {
    Ok(Self {
      sink: Arc::new(sink),
      queue: Queue::new()?,
      covers: Dirs::default(),
      cover_of_key: FxHashMap::default(),
      streamed: Dirs::default(),
      pending: Dirs::default(),
      streams: Vec::new(),
      retiring: Vec::new(),
      placed: 0,
      not_watched: FxHashMap::default(),
      covers_more: false,
      parent: None,
    })
  }
}

/// Runs on the dispatch queue. It only copies the records and hands them over.
extern "C" fn callback(
  _stream: FSEventStreamRef,
  info: *mut c_void,
  count: usize,
  paths: *mut c_void,
  flags: *const u32,
  _ids: *const u64,
) {
  // SAFETY: `info` is the sink that `Stream::start` got. Its caller, the backend, holds a count
  // of it until every stream is stopped and a `Queue::sync` after that has returned. Then no
  // callback is running and none starts, so the sink lives longer than every callback. A sink
  // is `Sync`, so this thread may use it next to the watcher thread.
  let sink = unsafe { &*info.cast::<Sink<Raw>>() };
  let paths = paths.cast::<*const c_char>();
  let mut raw = Raw { paths: Vec::with_capacity(count * 128), records: Vec::with_capacity(count) };
  for index in 0..count {
    // SAFETY: a stream without the UseCFTypes flag passes `count` C strings and `count` flags,
    // and they live until the callback returns.
    let (path, flags) = unsafe { (CStr::from_ptr(*paths.add(index)), *flags.add(index)) };
    raw.push(path.to_bytes(), flags);
  }
  sink.raw(raw);
}

/// The stream calls this when it is created: it holds the sink for as long as it lives.
extern "C" fn retain(info: *const c_void) -> *const c_void {
  // SAFETY: `info` comes from `Arc::as_ptr`, and the caller of `FSEventStreamCreate` holds a
  // count of that `Arc` while the stream takes its own.
  unsafe { Arc::increment_strong_count(info.cast::<Sink<Raw>>()) };
  info
}

/// The stream calls this when it is deallocated, some time after `FSEventStreamRelease`.
extern "C" fn release(info: *const c_void) {
  // SAFETY: this gives back the count that `retain` took.
  unsafe { Arc::decrement_strong_count(info.cast::<Sink<Raw>>()) };
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
    let has_stream = self.streamed.holds(&real) || self.pending.holds(&real);
    if !has_stream {
      if want == Want::Names {
        // A stream for a directory that is only on the way to a watched path would report the
        // whole tree below it. The engine polls such a directory.
        self.unwatch(key);
        self.not_watched.insert(key, real);
        return Coverage::NotWatched;
      }
      self.pending.insert(real.clone(), ());
    }
    if self.cover_of_key.get(&key) != Some(&real) {
      // The first call for the key, or another directory is at its path now.
      self.unwatch(key);
      match self.covers.get_mut(&real) {
        Some(keys) => keys.push(key),
        None => self.covers.insert(real.clone(), vec![key]),
      }
      self.cover_of_key.insert(key, real);
    }
    Coverage::WatchedTree
  }

  fn unwatch(&mut self, key: DirKey) {
    // The streams stay as they are. A path that no cover needs any more goes when the streams
    // are replaced.
    self.not_watched.remove(&key);
    let Some(real) = self.cover_of_key.remove(&key) else { return };
    let Some(keys) = self.covers.get_mut(&real) else { return };
    keys.retain(|known| *known != key);
    if keys.is_empty() {
      self.covers.remove(&real);
    }
  }

  fn commit(&mut self) -> io::Result<()> {
    self.retire();
    self.covers_more = false;
    self.parent = None;
    if self.pending.is_empty() {
      return Ok(());
    }
    let new = top_most(self.pending.drain());
    let result = if self.streams.len() >= MAX_STREAMS || self.has_too_many_paths(new.len()) {
      // For all covers, so also for the new ones.
      self.rebuild()
    } else {
      self.add_streams(new)
    };
    match result {
      Ok(()) => self.covers_more = self.not_watched.values().any(|dir| self.streamed.holds(dir)),
      Err(_) => self.forget_unstreamed(),
    }
    result
  }

  fn covers_more(&self) -> bool {
    self.covers_more
  }

  fn translate(&mut self, raw: Raw, hint: &mut dyn FnMut(Hint<'_>)) {
    self.retire();
    for (path, flags) in raw.records() {
      if flags & flag::HISTORY_DONE != 0 {
        // A marker of the stream, not a change.
        continue;
      }
      if flags & (flag::MUST_SCAN_SUB_DIRS | flag::USER_DROPPED | flag::KERNEL_DROPPED) != 0 {
        self.lost(path, flags, hint);
        continue;
      }
      if flags & flag::IS_DIR != 0 && flags & (flag::CREATED | flag::REMOVED | flag::RENAMED) == 0 {
        // A chmod or touch of a directory. `Changed` for it would make the engine scan
        // everything below it.
        continue;
      }
      // Every cover that holds the path gets the hint, not only the longest: a cover can lie
      // below another one, and the keys of both see the change.
      for (keys, rel) in self.covers.holders(path) {
        for &key in keys {
          hint(Hint::Changed { dir: key, rel });
        }
      }
    }
  }

  fn raw_bytes(raw: &Self::Raw) -> usize {
    raw.paths.len() + size_of_val(raw.records.as_slice())
  }
}

impl FsEventsBackend {
  /// Stops the replaced streams that have nothing left to deliver.
  ///
  /// Nothing waits for them: this runs at every `commit` and `translate`, so a stream that is
  /// due goes with the next call.
  fn retire(&mut self) {
    let now = Instant::now();
    let done: Vec<Retiring> = self
      .retiring
      .extract_if(.., |old| {
        now >= old.deadline || old.stream.latest_event_id().is_some_and(|id| id >= old.caught_up)
      })
      .collect();
    if !done.is_empty() {
      // On the queue: the stream may be in the middle of handing over the records that made it
      // catch up, and a stop from this thread could cut that short.
      self.queue.sync(move || drop(done));
    }
  }

  /// Stops the oldest replaced streams when more than [`MAX_RETIRING`] run. Each one is asked
  /// to deliver what it still holds first, and that call waits for it.
  fn stop_oldest_retiring(&mut self) {
    let too_many = self.retiring.len().saturating_sub(MAX_RETIRING);
    if too_many == 0 {
      return;
    }
    let oldest: Vec<Retiring> = self.retiring.drain(..too_many).collect();
    for old in &oldest {
      old.stream.deliver_all();
    }
    self.queue.sync(move || drop(oldest));
  }

  /// Whether `new` more stream paths are too many, so that all paths have to be placed again.
  ///
  /// Where the covers cannot go up to common directories, the placement leaves more than
  /// [`MAX_STREAM_PATHS`]. Then it is not worked out at every commit, only when the number of
  /// paths has doubled.
  fn has_too_many_paths(&self, new: usize) -> bool {
    let limit = if self.placed > MAX_STREAM_PATHS { self.placed * 2 } else { MAX_STREAM_PATHS };
    self.streamed.len() + new > limit
  }

  /// Starts a stream for `paths`, next to the running ones.
  fn add_streams(&mut self, paths: Vec<PathBuf>) -> io::Result<()> {
    let streams = self.start(&paths)?;
    self.streams.extend(streams);
    for path in paths {
      self.streamed.insert(path, ());
    }
    Ok(())
  }

  /// Starts new streams for all covers and lets the old streams run out.
  fn rebuild(&mut self) -> io::Result<()> {
    let paths = self.stream_paths();
    let streams = self.start(&paths)?;
    // A stream that is stopped keeps what it has not delivered yet, and the new streams report
    // that late or not at all. So an old stream runs until it has delivered everything from
    // before the start of the new ones.
    let caught_up = current_event_id();
    let deadline = Instant::now() + MAX_OVERLAP;
    let replaced = mem::replace(&mut self.streams, streams);
    self.retiring.extend(replaced.into_iter().map(|stream| Retiring {
      stream,
      caught_up,
      deadline,
    }));
    self.stop_oldest_retiring();
    self.placed = paths.len();
    self.streamed.clear();
    for path in paths {
      self.streamed.insert(path, ());
    }
    Ok(())
  }

  /// The stream paths for the covers of now: at most [`MAX_STREAM_PATHS`], if the covers can go
  /// up to common directories that are not too high. Every cover is at or below one of them.
  fn stream_paths(&self) -> Vec<PathBuf> {
    // Every cover counts as one that wants the contents: its key was told that a stream reports
    // its whole tree, whatever it asked for.
    let wanted: Vec<(DirKey, &Path, Want)> =
      self.cover_of_key.iter().map(|(&key, real)| (key, real.as_path(), Want::Contents)).collect();
    place(&wanted, MAX_STREAM_PATHS, is_too_high).into_iter().map(|place| place.dir).collect()
  }

  /// Starts the streams for `paths`: all of them, or none. It is one stream for every
  /// `MAX_PATHS` paths.
  fn start(&self, paths: &[PathBuf]) -> io::Result<Vec<Stream>> {
    paths.chunks(MAX_PATHS).map(|chunk| Stream::start(&self.queue, &self.sink, chunk)).collect()
  }

  /// Forgets every cover that no stream reports, after a `commit` that could not start its
  /// streams: nothing is watched for the keys of such a cover.
  fn forget_unstreamed(&mut self) {
    let Self { covers, cover_of_key, streamed, .. } = self;
    covers.retain(|real, keys| {
      if streamed.holds(real) {
        return true;
      }
      for key in keys.iter() {
        cover_of_key.remove(key);
      }
      false
    });
  }

  /// The hints for a record that says that records were lost.
  fn lost(&self, path: &Path, flags: u32, hint: &mut dyn FnMut(Hint<'_>)) {
    // Only `MustScanSubDirs` gives the path a meaning: records for it, or for something below
    // it, were lost. And `/` is everything.
    let names_a_place = flags & flag::MUST_SCAN_SUB_DIRS != 0 && path != Path::new("/");
    if !names_a_place {
      hint(Hint::Lost);
      return;
    }
    // The keys of a cover that holds the path see that place, and so do the keys of a cover
    // below the path. The hint is for the whole cover: one that names the place would make the
    // engine check that one name only.
    let holders = self.covers.holders(path).map(|(keys, _)| keys);
    for keys in holders.chain(self.covers.below(path)) {
      for &key in keys {
        hint(Hint::Changed { dir: key, rel: Path::new("") });
      }
    }
  }

  /// The real path of `dir`. Records carry real paths, and a stream for a path through a link
  /// can be dead.
  ///
  /// The OS works out a real path name by name. Most directories that are asked for the
  /// contents come one after the other from one parent. So the real path of the parent is kept,
  /// and for the next directory in it one look at its own name is enough.
  ///
  /// Only for `Want::Contents`: the engine commits after such an ask (rule 2), and the commit
  /// forgets the parent. So what is kept is never older than the asks of one round.
  fn real_path(&mut self, dir: &Path, want: Want) -> io::Result<PathBuf> {
    let (Want::Contents, Some(above), Some(name)) = (want, dir.parent(), dir.file_name()) else {
      return canonical(dir);
    };
    let parent = match self.parent.take() {
      Some(parent) if parent.asked == above => parent,
      _ => RealParent { asked: above.to_owned(), real: canonical(above)? },
    };
    // The name is taken from the disk only when it is the asked name in another letter case.
    // A link points anywhere, the top of a volume gives the name of the volume, and a name can
    // have several Unicode forms: all that is left to the OS.
    let real = match name_on_disk(dir)? {
      Some(on_disk) if on_disk.eq_ignore_ascii_case(name) => parent.real.join(on_disk),
      _ => canonical(dir)?,
    };
    self.parent = Some(parent);
    if real.to_str().is_none() {
      return Err(not_utf8());
    }
    Ok(real)
  }
}

impl Drop for FsEventsBackend {
  fn drop(&mut self) {
    self.streams.clear();
    self.retiring.clear();
    // Stopping a stream does not wait for a callback that is running. This does: when it
    // returns, that callback is over and no other starts. So the sink is not called again, and
    // only now may it be freed.
    self.queue.sync(|| {});
  }
}

/// Hooks for tests: what the backend holds.
impl FsEventsBackend {
  /// How many streams run for the covers.
  pub fn stream_count(&self) -> usize {
    self.streams.len()
  }

  /// How many replaced streams still run.
  pub fn retiring_count(&self) -> usize {
    self.retiring.len()
  }

  /// The covers, in path order.
  pub fn covers(&self) -> Vec<&Path> {
    self.covers.paths()
  }

  pub fn cover_of_key(&self) -> &FxHashMap<DirKey, PathBuf> {
    &self.cover_of_key
  }

  /// The stream paths, in path order.
  pub fn streamed(&self) -> Vec<&Path> {
    self.streamed.paths()
  }

  /// The paths for the next stream, in path order.
  pub fn pending(&self) -> Vec<&Path> {
    self.pending.paths()
  }
}

/// The directory above the one that `watch` was asked for, as the engine spelled it, and its
/// real path.
#[derive(Debug)]
struct RealParent {
  asked: PathBuf,
  real: PathBuf,
}

/// The real path of `dir`, worked out by the OS.
fn canonical(dir: &Path) -> io::Result<PathBuf> {
  let real = fs::canonicalize(dir)?;
  if real.to_str().is_none() {
    return Err(not_utf8());
  }
  Ok(real)
}

/// Whether `dir` is too high to become a stream path that no cover asked for. A stream there
/// would report most of what happens on the disk:
///
/// - `/` and a directory in it (`/Users`, `/Volumes`, `/private`, `/opt`): a whole disk, or all
///   homes, or all volumes.
/// - A directory one more name down: a home (`/Users/me`), the top of a volume
///   (`/Volumes/disk`), the temporary files of all users (`/private/tmp`, `/private/var`).
/// - `/private/var/folders` and three names down: `…/xx/yyyy/T` is the temporary directory of
///   one user, and every program writes there.
///
/// `/System/Volumes/Data` is another name of `/`, so the names after it are counted.
pub fn is_too_high(dir: &Path) -> bool {
  let names = dir.components().filter_map(|part| match part {
    Component::Normal(name) => Some(name.as_bytes()),
    _ => None,
  });
  let names: Vec<&[u8]> = names.collect();
  let names = names.strip_prefix(&[&b"System"[..], b"Volumes", b"Data"]).unwrap_or(&names);
  match names {
    [] | [_] | [_, _] => true,
    [b"private", b"var", b"folders", below @ ..] => below.len() <= 3,
    _ => false,
  }
}

/// Drops every path that lies below another path of the list: a stream for the upper one
/// reports the lower one too.
fn top_most(paths: impl Iterator<Item = PathBuf>) -> Vec<PathBuf> {
  let mut paths: Vec<PathBuf> = paths.collect();
  // Paths sort part by part, so everything below a path comes right after it.
  paths.sort_unstable();
  paths.dedup_by(|path, kept| path.starts_with(kept));
  paths
}

/// The error for a path that a stream cannot take.
fn not_utf8() -> io::Error {
  io::Error::new(io::ErrorKind::InvalidInput, "FSEvents cannot watch a path that is not UTF-8")
}

/// Directories by their real paths, with a value for each.
///
/// It finds the directories that hold a path with one table lookup for each directory above
/// the path that can be in the table. So the cost does not grow with the number of directories.
#[derive(Debug)]
struct Dirs<T> {
  by_path: FxHashMap<PathBuf, T>,
  /// No path in the table is shorter or longer than this, in bytes. A path that is longer or
  /// shorter is not looked up. Both only widen, until the table is empty again.
  shortest: usize,
  longest: usize,
}

impl<T> Default for Dirs<T> {
  fn default() -> Self {
    Self { by_path: FxHashMap::default(), shortest: usize::MAX, longest: 0 }
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
    self.shortest = self.shortest.min(len);
    self.longest = self.longest.max(len);
    self.by_path.insert(dir, value);
  }

  fn get_mut(&mut self, dir: &Path) -> Option<&mut T> {
    self.by_path.get_mut(dir)
  }

  fn remove(&mut self, dir: &Path) {
    self.by_path.remove(dir);
    self.forget_lengths_if_empty();
  }

  fn retain(&mut self, keep: impl FnMut(&PathBuf, &mut T) -> bool) {
    self.by_path.retain(keep);
    self.forget_lengths_if_empty();
  }

  fn clear(&mut self) {
    *self = Self::default();
  }

  /// Takes every directory out.
  fn drain(&mut self) -> impl Iterator<Item = PathBuf> {
    mem::take(self).by_path.into_keys()
  }

  fn forget_lengths_if_empty(&mut self) {
    if self.by_path.is_empty() {
      (self.shortest, self.longest) = (usize::MAX, 0);
    }
  }

  /// The directories, in path order.
  fn paths(&self) -> Vec<&Path> {
    let mut paths: Vec<&Path> = self.by_path.keys().map(PathBuf::as_path).collect();
    paths.sort_unstable();
    paths
  }

  /// Whether a directory of the table holds `path`.
  fn holds(&self, path: &Path) -> bool {
    self.holders(path).next().is_some()
  }

  /// The value of each directory that holds `path` (the path is the directory or lies below
  /// it), the longest directory first, and the rest of the path below that directory.
  ///
  /// Paths are compared part by part: `/a/bc` is not below `/a/b`.
  fn holders<'a>(&'a self, path: &'a Path) -> impl Iterator<Item = (&'a T, &'a Path)> {
    // Without a `/` at the end: then the text of a directory above is the start of the text
    // of the path.
    let path = path.components().as_path();
    let bytes = path.as_os_str().as_bytes();
    path
      .ancestors()
      .skip_while(|dir| dir.as_os_str().len() > self.longest)
      .take_while(|dir| dir.as_os_str().len() >= self.shortest)
      .filter_map(move |dir| {
        let value = self.by_path.get(dir)?;
        let rest = &bytes[dir.as_os_str().len()..];
        let rest = rest.strip_prefix(b"/").unwrap_or(rest);
        Some((value, Path::new(OsStr::from_bytes(rest))))
      })
  }

  /// The values of the directories that lie below `path`, in path order.
  ///
  /// It looks at every directory of the table. Only a loss report asks for it.
  fn below(&self, path: &Path) -> impl Iterator<Item = &T> {
    let is_below = |(dir, _): &(&PathBuf, &T)| dir.starts_with(path) && dir.as_path() != path;
    let mut below: Vec<(&PathBuf, &T)> = self.by_path.iter().filter(is_below).collect();
    below.sort_unstable_by_key(|(dir, _)| dir.as_path());
    below.into_iter().map(|(_, value)| value)
  }
}

// ---- Safe wrappers around the C functions ----

/// A serial dispatch queue. The callbacks of all streams run on it, one at a time.
#[derive(Debug)]
struct Queue(NonNull<c_void>);

// SAFETY: a dispatch queue may be used from any thread.
unsafe impl Send for Queue {}

impl Queue {
  fn new() -> io::Result<Self> {
    // SAFETY: the label is a NUL-terminated string, and a null attribute asks for a serial queue.
    let queue = unsafe { dispatch_queue_create(c"pathwatch.fsevents".as_ptr(), ptr::null()) };
    NonNull::new(queue).map(Self).ok_or_else(|| io::Error::other("dispatch_queue_create failed"))
  }

  /// Runs `work` on the queue and waits for it. No callback runs at the same time: one that was
  /// running has returned before `work` starts.
  ///
  /// It must not be called from a callback: the queue would wait for itself.
  fn sync<F: FnOnce() + Send>(&self, work: F) {
    extern "C" fn run<F: FnOnce() + Send>(context: *mut c_void) {
      // SAFETY: `context` is the `work` below. It lives until `dispatch_sync_f` returns, and
      // until then its own thread only waits.
      let work = unsafe { &mut *context.cast::<Option<F>>() };
      if let Some(work) = work.take() {
        work();
      }
    }
    let mut work = Some(work);
    // SAFETY: the queue is alive, and `run::<F>` gets the pointer to an `Option<F>` that it
    // expects. `run` is called on a thread of the queue, which `F: Send` allows. The call
    // returns after `run` has returned.
    unsafe { dispatch_sync_f(self.0.as_ptr(), (&raw mut work).cast(), run::<F>) };
  }
}

impl Drop for Queue {
  fn drop(&mut self) {
    // SAFETY: this gives back the reference from `dispatch_queue_create`. A stream that is still
    // being torn down holds its own.
    unsafe { dispatch_release(self.0.as_ptr()) };
  }
}

/// A running stream. Dropping it stops it.
#[derive(Debug)]
struct Stream(NonNull<c_void>);

// SAFETY: the stream functions used here may be called from any thread, and the struct holds
// nothing that belongs to one thread.
unsafe impl Send for Stream {}

impl Stream {
  /// Starts a stream for `paths` that reports from now on. The paths must be real paths: the
  /// stream matches them as text against the real paths of the changes.
  ///
  /// The stream holds a count of `sink` until it is deallocated. Apple does not say that no
  /// callback runs after that. So the caller keeps its own `sink` until the stream is dropped
  /// and a [`Queue::sync`] after that has returned.
  fn start(queue: &Queue, sink: &Arc<Sink<Raw>>, paths: &[PathBuf]) -> io::Result<Self> {
    let array = path_array(paths).ok_or_else(not_utf8)?;
    let context = FSEventStreamContext {
      version: 0,
      info: Arc::as_ptr(sink).cast_mut().cast(),
      retain: Some(retain),
      release: Some(release),
      copy_description: None,
    };
    // No WatchRoot flag: it opens a descriptor for every component of every path.
    // SAFETY: `context` and `array` are alive during the call and the stream copies both. The
    // stream takes its count of the sink behind `info` with `retain`.
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
    let stream =
      NonNull::new(stream).ok_or_else(|| io::Error::other("FSEventStreamCreate failed"))?;
    // SAFETY: `stream` is a new stream that is not scheduled yet, and the queue is alive.
    let started = unsafe {
      FSEventStreamSetDispatchQueue(stream.as_ptr(), queue.0.as_ptr());
      FSEventStreamStart(stream.as_ptr()) != 0
    };
    if started {
      return Ok(Self(stream));
    }
    // SAFETY: the stream is scheduled and not started, and this is its only reference.
    unsafe {
      FSEventStreamInvalidate(stream.as_ptr());
      FSEventStreamRelease(stream.as_ptr());
    }
    // The two limits that make a start fail: 512 streams for a process, `MAX_PATHS` for a stream.
    Err(watch_limit("FSEvents cannot start one more stream (512 streams, 4,096 paths each)"))
  }

  /// Makes the stream hand over every record for what happened up to now, and waits until the
  /// callback has run for them.
  ///
  /// It must not be called from a callback: the callback runs on the queue, and the queue
  /// would wait for itself.
  fn deliver_all(&self) {
    // SAFETY: the stream is alive and started.
    unsafe { FSEventStreamFlushSync(self.0.as_ptr()) };
  }

  /// The id of the newest event that the stream has handed to the callback, if there was one.
  fn latest_event_id(&self) -> Option<u64> {
    // SAFETY: the stream is alive.
    let id = unsafe { FSEventStreamGetLatestEventId(self.0.as_ptr()) };
    (id != SINCE_NOW).then_some(id)
  }
}

impl Drop for Stream {
  fn drop(&mut self) {
    // This does not wait for a callback that is running. The stream gives back its count of the
    // sink later, through `release`.
    // SAFETY: the stream is started and scheduled, this is its only reference, and this is the
    // order that the API asks for.
    unsafe {
      FSEventStreamStop(self.0.as_ptr());
      FSEventStreamInvalidate(self.0.as_ptr());
      FSEventStreamRelease(self.0.as_ptr());
    }
  }
}

/// The name of the entry at `path` as the disk spells it. `None` if the entry is a link.
fn name_on_disk(path: &Path) -> io::Result<Option<OsString>> {
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
  // What comes back: the length of all of it (4 bytes), where the name is (an offset from
  // this field and a length, 4 bytes each), the type (4 bytes), then the name itself. A name
  // has at most 255 UTF-16 units, which is at most 765 bytes.
  let mut buffer = [0u8; 16 + 768];
  // SAFETY: the path is a NUL-terminated string, `wanted` is an `attrlist`, and the call writes
  // at most `buffer.len()` bytes into `buffer`. All three live during the call.
  let result = unsafe {
    getattrlist(
      c_path.as_ptr(),
      (&raw const wanted).cast_mut().cast(),
      buffer.as_mut_ptr().cast(),
      buffer.len(),
      FSOPT_NOFOLLOW,
    )
  };
  if result != 0 {
    return Err(io::Error::last_os_error());
  }
  let field =
    |at: usize| u32::from_ne_bytes([buffer[at], buffer[at + 1], buffer[at + 2], buffer[at + 3]]);
  let (offset, len, kind) = (field(4) as usize, field(8) as usize, field(12));
  if kind == VLNK {
    return Ok(None);
  }
  // The length counts the NUL at the end of the name.
  let name = len.checked_sub(1).and_then(|len| buffer.get(4 + offset..4 + offset + len));
  let name = name.ok_or_else(|| io::Error::other("getattrlist gave no name"))?;
  Ok(Some(OsStr::from_bytes(name).to_owned()))
}

/// The id of the newest event of the whole system.
fn current_event_id() -> u64 {
  FSEventsGetCurrentEventId()
}

/// An owned CoreFoundation object.
struct Cf(NonNull<c_void>);

impl Cf {
  fn new(object: CFTypeRef) -> Option<Self> {
    NonNull::new(object.cast_mut()).map(Self)
  }
}

impl Drop for Cf {
  fn drop(&mut self) {
    // SAFETY: the object is alive and this gives back the reference that `Cf::new` took over.
    unsafe { CFRelease(self.0.as_ptr()) };
  }
}

/// A `CFArray` of `CFString`s. `None` if a path is not UTF-8.
fn path_array(paths: &[PathBuf]) -> Option<Cf> {
  let strings = paths
    .iter()
    .map(|path| {
      let bytes = path.as_os_str().as_bytes();
      // SAFETY: the pointer and the length describe `bytes`, which the call copies.
      Cf::new(unsafe {
        CFStringCreateWithBytes(
          ptr::null(),
          bytes.as_ptr(),
          bytes.len().cast_signed(),
          ENCODING_UTF8,
          0,
        )
      })
    })
    .collect::<Option<Vec<Cf>>>()?;
  let values: Vec<CFTypeRef> =
    strings.iter().map(|string| string.0.as_ptr().cast_const()).collect();
  // SAFETY: `values` holds `values.len()` live objects, and with these callbacks the array
  // retains each of them.
  Cf::new(unsafe {
    CFArrayCreate(
      ptr::null(),
      values.as_ptr(),
      values.len().cast_signed(),
      &raw const TYPE_ARRAY_CALLBACKS,
    )
  })
}

// ---- C declarations: what the backend needs of CoreServices, CoreFoundation and libdispatch ----

const CREATE_NO_DEFER: u32 = 0x2;
const CREATE_FILE_EVENTS: u32 = 0x10;
const SINCE_NOW: u64 = u64::MAX;
const ENCODING_UTF8: u32 = 0x0800_0100;

const ATTR_BIT_MAP_COUNT: u16 = 5;
const ATTR_CMN_NAME: u32 = 0x1;
const ATTR_CMN_OBJTYPE: u32 = 0x8;
const FSOPT_NOFOLLOW: u32 = 0x1;
/// The type of a symbolic link (`VLNK` of `enum vtype`).
const VLNK: u32 = 5;

/// `struct attrlist`: which attributes `getattrlist` gives.
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

/// `CFArrayCallBacks`. Only its address is used.
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
  fn FSEventStreamFlushSync(stream: FSEventStreamRef);
  fn FSEventStreamStop(stream: FSEventStreamRef);
  fn FSEventStreamInvalidate(stream: FSEventStreamRef);
  fn FSEventStreamRelease(stream: FSEventStreamRef);
  fn FSEventStreamGetLatestEventId(stream: FSEventStreamRef) -> u64;
  safe fn FSEventsGetCurrentEventId() -> u64;
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

// libdispatch is a part of libSystem, which every program links.
unsafe extern "C" {
  fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> DispatchQueue;
  fn dispatch_release(object: DispatchQueue);
  fn dispatch_sync_f(queue: DispatchQueue, context: *mut c_void, work: extern "C" fn(*mut c_void));
  fn getattrlist(
    path: *const c_char,
    wanted: *mut c_void,
    buffer: *mut c_void,
    size: usize,
    options: u32,
  ) -> i32;
}

#[cfg(test)]
mod tests {
  use std::path::{Path, PathBuf};

  use super::{Dirs, is_too_high, top_most};

  #[test]
  fn top_most_drops_what_lies_below_another_path() {
    let paths = ["/x/a/b", "/x/ab", "/x/a", "/y", "/x/a/b/c", "/x/a", "/x/a.b"];
    let kept = top_most(paths.map(PathBuf::from).into_iter());
    assert_eq!(kept, ["/x/a", "/x/a.b", "/x/ab", "/y"].map(PathBuf::from));

    let kept = top_most(["/x", "/", "/y/z"].map(PathBuf::from).into_iter());
    assert_eq!(kept, [PathBuf::from("/")]);
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

  /// Every path of up to `depth` names, with these names.
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
    // Many sets of covers: for each `step`, every path whose number in a mixed-up order is
    // a multiple of it. The small steps give covers below covers, the large ones give few.
    for step in [1, 2, 3, 5, 7, 11, 23, 47, 101, 211, 400] {
      let picked = |index: &usize| ((index * 2_654_435_761) % 1013).is_multiple_of(step);
      let picked: Vec<&PathBuf> = (0..paths.len()).filter(picked).map(|at| &paths[at]).collect();
      let mut covers = Dirs::default();
      for path in &picked {
        covers.insert((*path).clone(), path.as_path());
      }
      for path in &paths {
        // The value of a cover is its path here, so the value and the rest give the path back.
        let found: Vec<(&Path, &Path)> =
          covers.holders(path).map(|(cover, rel)| (*cover, rel)).collect();
        let expected: Vec<(&Path, &Path)> = path
          .ancestors()
          .filter(|dir| picked.iter().any(|cover| cover.as_path() == *dir))
          .map(|dir| (dir, path.strip_prefix(dir).unwrap()))
          .collect();
        assert_eq!(found, expected, "the covers that hold {}, step {step}", path.display());
        assert_eq!(covers.holds(path), !expected.is_empty());

        let found: Vec<&Path> = covers.below(path).copied().collect();
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

  #[test]
  fn a_table_that_is_empty_again_forgets_its_lengths() {
    let mut covers = Dirs::default();
    covers.insert(PathBuf::from("/x/a/b/c"), ());
    covers.remove(Path::new("/x/a/b/c"));
    covers.insert(PathBuf::from("/x"), ());
    assert!(covers.holds(Path::new("/x/a")));
    assert!(!covers.holds(Path::new("/y/a")));
    assert_eq!(covers.len(), 1);
  }
}
