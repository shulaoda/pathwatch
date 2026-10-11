//! The Windows backend: `ReadDirectoryChangesW` on at most 64 directory handles. See
//! `docs/design.md` 7.3 and `docs/windows-open-points.md`.

use std::{
  ffi::OsString,
  io, mem,
  os::windows::ffi::OsStringExt,
  path::{Path, PathBuf},
};

pub use self::{
  handles::{Handles, MAX_HANDLES, System, Told, WatchId},
  place::{Place, place},
  records::{Malformed, Record, Records},
};
use self::{port::Port, records::FILE_ACTION_MODIFIED};
use crate::backend::{Backend, Coverage, DirKey, Hint, Sink, Want};

#[derive(Debug)]
pub struct WindowsBackend {
  handles: Handles,
  port: Port,
  sink: Sink<Raw>,
  covered: Vec<DirKey>,
}

impl WindowsBackend {
  pub fn new(sink: Sink<Raw>) -> io::Result<Self> {
    let port = Port::new(sink.clone())?;
    Ok(Self { handles: Handles::default(), port, sink, covered: Vec::new() })
  }

  pub fn serves(&self, key: DirKey) -> bool {
    self.handles.serves(key)
  }

  fn tell(&mut self, told: Told) {
    self.covered.extend(told.covered);
    if !told.lost.is_empty() {
      self.sink.raw(Raw::Lost(told.lost));
    }
  }
}

impl Backend for WindowsBackend {
  type Raw = Raw;

  fn watch(&mut self, key: DirKey, dir: &Path, want: Want) -> Coverage {
    let (coverage, told) = self.handles.watch(key, dir, want, &mut self.port);
    self.tell(told);
    coverage
  }

  fn unwatch(&mut self, key: DirKey) {
    let told = self.handles.unwatch(key, &mut self.port);
    self.tell(told);
  }

  fn commit(&mut self) -> io::Result<Vec<DirKey>> {
    let told = self.handles.commit(&mut self.port);
    self.tell(told);
    Ok(mem::take(&mut self.covered))
  }

