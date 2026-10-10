//! The ReadDirectoryChangesW backend (Windows).
//!
//! [`place`] decides which directories get a handle, [`Handles`] keeps the handles there,
//! [`Records`] reads the bytes of a read and [`translate`] turns them into hints. These parts make
//! no Windows call, and `tests/windows_parts.rs` tests them alone. Every call to Windows is in
//! `port`, at the end of this file.
//!
//! One open directory handle has three types, and each is another thing:
//! - [`WatchId`] is the number that names the handle. The parts and the threads pass it around.
//! - `Handle` is what [`Handles`] remembers of it: the id and the directory. It is plain data.
//! - `port::Watch` is the handle itself, on the worker thread: the open directory and the
//!   memory of its reads.
//!
//! What is known to be missing, and what was not checked on a real machine, is in
//! `docs/windows-open-points.md`.

use std::{
  borrow::Cow,
  ffi::OsString,
  io, mem,
  path::{Component, Path, PathBuf},
};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::backend::{Backend, Coverage, DirKey, Hint, Sink, Want, watch_limit};

/// Each handle costs 64 KiB here and 64 KiB in the kernel, and pins the directories above it.
pub const MAX_HANDLES: usize = 64;

// Where each field of a record starts in the bytes of a read. See `Records`.
const NEXT_ENTRY_OFFSET: usize = 0;
const ACTION: usize = 4;
const FILE_NAME_LENGTH: usize = 8;
const FILE_NAME: usize = 12;

/// The content, the size or a time of the entry changed.
const FILE_ACTION_MODIFIED: u32 = 3;

const _: () = {
  use std::mem::offset_of;

  use windows_sys::Win32::Storage::FileSystem as os;

  assert!(offset_of!(os::FILE_NOTIFY_INFORMATION, NextEntryOffset) == NEXT_ENTRY_OFFSET);
  assert!(offset_of!(os::FILE_NOTIFY_INFORMATION, Action) == ACTION);
  assert!(offset_of!(os::FILE_NOTIFY_INFORMATION, FileNameLength) == FILE_NAME_LENGTH);
  assert!(offset_of!(os::FILE_NOTIFY_INFORMATION, FileName) == FILE_NAME);
  assert!(os::FILE_ACTION_MODIFIED == FILE_ACTION_MODIFIED);
};

// UTF-16 units. See `looks_short`.
const BACKSLASH: u16 = 0x5C;
const DOT: u16 = 0x2E;
const TILDE: u16 = 0x7E;
const DIGITS: std::ops::RangeInclusive<u16> = 0x30..=0x39;

/// Names one handle. It is the completion key of the handle.
///
/// A number is given out once, so a late packet of a closed handle is never taken for a packet of
/// a newer one.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct WatchId(pub usize);

/// One open directory handle.
///
/// - It reports its directory and everything below it, also when only files in the directory
///   itself are watched. It cannot be changed later, and no handle may be opened below it.
/// - While it is open, the user cannot rename the directories above it. Windows has no way
///   around this. The directory of the handle and all below it can be renamed.
/// - It follows its directory when that is renamed, and reports nothing about it.
#[derive(Debug)]
struct Handle {
  id: WatchId,
  dir: PathBuf,
}

// ---- The placement: which directories get a handle ----

/// One handle of a placement: its directory, and the keys that it serves.
#[derive(Debug, PartialEq, Eq)]
pub struct Place {
  pub dir: PathBuf,
  pub keys: Vec<DirKey>,
}

