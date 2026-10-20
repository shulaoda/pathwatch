//! The Windows backend: `ReadDirectoryChangesW` on at most 64 directory handles. See
//! `references/design.md` 7.3.

use std::{
  io, mem,
  path::{Path, PathBuf},
  sync::Arc,
};

use self::{handles::lower_case, port::Port, records::FILE_ACTION_MODIFIED};
pub use self::{
  handles::{Handles, MAX_HANDLES, System, Told, WatchId},
  place::{Place, place},
  records::{Malformed, Record, Records},
};
use crate::backend::{Backend, Committed, Coverage, DirKey, Hint, Sink, Want};

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

  /// Keeps what a call of the handles told, and waits for the handles that it closed: a
  /// directory above one of them can be renamed when the call of the backend returns.
  fn settled(&mut self, told: Told) {
    self.covered.extend(told.covered);
    if !told.lost.is_empty() {
      self.sink.raw(Raw::Lost(told.lost));
    }
    self.port.wait_for_closes();
  }
}

impl Backend for WindowsBackend {
  type Raw = Raw;

  fn watch(&mut self, key: DirKey, dir: &Path, want: Want) -> Coverage {
    let (coverage, told) = self.handles.watch(key, dir, want, &mut self.port);
    self.settled(told);
    coverage
  }

  fn unwatch(&mut self, key: DirKey) {
    self.handles.unwatch(key, &mut self.port);
    self.port.wait_for_closes();
  }

  fn commit(&mut self) -> Committed {
    let told = self.handles.commit(&mut self.port);
    self.settled(told);
    Committed { now_watched: mem::take(&mut self.covered), failed: Vec::new() }
  }