  fn translate(&mut self, raw: Raw, hint: &mut dyn FnMut(Hint<'_>)) {
    translate(&mut self.handles, raw, hint);
  }

  fn raw_bytes(raw: &Self::Raw) -> usize {
    match raw {
      Raw::Read { dir, result, .. } => dir.as_os_str().len() + result.as_ref().map_or(0, Vec::len),
      Raw::Lost(keys) => size_of_val(keys.as_slice()),
    }
  }
}

#[derive(Debug)]
pub enum Raw {
  Read { watch: WatchId, dir: PathBuf, extended: bool, result: io::Result<Vec<u8>> },
  Lost(Vec<DirKey>),
}

pub fn translate(handles: &mut Handles, raw: Raw, hint: &mut dyn FnMut(Hint<'_>)) {
  let lost = |key| Hint::Changed { dir: key, rel: Path::new("") };
  let (watch, dir, extended, result) = match raw {
    Raw::Read { watch, dir, extended, result } => (watch, dir, extended, result),
    Raw::Lost(keys) => return keys.into_iter().for_each(|key| hint(lost(key))),
  };
  let Ok(bytes) = result else {
    return handles.closed(watch).into_iter().for_each(|key| hint(lost(key)));
  };
  if bytes.is_empty() {
    return handles.below(&dir).into_iter().for_each(|key| hint(lost(key)));
  }
  let mut name = Vec::new();
  let records = if extended { Records::extended(&bytes) } else { Records::new(&bytes) };
  for record in records {
    let Ok(record) = record else {
      hint(Hint::Lost);
      return;
    };
    let written = record.action == FILE_ACTION_MODIFIED;
    name.clear();
    name.extend(record.name());
    if name.is_empty() {
      handles.below(&dir).into_iter().for_each(|key| hint(lost(key)));
      continue;
    }
    if written && record.is_dir == Some(true) {
      continue;
    }
    if let Some(parent) = before_short_name(&name) {
      let parent = dir.join(OsString::from_wide(parent));
      let mut found = false;
      handles.route(&parent, &mut |key, rel| {
        found = true;
        hint(Hint::Changed { dir: key, rel });
      });
      if !found {
        handles.below(&parent).into_iter().for_each(|key| hint(lost(key)));
      }
    } else {
      handles.route(&dir.join(OsString::from_wide(&name)), &mut |key, rel| {
        if !written {
          hint(Hint::Changed { dir: key, rel });
        } else if !rel.as_os_str().is_empty() {
          hint(Hint::Written { dir: key, rel });
        }
      });
    }
  }
}

const BACKSLASH: u16 = 0x5C;
const DOT: u16 = 0x2E;
const TILDE: u16 = 0x7E;
const DIGITS: std::ops::RangeInclusive<u16> = 0x30..=0x39;
const LOWER_CASE: std::ops::RangeInclusive<u16> = 0x61..=0x7A;

fn before_short_name(name: &[u16]) -> Option<&[u16]> {
  let mut start = 0_usize;
  for part in name.split(|&unit| unit == BACKSLASH) {
    if looks_short(part) {
      return Some(&name[..start.saturating_sub(1)]);
    }
    start += part.len() + 1;
  }
  None
}

fn looks_short(part: &[u16]) -> bool {
  let dot = part.iter().position(|&unit| unit == DOT).unwrap_or(part.len());
  let (base, extension) = part.split_at(dot);
  let digits = base.iter().rev().take_while(|unit| DIGITS.contains(unit)).count();
  let tilde = base.len().checked_sub(digits + 1).filter(|&at| at > 0 && base[at] == TILDE);
  // Windows makes 8.3 names in upper case: `v1~1.txt` is a long name.
  digits > 0
    && tilde.is_some()
    && base.len() <= 8
    && extension.len() <= 4
    && !extension[extension.len().min(1)..].contains(&DOT)
    && !part.iter().any(|unit| LOWER_CASE.contains(unit))
}

mod place {
  use std::path::{Component, Path, PathBuf};

  use crate::backend::{DirKey, Want};

  #[derive(Debug, PartialEq, Eq)]
  pub struct Place {
    pub dir: PathBuf,
    pub keys: Vec<DirKey>,
  }

  pub fn place(wanted: &[(DirKey, &Path, Want)], max: usize) -> Vec<Place> {
    let mut wanted: Vec<_> =
      wanted.iter().copied().filter(|&(_, dir, _)| may_hold_a_handle(dir)).collect();
    wanted.sort_by(|a, b| a.1.cmp(b.1));

    let mut tops: Vec<&Path> = Vec::new();
    for &(_, dir, want) in &wanted {
      let below_a_top = tops.last().is_some_and(|top| dir.starts_with(top));
      if want == Want::Contents && !below_a_top {
        tops.push(dir);
      }
    }

    let shared: Vec<usize> = tops.windows(2).map(|pair| common_parts(pair[0], pair[1])).collect();
    let mut joined = vec![false; shared.len()];
    let mut count = tops.len();
    let mut depths = shared.clone();
    depths.sort_unstable_by(|a, b| b.cmp(a));
    depths.dedup();
    'deepest_first: for depth in depths {
      let mut at = 0;
      while at < shared.len() {
        let end = at + shared[at..].iter().take_while(|&&parts| parts >= depth).count();
        if end == at {
          at += 1;
          continue;
        }
        if count <= max {
          break 'deepest_first;
        }
        if shared[at..end].contains(&depth) && may_hold_a_handle(&first_parts(tops[at], depth)) {
          for joined in &mut joined[at..end] {
            count -= usize::from(!*joined);
            *joined = true;
          }
        }
        at = end;
      }
    }

    let mut places = Vec::with_capacity(count);
    let mut first = 0;
    while first < tops.len() {
      let pairs = joined[first..].iter().take_while(|&&joined| joined).count();
      let dir = match shared[first..first + pairs].iter().min() {
        Some(&parts) => first_parts(tops[first], parts),
        None => tops[first].to_owned(),
      };
      places.push(Place { dir, keys: Vec::new() });
      first += pairs + 1;
    }

    let mut at = 0;
    for &(key, dir, _) in &wanted {
      while places
        .get(at)
        .is_some_and(|place| place.dir.as_path() < dir && !dir.starts_with(&place.dir))
      {
        at += 1;
      }
      if let Some(place) = places.get_mut(at)
        && dir.starts_with(&place.dir)
      {
        place.keys.push(key);
      }
    }
    places.truncate(max);
    places
  }

  fn common_parts(a: &Path, b: &Path) -> usize {
    a.components().zip(b.components()).take_while(|(a, b)| a == b).count()
  }

  fn first_parts(path: &Path, count: usize) -> PathBuf {
    path.components().take(count).collect()
  }

  pub(super) fn may_hold_a_handle(dir: &Path) -> bool {
    let mut names = dir.components().filter_map(|part| match part {
      Component::Normal(name) => Some(name),
      _ => None,
    });
    match (names.next(), names.next(), names.next()) {
      (None, ..) => false,
      (Some(first), _, None) => !first.eq_ignore_ascii_case("Users"),
      _ => true,
    }
  }
}

mod handles {
  use std::{
    borrow::Cow,
    io, mem,
    path::{Component, Path, PathBuf},
  };

  use rustc_hash::{FxHashMap, FxHashSet};

  use super::place::{may_hold_a_handle, place};
  use crate::backend::{Coverage, DirKey, Want, watch_limit};

  pub const MAX_HANDLES: usize = 64;

  #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
  pub struct WatchId(pub usize);

  pub trait System {
    fn open(&mut self, id: WatchId, dir: &Path) -> io::Result<()>;

