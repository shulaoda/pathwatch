//! The Windows backend: `ReadDirectoryChangesW` on at most 64 directory handles. See
//! `references/design.md` 7.3.

use std::{io, mem, path::Path, sync::Arc, time::SystemTime};

#[cfg(feature = "test-hooks")]
pub use self::{
  handles::MAX_HANDLES,
  place::{Place, place},
};
pub use self::{
  handles::{Handles, System, Told, WatchId},
  records::{Malformed, Record, Records},
};
use self::{port::Port, records::FILE_ACTION_MODIFIED};
use crate::backend::{Backend, Committed, Coverage, DirKey, Hint, Sink, Want};

#[derive(Debug)]
pub struct WindowsBackend {
  handles: Handles,
  port: Port,
  sink: Sink<Raw>,
  covered: Vec<DirKey>,
}

/// What the port sends: the bytes of one read on a handle, the keys that lost their handle,
/// and the answer to `ask_delivered`.
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
  /// Every record of a change before this time, by the clock of the system, came before: the
  /// answer to `ask_delivered`.
  Delivered(SystemTime),
}

impl WindowsBackend {
  pub fn new(sink: Sink<Raw>) -> io::Result<Self> {
    let port = Port::new(sink.clone())?;
    Ok(Self { handles: Handles::default(), port, sink, covered: Vec::new() })
  }

  #[cfg(feature = "test-hooks")]
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
      Raw::Delivered(_) => 0,
    }
  }

  fn ask_delivered(&mut self, after: SystemTime) {
    self.port.ask_delivered(after);
  }
}

