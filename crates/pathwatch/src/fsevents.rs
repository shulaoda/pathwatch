use std::{
  collections::VecDeque,
  ffi::OsStr,
  fs, io, mem,
  ops::Range,
  os::unix::ffi::OsStrExt,
  path::{Component, Path, PathBuf},
  sync::Arc,
  time::{Duration, Instant},
};

use rustc_hash::FxHashMap;

use self::ffi::{Queue, Stream};
use crate::backend::{Backend, Coverage, DirKey, Hint, Sink, Want};

pub const MAX_STREAMS: usize = 8;
pub const MAX_STREAM_PATHS: usize = 64;
pub const MAX_PATHS_PER_STREAM: usize = 4096;
const MAX_RETIRING_STREAMS: usize = 64;
const RETIRING_TIMEOUT: Duration = Duration::from_millis(500);
const EXPECTED_PATH_BYTES: usize = 128;

const ERROR_PATH_NOT_UTF8: &str = "FSEvents cannot watch a path that is not UTF-8";
const ERROR_QUEUE_CREATE: &str = "dispatch_queue_create failed";
const ERROR_STREAM_CREATE: &str = "FSEventStreamCreate failed";
const ERROR_STREAM_START: &str =
  "FSEvents cannot start one more stream (512 streams, 4,096 paths each)";