    fn close(&mut self, id: WatchId);

    fn is_link(&mut self, path: &Path) -> bool;
  }

  #[derive(Debug)]
  struct Handle {
    id: WatchId,
    dir: PathBuf,
  }

  #[derive(Debug)]
  struct Served {
    dir: PathBuf,
    want: Want,
    handle: Option<WatchId>,
  }

  #[derive(Debug, Default, PartialEq, Eq)]
  pub struct Told {
    pub lost: Vec<DirKey>,
    pub covered: Vec<DirKey>,
  }

  #[derive(Debug, Default)]
  pub struct Handles {
    keys: FxHashMap<DirKey, Served>,
    at: FxHashMap<PathBuf, Vec<DirKey>>,
    open: Vec<Handle>,
    next_id: usize,
    unsettled: bool,
    refused: FxHashSet<DirKey>,
    waiting: FxHashMap<DirKey, PathBuf>,
  }

  impl Handles {
    pub fn watch(
      &mut self,
      key: DirKey,
      dir: &Path,
      want: Want,
      system: &mut dyn System,
    ) -> (Coverage, Told) {
      let mut told = Told::default();
      self.waiting.remove(&key);
      let refused = self.refused.remove(&key);
      let want = self.keys.get(&key).map_or(want, |served| served.want.max(want));

      if let Some(at) = self.open.iter().position(|handle| dir.starts_with(&handle.dir)) {
        let coverage = self.share(at, key, dir, want, system);
        if matches!(coverage, Coverage::NotWatched | Coverage::Failed(_)) {
          self.drop_key(key, system, &mut told);
        }
        return (coverage, told);
      }
      if want == Want::Names || !may_hold_a_handle(dir) {
        self.waiting.insert(key, dir.to_owned());
        return (Coverage::NotWatched, told);
      }

      self.remember(key, dir, want, None);
      if !refused {
        self.unsettled = true;
        return (Coverage::WatchedTree, told);
      }
      let settled = self.settle(system, &mut told);
      told.lost.retain(|&lost| lost != key);
      let coverage = match settled {
        Ok(()) if self.serves(key) => Coverage::WatchedTree,
        Ok(()) => Coverage::Failed(watch_limit("the limit of open directory handles is reached")),
        Err(error) => Coverage::Failed(error),
      };
      (coverage, told)
    }

    pub fn unwatch(&mut self, key: DirKey, system: &mut dyn System) -> Told {
      let mut told = Told::default();
      self.waiting.remove(&key);
      self.refused.remove(&key);
      self.drop_key(key, system, &mut told);
      told
    }

    pub fn commit(&mut self, system: &mut dyn System) -> Told {
      let mut told = Told::default();
      if self.unsettled {
        let _ = self.settle(system, &mut told);
      }
      told
    }

    pub fn closed(&mut self, id: WatchId) -> Vec<DirKey> {
      let Some(at) = self.open.iter().position(|handle| handle.id == id) else { return Vec::new() };
      self.open.swap_remove(at);
      let served = self.keys.iter().filter(|(_, served)| served.handle == Some(id));
      let keys: Vec<DirKey> = served.map(|(&key, _)| key).collect();
      for &key in &keys {
        self.forget(key);
      }
      keys
    }

    pub fn serves(&self, key: DirKey) -> bool {
      self.keys.get(&key).is_some_and(|served| served.handle.is_some())
    }

    pub(super) fn route(&self, path: &Path, found: &mut dyn FnMut(DirKey, &Path)) {
      let lower = lower_case(path);
      // The directories of the keys told so far, and whether each is spelled like the record.
      // With more than these, a key above them gets a hint too many, which costs one walk.
      let mut told: [Option<(&Path, bool)>; 4] = [None; 4];
      let mut count = 0;
      for (dir, lower_dir) in path.ancestors().zip(lower.ancestors()) {
        let Some(keys) = self.at.get(lower_dir) else { continue };
        let Ok(rel) = path.strip_prefix(dir) else { continue };
        let below = count.min(told.len());
        for &key in keys {
          let own = self.keys[&key].dir.as_path();
          let exact = own == dir;
          // A key below it, on the way that this key spells, was told: the engine gets there
          // from that one. Not when only this key is spelled like the record: in a directory
          // that tells the case of letters apart, the one below is another directory.
          let is_covered =
            |&(told, told_exact): &(&Path, bool)| (told_exact || !exact) && told.starts_with(own);
          if told[..below].iter().flatten().any(is_covered) {
            continue;
          }
          found(key, rel);
          if let Some(slot) = told.get_mut(count) {
            *slot = Some((own, exact));
          }
          count += 1;
        }
      }
    }

    pub(super) fn below(&self, dir: &Path) -> Vec<DirKey> {
      let dir = lower_case(dir);
      let below = self.at.iter().filter(|(lower, _)| lower.starts_with(&dir));
      below.flat_map(|(_, keys)| keys).copied().collect()
    }