  fn translate(&mut self, raw: Raw, hint: &mut dyn FnMut(Hint<'_>)) {
    translate(&mut self.handles, raw, hint);
  }

  fn raw_bytes(raw: &Self::Raw) -> usize {
    match raw {
      Raw::Read { result, .. } => result.as_ref().map_or(0, Vec::len),
      Raw::Lost(keys) => size_of_val(keys.as_slice()),
    }
  }
}

#[derive(Debug)]
pub enum Raw {
  /// What one read on the handle `watch`, which stands on `dir`, gave.
  Read {
    watch: WatchId,
    dir: Arc<Path>,
    extended: bool,
    result: io::Result<Vec<u8>>,
  },
  Lost(Vec<DirKey>),
}

pub fn translate(handles: &mut Handles, raw: Raw, hint: &mut dyn FnMut(Hint<'_>)) {
  let lost = |key| Hint::Changed { dir: key, rel: Path::new("") };
  let (watch, dir, extended, result) = match raw {
    Raw::Read { watch, dir, extended, result } => (watch, dir, extended, result),
    Raw::Lost(keys) => return keys.into_iter().for_each(|key| hint(lost(key))),
  };
  let bytes = match result {
    Ok(bytes) => bytes,
    Err(error) => {
      return handles.closed(watch, &error).into_iter().for_each(|key| hint(lost(key)));
    }
  };
  if bytes.is_empty() {
    return handles.below(&dir).into_iter().for_each(|key| hint(lost(key)));
  }
  handles.delivered(watch);
  let records = if extended { Records::extended(&bytes) } else { Records::new(&bytes) };
  // The path of a record and its lower-case spelling go into the same room each time.
  let (mut path, mut lower, mut text) = (PathBuf::new(), PathBuf::new(), String::new());
  for record in records {
    let Ok(record) = record else {
      hint(Hint::Lost);
      return;
    };
    let written = record.action == FILE_ACTION_MODIFIED;
    if record.is_unnamed() {
      handles.below(&dir).into_iter().for_each(|key| hint(lost(key)));
      continue;
    }
    if written && record.is_dir == Some(true) {
      continue;
    }
    path.clear();
    path.push(&*dir);
    if !record.push_name(&mut path, &mut text) {
      continue;
    }
    lower_case(&path, &mut lower, &mut text);
    // Windows says "modified" for a directory when an entry in it changes, and that entry has
    // a record of its own.
    if written && handles.holds_a_key(&lower) {
      continue;
    }
    handles.route(&path, &lower, &mut |key, rel| {
      hint(if written {
        Hint::Written { dir: key, rel, new: false }
      } else {
        Hint::Changed { dir: key, rel }
      });
    });
  }
}

mod place {
  use std::path::{Component, Path, PathBuf};

  use crate::backend::{DirKey, Want};

  #[derive(Debug, PartialEq, Eq)]
  pub struct Place {
    pub dir: PathBuf,
    pub keys: Vec<DirKey>,
  }

  /// Where the handles go for these keys, at most `max` of them. `is_link`: whether the
  /// directory is a junction or a directory link. A handle reports nothing behind a link below
  /// it, so a directory with a link on the way down from a common directory is not joined
  /// there: it keeps a handle of its own, and the others share without it. Such a handle can
  /// stand below a shared one; a key goes to the deepest handle at or above its directory.
  pub fn place(
    wanted: &[(DirKey, &Path, Want)],
    max: usize,
    is_link: &mut dyn FnMut(&Path) -> bool,
  ) -> Vec<Place> {
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
          // A directory with a link between the common directory and itself cannot be served
          // from there, and neither can the ones it was joined with deeper down.
          let mut blocked: Vec<bool> =
            tops[at..=end].iter().map(|top| leads_through(top, depth, is_link)).collect();
          for pair in at..end {
            if joined[pair] && blocked[pair - at] {
              blocked[pair - at + 1] = true;
            }
          }
          for pair in (at..end).rev() {
            if joined[pair] && blocked[pair - at + 1] {
              blocked[pair - at] = true;
            }
          }
          for pair in at..end {
            if !joined[pair] && !blocked[pair - at] && !blocked[pair - at + 1] {
              joined[pair] = true;
              count -= 1;
            }
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
    // A place can lie below another one: the deepest at or above a directory serves it.
    places.sort_by(|a, b| a.dir.cmp(&b.dir));
    let (mut at, mut above) = (0, Vec::new());
    for &(key, dir, _) in &wanted {
      while places.get(at).is_some_and(|place| place.dir.as_path() <= dir) {
        while above.last().is_some_and(|&top: &usize| !places[at].dir.starts_with(&places[top].dir))
        {
          above.pop();
        }
        above.push(at);
        at += 1;
      }
      while above.last().is_some_and(|&top| !dir.starts_with(&places[top].dir)) {
        above.pop();
      }
      if let Some(&top) = above.last() {
        places[top].keys.push(key);
      }
    }
    places.truncate(max);
    places
  }

  /// Whether a link lies on the way from the directory `depth` parts up to `dir`, `dir` itself
  /// included.
  fn leads_through(dir: &Path, depth: usize, is_link: &mut dyn FnMut(&Path) -> bool) -> bool {
    let below = dir.components().count().saturating_sub(depth);
    dir.ancestors().take(below).any(is_link)
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
    io, mem,
    path::{Component, Path, PathBuf},
  };

  use rustc_hash::{FxHashMap, FxHashSet};

  use super::place::{may_hold_a_handle, place};
  use crate::backend::{Coverage, DirKey, Want, same, watch_limit};

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

  /// What the last handle on a directory died of, and how many died there in a row.
  #[derive(Debug)]
  struct Death {
    count: u8,
    error: io::Error,
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
    /// The directories whose handle died before it delivered anything.
    dead: FxHashMap<PathBuf, Death>,
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
      let waited = self.waiting.remove(&key).is_some();
      let refused = self.refused.remove(&key);
      let want = self.keys.get(&key).map_or(want, |served| served.want.max(want));

      // The deepest handle at or above the directory: one can stand below another, behind a
      // link.
      let above = self.open.iter().enumerate().filter(|(_, handle)| dir.starts_with(&handle.dir));
      if let Some((at, _)) = above.max_by_key(|(_, handle)| handle.dir.as_os_str().len()) {
        let coverage = self.share(at, key, dir, want, !(waited || refused), system);
        if matches!(coverage, Coverage::NotWatched | Coverage::Failed(_)) {
          self.drop_key(key, system);
        }
        return (coverage, told);
      }
      if want == Want::Names || !may_hold_a_handle(dir) {
        self.waiting.insert(key, dir.to_owned());
        return (Coverage::NotWatched, told);
      }
      // The handles on this directory die as soon as they are opened (a server that refuses
      // the request): the engine polls it, and asks again at each poll.
      if self.dead.get(dir).is_some_and(|death| death.count >= 2)
        && let Some(death) = self.dead.remove(dir)
      {
        return (Coverage::Failed(death.error), told);
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
        // The handle that came to serve the key stands above a link: the key was forgotten,
        // and not refused.
        Ok(()) if !self.refused.contains(&key) => Coverage::NotWatched,
        Ok(()) => Coverage::Failed(watch_limit("the limit of open directory handles is reached")),
        Err(error) => Coverage::Failed(error),
      };
      (coverage, told)
    }

    pub fn unwatch(&mut self, key: DirKey, system: &mut dyn System) {
      self.waiting.remove(&key);
      self.refused.remove(&key);
      self.drop_key(key, system);
    }

    pub fn commit(&mut self, system: &mut dyn System) -> Told {
      let mut told = Told::default();
      if self.unsettled {
        let _ = self.settle(system, &mut told);
      }
      told
    }

    /// The handle died: the read on it failed. The keys it served, which are forgotten.
    pub fn closed(&mut self, id: WatchId, error: &io::Error) -> Vec<DirKey> {
      let Some(at) = self.open.iter().position(|handle| handle.id == id) else { return Vec::new() };
      let handle = self.open.swap_remove(at);
      let count = self.dead.get(&handle.dir).map_or(0, |death| death.count);
      self.dead.insert(handle.dir, Death { count: count.saturating_add(1), error: same(error) });
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

    /// The handle delivered records: a death before it is forgotten.
    pub(super) fn delivered(&mut self, id: WatchId) {
      if let Some(handle) = self.open.iter().find(|handle| handle.id == id) {
        self.dead.remove(&handle.dir);
      }
    }

    /// Every key whose directory holds `path`, the deepest first; `lower` is the path in lower
    /// case (`lower_case`). A key above one that was told gets a hint it may not need, which
    /// costs one walk; left out, it would miss the path when the tree reaches it only from
    /// above: through another spelling of a name, or from the other letter case of the drive,
    /// which is another top node.
    pub(super) fn route(&self, path: &Path, lower: &Path, found: &mut dyn FnMut(DirKey, &Path)) {
      for (dir, lower_dir) in path.ancestors().zip(lower.ancestors()) {
        let Some(keys) = self.at.get(lower_dir) else { continue };
        let Ok(rel) = path.strip_prefix(dir) else { continue };
        for &key in keys {
          found(key, rel);
        }
      }
    }

    /// Whether a key stands at the path whose lower case is `lower`.
    pub(super) fn holds_a_key(&self, lower: &Path) -> bool {
      self.at.contains_key(lower)
    }

    pub(super) fn below(&self, dir: &Path) -> Vec<DirKey> {
      let dir = lowered(dir);
      let below = self.at.iter().filter(|(lower, _)| lower.starts_with(&dir));
      below.flat_map(|(_, keys)| keys).copied().collect()
    }

    /// `fresh`: the request is no second ask of a key that waited for a handle, or got none.
    fn share(
      &mut self,
      at: usize,
      key: DirKey,
      dir: &Path,
      want: Want,
      fresh: bool,
      system: &mut dyn System,
    ) -> Coverage {
      if leads_through_a_link(system, &self.open[at].dir, dir) {
        return Coverage::NotWatched;
      }
      // A `watch` for the very directory of the handle puts a new handle there: the old one may
      // be on a directory that was renamed away. A key that only waited, or got no handle at
      // the last commit, takes the one that the commit just opened.
      if fresh
        && self.open[at].dir == dir
        && let Err(error) = self.reopen(at, system)
      {
        return Coverage::Failed(error);
      }
      self.remember(key, dir, want, Some(self.open[at].id));
      if want == Want::Contents { Coverage::WatchedTree } else { Coverage::Watched }
    }

    /// The key is forgotten. A handle that it alone used closes now (rule 6: the OS watch goes
    /// with its last key); where the handles stand is worked out once, at the next commit, not
    /// for each key that goes.
    fn drop_key(&mut self, key: DirKey, system: &mut dyn System) {
      let Some(gone) = self.forget(key) else { return };
      if gone.want == Want::Contents {
        self.unsettled = true;
      }
      if let Some(id) = gone.handle
        && !self.keys.values().any(|served| served.handle == Some(id))
      {
        self.open.retain(|handle| handle.id != id);
        system.close(id);
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
      // The links are found where a key would come to lie behind one, and the placement is
      // made again with them known, until no new one turns up: a directory behind a link then
      // has a handle of its own. A key that still lies behind a link (below a directory that
      // has a handle anyway) is not served.
      let mut links: Vec<PathBuf> = Vec::new();
      let mut looked: FxHashMap<PathBuf, bool> = FxHashMap::default();
      let (places, behind_a_link) = loop {
        let places = place(&wanted, MAX_HANDLES, &mut |path| links.iter().any(|link| link == path));
        let mut behind = Vec::new();
        let mut found_more = false;
        for place in &places {
          let id = self.open.iter().find(|handle| handle.dir == place.dir).map(|handle| handle.id);
          for &key in &place.keys {
            let served = &self.keys[&key];
            if id.is_some() && id == served.handle {
              continue;
            }
            let is_link =
              |path: &Path| *looked.entry(path.to_owned()).or_insert_with(|| system.is_link(path));
            if let Some(link) = link_between(&place.dir, &served.dir, is_link) {
              behind.push(key);
              if !links.contains(&link) {
                links.push(link);
                found_more = true;
              }
            }
          }
        }
        if !found_more {
          break (places, behind);
        }
      };

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
        // A key behind a link below the new handle is not reported by it.
        let covered = waiting.filter(|(_, dir)| {
          let is_link =
            |path: &Path| *looked.entry(path.to_owned()).or_insert_with(|| system.is_link(path));
          link_between(&handle.dir, dir.as_path(), is_link).is_none()
        });
        told.covered.extend(covered.map(|(&key, _)| key));
      }
      self.open.extend(new);

      let mut place_of = FxHashMap::default();
      for place in &places {
        let Some(at) = self.open.iter().position(|handle| handle.dir == place.dir) else {
          continue;
        };
        place_of.extend(place.keys.iter().map(|&key| (key, at)));
      }
      for (key, served) in &mut self.keys {
        served.handle = place_of.get(key).map(|&at| self.open[at].id);
      }
      for key in behind_a_link {
        self.forget(key);
        told.lost.push(key);
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
      self.at.entry(lowered(dir)).or_default().push(key);
      self.keys.insert(key, Served { dir: dir.to_owned(), want, handle });
    }

    fn forget(&mut self, key: DirKey) -> Option<Served> {
      let gone = self.keys.remove(&key)?;
      let lower = lowered(&gone.dir);
      if let Some(keys) = self.at.get_mut(&lower) {
        keys.retain(|&other| other != key);
        if keys.is_empty() {
          self.at.remove(&lower);
        }
      }
      self.shrink();
      Some(gone)
    }

    /// The two tables are given back when most of their keys are gone.
    fn shrink(&mut self) {
      if self.keys.len() < self.keys.capacity() / 4 {
        self.keys.shrink_to_fit();
        self.at.shrink_to_fit();
      }
    }
  }

  fn leads_through_a_link(system: &mut dyn System, top: &Path, dir: &Path) -> bool {
    link_between(top, dir, |path| system.is_link(path)).is_some()
  }

  /// The deepest link on the way from `top` down to `dir`, `dir` itself included.
  fn link_between(
    top: &Path,
    dir: &Path,
    mut is_link: impl FnMut(&Path) -> bool,
  ) -> Option<PathBuf> {
    let names = dir.strip_prefix(top).map_or(0, |below| below.components().count());
    dir.ancestors().take(names).find(|path| is_link(path)).map(Path::to_owned)
  }

  /// `path` with its names in lower case, into `lower`: NTFS takes the two cases of a letter
  /// for one name. `text` is room for one name on the way, so the call asks for no memory
  /// once the two have grown. A name that is no Unicode text gets the ASCII letters folded.
  pub(super) fn lower_case(path: &Path, lower: &mut PathBuf, text: &mut String) {
    lower.clear();
    for part in path.components() {
      match part {
        Component::Normal(name) => match name.to_str() {
          Some(name) => {
            text.clear();
            text.extend(name.chars().flat_map(char::to_lowercase));
            lower.push(&*text);
          }
          None => lower.push(name.to_ascii_lowercase()),
        },
        other => lower.push(other.as_os_str()),
      }
    }
  }

  fn lowered(path: &Path) -> PathBuf {
    let (mut lower, mut text) = (PathBuf::new(), String::new());
    lower_case(path, &mut lower, &mut text);
    lower
  }
}

mod records {
  use std::{
    borrow::Cow,
    ffi::OsString,
    mem::offset_of,
    os::windows::ffi::OsStringExt,
    path::{Component, Path, PathBuf},
  };

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

    pub fn is_unnamed(&self) -> bool {
      self.name.is_empty()
    }

    /// Puts the name below `path`, through `text`: a name is Unicode text unless it holds a
    /// lone surrogate, and only such a one asks for memory. `false`, and nothing is put: the
    /// name is no name of an entry below the directory. NTFS gives names with separators
    /// between them and nothing else; a disk that another system filled, or a server, could
    /// give a `..`, a root, a drive, a NUL, or a `:`, which starts a stream of a file.
    pub fn push_name(&self, path: &mut PathBuf, text: &mut String) -> bool {
      text.clear();
      let decoded = char::decode_utf16(self.name())
        .try_for_each(|unit| unit.map(|character| text.push(character)));
      let name = match decoded {
        Ok(()) => Cow::Borrowed(Path::new(text.as_str())),
        Err(_) => Cow::Owned(OsString::from_wide(&self.name().collect::<Vec<u16>>()).into()),
      };
      let is_entry = |part| match part {
        Component::Normal(name) => {
          !name.as_encoded_bytes().iter().any(|&byte| byte == 0 || byte == b':')
        }
        _ => false,
      };
      let is_below = name.components().all(is_entry);
      if is_below {
        path.push(&*name);
      }
      is_below
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
    path::Path,
    ptr, slice,
    sync::{Arc, OnceLock, mpsc},
    thread,
    time::{Duration, Instant},
  };

  use rustc_hash::FxHashMap;
  use windows_sys::Win32::{
    Foundation::{
      ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED, ERROR_NOTIFY_ENUM_DIR,
      HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
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

  /// The kernel completes a read as soon as one record is in it. So after a read that came
  /// back with less than this, the next read is queued `REARM_DELAY` later: while no read is
  /// queued the kernel keeps the records in a buffer of its own, as big as ours, and the next
  /// read takes them all. Without the rest a flood is one read, one packet and one wake-up of
  /// two threads for each record (93,000 reads for the 63,000 records of 30,000 new files,
  /// measured). A read that came back fuller is queued again at once: the flood is heavy.
  const BATCH_BYTES: usize = BUFFER_BYTES as usize / 2;
  /// The rest before the next read. A flood of 20,000 records a second fills 2 KiB of the
  /// kernel's 64 KiB in it. The first record of a quiet handle is not delayed: the read that
  /// waits for it comes back at once, and the rest comes after it.
  const REARM_DELAY: Duration = Duration::from_millis(1);

  #[derive(Debug)]
  pub(super) struct Port {
    iocp: Arc<Iocp>,
    commands: mpsc::Sender<Command>,
    worker: Option<thread::JoinHandle<()>>,
    /// A close was queued since the last wait.
    closing: bool,
  }

  impl Port {
    pub(super) fn new(sink: Sink<Raw>) -> io::Result<Self> {
      let iocp = Arc::new(Iocp::new()?);
      let (commands, receiver) = mpsc::channel();
      let worker = thread::Builder::new().name("pathwatch-windows".to_owned()).spawn({
        let iocp = Arc::clone(&iocp);
        move || {
          let (watches, waiting) = (FxHashMap::default(), None);
          Worker { iocp, commands: receiver, sink, watches, next_token: 0, waiting }.run();
        }
      })?;
      Ok(Self { iocp, commands, worker: Some(worker), closing: false })
    }

    fn send(&self, command: Command) -> io::Result<()> {
      self.commands.send(command).map_err(|_| stopped())?;
      self.iocp.wake()
    }

    /// Returns when every handle whose close was queued is closed: a directory above one of
    /// them can be renamed then. A worker that stopped answers by dropping the reply.
    pub(super) fn wait_for_closes(&mut self) {
      if !mem::take(&mut self.closing) {
        return;
      }
      let (reply, answer) = mpsc::sync_channel(1);
      if self.send(Command::Closed { reply }).is_ok() {
        let _ = answer.recv();
      }
    }
  }

  impl System for Port {
    fn open(&mut self, id: WatchId, dir: &Path) -> io::Result<()> {
      let (reply, answer) = mpsc::sync_channel(1);
      self.send(Command::Open { id, dir: Arc::from(dir), reply }).inspect_err(|_| {
        let _ = self.commands.send(Command::Close { id });
      })?;
      match answer.recv() {
        Ok(result) => result,
        Err(mpsc::RecvError) => Err(stopped()),
      }
    }

    fn close(&mut self, id: WatchId) {
      self.closing = true;
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
    Open {
      id: WatchId,
      dir: Arc<Path>,
      reply: mpsc::SyncSender<io::Result<()>>,
    },
    Close {
      id: WatchId,
    },
    /// Answered when no handle is closing any more.
    Closed {
      reply: mpsc::SyncSender<()>,
    },
    Stop,
  }

  /// The handles. The port knows each by a token of its own, not by the id of the watch: a
  /// watch whose handle is opened again keeps its id, and a packet names the handle.
  struct Worker {
    iocp: Arc<Iocp>,
    commands: mpsc::Receiver<Command>,
    sink: Sink<Raw>,
    watches: FxHashMap<usize, Watch>,
    next_token: usize,
    /// The reply that waits for the closes.
    waiting: Option<mpsc::SyncSender<()>>,
  }

  impl Worker {
    fn run(mut self) {
      loop {
        let packet = match self.next_rest() {
          None => self.iocp.wait(INFINITE),
          Some(at) => {
            let now = Instant::now();
            if at <= now {
              self.rearm(now);
              continue;
            }
            // The timeout of the port counts in ticks of the clock (15.6 ms): the port is
            // looked at, and the thread sleeps the rest of the rest, which a sleep keeps to
            // half a millisecond.
            match self.iocp.wait(0) {
              Packet::Timeout => {
                thread::sleep(at.saturating_duration_since(now));
                continue;
              }
              packet => packet,
            }
          }
        };
        match packet {
          Packet::Wake => {
            if !self.run_commands() {
              break;
            }
          }
          Packet::Completed { token, result } => self.completed(token, result),
          Packet::Timeout => {}
          Packet::Nothing(error) => {
            self.sink.failed(error);
            break;
          }
        }
      }
      self.stop();
    }

    /// The earliest time at which a resting handle takes its next read.
    fn next_rest(&self) -> Option<Instant> {
      self.watches.values().filter_map(|watch| watch.rest_until).min()
    }

    /// Queues the next read of every handle whose rest is over.
    fn rearm(&mut self, now: Instant) {
      let over = |watch: &Watch| watch.rest_until.is_some_and(|at| at <= now);
      let due: Vec<usize> = self.watches.iter().filter(|(_, w)| over(w)).map(|(&t, _)| t).collect();
      for token in due {
        let Some(watch) = self.watches.get_mut(&token) else { continue };
        watch.rest_until = None;
        if let Err(error) = watch.read() {
          self.died(token, error);
        }
      }
    }

    fn run_commands(&mut self) -> bool {
      loop {
        match self.commands.try_recv() {
          Ok(Command::Open { id, dir, reply }) => {
            let _ = reply.send(self.open(id, &dir));
          }
          Ok(Command::Close { id }) => self.close(id),
          Ok(Command::Closed { reply }) => {
            self.waiting = Some(reply);
            self.answer_closed();
          }
          Ok(Command::Stop) | Err(mpsc::TryRecvError::Disconnected) => return false,
          Err(mpsc::TryRecvError::Empty) => return true,
        }
      }
    }

    fn open(&mut self, id: WatchId, path: &Arc<Path>) -> io::Result<()> {
      let mut extended = read_extended().is_some();
      loop {
        let dir = open_dir(path)?;
        let token = self.next_token;
        self.next_token += 1;
        self.iocp.attach(&dir, token)?;
        let mut watch = Watch::new(id, dir, Arc::clone(path), extended);
        match watch.read() {
          Ok(()) => {
            self.watches.insert(token, watch);
            return Ok(());
          }
          // The disk does not know the longer records: the plain read goes on a new handle.
          Err(error) if extended && not_supported(&error) => extended = false,
          Err(error) => return Err(error),
        }
      }
    }

    fn close(&mut self, id: WatchId) {
      let found = self.watches.iter_mut().find(|(_, watch)| watch.id == id);
      let Some((&token, watch)) = found else { return };
      watch.closing = true;
      if watch.pending {
        watch.cancel();
      } else {
        self.watches.remove(&token);
      }
      self.answer_closed();
    }

    /// The reply that waits for the closes gets it once no handle is closing.
    fn answer_closed(&mut self) {
      if self.watches.values().any(|watch| watch.closing) {
        return;
      }
      if let Some(reply) = self.waiting.take() {
        let _ = reply.send(());
      }
    }

    fn completed(&mut self, token: usize, result: io::Result<u32>) {
      let Some(watch) = self.watches.get_mut(&token) else { return };
      watch.pending = false;
      if watch.closing {
        // The kernel keeps the records of a handle between two reads and drops them with the
        // handle: a read that came back whole may have left some there. One more read takes
        // them, and the cancel ends the handle once nothing is left.
        if let Ok(len) = result {
          self.sink.raw(watch.raw(Ok(watch.records(len))));
          if watch.read().is_ok() {
            watch.cancel();
            return;
          }
        }
        self.watches.remove(&token);
        self.answer_closed();
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
      let len = records.len();
      self.sink.raw(watch.raw(Ok(records)));
      if watch.empty_reads >= MAX_EMPTY_READS {
        self.died(token, io::Error::other("every read of the handle comes back with no bytes"));
        return;
      }
      // A read that came back nearly full, or with a loss (no bytes), is queued again at once.
      if len == 0 || len >= BATCH_BYTES {
        if let Err(error) = watch.read() {
          self.died(token, error);
        }
      } else {
        watch.rest_until = Some(Instant::now() + REARM_DELAY);
      }
    }

    fn died(&mut self, token: usize, error: io::Error) {
      if let Some(watch) = self.watches.remove(&token) {
        self.sink.raw(watch.raw(Err(error)));
      }
    }

    fn stop(mut self) {
      for watch in self.watches.values_mut() {
        watch.closing = true;
        watch.cancel();
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
          Packet::Timeout | Packet::Nothing(_) => break,
        }
      }
    }
  }

  struct Watch {
    id: WatchId,
    dir: Option<File>,
    path: Arc<Path>,
    overlapped: Box<OVERLAPPED>,
    buffer: Box<[u64]>,
    pending: bool,
    empty_reads: u8,
    extended: bool,
    /// The handle is given up: it is read until nothing is left, then closed.
    closing: bool,
    /// No read is queued until then: the kernel gathers the records of a flood meanwhile.
    rest_until: Option<Instant>,
  }

  impl Watch {
    fn new(id: WatchId, dir: File, path: Arc<Path>, extended: bool) -> Self {
      Self {
        id,
        dir: Some(dir),
        path,
        overlapped: Box::default(),
        buffer: vec![0; BUFFER_WORDS].into_boxed_slice(),
        pending: false,
        empty_reads: 0,
        extended,
        closing: false,
        rest_until: None,
      }
    }

    /// Queues one read. When the call fails it started no request: no read is pending, no
    /// packet comes, and the blocks are free.
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
      Raw::Read { watch: self.id, dir: Arc::clone(&self.path), extended: self.extended, result }
    }

    /// The first `len` bytes of the buffer.
    fn records(&self, len: u32) -> Vec<u8> {
      let len = (len as usize).min(BUFFER_BYTES as usize);
      // SAFETY: the buffer is `BUFFER_BYTES` long, and the bytes of its words are plain bytes.
      unsafe { slice::from_raw_parts(self.buffer.as_ptr().cast::<u8>(), len) }.to_vec()
    }

    /// Cancels the pending read. It fails when the read is over already: then its packet is
    /// on its way, and the handle stays until it is taken.
    fn cancel(&mut self) {
      if self.pending
        && let Some(dir) = &self.dir
      {
        // SAFETY: the handle is open, and `overlapped` is the block of its pending read.
        unsafe { CancelIoEx(dir.as_raw_handle(), &raw const *self.overlapped) };
      }
    }
  }

  impl Drop for Watch {
    fn drop(&mut self) {
      // The kernel may still write into the blocks of a pending read: they are left to it.
      if self.pending {
        Box::leak(mem::take(&mut self.buffer));
        Box::leak(mem::take(&mut self.overlapped));
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
        (true, Err(error)) if error.raw_os_error() == Some(WAIT_TIMEOUT.cast_signed()) => {
          Packet::Timeout
        }
        (true, Err(error)) => Packet::Nothing(error),
      }
    }
  }

  enum Packet {
    Wake,
    Completed {
      token: usize,
      result: io::Result<u32>,
    },
    /// The time given to the wait is over.
    Timeout,
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