const ERROR_NAME_ON_DISK: &str = "getattrlist gave no name";

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
    Ok(Self {
      covers: Covers::default(),
      streams: Streams {
        sink: Arc::new(sink),
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
          _ => RealParent { asked: parent.to_owned(), real: fs::canonicalize(parent)? },
        };
        let real_parent = &self.last_parent.insert(last).real;
        match ffi::name_on_disk(dir)? {
          Some(on_disk) if on_disk.eq_ignore_ascii_case(name) => real_parent.join(on_disk),
          _ => fs::canonicalize(dir)?,
        }
      }
      _ => fs::canonicalize(dir)?,
    };
    if real.to_str().is_none() {
      return Err(io::Error::new(io::ErrorKind::InvalidInput, ERROR_PATH_NOT_UTF8));
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
    if let Err(error) = started {
      self.covers.retain(|dir| streamed.holds(dir));
      return Err(error);
    }
    let now_streamed = self.not_watched.iter().filter(|(_, dir)| streamed.holds(dir));
    Ok(now_streamed.map(|(&key, _)| key).collect())
  }

  fn translate(&mut self, raw: Raw, hint: &mut dyn FnMut(Hint<'_>)) {
    self.streams.stop_retired();
    let covers = &self.covers.by_dir;
    for (path, flags) in raw.records() {
      if flags & flag::HISTORY_DONE != 0 {
        continue;
      }
      if flags & flag::RECORDS_LOST != 0 {
        let names_a_place = flags & flag::MUST_SCAN_SUB_DIRS != 0 && path != Path::new("/");
        if !names_a_place {
          hint(Hint::Lost);
          continue;
        }
        let holders = covers.holders(path).map(|(keys, _)| keys);
        for keys in holders.chain(covers.below(path)) {
          for &key in keys {
            hint(Hint::Changed { dir: key, rel: Path::new("") });
          }
        }
        continue;
      }
      let is_touched_dir = flags & flag::IS_DIR != 0 && flags & flag::ENTRY_CAME_OR_WENT == 0;
      if is_touched_dir {
        continue;
      }
      for (keys, rel) in covers.holders(path) {
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

#[derive(Debug)]
struct RealParent {
  asked: PathBuf,
  real: PathBuf,
}

#[derive(Debug, Default)]
pub struct Raw {
  paths: Vec<u8>,
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
}

#[derive(Debug)]
struct Retiring {
  streams: Vec<Stream>,
  deadline: Instant,
}

impl Streams {
  fn start_pending(&mut self, covers: &Covers) -> io::Result<()> {
    let new = top_most(self.pending.drain().collect());
    let lies_above_a_stream = |path: &PathBuf| self.paths.below(path).next().is_some();
    let replace = self.running.len() >= MAX_STREAMS
      || self.paths.len() + new.len() > self.path_limit
      || new.iter().any(lies_above_a_stream);
    let paths = if replace {
      place(covers.dir_of.values().cloned().collect(), MAX_STREAM_PATHS)
    } else {
      new
    };
    let streams: Vec<Stream> = paths
      .chunks(MAX_PATHS_PER_STREAM)
      .map(|chunk| Stream::start(&self.queue, &self.sink, chunk))
      .collect::<io::Result<_>>()?;
    if replace {
      let replaced = mem::take(&mut self.running);
      if !replaced.is_empty() {
        let deadline = Instant::now() + RETIRING_TIMEOUT;
        self.retiring.push_back(Retiring { streams: replaced, deadline });
      }
      self.stop_retired();
      self.paths = Dirs::default();
      self.path_limit =
        if paths.len() > MAX_STREAM_PATHS { 2 * paths.len() } else { MAX_STREAM_PATHS };
    }
    self.running.extend(streams);
    for path in paths {
      self.paths.insert(path, ());
    }
    Ok(())
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

pub fn is_too_high(dir: &Path) -> bool {
  const DATA_VOLUME: [&[u8]; 3] = [b"System", b"Volumes", b"Data"];
  let names = dir.components().filter_map(|part| match part {
    Component::Normal(name) => Some(name.as_bytes()),
    _ => None,
  });
  let names: Vec<&[u8]> = names.collect();
  let names = names.strip_prefix(&DATA_VOLUME).unwrap_or(&names);
  match names {
    [] | [_] | [_, _] => true,
    [b"private", b"var", b"folders", below @ ..] => below.len() <= 3,
    _ => false,
  }
}

fn place(covers: Vec<PathBuf>, max: usize) -> Vec<PathBuf> {
  let mut paths = top_most(covers);
  let mut joins: Vec<Option<usize>> = paths
    .windows(2)
    .map(|pair| {
      let shared = pair[0].components().zip(pair[1].components()).take_while(|(a, b)| a == b);
      Some(shared.count())
    })
    .collect();
  while paths.len() > max {
    let Some(depth) = joins.iter().flatten().copied().max() else { break };
    let Some(first) = joins.iter().position(|&join| join == Some(depth)) else { break };
    let dir: PathBuf = paths[first].components().take(depth).collect();
    let below = paths[first..].iter().take_while(|path| path.starts_with(&dir)).count();
    let joins_below = first..first + below - 1;
    if is_too_high(&dir) {
      joins[joins_below].fill(None);
    } else {
      paths.splice(first..first + below, [dir]);
      joins.drain(joins_below);
    }
  }
  paths
}

fn top_most(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
  paths.sort_unstable();
  paths.dedup_by(|path, kept| path.starts_with(kept));
  paths
}

#[derive(Debug)]
struct Dirs<T> {
  by_path: FxHashMap<PathBuf, T>,
  path_lengths: Range<usize>,
}

impl<T> Default for Dirs<T> {
  fn default() -> Self {
    Self { by_path: FxHashMap::default(), path_lengths: Range { start: usize::MAX, end: 0 } }
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
    self.by_path.insert(dir, value);
  }

  fn get_mut(&mut self, dir: &Path) -> Option<&mut T> {
    self.by_path.get_mut(dir)
  }

  fn remove(&mut self, dir: &Path) {
    self.by_path.remove(dir);
    self.reset_lengths_if_empty();
  }

  fn retain(&mut self, keep: impl FnMut(&PathBuf, &mut T) -> bool) {
    self.by_path.retain(keep);
    self.reset_lengths_if_empty();
  }

  fn drain(&mut self) -> impl Iterator<Item = PathBuf> {
    mem::take(self).by_path.into_keys()
  }

  fn reset_lengths_if_empty(&mut self) {
    if self.by_path.is_empty() {
      self.path_lengths = Range { start: usize::MAX, end: 0 };
    }
  }

  fn paths(&self) -> Vec<&Path> {
    let mut paths: Vec<&Path> = self.by_path.keys().map(PathBuf::as_path).collect();
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
        let value = self.by_path.get(dir)?;
        let rest = &bytes[dir.as_os_str().len()..];
        let rest = rest.strip_prefix(b"/").unwrap_or(rest);
        Some((value, Path::new(OsStr::from_bytes(rest))))
      })
  }

  fn below(&self, path: &Path) -> impl Iterator<Item = &T> {
    let is_below = |(dir, _): &(&PathBuf, &T)| dir.starts_with(path) && dir.as_path() != path;
    let mut below: Vec<(&PathBuf, &T)> = self.by_path.iter().filter(is_below).collect();
    below.sort_unstable_by_key(|(dir, _)| dir.as_path());
    below.into_iter().map(|(_, value)| value)
  }
}

#[expect(clippy::undocumented_unsafe_blocks, reason = "this file has no comments")]
mod ffi {
  use std::{
    ffi::{CStr, CString, OsStr, OsString, c_char, c_void},
    io, mem,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr::{self, NonNull},
    sync::Arc,
  };

  use super::{
    ERROR_NAME_ON_DISK, ERROR_PATH_NOT_UTF8, ERROR_QUEUE_CREATE, ERROR_STREAM_CREATE,
    ERROR_STREAM_START, EXPECTED_PATH_BYTES, Raw,
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
    pub(super) fn start(
      queue: &Queue,
      sink: &Arc<Sink<Raw>>,
      paths: &[PathBuf],
    ) -> io::Result<Self> {
      let array = path_array(paths)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, ERROR_PATH_NOT_UTF8))?;
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

  fn path_array(paths: &[PathBuf]) -> Option<Cf> {
    let strings = paths
      .iter()
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
}