    fn share(
      &mut self,
      at: usize,
      key: DirKey,
      dir: &Path,
      want: Want,
      system: &mut dyn System,
    ) -> Coverage {
      if leads_through_a_link(system, &self.open[at].dir, dir) {
        return Coverage::NotWatched;
      }
      if self.open[at].dir == dir
        && let Err(error) = self.reopen(at, system)
      {
        return Coverage::Failed(error);
      }
      self.remember(key, dir, want, Some(self.open[at].id));
      if want == Want::Contents { Coverage::WatchedTree } else { Coverage::Watched }
    }

    fn drop_key(&mut self, key: DirKey, system: &mut dyn System, told: &mut Told) {
      if self.forget(key).is_some_and(|gone| gone.want == Want::Contents) {
        let _ = self.settle(system, told);
      }
    }

    fn settle(&mut self, system: &mut dyn System, told: &mut Told) -> io::Result<()> {
      self.unsettled = false;
      let moved = self.move_handles(system, told);
      let left = self.keys.iter().filter(|(_, served)| served.handle.is_none());
      let left: Vec<DirKey> = left.map(|(&key, _)| key).collect();
      for key in left {
        self.forget(key);
        self.refused.insert(key);
        told.lost.push(key);
      }
      moved
    }

    fn move_handles(&mut self, system: &mut dyn System, told: &mut Told) -> io::Result<()> {
      let wanted: Vec<(DirKey, &Path, Want)> =
        self.keys.iter().map(|(&key, served)| (key, &*served.dir, served.want)).collect();
      let places = place(&wanted, MAX_HANDLES);

      let mut new = Vec::new();
      for place in &places {
        if self.open.iter().any(|handle| handle.dir == place.dir) {
          continue;
        }
        match self.next_id().and_then(|id| system.open(id, &place.dir).map(|()| id)) {
          Ok(id) => new.push(Handle { id, dir: place.dir.clone() }),
          Err(error) => {
            for handle in &new {
              system.close(handle.id);
            }
            self.close_unused(system);
            return Err(error);
          }
        }
      }
      let is_placed = |handle: &Handle| places.iter().any(|place| place.dir == handle.dir);
      let (kept, gone): (Vec<_>, Vec<_>) =
        mem::take(&mut self.open).into_iter().partition(is_placed);
      for handle in &gone {
        system.close(handle.id);
      }
      self.open = kept;
      for handle in &new {
        let waiting = self.waiting.iter().filter(|(_, dir)| dir.starts_with(&handle.dir));
        told.covered.extend(waiting.map(|(&key, _)| key));
      }
      self.open.extend(new);

      let mut place_of = FxHashMap::default();
      for place in &places {
        let Some(at) = self.open.iter().position(|handle| handle.dir == place.dir) else {
          continue;
        };
        place_of.extend(place.keys.iter().map(|&key| (key, at)));
      }
      let mut behind_a_link = Vec::new();
      for (key, served) in &mut self.keys {
        let handle = place_of.get(key).map(|&at| &self.open[at]);
        let id = handle.map(|handle| handle.id);
        if id != served.handle
          && handle.is_some_and(|handle| leads_through_a_link(system, &handle.dir, &served.dir))
        {
          behind_a_link.push(*key);
        }
        served.handle = id;
      }
      for key in behind_a_link {
        self.forget(key);
        told.lost.push(key);
        self.unsettled = true;
      }
      Ok(())
    }

    fn reopen(&mut self, at: usize, system: &mut dyn System) -> io::Result<()> {
      let id = self.next_id()?;
      system.open(id, &self.open[at].dir)?;
      let old = mem::replace(&mut self.open[at].id, id);
      system.close(old);
      for served in self.keys.values_mut() {
        if served.handle == Some(old) {
          served.handle = Some(id);
        }
      }
      Ok(())
    }

    fn close_unused(&mut self, system: &mut dyn System) {
      let used: FxHashSet<WatchId> =
        self.keys.values().filter_map(|served| served.handle).collect();
      self.open.retain(|handle| {
        let is_used = used.contains(&handle.id);
        if !is_used {
          system.close(handle.id);
        }
        is_used
      });
    }

    fn next_id(&mut self) -> io::Result<WatchId> {
      let id = WatchId(self.next_id);
      let next = self.next_id.checked_add(1);
      self.next_id = next.ok_or_else(|| watch_limit("every id of a directory handle is used"))?;
      Ok(id)
    }

    fn remember(&mut self, key: DirKey, dir: &Path, want: Want, handle: Option<WatchId>) {
      self.forget(key);
      self.at.entry(lower_case(dir)).or_default().push(key);
      self.keys.insert(key, Served { dir: dir.to_owned(), want, handle });
    }