/// Decides where the handles go for the directories that the engine asked for: at most `max`
/// handles, and none below another. A key that is in no place is not watched. The same
/// directories give the same placement, in whatever order they come. It is the rule for the
/// stream paths of the FSEvents backend too (`place` in `fsevents.rs`).
///
/// 1. Each directory that wants the contents, with no other such directory above it, gets a
///    place. It serves every key at or below its directory, also a key that wants names only.
/// 2. If that makes more than `max` places, places go up to a directory that they have in
///    common and become one: the deepest common directory first, and of two that are equally
///    deep the first in path order. So the places stay as low as the limit allows.
/// 3. No place is on a directory for which `may_hold_a_handle` says no, and none goes up to
///    one. If more than `max` places are left for that reason, the last ones in path order
///    are not opened.
pub fn place(wanted: &[(DirKey, &Path, Want)], max: usize) -> Vec<Place> {
  // A directory that may hold no handle gets none, and no handle is above it either: every
  // directory above it may hold none. So no place serves it, and it is left out.
  let mut wanted: Vec<_> =
    wanted.iter().copied().filter(|&(_, dir, _)| may_hold_a_handle(dir)).collect();
  // In this order a directory comes before everything below it, and nothing else is between.
  wanted.sort_by(|a, b| a.1.cmp(b.1));

  let mut tops: Vec<&Path> = Vec::new();
  for &(_, dir, want) in &wanted {
    let below_a_top = tops.last().is_some_and(|top| dir.starts_with(top));
    if want == Want::Contents && !below_a_top {
      tops.push(dir);
    }
  }

  // `shared[i]`: how many parts the tops `i` and `i + 1` have in common at their start. The
  // directory made of these parts is the lowest one that holds both. `joined[i]`: one place
  // serves both.
  let shared: Vec<usize> = tops.windows(2).map(|pair| common_parts(pair[0], pair[1])).collect();
  let mut joined = vec![false; shared.len()];
  let mut count = tops.len();
  let mut depths = shared.clone();
  depths.sort_unstable_by(|a, b| b.cmp(a));
  depths.dedup();
  'deepest_first: for depth in depths {
    let mut at = 0;
    while at < shared.len() {
      // The tops `at..=end` are all the tops below one directory of `depth` parts.
      let end = at + shared[at..].iter().take_while(|&&parts| parts >= depth).count();
      if end == at {
        at += 1;
        continue;
      }
      if count <= max {
        break 'deepest_first;
      }
      // Two of them part right below that directory: it is the lowest one that holds them
      // all. If they all go on together, a directory below it holds them, and its turn was
      // before.
      if shared[at..end].contains(&depth) && may_hold_a_handle(&first_parts(tops[at], depth)) {
        for joined in &mut joined[at..end] {
          count -= usize::from(!*joined);
          *joined = true;
        }
      }
      at = end;
    }
  }

  // One place for each run of joined tops, on the directory that the whole run has in common.
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

  // Each key goes to the place at or above its directory. Both lists are in path order.
  let mut at = 0;
  for &(key, dir, _) in &wanted {
    // A place that is before this directory and does not hold it is before all that follow.
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

/// How many parts the two paths have in common at their start.
fn common_parts(a: &Path, b: &Path) -> usize {
  a.components().zip(b.components()).take_while(|(a, b)| a == b).count()
}

fn first_parts(path: &Path, count: usize) -> PathBuf {
  path.components().take(count).collect()
}

/// Whether a handle may be opened on `dir`. Not on a top directory (`C:\`), not on `Users` in
/// it, and not on a home directory (`C:\Users\<name>`): a handle reports everything below its
/// directory, and there that is most of what happens on the disk.
fn may_hold_a_handle(dir: &Path) -> bool {
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

// ---- The handles: where they are, and which keys they serve ----

/// The open handles, and every key that one of them serves. `Handles` only decides: the caller
/// opens and closes the handles.
///
/// - The handles are where [`place`] puts them for the keys of now. So they move when a key
///   comes or goes: up to a common directory when there are more than [`MAX_HANDLES`], and
///   down again when there is room. A handle cannot be moved, so new handles are opened
///   first. Then the old ones are closed, and each key that got another handle is told
///   ([`Change`]). If one of the new handles cannot be opened, nothing changes.
/// - A request for a directory at or below a handle opens nothing: that handle serves the key.
/// - A request for the very directory of a handle puts a new handle there. The old one may be
///   on a directory that was renamed away, and Windows gives no notice of that. This is also
///   how the engine repairs a handle that several keys share: it asks for the directory of
///   the handle when it finds another directory there.
/// - A `Names` request that no handle covers is not watched. Such a directory is often a top
///   directory (`C:\`) or a home directory, and a handle there would report the whole disk.
/// - Directories are compared by the text of their paths, part by part. Two spellings of one
///   directory (another letter case, a short name, a junction) can get two handles, one below
///   the other.
/// - A junction or a directory link below a handle gets no events. Windows reports what
///   happens behind it under the real path. But the path text says that the handle covers the
///   directory behind the link, so the answer is `WatchedTree` and the engine does not poll it.
#[derive(Debug, Default)]
pub struct Handles {
  /// The keys by their directory, with ASCII letters in lower case. See `route`.
  keys: FxHashMap<PathBuf, Vec<Served>>,
  /// No handle is below another.
  open: Vec<Handle>,
  next_id: usize,
}

impl Handles {
  /// The engine asks to watch `dir` under `key`, maybe not for the first time.
  pub fn watch(
    &mut self,
    key: DirKey,
    dir: &Path,
    want: Want,
    open: Open<'_>,
  ) -> (Coverage, Change) {
    let mut change = Change::default();
    // For one key the want only goes up (rule 5).
    let want = self.served(key).map_or(want, |served| served.want.max(want));

    if let Some(at) = self.open.iter().position(|handle| dir.starts_with(&handle.dir)) {
      if self.open[at].dir == dir
        && let Err(error) = self.reopen(at, open, &mut change)
      {
        // After `Failed` nothing is watched for the key (rule 4).
        return (Coverage::Failed(error), self.unwatch(key, open));
      }
      // No new placement: with one more directory at or below a handle, `place` puts every
      // handle where it is now (`tests/windows_parts.rs` checks that step by step).
      self.remember(key, dir, want, Some(self.open[at].id));
      // `WatchedTree`, also for a key that shares a handle on a directory above its own: the
      // handle reports every directory below the key's too, and each record goes to the
      // deepest key above it, so the answer is the truth (rule 4). With `Watched` the engine
      // would ask for each directory below, and each would be one more key for no hint more.
      return (Coverage::WatchedTree, change);
    }
    if want == Want::Names || !may_hold_a_handle(dir) {
      return (Coverage::NotWatched, change);
    }

    self.remember(key, dir, want, None);
    let coverage = match self.settle(open, &mut change) {
      Ok(()) if self.served(key).is_some() => Coverage::WatchedTree,
      Ok(()) => Coverage::Failed(watch_limit("the limit of open directory handles is reached")),
      Err(error) => {
        self.forget(key);
        Coverage::Failed(error)
      }
    };
    (coverage, change)
  }

  /// Forgets `key`. A handle goes with its last key (rule 6).
  pub fn unwatch(&mut self, key: DirKey, open: Open<'_>) -> Change {
    let mut change = Change::default();
    // A key that wants names only has no say in where the handles are.
    if self.forget(key).is_some_and(|gone| gone.want == Want::Contents) {
      // On an error the handles stay where they are. They still report all that the other
      // keys need, from higher up than needed.
      let _ = self.settle(open, &mut change);
    }
    change
  }

  /// Handle `id` died and is closed. It is forgotten with its keys (rule 10), and these are
  /// returned. None if the handle was replaced or closed before.
  pub fn closed(&mut self, id: WatchId) -> Vec<DirKey> {
    let Some(at) = self.open.iter().position(|handle| handle.id == id) else { return Vec::new() };
    self.open.swap_remove(at);
    let mut keys = Vec::new();
    self.keys.retain(|_, served| {
      served.retain(|served| {
        let gone = served.handle == Some(id);
        if gone {
          keys.push(served.key);
        }
        !gone
      });
      !served.is_empty()
    });
    keys
  }

  /// Puts the handles where [`place`] wants them for the keys of now. On an error no key has
  /// another handle than before.
  fn settle(&mut self, open: Open<'_>, change: &mut Change) -> io::Result<()> {
    let wanted: Vec<(DirKey, &Path, Want)> =
      self.keys.values().flatten().map(|served| (served.key, &*served.dir, served.want)).collect();
    let places = place(&wanted, MAX_HANDLES);

    // First the new handles. The old ones still report while these are opened.
    let mut new = Vec::new();
    for place in &places {
      if self.open.iter().any(|handle| handle.dir == place.dir) {
        continue;
      }
      let opened = self.next_id().and_then(|id| open(id, &place.dir).map(|()| id));
      match opened {
        Ok(id) => new.push(Handle { id, dir: place.dir.clone() }),
        Err(error) => {
          change.close.extend(new.iter().map(|handle| handle.id));
          self.close_unused(change);
          return Err(error);
        }
      }
    }
    for handle in mem::take(&mut self.open) {
      if places.iter().any(|place| place.dir == handle.dir) {
        self.open.push(handle);
      } else {
        change.close.push(handle.id);
      }
    }
    self.open.extend(new);

    // Each key gets the handle of its place. A key that had another one before is told.
    let mut handle_of = FxHashMap::default();
    for place in &places {
      let Some(handle) = self.open.iter().find(|handle| handle.dir == place.dir) else { continue };
      handle_of.extend(place.keys.iter().map(|&key| (key, handle.id)));
    }
    for served in self.keys.values_mut().flatten() {
      let handle = handle_of.get(&served.key).copied();
      if served.handle.is_some() && served.handle != handle {
        change.moved.push(served.key);
      }
      served.handle = handle;
    }
    // A key without a place is not watched any more.
    self.keys.retain(|_, served| {
      served.retain(|served| served.handle.is_some());
      !served.is_empty()
    });
    Ok(())
  }

  /// Puts a new handle on the directory of the handle `at`, for the same keys. These keys are
  /// not told: the engine asked for this, and looks at the directory after it.
  fn reopen(&mut self, at: usize, open: Open<'_>, change: &mut Change) -> io::Result<()> {
    let id = self.next_id()?;
    open(id, &self.open[at].dir)?;
    let old = mem::replace(&mut self.open[at].id, id);
    change.close.push(old);
    for served in self.keys.values_mut().flatten() {
      if served.handle == Some(old) {
        served.handle = Some(id);
      }
    }
    Ok(())
  }

  /// Closes the handles that serve no key.
  fn close_unused(&mut self, change: &mut Change) {
    let used: FxHashSet<WatchId> =
      self.keys.values().flatten().filter_map(|served| served.handle).collect();
    self.open.retain(|handle| {
      let is_used = used.contains(&handle.id);
      if !is_used {
        change.close.push(handle.id);
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

  fn served(&self, key: DirKey) -> Option<&Served> {
    self.keys.values().flatten().find(|served| served.key == key)
  }

  fn remember(&mut self, key: DirKey, dir: &Path, want: Want, handle: Option<WatchId>) {
    self.forget(key);
    let served = Served { key, dir: dir.to_owned(), want, handle };
    self.keys.entry(lower_case(dir)).or_default().push(served);
  }

  fn forget(&mut self, key: DirKey) -> Option<Served> {
    let dir = lower_case(&self.served(key)?.dir);
    let served = self.keys.get_mut(&dir)?;
    let gone = served.swap_remove(served.iter().position(|served| served.key == key)?);
    if served.is_empty() {
      self.keys.remove(&dir);
    }
    Some(gone)
  }

  /// Calls `found` for each key of the deepest directory that holds `path`, with the rest of
  /// the path. A path that no key holds is dropped: nobody watches that place. A handle that
  /// keys share also reports what happens next to their directories.
  ///
  /// The names in `path` are spelled as on the disk, and the directory of a key as the engine
  /// spelled it. A directory that is spelled like the path wins. Only if there is none, one
  /// that differs in the case of ASCII letters is taken: without this, such a key would get
  /// no hint from a handle above its directory. A key that is spelled in yet another way (a
  /// short name, another case of a letter that is not ASCII) gets no hints while it shares a
  /// handle.
  fn route(&self, path: &Path, found: &mut dyn FnMut(DirKey, &Path)) {
    let lower = lower_case(path);
    let mut other_case = None;
    for (dir, lower_dir) in path.ancestors().zip(lower.ancestors()) {
      let Some(served) = self.keys.get(lower_dir) else { continue };
      let Ok(rel) = path.strip_prefix(dir) else { continue };
      let mut same_text = served.iter().filter(|served| served.dir == dir).peekable();
      if same_text.peek().is_some() {
        same_text.for_each(|served| found(served.key, rel));
        return;
      }
      other_case.get_or_insert((served, rel));
    }
    if let Some((served, rel)) = other_case {
      for served in served {
        found(served.key, rel);
      }
    }
  }

  /// Every key whose directory is at or below `dir`, in any case of ASCII letters.
  fn below(&self, dir: &Path) -> Vec<DirKey> {
    let dir = lower_case(dir);
    let below = self.keys.iter().filter(|(lower, _)| lower.starts_with(&dir));
    below.flat_map(|(_, served)| served).map(|served| served.key).collect()
  }
}

/// A key that a handle serves.
#[derive(Debug)]
struct Served {
  key: DirKey,
  dir: PathBuf,
  want: Want,
  /// `None` only inside `watch`, while the place of a new key is worked out.
  handle: Option<WatchId>,
}

/// What the caller has to do after [`Handles::watch`] or [`Handles::unwatch`], in this order.
/// The handles that were opened in the call are live by now.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Change {
  /// Close these handles.
  pub close: Vec<WatchId>,
  /// Then send `Changed` with an empty `rel` for each of these keys. Another handle serves
  /// them now, and what waited in the closed one is lost (rule 11). Or no handle serves them
  /// any more (rule 10). The engine asks again for each of them.
  pub moved: Vec<DirKey>,
}

/// Opens a handle with this id on this directory. After `Ok` the handle is live.
pub type Open<'a> = &'a mut dyn FnMut(WatchId, &Path) -> io::Result<()>;

/// `path` with the ASCII letters of its names in lower case. A prefix stays as it is: `Path`
/// compares a drive letter without case already, and `\\?\UNC\` is only read as a prefix with
/// these capitals.
fn lower_case(path: &Path) -> PathBuf {
  let lower = path.components().map(|part| match part {
    Component::Normal(name) => Cow::Owned(name.to_ascii_lowercase()),
    other => Cow::Borrowed(other.as_os_str()),
  });
  lower.collect()
}

// ---- The backend ----

/// The ReadDirectoryChangesW backend. `Handles` decides which directories get a handle, and
/// `Port` opens and closes them on its thread.
#[derive(Debug)]
pub struct WindowsBackend {
  handles: Handles,
  port: port::Port,
  /// For the keys that moved to another handle: only `translate` can make a hint.
  sink: Sink<Raw>,
}

impl WindowsBackend {
  pub fn new(sink: Sink<Raw>) -> io::Result<Self> {
    Ok(Self { handles: Handles::default(), port: port::Port::new(sink.clone())?, sink })
  }
}

impl Backend for WindowsBackend {
  type Raw = Raw;

  fn watch(&mut self, key: DirKey, dir: &Path, want: Want) -> Coverage {
    let port = &self.port;
    let (coverage, change) = self.handles.watch(key, dir, want, &mut |id, dir| port.open(id, dir));
    self.finish(change);
    coverage
  }

  fn unwatch(&mut self, key: DirKey) {
    let port = &self.port;
    let change = self.handles.unwatch(key, &mut |id, dir| port.open(id, dir));
    self.finish(change);
  }

  fn translate(&mut self, raw: Raw, hint: &mut dyn FnMut(Hint<'_>)) {
    translate(&mut self.handles, raw, hint);
  }

  fn raw_bytes(raw: &Self::Raw) -> usize {
    match raw {
      Raw::Read { dir, result, .. } => dir.as_os_str().len() + result.as_ref().map_or(0, Vec::len),
      Raw::Moved(keys) => size_of_val(keys.as_slice()),
    }
  }
}

impl WindowsBackend {
  /// The new handles are live. Only now the old ones are closed, and then the keys that moved
  /// are told.
  fn finish(&self, change: Change) {
    for id in change.close {
      self.port.close(id);
    }
    if !change.moved.is_empty() {
      self.sink.raw(Raw::Moved(change.moved));
    }
  }
}

/// What the backend hands to the engine.
#[derive(Debug)]
pub enum Raw {
  /// What one read on a handle gave.
  Read {
    watch: WatchId,
    /// The directory that the handle was opened on. It travels with the bytes, because a handle
    /// that was replaced is forgotten before its last reads are translated.
    dir: PathBuf,
    /// `Ok`: the records, and the handle reads on. No bytes at all means that the buffer of
    /// this handle was too small and the kernel dropped its records. `Err`: the read failed
    /// and the handle is closed.
    result: io::Result<Vec<u8>>,
  },
  /// Another handle serves these keys from now on, or none does. What waited in the handle
  /// that was closed for it is lost.
  Moved(Vec<DirKey>),
}

// ---- The bytes of one completed read ----

/// Walks the records in the bytes of one completed `ReadDirectoryChangesW` call. After a
/// [`Malformed`] it yields nothing more.
///
/// The bytes are a chain of `FILE_NOTIFY_INFORMATION` records:
///
/// ```text
/// u32 NextEntryOffset   from the start of this record to the next one; 0 in the last record
/// u32 Action
/// u32 FileNameLength    in bytes
/// u16 FileName[]        relative to the directory of the handle, with no NUL at the end
/// ```
///
/// The bytes are copied out of a buffer that the kernel filled, sometimes with what a network
/// server sent. Nothing in them is trusted: every offset and length is checked against the slice,
/// and no field is read through a pointer, so the slice needs no alignment.
#[derive(Debug)]
pub struct Records<'a> {
  /// From the start of the next record to the end of the buffer.
  rest: Option<&'a [u8]>,
}

impl<'a> Records<'a> {
  pub fn new(bytes: &'a [u8]) -> Self {
    Self { rest: (!bytes.is_empty()).then_some(bytes) }
  }
}

impl<'a> Iterator for Records<'a> {
  type Item = Result<Record<'a>, Malformed>;

  fn next(&mut self) -> Option<Self::Item> {
    let bytes = self.rest.take()?;
    let Some((record, rest)) = split_first(bytes) else { return Some(Err(Malformed)) };
    self.rest = rest;
    Some(Ok(record))
  }
}

/// One record of a read.
#[derive(Debug, PartialEq, Eq)]
pub struct Record<'a> {
  pub action: u32,
  name: &'a [u8],
}

impl Record<'_> {
  /// The path of the entry below the directory of the handle, in UTF-16.
  pub fn name(&self) -> impl Iterator<Item = u16> {
    self.name.as_chunks().0.iter().map(|&unit| u16::from_ne_bytes(unit))
  }
}

/// A record does not fit into the bytes, or it points into itself. The records after it cannot be
/// found.
#[derive(Debug, PartialEq, Eq)]
pub struct Malformed;

/// The record at the start of `bytes`, and the bytes from the next record on.
fn split_first(bytes: &[u8]) -> Option<(Record<'_>, Option<&[u8]>)> {
  let next = field(bytes, NEXT_ENTRY_OFFSET)?;
  let action = field(bytes, ACTION)?;
  let name_len = usize::try_from(field(bytes, FILE_NAME_LENGTH)?).ok()?;
  let name = bytes.get(FILE_NAME..)?.get(..name_len)?;
  if name_len % 2 != 0 {
    return None;
  }
  let rest = match usize::try_from(next).ok()? {
    0 => None,
    // `name` is inside `bytes`, so the sum cannot overflow.
    next if next >= FILE_NAME + name_len => Some(bytes.get(next..)?),
    _ => return None,
  };
  Some((Record { action, name }, rest))
}

fn field(bytes: &[u8], at: usize) -> Option<u32> {
  let field = bytes.get(at..)?.first_chunk()?;
  Some(u32::from_ne_bytes(*field))
}

// ---- From one completed read to hints ----

/// Turns what the backend handed over into hints.
///
/// A record goes to the deepest key whose directory holds its path, with the path below that
/// directory. So a key gets its records from whatever handle serves it, also from one that
/// was closed a moment ago.
pub fn translate(handles: &mut Handles, raw: Raw, hint: &mut dyn FnMut(Hint<'_>)) {
  // The engine checks the directory, asks for it again and scans everything below it.
  let lost = |key| Hint::Changed { dir: key, rel: Path::new("") };
  let (watch, dir, result) = match raw {
    Raw::Read { watch, dir, result } => (watch, dir, result),
    Raw::Moved(keys) => return keys.into_iter().for_each(|key| hint(lost(key))),
  };
  let Ok(bytes) = result else {
    // The end of a handle that was replaced or closed before is no news for the engine.
    return handles.closed(watch).into_iter().for_each(|key| hint(lost(key)));
  };
  if bytes.is_empty() {
    // The buffer of this handle was too small: records were lost at or below its directory.
    return handles.below(&dir).into_iter().for_each(|key| hint(lost(key)));
  }
  let mut name = Vec::new();
  for record in Records::new(&bytes) {
    let Ok(record) = record else {
      // The records after this one cannot be found.
      hint(Hint::Lost);
      return;
    };
    name.clear();
    name.extend(record.name());
    if name.is_empty() {
      // Some Windows versions send this when the directory of the handle is deleted.
      handles.below(&dir).into_iter().for_each(|key| hint(lost(key)));
    } else if let Some(parent) = before_short_name(&name) {
      let parent = dir.join(os_string(parent));
      let mut found = false;
      handles.route(&parent, &mut |key, rel| {
        found = true;
        hint(Hint::Changed { dir: key, rel });
      });
      if !found {
        // No key is at or above the directory that holds the short name, and the short name
        // can be a directory on the way to any key below.
        handles.below(&parent).into_iter().for_each(|key| hint(lost(key)));
      }
    } else {
      let written = record.action == FILE_ACTION_MODIFIED;
      handles.route(&dir.join(os_string(&name)), &mut |key, rel| {
        match (written, rel.as_os_str().is_empty()) {
          // Windows sends this record for a directory when an entry in it changes. That is
          // noise, and here the directory is the one of the key itself.
          (true, true) => {}
          // For a directory below the key it is the same noise, and `Written` for a
          // directory means nothing to the engine.
          (true, false) => hint(Hint::Written { dir: key, rel }),
          // With an empty `rel`: the directory of the key itself came, went or was replaced.
          (false, _) => hint(Hint::Changed { dir: key, rel }),
        }
      });
    }
  }
}

/// The path before the first part of `name` that looks like a short (8.3) name.
///
/// Windows reports a change under the short name when a program opened the file by it. A stat of
/// that name works, so the engine would take it for a new file and miss the change of the file
/// it knows. A listing shows only long names, so the hint names the directory that holds the
/// name.
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

/// `PROGRA~1` or `LONGFI~1.TXT`: at most eight units that end with `~` and digits, then at most
/// three more after one dot.
fn looks_short(part: &[u16]) -> bool {
  let dot = part.iter().position(|&unit| unit == DOT).unwrap_or(part.len());
  let (base, extension) = part.split_at(dot);
  let digits = base.iter().rev().take_while(|unit| DIGITS.contains(unit)).count();
  let tilde = base.len().checked_sub(digits + 1).filter(|&at| at > 0 && base[at] == TILDE);
  digits > 0
    && tilde.is_some()
    && base.len() <= 8
    && extension.len() <= 4
    && !extension[extension.len().min(1)..].contains(&DOT)
}

fn os_string(name: &[u16]) -> OsString {
  std::os::windows::ffi::OsStringExt::from_wide(name)
}

// ---- The port and its thread ----

/// The thread that owns the completion port and every directory handle. Every call to Windows
/// is in here. The thread only copies the bytes of each completed read into the sink.
///
/// The watcher thread sends a command and then posts a packet without an `OVERLAPPED` to wake
/// the worker. Every other packet is a completed read, and its completion key is the id of the
/// watch.
///
/// The kernel keeps the addresses of the buffer and of the `OVERLAPPED` of a read until the read
/// completes. So these two blocks are only freed when no read is pending: after the packet of the
/// read was taken from the port, or when the call that should have queued the read failed. If
/// neither happens they are leaked: the `Drop` of `Watch` does that.
mod port {
  use std::{
    fs::{File, OpenOptions},
    io, mem,
    os::windows::{
      fs::OpenOptionsExt,
      io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr,
    sync::{Arc, mpsc},
    thread,
  };

  use rustc_hash::FxHashMap;
  use windows_sys::Win32::{
    Foundation::{ERROR_NOTIFY_ENUM_DIR, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{
      FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
      FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
      FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ,
      FILE_SHARE_WRITE, ReadDirectoryChangesW,
    },
    System::{
      IO::{
        CancelIoEx, CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED,
        PostQueuedCompletionStatus,
      },
      Threading::INFINITE,
    },
  };

  use super::{Raw, WatchId};
  use crate::backend::Sink;

  /// No `LAST_ACCESS`: the stat and listing calls of the engine must not make records. No
  /// `ATTRIBUTES` and no `SECURITY`: tools that sync or scan files change them all the time.
  const FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_SIZE
    | FILE_NOTIFY_CHANGE_LAST_WRITE
    | FILE_NOTIFY_CHANGE_CREATION;

  /// The largest size that a network share accepts.
  const BUFFER_BYTES: u32 = 64 * 1024;
  /// The buffer is made of `u32` words because the call refuses one that is not aligned to four
  /// bytes.
  const BUFFER_WORDS: usize = BUFFER_BYTES as usize / size_of::<u32>();

  /// The docs say that a read also comes back with no bytes when the kernel cannot get memory for
  /// its own buffer. Then every read does so at once and the worker would spin. After this many in
  /// a row the handle is closed, and the engine opens a new one at its own pace.
  const MAX_EMPTY_READS: u8 = 16;

  /// How long the worker waits for the last packet of a closed handle when it stops.
  const STOP_WAIT_MS: u32 = 2000;

  /// The end that the watcher thread holds: it sends commands to the worker and wakes it.
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
        move || Worker { iocp, commands: receiver, sink, watches: FxHashMap::default() }.run()
      })?;
      Ok(Self { iocp, commands, worker: Some(worker) })
    }

    /// Opens a handle on `dir`. When this returns `Ok`, the first read is queued: every change from
    /// now on is reported.
    pub(super) fn open(&self, id: WatchId, dir: &Path) -> io::Result<()> {
      let (reply, answer) = mpsc::sync_channel(1);
      self.send(Command::Open { id, dir: dir.to_owned(), reply }).inspect_err(|_| {
        // The worker was not woken, but it may find the command at a later wake-up. Then it must
        // not keep a handle that the engine knows nothing about.
        let _ = self.commands.send(Command::Close { id });
      })?;
      match answer.recv() {
        Ok(result) => result,
        Err(mpsc::RecvError) => Err(stopped()),
      }
    }

    pub(super) fn close(&self, id: WatchId) {
      // On an error the worker is gone and has closed every handle, or it finds the command at its
      // next wake-up.
      let _ = self.send(Command::Close { id });
    }

    fn send(&self, command: Command) -> io::Result<()> {
      self.commands.send(command).map_err(|_| stopped())?;
      self.iocp.wake()
    }
  }

  impl Drop for Port {
    fn drop(&mut self) {
      // The worker ends when it is woken and reads `Stop`, or when it has ended before. If neither
      // is true, a join would wait forever.
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

  struct Worker {
    iocp: Arc<Iocp>,
    commands: mpsc::Receiver<Command>,
    sink: Sink<Raw>,
    watches: FxHashMap<WatchId, Watch>,
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
          Packet::Completed { id, result } => self.completed(id, result),
          Packet::Nothing(error) => {
            self.sink.failed(error);
            break;
          }
        }
      }
      self.stop();
    }

    /// False when the worker has to stop.
    fn run_commands(&mut self) -> bool {
      loop {
        match self.commands.try_recv() {
          Ok(Command::Open { id, dir, reply }) => {
            let _ = reply.send(self.open(id, dir));
          }
          Ok(Command::Close { id }) => self.close(id),
          Ok(Command::Stop) | Err(mpsc::TryRecvError::Disconnected) => return false,
          Err(mpsc::TryRecvError::Empty) => return true,
        }
      }
    }

    fn open(&mut self, id: WatchId, path: PathBuf) -> io::Result<()> {
      let dir = open_dir(&path)?;
      self.iocp.attach(&dir, id)?;
      let mut watch = Watch::new(dir, path);
      watch.read()?;
      self.watches.insert(id, watch);
      Ok(())
    }

    fn close(&mut self, id: WatchId) {
      let Some(watch) = self.watches.get_mut(&id) else { return };
      watch.close();
      if !watch.pending {
        self.watches.remove(&id);
      }
    }

    fn completed(&mut self, id: WatchId, result: io::Result<u32>) {
      let Some(watch) = self.watches.get_mut(&id) else { return };
      watch.pending = false;
      // This test comes first: the read of a handle that we closed also ends with no bytes.
      if watch.dir.is_none() {
        // Records in the packet were written before the close. They can be the only notice of a
        // change that was made while the handle that replaces this one was being opened.
        if let Ok(len @ 1..) = result {
          let result = Ok(watch.records(len));
          self.sink.raw(Raw::Read { watch: id, dir: watch.path.clone(), result });
        }
        self.watches.remove(&id);
        return;
      }
      let records = match result {
        Ok(len) => watch.records(len),
        // A completion port reports dropped records as a read with no bytes. Other ways to wait,
        // and Wine, report this code.
        Err(error) if error.raw_os_error() == Some(ERROR_NOTIFY_ENUM_DIR.cast_signed()) => {
          Vec::new()
        }
        Err(error) => {
          self.died(id, error);
          return;
        }
      };
      watch.empty_reads = if records.is_empty() { watch.empty_reads + 1 } else { 0 };
      self.sink.raw(Raw::Read { watch: id, dir: watch.path.clone(), result: Ok(records) });
      let next = if watch.empty_reads < MAX_EMPTY_READS {
        watch.read()
      } else {
        Err(io::Error::other("every read of the handle comes back with no bytes"))
      };
      if let Err(error) = next {
        self.died(id, error);
      }
    }

    /// No read is pending on the watch and none can be queued: close it and tell the engine.
    fn died(&mut self, id: WatchId, error: io::Error) {
      if let Some(watch) = self.watches.remove(&id) {
        self.sink.raw(Raw::Read { watch: id, dir: watch.path.clone(), result: Err(error) });
      }
    }

    /// Closes every handle and waits for the packets of the reads that were pending.
    fn stop(mut self) {
      for watch in self.watches.values_mut() {
        watch.close();
      }
      self.watches.retain(|_, watch| watch.pending);
      while !self.watches.is_empty() {
        match self.iocp.wait(STOP_WAIT_MS) {
          Packet::Completed { id, .. } => {
            if let Some(mut watch) = self.watches.remove(&id) {
              watch.pending = false;
            }
          }
          Packet::Wake => {}
          // A read that never completes: its watch stays pending and leaks its blocks.
          Packet::Nothing(_) => break,
        }
      }
    }
  }

  /// One handle, with the memory that its reads use.
  struct Watch {
    /// `None` when the engine asked to close it. The watch then only waits for the packet of its
    /// last read.
    dir: Option<File>,
    /// The path that the handle was opened on. It goes out with every read.
    path: PathBuf,
    /// Blocks of their own on the heap, so that they keep their addresses while the kernel has
    /// them.
    overlapped: Box<OVERLAPPED>,
    buffer: Box<[u32]>,
    /// A read is queued and its packet was not taken from the port yet.
    pending: bool,
    empty_reads: u8,
  }

  impl Watch {
    fn new(dir: File, path: PathBuf) -> Self {
      Self {
        dir: Some(dir),
        path,
        overlapped: Box::default(),
        buffer: vec![0; BUFFER_WORDS].into_boxed_slice(),
        pending: false,
        empty_reads: 0,
      }
    }

    /// Queues the next read. Only called when no read is pending. After an error none is pending.
    fn read(&mut self) -> io::Result<()> {
      let Some(dir) = &self.dir else { return Err(io::Error::other("the handle is closed")) };
      *self.overlapped = OVERLAPPED::default();
      // SAFETY: the handle is open. The kernel writes to `buffer`, which is `BUFFER_BYTES` long,
      // and to `overlapped` until the read completes. Both are heap blocks that do not move, and
      // they are freed only when `pending` is false again (see `Drop`). No read is pending now, so
      // the kernel does not use the blocks for another read: `Worker::open` calls this for a new
      // watch, and `Worker::completed` after it took the packet of the last read.
      let queued = unsafe {
        ReadDirectoryChangesW(
          dir.as_raw_handle(),
          self.buffer.as_mut_ptr().cast(),
          BUFFER_BYTES,
          i32::from(true),
          FILTER,
          ptr::null_mut(),
          &raw mut *self.overlapped,
          None,
        )
      };
      if queued == 0 {
        return Err(io::Error::last_os_error());
      }
      self.pending = true;
      Ok(())
    }

    /// The first `len` bytes of the buffer. Only called when no read is pending.
    fn records(&self, len: u32) -> Vec<u8> {
      self.buffer.iter().flat_map(|word| word.to_ne_bytes()).take(len as usize).collect()
    }

    /// Closes the handle. A pending read then completes, and its packet still comes to the port.
    fn close(&mut self) {
      let Some(dir) = self.dir.take() else { return };
      if self.pending {
        // SAFETY: the handle is open, and `overlapped` is the block of the read that is pending on
        // it. The call only reads the address.
        unsafe { CancelIoEx(dir.as_raw_handle(), &raw const *self.overlapped) };
      }
    }
  }

  impl Drop for Watch {
    fn drop(&mut self) {
      if self.pending {
        // The kernel may still write to both blocks. They are never freed.
        mem::forget(mem::take(&mut self.buffer));
        mem::forget(mem::take(&mut self.overlapped));
      }
    }
  }

  /// The completion port. Both threads call it at the same time: the kernel orders the calls.
  #[derive(Debug)]
  struct Iocp(OwnedHandle);

  impl Iocp {
    fn new() -> io::Result<Self> {
      // SAFETY: the call reads no memory of ours. With these arguments it makes a new port.
      let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, ptr::null_mut(), 0, 1) };
      if port.is_null() {
        return Err(io::Error::last_os_error());
      }
      // SAFETY: the port is a new open handle that nothing else owns, and `CloseHandle` is the way
      // to close it.
      Ok(Self(unsafe { OwnedHandle::from_raw_handle(port) }))
    }

    /// From now on every read that completes on `dir` puts a packet with `id` into the port.
    fn attach(&self, dir: &File, id: WatchId) -> io::Result<()> {
      // SAFETY: both handles are open, and the call reads no memory of ours.
      let port =
        unsafe { CreateIoCompletionPort(dir.as_raw_handle(), self.0.as_raw_handle(), id.0, 0) };
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
      // The `OVERLAPPED` is only compared with null and never read: the id finds the watch.
      match (overlapped.is_null(), result) {
        (false, result) => Packet::Completed { id: WatchId(key), result },
        (true, Ok(_)) => Packet::Wake,
        (true, Err(error)) => Packet::Nothing(error),
      }
    }
  }

  enum Packet {
    /// Posted by [`Iocp::wake`].
    Wake,
    /// A read ended: with this many bytes in the buffer, or with an error.
    Completed { id: WatchId, result: io::Result<u32> },
    /// No packet: the wait timed out or the port is broken.
    Nothing(io::Error),
  }

  /// Opens the directory the way std opens every path, so that this handle and the checks of the
  /// engine are on the same directory.
  fn open_dir(path: &Path) -> io::Result<File> {
    let dir = OpenOptions::new()
      .access_mode(FILE_LIST_DIRECTORY)
      // Other programs may still rename, delete and write what we watch.
      .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
      // `BACKUP_SEMANTICS`: without it a directory cannot be opened at all.
      .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED)
      .open(path)?;
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
      "toolongna~1.txt",
      "backup~",
      "~$doc.docx",
    ] {
      assert!(!looks_short(name), "{name}");
    }
  }
}