/// The hints of one message of the port.
pub fn translate(handles: &mut Handles, raw: Raw, hint: &mut dyn FnMut(Hint<'_>)) {
  let lost = |key| Hint::Changed { dir: key, rel: Path::new("") };
  let (watch, dir, extended, result) = match raw {
    Raw::Read { watch, dir, extended, result } => (watch, dir, extended, result),
    Raw::Lost(keys) => return keys.into_iter().for_each(|key| hint(lost(key))),
    Raw::Delivered(before) => return hint(Hint::Delivered { before }),
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
  let mut route = handles.route_below(&dir);
  // A record with no name is a loss for every key below the handle: once a read is enough.
  let mut told_lost = false;
  for record in records {
    let Ok(record) = record else {
      hint(Hint::Lost);
      return;
    };
    let written = record.action == FILE_ACTION_MODIFIED;
    if record.is_unnamed() {
      if !mem::replace(&mut told_lost, true) {
        handles.below(&dir).into_iter().for_each(|key| hint(lost(key)));
      }
      continue;
    }
    if written && record.is_dir == Some(true) || !route.take(record.name()) {
      continue;
    }
    // Windows says "modified" for a directory when an entry in it changes, and that entry has
    // a record of its own.
    if written && route.is_at_a_key() {
      continue;
    }
    route.keys(&mut |key, rel| {
      hint(if written {
        Hint::Written { dir: key, rel, new: false }
      } else {
        Hint::Changed { dir: key, rel }
      });
    });
  }
}

mod place {
  use std::path::{Path, PathBuf};

  use crate::backend::{DirKey, Want, may_watch_a_tree};

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
      wanted.iter().copied().filter(|&(_, dir, _)| may_watch_a_tree(dir)).collect();
    wanted.sort_by(|a, b| a.1.cmp(b.1));

    let mut tops: Vec<&Path> = Vec::new();
    for &(_, dir, want) in &wanted {
      let below_a_top = tops.last().is_some_and(|top| dir.starts_with(top));
      if want == Want::Contents && !below_a_top {
        tops.push(dir);
      }
    }

    let shared: Vec<usize> = tops.windows(2).map(|pair| common_parts(pair[0], pair[1])).collect();
    // The tops that share a handle: each top names the first top of its group.
    let mut group: Vec<usize> = (0..tops.len()).collect();
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
        if shared[at..end].contains(&depth)
          && let common = first_parts(tops[at], depth)
          && may_watch_a_tree(&common)
        {
          // A directory with a link between the common directory and itself cannot be served
          // from there; the others join, also across it. The tops of a group have no link
          // below their common directory, so a link above it holds back all of them.
          let mut first = None;
          for top in at..=end {
            if link_between(&common, tops[top], &mut *is_link) {
              continue;
            }
            let first = *first.get_or_insert(top);
            if group[top] == top && top != first {
              count -= 1;
            }
            group[top] = first;
          }
        }
        at = end;
      }
    }

    // A group shares the common directory of its tops: of the first and the last of them.
    let mut last: Vec<usize> = (0..tops.len()).collect();
    for (top, &first) in group.iter().enumerate() {
      last[first] = top;
    }
    let mut places = Vec::with_capacity(count);
    for (first, &top) in tops.iter().enumerate().filter(|&(top, _)| group[top] == top) {
      let dir = first_parts(top, common_parts(top, tops[last[first]]));
      places.push(Place { dir, keys: Vec::new() });
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

  fn common_parts(a: &Path, b: &Path) -> usize {
    a.components().zip(b.components()).take_while(|(a, b)| a == b).count()
  }

  fn first_parts(path: &Path, count: usize) -> PathBuf {
    path.components().take(count).collect()
  }

  /// Whether a link lies on the way from `top` down to `dir`, `dir` itself included.
  pub(super) fn link_between(top: &Path, dir: &Path, is_link: impl FnMut(&Path) -> bool) -> bool {
    let names = dir.strip_prefix(top).map_or(0, |below| below.components().count());
    dir.ancestors().take(names).any(is_link)
  }
}

mod handles {
  use std::{
    borrow::Cow,
    ffi::OsStr,
    io, mem,
    path::{Component, Path, PathBuf},
  };

  use rustc_hash::{FxHashMap, FxHashSet};

  use super::place::{link_between, place};
  use crate::{
    backend::{Coverage, DirKey, Want, give_room_back, may_watch_a_tree, same, watch_limit},
    fs::wide_name,
  };

  /// At most this many handles are open: more go up to a directory they have in common
  /// (`place`).
  pub const MAX_HANDLES: usize = 64;

  /// One open handle: new for each open, so a read of a handle that closed is told apart.
  #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
  pub struct WatchId(pub usize);

  /// What the handles ask of the system: the port, or a fake one in the tests.
  pub trait System {
    fn open(&mut self, id: WatchId, dir: &Path) -> io::Result<()>;

    fn close(&mut self, id: WatchId);

    fn is_link(&mut self, path: &Path) -> bool;
  }

  /// What a call tells besides its answer: the keys that lost their handle, and the keys that
  /// waited and that a new handle covers.
  #[derive(Debug, Default, PartialEq, Eq)]
  pub struct Told {
    pub lost: Vec<DirKey>,
    pub covered: Vec<DirKey>,
  }

  /// The handles, and the keys that each serves (`references/design.md` 7.3).
  #[derive(Debug, Default)]
  pub struct Handles {
    keys: FxHashMap<DirKey, Served>,
    at: FxHashMap<PathBuf, Vec<DirKey>>,
    open: Vec<Handle>,
    next_id: usize,
    unsettled: bool,
    refused: FxHashSet<DirKey>,
    waiting: FxHashMap<DirKey, PathBuf>,
    /// The keys whose last handle died before it delivered anything. Kept by key, not by the
    /// directory of the handle: a shared handle stands on a directory of no key.
    dead: FxHashMap<DirKey, Death>,
  }

  #[derive(Debug)]
  struct Handle {
    id: WatchId,
    dir: PathBuf,
    /// It delivered records: its death is no death at once.
    delivered: bool,
  }

  #[derive(Debug)]
  struct Served {
    dir: PathBuf,
    want: Want,
    handle: Option<WatchId>,
  }

  /// What the last handle of a key died of, and whether it died before it delivered anything
  /// while the death before it was still on record.
  #[derive(Debug)]
  struct Death {
    again: bool,
    error: io::Error,
  }

  /// The calls of the backend, and what a read on a handle tells.
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

      // The deepest handle at or above the directory: one can stand below another, behind a
      // link.
      let above = self.open.iter().enumerate().filter(|(_, handle)| dir.starts_with(&handle.dir));
      if let Some((at, _)) = above.max_by_key(|(_, handle)| handle.dir.as_os_str().len()) {
        if link_between(&self.open[at].dir, dir, |path| system.is_link(path)) {
          self.drop_key(key, system);
          return (Coverage::NotWatched, told);
        }
        // A `watch` for the very directory of the handle puts a new handle there: the old one
        // may be on a directory that was renamed away. A key that only waited, or got no handle
        // at the last commit, takes the one that the commit just opened.
        if !(waited || refused)
          && self.open[at].dir == dir
          && let Err(error) = self.reopen(at, system)
        {
          self.drop_key(key, system);
          return (Coverage::Failed(error), told);
        }
        self.remember(key, dir, want, Some(self.open[at].id));
        let coverage =
          if want == Want::Contents { Coverage::WatchedTree } else { Coverage::Watched };
        return (coverage, told);
      }
      if want == Want::Names || !may_watch_a_tree(dir) {
        self.waiting.insert(key, dir.to_owned());
        return (Coverage::NotWatched, told);
      }
      // The handles of this key die as soon as they are opened (a server that refuses the
      // request): the engine polls it, and asks again at each poll.
      if self.dead.get(&key).is_some_and(|death| death.again)
        && let Some(death) = self.dead.remove(&key)
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
      self.dead.remove(&key);
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
      let served = self.keys.iter().filter(|(_, served)| served.handle == Some(id));
      let keys: Vec<DirKey> = served.map(|(&key, _)| key).collect();
      for &key in &keys {
        self.forget(key);
        let again = !handle.delivered && self.dead.contains_key(&key);
        self.dead.insert(key, Death { again, error: same(error) });
      }
      keys
    }

    pub fn serves(&self, key: DirKey) -> bool {
      self.keys.get(&key).is_some_and(|served| served.handle.is_some())
    }

    /// The handle delivered records: the deaths of its keys before it are forgotten, at its
    /// first read.
    pub(super) fn delivered(&mut self, id: WatchId) {
      let Some(handle) = self.open.iter_mut().find(|handle| handle.id == id) else { return };
      if mem::replace(&mut handle.delivered, true) || self.dead.is_empty() {
        return;
      }
      for (key, _) in self.keys.iter().filter(|(_, served)| served.handle == Some(id)) {
        self.dead.remove(key);
      }
    }
  }

  /// Where the handles stand: worked out at a commit for every key together (`place`), and the
  /// keys that each serves.
  impl Handles {
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
      let mut looked: FxHashMap<PathBuf, bool> = FxHashMap::default();
      let (places, behind_a_link) = loop {
        let known = looked.values().filter(|&&is| is).count();
        let places = place(&wanted, MAX_HANDLES, &mut |path| looked.get(path) == Some(&true));
        let mut behind = Vec::new();
        for place in &places {
          let id = self.handle_on(&place.dir);
          for &key in &place.keys {
            let served = &self.keys[&key];
            if id.is_some() && id == served.handle {
              continue;
            }
            if link_between(&place.dir, &served.dir, |path| is_link(&mut looked, system, path)) {
              behind.push(key);
            }
          }
        }
        if looked.values().filter(|&&is| is).count() == known {
          break (places, behind);
        }
      };

      let mut new = Vec::new();
      for place in &places {
        if self.handle_on(&place.dir).is_some() {
          continue;
        }
        let id = self.next_id();
        match system.open(id, &place.dir) {
          Ok(()) => new.push(Handle { id, dir: place.dir.clone(), delivered: false }),
          Err(error) => {
            for handle in &new {
              system.close(handle.id);
            }
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
          !link_between(&handle.dir, dir, |path| is_link(&mut looked, system, path))
        });
        told.covered.extend(covered.map(|(&key, _)| key));
      }
      self.open.extend(new);

      for served in self.keys.values_mut() {
        served.handle = None;
      }
      for place in &places {
        let id = self.handle_on(&place.dir);
        for key in &place.keys {
          if let Some(served) = self.keys.get_mut(key) {
            served.handle = id;
          }
        }
      }
      for key in behind_a_link {
        self.forget(key);
        told.lost.push(key);
      }
      Ok(())
    }

    /// The open handle on `dir`.
    fn handle_on(&self, dir: &Path) -> Option<WatchId> {
      self.open.iter().find(|handle| handle.dir == dir).map(|handle| handle.id)
    }

    fn reopen(&mut self, at: usize, system: &mut dyn System) -> io::Result<()> {
      let id = self.next_id();
      system.open(id, &self.open[at].dir)?;
      let old = mem::replace(&mut self.open[at].id, id);
      self.open[at].delivered = false;
      system.close(old);
      for served in self.keys.values_mut() {
        if served.handle == Some(old) {
          served.handle = Some(id);
        }
      }
      Ok(())
    }

    fn next_id(&mut self) -> WatchId {
      self.next_id += 1;
      WatchId(self.next_id - 1)
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
      give_room_back(&mut self.keys);
      give_room_back(&mut self.at);
      Some(gone)
    }
  }

  /// Whether `path` is a junction or a directory link: the system is asked once for each path.
  fn is_link(looked: &mut FxHashMap<PathBuf, bool>, system: &mut dyn System, path: &Path) -> bool {
    *looked.entry(path.to_owned()).or_insert_with(|| system.is_link(path))
  }

  /// `path` with its names in lower case (`lower_name`). The drive or the share stays as it is.
  fn lowered(path: &Path) -> PathBuf {
    let (mut lower, mut text) = (PathBuf::new(), String::new());
    for part in path.components() {
      match part {
        Component::Normal(name) => lower.push(lower_name(name, &mut text)),
        other => lower.push(other.as_os_str()),
      }
    }
    lower
  }

  /// A name in lower case, through `text`: NTFS takes the two cases of a letter for one name,
  /// and folds each character by itself (`ΟΔΟΣ` is `οδοσ`, not `οδος`). A name that is no
  /// Unicode text gets its ASCII letters folded.
  fn lower_name<'a>(name: &'a OsStr, text: &'a mut String) -> Cow<'a, OsStr> {
    let Some(name) = name.to_str() else { return Cow::Owned(name.to_ascii_lowercase()) };
    text.clear();
    text.extend(name.chars().flat_map(char::to_lowercase));
    Cow::Borrowed(OsStr::new(text.as_str()))
  }

  /// The way from the records of a read to the keys.
  impl Handles {
    /// The way to the keys for the records of a read on the handle at `dir`.
    pub(super) fn route_below(&self, dir: &Path) -> Route<'_> {
      let is_separator = |byte: &u8| matches!(byte, b'\\' | b'/');
      let lower_dir = lowered(dir);
      let mut path = dir.as_os_str().as_encoded_bytes().to_vec();
      let mut lower = lower_dir.as_os_str().as_encoded_bytes().to_vec();
      path.push(b'\\');
      lower.push(b'\\');
      // The directories on the way up, each with its spelling in lower case. The names below
      // one start after the separators that follow it in `path`.
      let above = dir.ancestors().zip(lower_dir.ancestors()).filter_map(|(up, lower_up)| {
        let keys = self.at.get(lower_up)?;
        let end = up.as_os_str().len();
        Some((
          keys.as_slice(),
          end + path[end..].iter().take_while(|byte| is_separator(byte)).count(),
        ))
      });
      let above = above.collect();
      let base = (path.len(), lower.len());
      let (ends, text, name) = (Vec::new(), String::new(), String::new());
      Route { at: &self.at, above, path, lower, base, ends, text, name }
    }

    pub(super) fn below(&self, dir: &Path) -> Vec<DirKey> {
      let dir = lowered(dir);
      let below = self.at.iter().filter(|(lower, _)| lower.starts_with(&dir));
      below.flat_map(|(_, keys)| keys).copied().collect()
    }
  }

  /// The way of the records of one read to every key whose directory holds the path of the
  /// record, the deepest first. What the records share is worked out once for the read: the
  /// directory of the handle in lower case, and the keys at and above it. Each record then
  /// costs a look for each of its own names, by its text: 0.53 µs a record in a flood of
  /// 63,000, where a walk up the whole path of each in lower case took 2.1 µs (measured).
  ///
  /// A key above one that was told gets a hint it may not need; left out, it would miss the
  /// path when the tree reaches it only from above: through another spelling of a name, or from
  /// the other letter case of the drive, which is another top node.
  pub(super) struct Route<'a> {
    at: &'a FxHashMap<PathBuf, Vec<DirKey>>,
    /// The keys at and above the directory, the deepest first, each with where the names of a
    /// record below their directory start in `path`.
    above: Vec<(&'a [DirKey], usize)>,
    /// The directory, a separator, and the names of the record with one separator between
    /// them, as encoded text (`OsStr::as_encoded_bytes`); `lower` the same in lower case.
    path: Vec<u8>,
    lower: Vec<u8>,
    /// How long the directory and its separator are in `path` and in `lower`.
    base: (usize, usize),
    /// Where each name of the record ends in `path` and in `lower`.
    ends: Vec<(usize, usize)>,
    /// Room for a name on the way, and for its lower case.
    text: String,
    name: String,
  }

  impl Route<'_> {
    /// Takes the name of a record, from its UTF-16 units. `false`: it is no name of an entry
    /// below the directory. NTFS gives names with separators between them and nothing else; a
    /// disk that another system filled, or a server, could give a root, a drive, a `.` or a
    /// `..`, a NUL, or a `:`, which starts a stream of a file.
    pub(super) fn take(&mut self, units: impl Iterator<Item = u16> + Clone) -> bool {
      let Self { path, lower, base, ends, text, name, .. } = self;
      let given = wide_name(units, text);
      path.truncate(base.0);
      lower.truncate(base.1);
      ends.clear();
      let is_separator = |byte: &u8| matches!(byte, b'\\' | b'/');
      let bytes = given.as_encoded_bytes();
      if bytes.first().is_some_and(is_separator) {
        return false;
      }
      for part in bytes.split(is_separator).filter(|part| !part.is_empty()) {
        if matches!(part, b"." | b"..") || part.iter().any(|&byte| byte == 0 || byte == b':') {
          return false;
        }
        if !ends.is_empty() {
          path.push(b'\\');
          lower.push(b'\\');
        }
        path.extend_from_slice(part);
        // SAFETY: the bytes are a name of `given` between two separators, which are ASCII:
        // encoded text may be cut there (`OsStr::from_encoded_bytes_unchecked`).
        let part = unsafe { OsStr::from_encoded_bytes_unchecked(part) };
        lower.extend_from_slice(lower_name(part, name).as_encoded_bytes());
        ends.push((path.len(), lower.len()));
      }
      !ends.is_empty()
    }

    /// Whether a key stands at the path of the record.
    pub(super) fn is_at_a_key(&self) -> bool {
      self.at.contains_key(cut(&self.lower, 0, self.lower.len()))
    }

    /// Calls `found` with every key whose directory holds the path of the record, the deepest
    /// first, and the path below that directory.
    pub(super) fn keys(&self, found: &mut dyn FnMut(DirKey, &Path)) {
      let len = self.path.len();
      for &(end, lower_end) in self.ends.iter().rev() {
        let Some(keys) = self.at.get(cut(&self.lower, 0, lower_end)) else { continue };
        let rel = cut(&self.path, (end + 1).min(len), len);
        for &key in keys {
          found(key, rel);
        }
      }
      for &(keys, below) in &self.above {
        let rel = cut(&self.path, below, len);
        for &key in keys {
          found(key, rel);
        }
      }
    }
  }

  /// The path in `text[start..end]`. `text` is encoded text with separators between its
  /// pieces (`Route::path`), and each end of the range is an end of it or lies next to a
  /// separator.
  fn cut(text: &[u8], start: usize, end: usize) -> &Path {
    let next_to_ascii =
      |at: usize| at == 0 || at == text.len() || text[at - 1].is_ascii() || text[at].is_ascii();
    assert!(next_to_ascii(start) && next_to_ascii(end), "a cut inside a character");
    // SAFETY: the pieces of `text` came from `OsStr::as_encoded_bytes` with ASCII bytes between
    // them, and it is cut only next to an ASCII byte (`OsStr::from_encoded_bytes_unchecked`).
    Path::new(unsafe { OsStr::from_encoded_bytes_unchecked(&text[start..end]) })
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
    pub fn name(&self) -> impl Iterator<Item = u16> + Clone {
      self.name.as_chunks().0.iter().map(|&unit| u16::from_ne_bytes(unit))
    }

    pub fn is_unnamed(&self) -> bool {
      self.name.is_empty()
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
    time::{Duration, Instant, SystemTime},
  };

  use rustc_hash::FxHashMap;
  use windows_sys::Win32::{
    Foundation::{
      ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED, ERROR_NOTIFY_ENUM_DIR,
      FARPROC, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
    },
    Storage::FileSystem::{
      FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
      FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
      FILE_NOTIFY_CHANGE_SIZE, ReadDirectoryChangesW, ReadDirectoryNotifyExtendedInformation,
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
    fs::{FileType, Fs, SHARE_ALL, StdFs, kernel32},
  };

  // No creation time: a tool that only sets it would give a `Written` hint.
  const FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_SIZE
    | FILE_NOTIFY_CHANGE_LAST_WRITE;

  const BUFFER_BYTES: u32 = 64 * 1024;
  const BUFFER_WORDS: usize = BUFFER_BYTES as usize / size_of::<u64>();

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
          let (watches, waiting, deliver_after) = (FxHashMap::default(), None, None);
          Worker { iocp, commands: receiver, sink, watches, waiting, deliver_after }.run();
        }
      })?;
      Ok(Self { iocp, commands, worker: Some(worker), closing: false })
    }

    fn send(&self, command: Command) -> io::Result<()> {
      self.commands.send(command).map_err(|_| stopped())?;
      self.iocp.wake()
    }

    /// Asks the worker for `Raw::Delivered` at or after `after`. A later question takes the
    /// place of an earlier one.
    pub(super) fn ask_delivered(&self, after: SystemTime) {
      let _ = self.send(Command::Deliver { after });
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
      answer.recv().unwrap_or_else(|mpsc::RecvError| Err(stopped()))
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
    Deliver {
      after: SystemTime,
    },
    Stop,
  }

  /// The handles, each by its id, whose number is the key of its packets on the port:
  /// every open gets a new id, and a read that failed at once sends no packet (`Watch::read`).
  struct Worker {
    iocp: Arc<Iocp>,
    commands: mpsc::Receiver<Command>,
    sink: Sink<Raw>,
    watches: FxHashMap<WatchId, Watch>,
    /// The reply that waits for the closes.
    waiting: Option<mpsc::SyncSender<()>>,
    /// The time the engine asked about (`Backend::ask_delivered`): then every handle is read
    /// until nothing is left, and `Raw::Delivered` says so.
    deliver_after: Option<SystemTime>,
  }

  impl Worker {
    fn run(mut self) {
      loop {
        if self.deliver_after.is_some_and(|after| after <= SystemTime::now()) {
          if !self.read_empty() {
            break;
          }
          continue;
        }
        let packet = match self.next_rest() {
          // The time of a question is waited for on the port, not by a sleep, which would leave
          // the records of the time between alone. Its wait counts in ticks of the clock (15.6
          // ms): a late answer only keeps the doubt of a young file a little longer.
          None => match self.deliver_after {
            None => self.iocp.wait(INFINITE),
            Some(after) => {
              let wait = after.duration_since(SystemTime::now()).unwrap_or(Duration::ZERO);
              self.iocp.wait(u32::try_from(wait.as_millis()).unwrap_or(u32::MAX).max(1))
            }
          },
          Some(at) => {
            let now = Instant::now();
            if at <= now {
              self.rearm(Some(now));
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
          Packet::Completed { id, result } => self.completed(id, result, false),
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

    /// Reads every handle until nothing is left, then sends the time the last look at the port
    /// began: a queued read completes as soon as one record is in it, its packet is queued
    /// before the call that made the change returns, and a resting handle keeps its records for
    /// the next read, so every record of a change before that time is sent (design.md 7.3).
    /// `false`: the worker stops.
    fn read_empty(&mut self) -> bool {
      self.rearm(None);
      loop {
        let began = SystemTime::now();
        match self.iocp.wait(0) {
          Packet::Timeout => {
            self.sink.raw(Raw::Delivered(began));
            if self.deliver_after.is_some_and(|after| after <= began) {
              self.deliver_after = None;
            }
            return true;
          }
          Packet::Completed { id, result } => self.completed(id, result, true),
          Packet::Wake => {
            if !self.run_commands() {
              return false;
            }
          }
          Packet::Nothing(error) => {
            self.sink.failed(error);
            return false;
          }
        }
      }
    }

    /// Queues the next read of every handle whose rest is over at `now`, or of every resting one.
    fn rearm(&mut self, now: Option<Instant>) {
      let over = |watch: &Watch| watch.rest_until.is_some_and(|at| now.is_none_or(|now| at <= now));
      let due: Vec<WatchId> =
        self.watches.iter().filter(|(_, w)| over(w)).map(|(&id, _)| id).collect();
      for id in due {
        let Some(watch) = self.watches.get_mut(&id) else { continue };
        watch.rest_until = None;
        if let Err(error) = watch.read() {
          self.died(id, error);
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
          Ok(Command::Deliver { after }) => {
            self.deliver_after = Some(self.deliver_after.map_or(after, |at| at.max(after)));
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
        self.iocp.attach(&dir, id)?;
        let mut watch = Watch::new(dir, Arc::clone(path), extended);
        match watch.read() {
          Ok(()) => {
            self.watches.insert(id, watch);
            return Ok(());
          }
          // The disk does not know the longer records: the plain read goes on a new handle.
          Err(error) if extended && not_supported(&error) => extended = false,
          Err(error) => return Err(error),
        }
      }
    }

    fn close(&mut self, id: WatchId) {
      let Some(watch) = self.watches.get_mut(&id) else { return };
      // A resting handle keeps its records in the kernel: the read after the close takes them.
      let resting = watch.rest_until.take().is_some();
      watch.closing =
        if resting && watch.read().is_ok() { Closing::LastRead } else { Closing::Asked };
      if watch.pending {
        watch.cancel();
      } else {
        self.watches.remove(&id);
      }
    }

    /// The reply that waits for the closes gets it once no handle is closing.
    fn answer_closed(&mut self) {
      if self.watches.values().any(|watch| watch.closing != Closing::No) {
        return;
      }
      if let Some(reply) = self.waiting.take() {
        let _ = reply.send(());
      }
    }

    /// `draining`: the handles are read until nothing is left, so none rests.
    fn completed(&mut self, id: WatchId, result: io::Result<u32>, draining: bool) {
      let Some(watch) = self.watches.get_mut(&id) else { return };
      watch.pending = false;
      let result = match result {
        Ok(len) => Ok(watch.records(len)),
        // The kernel had no room for the records: a loss, as a read with no bytes.
        Err(error) if error.raw_os_error() == Some(ERROR_NOTIFY_ENUM_DIR.cast_signed()) => {
          Ok(Vec::new())
        }
        Err(error) => Err(error),
      };
      if watch.closing != Closing::No {
        // The kernel keeps the records of a handle between two reads and drops them with the
        // handle: a read queued before the close may have left some there. One read queued
        // after it takes them, and the cancel ends it at once when nothing is left. What comes
        // later, the handle that took over reports: it was opened before the close. Reading
        // on until nothing is left would not end in a flood.
        if let Ok(records) = result {
          self.sink.raw(watch.raw(id, Ok(records)));
          let before = mem::replace(&mut watch.closing, Closing::LastRead);
          if before == Closing::Asked && watch.read().is_ok() {
            watch.cancel();
            return;
          }
        }
        self.watches.remove(&id);
        self.answer_closed();
        return;
      }
      let records = match result {
        Ok(records) => records,
        Err(error) => {
          // The read is over: the handle can take the next one.
          if watch.extended && not_supported(&error) {
            watch.extended = false;
            if watch.read().is_ok() {
              return;
            }
          }
          self.died(id, error);
          return;
        }
      };
      watch.empty_reads = if records.is_empty() { watch.empty_reads + 1 } else { 0 };
      let len = records.len();
      self.sink.raw(watch.raw(id, Ok(records)));
      if watch.empty_reads >= MAX_EMPTY_READS {
        self.died(id, io::Error::other("every read of the handle comes back with no bytes"));
        return;
      }
      // A read that came back nearly full, or with a loss (no bytes), is queued again at once.
      if len == 0 || len >= BATCH_BYTES || draining {
        if let Err(error) = watch.read() {
          self.died(id, error);
        }
      } else {
        watch.rest_until = Some(Instant::now() + REARM_DELAY);
      }
    }

    fn died(&mut self, id: WatchId, error: io::Error) {
      if let Some(watch) = self.watches.remove(&id) {
        self.sink.raw(watch.raw(id, Err(error)));
      }
    }

    fn stop(mut self) {
      self.watches.values_mut().for_each(Watch::cancel);
      self.watches.retain(|_, watch| watch.pending);
      while !self.watches.is_empty() {
        match self.iocp.wait(STOP_WAIT_MS) {
          Packet::Completed { id, .. } => {
            if let Some(mut watch) = self.watches.remove(&id) {
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
    dir: File,
    path: Arc<Path>,
    overlapped: Box<OVERLAPPED>,
    buffer: Box<[u64]>,
    pending: bool,
    empty_reads: u8,
    extended: bool,
    closing: Closing,
    /// No read is queued until then: the kernel gathers the records of a flood meanwhile.
    rest_until: Option<Instant>,
  }

  /// Whether the handle is given up: then it is read once more, and closed.
  #[derive(Clone, Copy, PartialEq, Eq)]
  enum Closing {
    No,
    /// The read that is queued, if any, was queued before the close.
    Asked,
    /// The read after the close is queued: once it is over, nothing is left that only this
    /// handle reports.
    LastRead,
  }

  impl Watch {
    fn new(dir: File, path: Arc<Path>, extended: bool) -> Self {
      Self {
        dir,
        path,
        overlapped: Box::default(),
        buffer: vec![0; BUFFER_WORDS].into_boxed_slice(),
        pending: false,
        empty_reads: 0,
        extended,
        closing: Closing::No,
        rest_until: None,
      }
    }

    /// Queues one read. When the call fails it started no request: no read is pending, no
    /// packet comes, and the blocks are free.
    fn read(&mut self) -> io::Result<()> {
      *self.overlapped = OVERLAPPED::default();
      let (handle, buffer) = (self.dir.as_raw_handle(), self.buffer.as_mut_ptr().cast());
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

    fn raw(&self, id: WatchId, result: io::Result<Vec<u8>>) -> Raw {
      Raw::Read { watch: id, dir: Arc::clone(&self.path), extended: self.extended, result }
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
      if self.pending {
        // SAFETY: the handle is open, and `overlapped` is the block of its pending read.
        unsafe { CancelIoEx(self.dir.as_raw_handle(), &raw const *self.overlapped) };
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
      match (overlapped.is_null(), result) {
        (false, result) => Packet::Completed { id: WatchId(key), result },
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
      id: WatchId,
      result: io::Result<u32>,
    },
    /// The time given to the wait is over.
    Timeout,
    Nothing(io::Error),
  }

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
      // SAFETY: the function with this name has this signature.
      unsafe {
        mem::transmute::<FARPROC, Option<ReadExtended>>(kernel32(c"ReadDirectoryChangesExW"))
      }
    })
  }

  fn not_supported(error: &io::Error) -> bool {
    let code = error.raw_os_error().map(i32::cast_unsigned);
    matches!(code, Some(ERROR_INVALID_FUNCTION | ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED))
  }

  fn open_dir(path: &Path) -> io::Result<File> {
    let dir = OpenOptions::new()
      .access_mode(FILE_LIST_DIRECTORY)
      .share_mode(SHARE_ALL)
      .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED)
      .open(crate::fs::literal(path))?;
    if dir.metadata()?.is_dir() { Ok(dir) } else { Err(io::ErrorKind::NotADirectory.into()) }
  }

  fn stopped() -> io::Error {
    io::Error::other("the thread of the Windows backend has stopped")
  }
}