    fn forget(&mut self, key: DirKey) -> Option<Served> {
      let gone = self.keys.remove(&key)?;
      let lower = lower_case(&gone.dir);
      let keys = self.at.get_mut(&lower)?;
      keys.retain(|&other| other != key);
      if keys.is_empty() {
        self.at.remove(&lower);
      }
      Some(gone)
    }
  }

  fn leads_through_a_link(system: &mut dyn System, top: &Path, dir: &Path) -> bool {
    let names = dir.strip_prefix(top).map_or(0, |below| below.components().count());
    dir.ancestors().take(names).any(|path| system.is_link(path))
  }

  fn lower_case(path: &Path) -> PathBuf {
    let lower = path.components().map(|part| match part {
      Component::Normal(name) => Cow::Owned(name.to_ascii_lowercase()),
      other => Cow::Borrowed(other.as_os_str()),
    });
    lower.collect()
  }
}

mod records {
  use std::mem::offset_of;

  pub(super) use windows_sys::Win32::Storage::FileSystem::FILE_ACTION_MODIFIED;
  use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_NOTIFY_EXTENDED_INFORMATION as Extended,
    FILE_NOTIFY_INFORMATION as Short,
  };

  const NEXT_ENTRY_OFFSET: usize = offset_of!(Short, NextEntryOffset);
  const ACTION: usize = offset_of!(Short, Action);
  // Both record layouts are read with these two offsets.
  const _: () = assert!(
    offset_of!(Extended, NextEntryOffset) == NEXT_ENTRY_OFFSET
      && offset_of!(Extended, Action) == ACTION
  );

  #[derive(Clone, Copy, Debug)]
  struct Layout {
    name_length: usize,
    name: usize,
    attributes: Option<usize>,
  }

  const SHORT: Layout = Layout {
    name_length: offset_of!(Short, FileNameLength),
    name: offset_of!(Short, FileName),
    attributes: None,
  };
  const EXTENDED: Layout = Layout {
    name_length: offset_of!(Extended, FileNameLength),
    name: offset_of!(Extended, FileName),
    attributes: Some(offset_of!(Extended, FileAttributes)),
  };

  #[derive(Debug)]
  pub struct Records<'a> {
    rest: Option<&'a [u8]>,
    layout: Layout,
  }

  impl<'a> Records<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
      Self { rest: (!bytes.is_empty()).then_some(bytes), layout: SHORT }
    }

    pub fn extended(bytes: &'a [u8]) -> Self {
      Self { layout: EXTENDED, ..Self::new(bytes) }
    }
  }

  impl<'a> Iterator for Records<'a> {
    type Item = Result<Record<'a>, Malformed>;

    fn next(&mut self) -> Option<Self::Item> {
      let bytes = self.rest.take()?;
      let Some((record, rest)) = split_first(bytes, self.layout) else {
        return Some(Err(Malformed));
      };
      self.rest = rest;
      Some(Ok(record))
    }
  }

  #[derive(Debug, PartialEq, Eq)]
  pub struct Record<'a> {
    pub action: u32,
    pub is_dir: Option<bool>,
    name: &'a [u8],
  }

  impl Record<'_> {
    pub fn name(&self) -> impl Iterator<Item = u16> {
      self.name.as_chunks().0.iter().map(|&unit| u16::from_ne_bytes(unit))
    }
  }

  #[derive(Debug, PartialEq, Eq)]
  pub struct Malformed;

  fn split_first(bytes: &[u8], layout: Layout) -> Option<(Record<'_>, Option<&[u8]>)> {
    let next = field(bytes, NEXT_ENTRY_OFFSET)?;
    let action = field(bytes, ACTION)?;
    let name_len = usize::try_from(field(bytes, layout.name_length)?).ok()?;
    let name = bytes.get(layout.name..)?.get(..name_len)?;
    let is_dir = match layout.attributes {
      Some(at) => Some(field(bytes, at)? & FILE_ATTRIBUTE_DIRECTORY != 0),
      None => None,
    };
    if name_len % 2 != 0 {
      return None;
    }
    let rest = match usize::try_from(next).ok()? {
      0 => None,
      next if next >= layout.name + name_len => Some(bytes.get(next..)?),
      _ => return None,
    };
    Some((Record { action, is_dir, name }, rest))
  }

  fn field(bytes: &[u8], at: usize) -> Option<u32> {
    let field = bytes.get(at..)?.first_chunk()?;
    Some(u32::from_ne_bytes(*field))
  }
}

mod port {
  use std::{
    ffi::c_void,
    fs::{File, OpenOptions},
    io, mem,
    os::windows::{
      fs::OpenOptionsExt,
      io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr,
    sync::{Arc, OnceLock, mpsc},
    thread,
  };

  use rustc_hash::FxHashMap;
  use windows_sys::Win32::{
    Foundation::{
      ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED, ERROR_NOTIFY_ENUM_DIR,
      HANDLE, INVALID_HANDLE_VALUE,
    },
    Storage::FileSystem::{
      FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
      FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
      FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
      ReadDirectoryChangesW, ReadDirectoryNotifyExtendedInformation,
    },
    System::{
      IO::{
        CancelIoEx, CreateIoCompletionPort, GetQueuedCompletionStatus,
        LPOVERLAPPED_COMPLETION_ROUTINE, OVERLAPPED, PostQueuedCompletionStatus,
      },
      Threading::INFINITE,
    },
  };

  use super::{Raw, System, WatchId};
  use crate::{
    backend::Sink,
    fs::{FileType, Fs, StdFs, Unknown, kernel32},
  };

  // No creation time: a tool that only sets it would give a `Written` hint.
  const FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_SIZE
    | FILE_NOTIFY_CHANGE_LAST_WRITE;

  const BUFFER_BYTES: u32 = 64 * 1024;
  const BUFFER_WORDS: usize = BUFFER_BYTES as usize / size_of::<u64>();

  type ReadExtended = unsafe extern "system" fn(
    HANDLE,
    *mut c_void,
    u32,
    i32,
    u32,
    *mut u32,
    *mut OVERLAPPED,
    LPOVERLAPPED_COMPLETION_ROUTINE,
    i32,
  ) -> i32;

  fn read_extended() -> Option<ReadExtended> {
    static CALL: OnceLock<Option<ReadExtended>> = OnceLock::new();
    *CALL.get_or_init(|| {
      let found = kernel32(c"ReadDirectoryChangesExW");
      // SAFETY: the function with this name has this signature.
      found.map(|call| unsafe { mem::transmute::<Unknown, ReadExtended>(call) })
    })
  }

  fn not_supported(error: &io::Error) -> bool {
    let code = error.raw_os_error().map(i32::cast_unsigned);
    matches!(code, Some(ERROR_INVALID_FUNCTION | ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED))
  }

  const MAX_EMPTY_READS: u8 = 16;

  const STOP_WAIT_MS: u32 = 2000;

  #[derive(Debug)]
  pub(super) struct Port {
    iocp: Arc<Iocp>,
    commands: mpsc::Sender<Command>,
    worker: Option<thread::JoinHandle<()>>,
  }

  impl Port {
    pub(super) fn new(sink: Sink<Raw>) -> io::Result<Self> {
      let iocp = Arc::new(Iocp::new()?);
      let (commands, receiver) = mpsc::channel();
      let worker = thread::Builder::new().name("pathwatch-windows".to_owned()).spawn({
        let iocp = Arc::clone(&iocp);
        move || {
          let watches = FxHashMap::default();
          Worker { iocp, commands: receiver, sink, watches, next_token: 0 }.run();
        }
      })?;
      Ok(Self { iocp, commands, worker: Some(worker) })
    }

    fn send(&self, command: Command) -> io::Result<()> {
      self.commands.send(command).map_err(|_| stopped())?;
      self.iocp.wake()
    }
  }

  impl System for Port {
    fn open(&mut self, id: WatchId, dir: &Path) -> io::Result<()> {
      let (reply, answer) = mpsc::sync_channel(1);
      self.send(Command::Open { id, dir: dir.to_owned(), reply }).inspect_err(|_| {
        let _ = self.commands.send(Command::Close { id });
      })?;
      match answer.recv() {
        Ok(result) => result,
        Err(mpsc::RecvError) => Err(stopped()),
      }
    }

    fn close(&mut self, id: WatchId) {
      let _ = self.send(Command::Close { id });
    }

    fn is_link(&mut self, path: &Path) -> bool {
      StdFs.lstat(path).is_ok_and(|at| at.kind == FileType::Symlink)
    }
  }

  impl Drop for Port {
    fn drop(&mut self) {
      let ends = self.commands.send(Command::Stop).is_err() || self.iocp.wake().is_ok();
      if let Some(worker) = self.worker.take()
        && ends
      {
        let _ = worker.join();
      }
    }
  }

  enum Command {
    Open { id: WatchId, dir: PathBuf, reply: mpsc::SyncSender<io::Result<()>> },
    Close { id: WatchId },
    Stop,
  }

  /// The handles. The port knows each by a token of its own, not by the id of the watch: a
  /// handle whose read failed at once is given up, and a late packet for it finds no watch.
  struct Worker {
    iocp: Arc<Iocp>,
    commands: mpsc::Receiver<Command>,
    sink: Sink<Raw>,
    watches: FxHashMap<usize, Watch>,
    next_token: usize,
  }

  impl Worker {
    fn run(mut self) {
      loop {
        match self.iocp.wait(INFINITE) {
          Packet::Wake => {
            if !self.run_commands() {
              break;
            }
          }
          Packet::Completed { token, result } => self.completed(token, result),
          Packet::Nothing(error) => {
            self.sink.failed(error);
            break;
          }
        }
      }
      self.stop();
    }

    fn run_commands(&mut self) -> bool {
      loop {
        match self.commands.try_recv() {
          Ok(Command::Open { id, dir, reply }) => {
            let _ = reply.send(self.open(id, &dir));
          }
          Ok(Command::Close { id }) => self.close(id),
          Ok(Command::Stop) | Err(mpsc::TryRecvError::Disconnected) => return false,
          Err(mpsc::TryRecvError::Empty) => return true,
        }
      }
    }

    fn open(&mut self, id: WatchId, path: &Path) -> io::Result<()> {
      let mut extended = read_extended().is_some();
      loop {
        let dir = open_dir(path)?;
        let token = self.next_token;
        self.next_token += 1;
        self.iocp.attach(&dir, token)?;
        let mut watch = Watch::new(id, dir, path.to_path_buf(), extended);
        match watch.read() {
          Ok(()) => {
            self.watches.insert(token, watch);
            return Ok(());
          }
          // The disk does not know the longer records. The handle goes with the watch: a
          // packet for the failed read must not pass for one of the next read.
          Err(error) if extended && not_supported(&error) => extended = false,
          Err(error) => return Err(error),
        }
      }
    }

    fn close(&mut self, id: WatchId) {
      let found = self.watches.iter_mut().find(|(_, watch)| watch.id == id);
      let Some((&token, watch)) = found else { return };
      watch.close();
      if !watch.pending {
        self.watches.remove(&token);
      }
    }

    fn completed(&mut self, token: usize, result: io::Result<u32>) {
      let Some(watch) = self.watches.get_mut(&token) else { return };
      watch.pending = false;
      if watch.dir.is_none() {
        if let Ok(len) = result {
          self.sink.raw(watch.raw(Ok(watch.records(len))));
        }
        self.watches.remove(&token);
        return;
      }
      let records = match result {
        Ok(len) => watch.records(len),
        Err(error) if error.raw_os_error() == Some(ERROR_NOTIFY_ENUM_DIR.cast_signed()) => {
          Vec::new()
        }
        Err(error) => {
          // The read is over: the handle can take the next one.
          if watch.extended && not_supported(&error) {
            watch.extended = false;
            if watch.read().is_ok() {
              return;
            }
          }
          self.died(token, error);
          return;
        }
      };
      watch.empty_reads = if records.is_empty() { watch.empty_reads + 1 } else { 0 };
      self.sink.raw(watch.raw(Ok(records)));
      let next = if watch.empty_reads < MAX_EMPTY_READS {
        watch.read()
      } else {
        Err(io::Error::other("every read of the handle comes back with no bytes"))
      };
      if let Err(error) = next {
        self.died(token, error);
      }
    }

    fn died(&mut self, token: usize, error: io::Error) {
      if let Some(watch) = self.watches.remove(&token) {
        self.sink.raw(watch.raw(Err(error)));
      }
    }

    fn stop(mut self) {
      for watch in self.watches.values_mut() {
        watch.close();
      }
      self.watches.retain(|_, watch| watch.pending);
      while !self.watches.is_empty() {
        match self.iocp.wait(STOP_WAIT_MS) {
          Packet::Completed { token, .. } => {
            if let Some(mut watch) = self.watches.remove(&token) {
              watch.pending = false;
            }
          }
          Packet::Wake => {}
          Packet::Nothing(_) => break,
        }
      }
    }
  }

  struct Watch {
    id: WatchId,
    dir: Option<File>,
    path: PathBuf,
    overlapped: Box<OVERLAPPED>,
    buffer: Box<[u64]>,
    pending: bool,
    empty_reads: u8,
    extended: bool,
  }

  impl Watch {
    fn new(id: WatchId, dir: File, path: PathBuf, extended: bool) -> Self {
      Self {
        id,
        dir: Some(dir),
        path,
        overlapped: Box::default(),
        buffer: vec![0; BUFFER_WORDS].into_boxed_slice(),
        pending: false,
        empty_reads: 0,
        extended,
      }
    }

    /// Queues one read. When it fails, no read is pending and the kernel is done with the
    /// blocks, but a packet can still come under the token of the handle.
    fn read(&mut self) -> io::Result<()> {
      let Some(dir) = &self.dir else { return Err(io::Error::other("the handle is closed")) };
      *self.overlapped = OVERLAPPED::default();
      let (handle, buffer) = (dir.as_raw_handle(), self.buffer.as_mut_ptr().cast());
      let (subtree, overlapped) = (i32::from(true), &raw mut *self.overlapped);
      let extended = read_extended().filter(|_| self.extended);
      // SAFETY: the handle is open and no read is pending on it. `buffer` is `BUFFER_BYTES`
      // long, and both heap blocks stay put and are freed only when `pending` is false (`Drop`).
      let queued = unsafe {
        let (size, no_count) = (BUFFER_BYTES, ptr::null_mut());
        match extended {
          Some(read) => {
            let class = ReadDirectoryNotifyExtendedInformation;
            read(handle, buffer, size, subtree, FILTER, no_count, overlapped, None, class)
          }
          None => {
            ReadDirectoryChangesW(handle, buffer, size, subtree, FILTER, no_count, overlapped, None)
          }
        }
      };
      if queued == 0 {
        return Err(io::Error::last_os_error());
      }
      self.pending = true;
      Ok(())
    }

    fn raw(&self, result: io::Result<Vec<u8>>) -> Raw {
      Raw::Read { watch: self.id, dir: self.path.clone(), extended: self.extended, result }
    }

    fn records(&self, len: u32) -> Vec<u8> {
      let len = (len as usize).min(BUFFER_BYTES as usize);
      let words = &self.buffer[..len.div_ceil(size_of::<u64>())];
      let mut bytes = Vec::with_capacity(size_of_val(words));
      for word in words {
        bytes.extend_from_slice(&word.to_ne_bytes());
      }
      bytes.truncate(len);
      bytes
    }

    fn close(&mut self) {
      let Some(dir) = self.dir.take() else { return };
      if self.pending {
        // SAFETY: the handle is open, and `overlapped` is the block of its pending read.
        unsafe { CancelIoEx(dir.as_raw_handle(), &raw const *self.overlapped) };
      }
    }
  }

  impl Drop for Watch {
    fn drop(&mut self) {
      if self.pending {
        mem::forget(mem::take(&mut self.buffer));
        mem::forget(mem::take(&mut self.overlapped));
      }
    }
  }

  #[derive(Debug)]
  struct Iocp(OwnedHandle);

  impl Iocp {
    fn new() -> io::Result<Self> {
      // SAFETY: the call reads no memory of ours. With these arguments it makes a new port.
      let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, ptr::null_mut(), 0, 1) };
      if port.is_null() {
        return Err(io::Error::last_os_error());
      }
      // SAFETY: the port is a new open handle that nothing else owns.
      Ok(Self(unsafe { OwnedHandle::from_raw_handle(port) }))
    }

    fn attach(&self, dir: &File, token: usize) -> io::Result<()> {
      // SAFETY: both handles are open, and the call reads no memory of ours.
      let port =
        unsafe { CreateIoCompletionPort(dir.as_raw_handle(), self.0.as_raw_handle(), token, 0) };
      if port.is_null() { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    fn wake(&self) -> io::Result<()> {
      // SAFETY: the port is open. The call allows a null `OVERLAPPED`.
      let posted = unsafe { PostQueuedCompletionStatus(self.0.as_raw_handle(), 0, 0, ptr::null()) };
      if posted == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    fn wait(&self, timeout_ms: u32) -> Packet {
      let mut bytes = 0;
      let mut key = 0;
      let mut overlapped = ptr::null_mut();
      // SAFETY: the port is open, and the three pointers are valid places for the answers.
      let ok = unsafe {
        GetQueuedCompletionStatus(
          self.0.as_raw_handle(),
          &raw mut bytes,
          &raw mut key,
          &raw mut overlapped,
          timeout_ms,
        )
      };
      let result = if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(bytes) };
      match (overlapped.is_null(), result) {
        (false, result) => Packet::Completed { token: key, result },
        (true, Ok(_)) => Packet::Wake,
        (true, Err(error)) => Packet::Nothing(error),
      }
    }
  }

  enum Packet {
    Wake,
    Completed { token: usize, result: io::Result<u32> },
    Nothing(io::Error),
  }

  fn open_dir(path: &Path) -> io::Result<File> {
    let dir = OpenOptions::new()
      .access_mode(FILE_LIST_DIRECTORY)
      .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
      .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED)
      .open(crate::fs::literal(path))?;
    if dir.metadata()?.is_dir() { Ok(dir) } else { Err(io::ErrorKind::NotADirectory.into()) }
  }

  fn stopped() -> io::Error {
    io::Error::other("the thread of the Windows backend has stopped")
  }
}

#[cfg(test)]
mod tests {
  use super::looks_short;

  #[test]
  fn what_looks_like_a_short_name() {
    let looks_short = |name: &str| looks_short(&name.encode_utf16().collect::<Vec<_>>());
    for name in ["PROGRA~1", "LONGFI~1.TXT", "A~1", "AB12CD~9.JS", "ABCD~123.X", "A~1."] {
      assert!(looks_short(name), "{name}");
    }
    for name in [
      "",
      "a.txt",
      "~1",
      "~1.txt",
      "a~.txt",
      "a~b.txt",
      "a~1b.txt",
      "file.txt~1",
      "a~1.json",
      "a~1.b.c",
      "v1~1.txt",
      "a~1.c",
      "api~2.md",
      "toolongna~1.txt",
      "backup~",
      "~$doc.docx",
    ] {
      assert!(!looks_short(name), "{name}");
    }
  }
}
