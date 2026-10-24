use std::{io, mem, path::Path, sync::Arc, time::SystemTime};

#[cfg(feature = "test-hooks")]
pub use self::{
  handles::MAX_HANDLES,
  place::{Place, place},
};
pub use self::{
  handles::{Handles, Report, System, WatchId},
  records::{Malformed, Record, Records},
};
use self::{port::Port, records::FILE_ACTION_MODIFIED};
use crate::backend::{Backend, Committed, Coverage, Hint, NodeKey, Sink, Want};

#[derive(Debug)]
pub struct WindowsBackend {
  handles: Handles,
  port: Port,
  sink: Sink<Raw>,
  covered: Vec<NodeKey>,
}

#[derive(Debug)]
pub enum Raw {
  Read { watch: WatchId, dir: Arc<Path>, extended: bool, result: io::Result<Vec<u8>> },
  Lost(Vec<NodeKey>),
  Delivered(SystemTime),
}

impl WindowsBackend {
  pub fn new(sink: Sink<Raw>) -> io::Result<Self> {
    let port = Port::new(sink.clone())?;
    Ok(Self { handles: Handles::default(), port, sink, covered: Vec::new() })
  }

  #[cfg(feature = "test-hooks")]
  pub fn serves(&self, key: NodeKey) -> bool {
    self.handles.serves(key)
  }

  fn finish_call(&mut self, report: Report) {
    self.covered.extend(report.covered);
    if !report.lost.is_empty() {
      self.sink.raw(Raw::Lost(report.lost));
    }
    self.port.wait_for_closes();
  }
}

impl Backend for WindowsBackend {
  type Raw = Raw;

  fn watch(&mut self, key: NodeKey, dir: &Path, want: Want) -> Coverage {
    let (coverage, report) = self.handles.watch(key, dir, want, &mut self.port);
    self.finish_call(report);
    coverage
  }

  fn unwatch(&mut self, key: NodeKey) {
    self.handles.unwatch(key, &mut self.port);
    self.port.wait_for_closes();
  }

  fn commit(&mut self) -> Committed {
    let report = self.handles.commit(&mut self.port);
    self.finish_call(report);
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
  let mut reported_lost = false;
  for record in records {
    let Ok(record) = record else {
      hint(Hint::Lost);
      return;
    };
    let written = record.action == FILE_ACTION_MODIFIED;
    if record.is_unnamed() {
      if !mem::replace(&mut reported_lost, true) {
        handles.below(&dir).into_iter().for_each(|key| hint(lost(key)));
      }
      continue;
    }
    if written && record.is_dir == Some(true) || !route.follow(record.name()) {
      continue;
    }
    if written && route.is_at_a_key() {
      continue;
    }
    route.each_key(&mut |key, rel| {
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

  use crate::backend::{NodeKey, Want, may_watch_a_tree};

  #[derive(Debug, PartialEq, Eq)]
  pub struct Place {
    pub dir: PathBuf,
    pub keys: Vec<NodeKey>,
  }

  pub fn place(
    wanted: &[(NodeKey, &Path, Want)],
    max: usize,
    is_link: &mut dyn FnMut(&Path) -> bool,
  ) -> Vec<Place> {
    let mut wanted = wanted.to_vec();
    wanted.sort_by(|a, b| a.1.cmp(b.1));
    let dirs = merged(&tops(&wanted), max, is_link);
    let mut places: Vec<Place> =
      dirs.into_iter().map(|dir| Place { dir, keys: Vec::new() }).collect();
    serve(&mut places, &wanted);
    places.truncate(max);
    places
  }

  fn tops<'a>(wanted: &[(NodeKey, &'a Path, Want)]) -> Vec<&'a Path> {
    let mut tops: Vec<&Path> = Vec::new();
    for &(_, dir, want) in wanted {
      let below_a_top = tops.last().is_some_and(|top| dir.starts_with(top));
      if want == Want::Contents && !below_a_top && may_watch_a_tree(dir) {
        tops.push(dir);
      }
    }
    tops
  }

  fn merged(tops: &[&Path], max: usize, is_link: &mut dyn FnMut(&Path) -> bool) -> Vec<PathBuf> {
    let shared: Vec<usize> = tops.windows(2).map(|pair| common_parts(pair[0], pair[1])).collect();
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

    let mut last: Vec<usize> = (0..tops.len()).collect();
    for (top, &first) in group.iter().enumerate() {
      last[first] = top;
    }
    let mut dirs = Vec::with_capacity(count);
    for (first, &top) in tops.iter().enumerate().filter(|&(top, _)| group[top] == top) {
      dirs.push(first_parts(top, common_parts(top, tops[last[first]])));
    }
    dirs.sort_unstable();
    dirs
  }

  fn serve(places: &mut [Place], wanted: &[(NodeKey, &Path, Want)]) {
    let (mut at, mut above) = (0, Vec::new());
    for &(key, dir, _) in wanted {
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
  }

  fn common_parts(a: &Path, b: &Path) -> usize {
    a.components().zip(b.components()).take_while(|(a, b)| a == b).count()
  }

  fn first_parts(path: &Path, count: usize) -> PathBuf {
    path.components().take(count).collect()
  }

  pub(super) fn link_between(top: &Path, dir: &Path, is_link: impl FnMut(&Path) -> bool) -> bool {
    let names = dir.strip_prefix(top).map_or(0, |below| below.components().count());
    dir.ancestors().take(names).any(is_link)
  }
}

mod handles {
  use std::{
    borrow::Cow,
    collections::hash_map,
    ffi::OsStr,
    io, mem,
    path::{Component, Path, PathBuf},
  };

  use rustc_hash::{FxHashMap, FxHashSet};

  use super::place::{Place, link_between, place};
  use crate::{
    backend::{Coverage, NodeKey, Want, copy_error, give_room_back, may_watch_a_tree, watch_limit},
    fs::wide_name,
  };

  pub const MAX_HANDLES: usize = 64;

  #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
  pub struct WatchId(pub usize);

  pub trait System {
    fn open(&mut self, id: WatchId, dir: &Path) -> io::Result<()>;

    fn close(&mut self, id: WatchId);

    fn is_link(&mut self, path: &Path) -> bool;
  }

  #[derive(Debug, Default)]
  pub struct Report {
    pub lost: Vec<NodeKey>,
    pub covered: Vec<NodeKey>,
  }

  #[derive(Debug, Default)]
  pub struct Handles {
    keys: FxHashMap<NodeKey, Served>,
    keys_at: FxHashMap<PathBuf, Vec<NodeKey>>,
    open: Vec<Handle>,
    next_id: usize,
    unsettled: bool,
    refused: FxHashSet<NodeKey>,
    waiting: FxHashMap<NodeKey, PathBuf>,
    dead: FxHashMap<NodeKey, Death>,
  }

  #[derive(Debug)]
  struct Handle {
    id: WatchId,
    dir: PathBuf,
    delivered: bool,
  }

  #[derive(Debug)]
  struct Served {
    dir: PathBuf,
    want: Want,
    handle: Option<WatchId>,
  }

  #[derive(Debug)]
  struct Death {
    again: bool,
    error: io::Error,
  }

  impl Handles {
    pub fn watch(
      &mut self,
      key: NodeKey,
      dir: &Path,
      want: Want,
      system: &mut dyn System,
    ) -> (Coverage, Report) {
      let mut report = Report::default();
      let waited = self.waiting.remove(&key).is_some();
      let refused = self.refused.remove(&key);
      let told_to_ask = waited || refused;

      if let Some(at) = self.handle_above(dir) {
        if link_between(&self.open[at].dir, dir, |path| system.is_link(path)) {
          self.drop_key(key, system);
          return (Coverage::NotWatched, report);
        }
        if !told_to_ask
          && self.open[at].dir == dir
          && let Err(error) = self.reopen(at, system)
        {
          self.drop_key(key, system);
          return (Coverage::Failed(error), report);
        }
        self.remember(key, dir, want, Some(self.open[at].id));
        let coverage =
          if want == Want::Contents { Coverage::WatchedTree } else { Coverage::Watched };
        return (coverage, report);
      }
      if want == Want::Names || !may_watch_a_tree(dir) {
        self.waiting.insert(key, dir.to_owned());
        return (Coverage::NotWatched, report);
      }
      if let hash_map::Entry::Occupied(death) = self.dead.entry(key)
        && death.get().again
      {
        return (Coverage::Failed(death.remove().error), report);
      }

      self.remember(key, dir, want, None);
      if !refused {
        self.unsettled = true;
        return (Coverage::WatchedTree, report);
      }
      let settled = self.settle(system, &mut report);
      report.lost.retain(|&lost| lost != key);
      let coverage = match settled {
        Ok(()) if self.serves(key) => Coverage::WatchedTree,
        Ok(()) if !self.refused.contains(&key) => Coverage::NotWatched,
        Ok(()) => Coverage::Failed(watch_limit("the limit of open directory handles is reached")),
        Err(error) => Coverage::Failed(error),
      };
      (coverage, report)
    }

    pub fn unwatch(&mut self, key: NodeKey, system: &mut dyn System) {
      self.waiting.remove(&key);
      self.refused.remove(&key);
      self.dead.remove(&key);
      self.drop_key(key, system);
    }

    pub fn commit(&mut self, system: &mut dyn System) -> Report {
      let mut report = Report::default();
      if self.unsettled {
        let _ = self.settle(system, &mut report);
      }
      report
    }

    pub fn closed(&mut self, id: WatchId, error: &io::Error) -> Vec<NodeKey> {
      let Some(at) = self.open.iter().position(|handle| handle.id == id) else { return Vec::new() };
      let handle = self.open.swap_remove(at);
      let served = self.keys.iter().filter(|(_, served)| served.handle == Some(id));
      let keys: Vec<NodeKey> = served.map(|(&key, _)| key).collect();
      for &key in &keys {
        self.forget(key);
        let again = !handle.delivered && self.dead.contains_key(&key);
        self.dead.insert(key, Death { again, error: copy_error(error) });
      }
      keys
    }

    pub fn serves(&self, key: NodeKey) -> bool {
      self.keys.get(&key).is_some_and(|served| served.handle.is_some())
    }

    pub(super) fn delivered(&mut self, id: WatchId) {
      let Some(handle) = self.open.iter_mut().find(|handle| handle.id == id) else { return };
      if mem::replace(&mut handle.delivered, true) || self.dead.is_empty() {
        return;
      }
      for (key, _) in self.keys.iter().filter(|(_, served)| served.handle == Some(id)) {
        self.dead.remove(key);
      }
    }

    pub(super) fn route_below(&self, dir: &Path) -> Route<'_> {
      let is_separator = |byte: &u8| matches!(byte, b'\\' | b'/');
      let lower_dir = lowered(dir);
      let mut path = dir.as_os_str().as_encoded_bytes().to_vec();
      let mut lower = lower_dir.as_os_str().as_encoded_bytes().to_vec();
      path.push(b'\\');
      lower.push(b'\\');
      let above = dir.ancestors().zip(lower_dir.ancestors()).filter_map(|(up, lower_up)| {
        let keys = self.keys_at.get(lower_up)?.as_slice();
        let end = up.as_os_str().len();
        let rel_from = end + path[end..].iter().take_while(|byte| is_separator(byte)).count();
        Some(Above { keys, rel_from })
      });
      let above = above.collect();
      let base = Lengths { path: path.len(), lower: lower.len() };
      let (ends, text, name) = (Vec::new(), String::new(), String::new());
      Route { keys_at: &self.keys_at, above, path, lower, base, ends, text, name }
    }

    pub(super) fn below(&self, dir: &Path) -> Vec<NodeKey> {
      let dir = lowered(dir);
      let below = self.keys_at.iter().filter(|(lower, _)| lower.starts_with(&dir));
      below.flat_map(|(_, keys)| keys).copied().collect()
    }
  }

  impl Handles {
    fn drop_key(&mut self, key: NodeKey, system: &mut dyn System) {
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

    fn settle(&mut self, system: &mut dyn System, report: &mut Report) -> io::Result<()> {
      self.unsettled = false;
      let moved = self.move_handles(system, report);
      let left = self.keys.iter().filter(|(_, served)| served.handle.is_none());
      let left: Vec<NodeKey> = left.map(|(&key, _)| key).collect();
      for key in left {
        self.forget(key);
        self.refused.insert(key);
        report.lost.push(key);
      }
      moved
    }

    fn move_handles(&mut self, system: &mut dyn System, report: &mut Report) -> io::Result<()> {
      let mut looked: FxHashMap<PathBuf, bool> = FxHashMap::default();
      let (places, behind_a_link) = self.placement(&mut looked, system);
      let new = self.open_new(&places, system)?;
      let is_placed = |handle: &Handle| places.iter().any(|place| place.dir == handle.dir);
      let (kept, gone): (Vec<_>, Vec<_>) =
        mem::take(&mut self.open).into_iter().partition(is_placed);
      for handle in &gone {
        system.close(handle.id);
      }
      self.open = kept;
      for handle in &new {
        let waiting = self.waiting.iter().filter(|(_, dir)| dir.starts_with(&handle.dir));
        let covered = waiting.filter(|(_, dir)| {
          !link_between(&handle.dir, dir, |path| is_link(&mut looked, system, path))
        });
        report.covered.extend(covered.map(|(&key, _)| key));
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
        report.lost.push(key);
      }
      Ok(())
    }

    fn placement(
      &self,
      looked: &mut FxHashMap<PathBuf, bool>,
      system: &mut dyn System,
    ) -> (Vec<Place>, Vec<NodeKey>) {
      let wanted: Vec<(NodeKey, &Path, Want)> =
        self.keys.iter().map(|(&key, served)| (key, &*served.dir, served.want)).collect();
      loop {
        let links_known = looked.values().filter(|&&is| is).count();
        let places = place(&wanted, MAX_HANDLES, &mut |path| looked.get(path) == Some(&true));
        let mut behind_a_link = Vec::new();
        for place in &places {
          let id = self.handle_on(&place.dir);
          for &key in &place.keys {
            let served = &self.keys[&key];
            if id.is_some() && id == served.handle {
              continue;
            }
            if link_between(&place.dir, &served.dir, |path| is_link(looked, system, path)) {
              behind_a_link.push(key);
            }
          }
        }
        if looked.values().filter(|&&is| is).count() == links_known {
          return (places, behind_a_link);
        }
      }
    }

    fn open_new(&mut self, places: &[Place], system: &mut dyn System) -> io::Result<Vec<Handle>> {
      let mut new = Vec::new();
      for place in places {
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
      Ok(new)
    }

    fn handle_above(&self, dir: &Path) -> Option<usize> {
      let above = self.open.iter().enumerate().filter(|(_, handle)| dir.starts_with(&handle.dir));
      above.max_by_key(|(_, handle)| handle.dir.as_os_str().len()).map(|(at, _)| at)
    }

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

    fn remember(&mut self, key: NodeKey, dir: &Path, want: Want, handle: Option<WatchId>) {
      self.forget(key);
      self.keys_at.entry(lowered(dir)).or_default().push(key);
      self.keys.insert(key, Served { dir: dir.to_owned(), want, handle });
    }

    fn forget(&mut self, key: NodeKey) -> Option<Served> {
      let gone = self.keys.remove(&key)?;
      let lower = lowered(&gone.dir);
      if let Some(keys) = self.keys_at.get_mut(&lower) {
        keys.retain(|&other| other != key);
        if keys.is_empty() {
          self.keys_at.remove(&lower);
        }
      }
      give_room_back(&mut self.keys);
      give_room_back(&mut self.keys_at);
      Some(gone)
    }
  }

  fn is_link(looked: &mut FxHashMap<PathBuf, bool>, system: &mut dyn System, path: &Path) -> bool {
    *looked.entry(path.to_owned()).or_insert_with(|| system.is_link(path))
  }

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

  fn lower_name<'a>(name: &'a OsStr, text: &'a mut String) -> Cow<'a, OsStr> {
    let Some(name) = name.to_str() else { return Cow::Owned(name.to_ascii_lowercase()) };
    text.clear();
    text.extend(name.chars().flat_map(char::to_lowercase));
    Cow::Borrowed(OsStr::new(text.as_str()))
  }

  pub(super) struct Route<'a> {
    keys_at: &'a FxHashMap<PathBuf, Vec<NodeKey>>,
    above: Vec<Above<'a>>,
    path: Vec<u8>,
    lower: Vec<u8>,
    base: Lengths,
    ends: Vec<Lengths>,
    text: String,
    name: String,
  }

  struct Above<'a> {
    keys: &'a [NodeKey],
    rel_from: usize,
  }

  struct Lengths {
    path: usize,
    lower: usize,
  }

  impl Route<'_> {
    pub(super) fn follow(&mut self, units: impl Iterator<Item = u16> + Clone) -> bool {
      let Self { path, lower, base, ends, text, name, .. } = self;
      let given = wide_name(units, text);
      path.truncate(base.path);
      lower.truncate(base.lower);
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
        ends.push(Lengths { path: path.len(), lower: lower.len() });
      }
      !ends.is_empty()
    }

    pub(super) fn is_at_a_key(&self) -> bool {
      self.keys_at.contains_key(cut(&self.lower, 0, self.lower.len()))
    }

    pub(super) fn each_key(&self, found: &mut dyn FnMut(NodeKey, &Path)) {
      let len = self.path.len();
      for end in self.ends.iter().rev() {
        let Some(keys) = self.keys_at.get(cut(&self.lower, 0, end.lower)) else { continue };
        let rel = cut(&self.path, (end.path + 1).min(len), len);
        for &key in keys {
          found(key, rel);
        }
      }
      for above in &self.above {
        let rel = cut(&self.path, above.rel_from, len);
        for &key in above.keys {
          found(key, rel);
        }
      }
    }
  }

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
    let name_len = field(bytes, layout.name_length)? as usize;
    let name = bytes.get(layout.name..)?.get(..name_len)?;
    let is_dir = match layout.attributes {
      Some(at) => Some(field(bytes, at)? & FILE_ATTRIBUTE_DIRECTORY != 0),
      None => None,
    };
    if !name_len.is_multiple_of(2) {
      return None;
    }
    let rest = match next as usize {
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

  const FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_SIZE
    | FILE_NOTIFY_CHANGE_LAST_WRITE;

  const BUFFER_BYTES: u32 = 64 * 1024;
  const BUFFER_WORDS: usize = BUFFER_BYTES as usize / size_of::<u64>();

  const MAX_EMPTY_READS: u8 = 16;

  const STOP_WAIT_MS: u32 = 2000;

  const BATCH_BYTES: usize = BUFFER_BYTES as usize / 2;
  const REARM_DELAY: Duration = Duration::from_millis(1);

  #[derive(Debug)]
  pub(super) struct Port {
    iocp: Arc<Iocp>,
    commands: mpsc::Sender<Command>,
    worker: Option<thread::JoinHandle<()>>,
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

    pub(super) fn ask_delivered(&self, after: SystemTime) {
      let _ = self.send(Command::Deliver { after });
    }

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
    Open { id: WatchId, dir: Arc<Path>, reply: mpsc::SyncSender<io::Result<()>> },
    Close { id: WatchId },
    Closed { reply: mpsc::SyncSender<()> },
    Deliver { after: SystemTime },
    Stop,
  }

  struct Worker {
    iocp: Arc<Iocp>,
    commands: mpsc::Receiver<Command>,
    sink: Sink<Raw>,
    watches: FxHashMap<WatchId, Watch>,
    waiting: Option<mpsc::SyncSender<()>>,
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
        let Some(packet) = self.next_packet() else { continue };
        match packet {
          Packet::Wake => {
            if !self.run_commands() {
              break;
            }
          }
          Packet::Completed { id, result } => self.completed(id, result, false),
          Packet::Timeout => {}
          Packet::Failed(error) => {
            self.sink.failed(error);
            break;
          }
        }
      }
      self.stop();
    }

    fn next_packet(&mut self) -> Option<Packet> {
      let Some(at) = self.next_rest() else {
        return Some(match self.deliver_after {
          None => self.iocp.wait(INFINITE),
          Some(after) => {
            let wait = after.duration_since(SystemTime::now()).unwrap_or(Duration::ZERO);
            self.iocp.wait(u32::try_from(wait.as_millis()).unwrap_or(u32::MAX).max(1))
          }
        });
      };
      let now = Instant::now();
      if at <= now {
        self.rearm(|rest| rest <= now);
        return None;
      }
      match self.iocp.wait(0) {
        Packet::Timeout => {
          thread::sleep(at.saturating_duration_since(now));
          None
        }
        packet => Some(packet),
      }
    }

    fn next_rest(&self) -> Option<Instant> {
      self.watches.values().filter_map(|watch| watch.rest_until).min()
    }

    fn read_empty(&mut self) -> bool {
      self.rearm(|_| true);
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
          Packet::Failed(error) => {
            self.sink.failed(error);
            return false;
          }
        }
      }
    }

    fn rearm(&mut self, is_due: impl Fn(Instant) -> bool) {
      let over = |watch: &Watch| watch.rest_until.is_some_and(&is_due);
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
          Err(error) if extended && not_supported(&error) => extended = false,
          Err(error) => return Err(error),
        }
      }
    }

    fn close(&mut self, id: WatchId) {
      let Some(watch) = self.watches.get_mut(&id) else { return };
      let resting = watch.rest_until.take().is_some();
      if resting && watch.last_read() {
        return;
      }
      watch.closing = Closing::Asked;
      if watch.pending {
        watch.cancel();
      } else {
        self.watches.remove(&id);
      }
    }

    fn answer_closed(&mut self) {
      if self.watches.values().any(|watch| watch.closing != Closing::No) {
        return;
      }
      if let Some(reply) = self.waiting.take() {
        let _ = reply.send(());
      }
    }

    fn completed(&mut self, id: WatchId, result: io::Result<u32>, draining: bool) {
      let Some(watch) = self.watches.get_mut(&id) else { return };
      watch.pending = false;
      let result = match result {
        Ok(len) => Ok(watch.records(len)),
        Err(error) if error.raw_os_error() == Some(ERROR_NOTIFY_ENUM_DIR.cast_signed()) => {
          Ok(Vec::new())
        }
        Err(error) => Err(error),
      };
      if watch.closing != Closing::No {
        if let Ok(records) = result {
          self.sink.raw(watch.raw(id, Ok(records)));
          if watch.closing == Closing::Asked && watch.last_read() {
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
          Packet::Timeout | Packet::Failed(_) => break,
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
    rest_until: Option<Instant>,
  }

  #[derive(PartialEq)]
  enum Closing {
    No,
    Asked,
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

    fn records(&self, len: u32) -> Vec<u8> {
      let len = (len as usize).min(BUFFER_BYTES as usize);
      // SAFETY: the buffer is `BUFFER_BYTES` long, and the bytes of its words are plain bytes.
      unsafe { slice::from_raw_parts(self.buffer.as_ptr().cast::<u8>(), len) }.to_vec()
    }

    fn cancel(&mut self) {
      if self.pending {
        // SAFETY: the handle is open, and `overlapped` is the block of its pending read.
        unsafe { CancelIoEx(self.dir.as_raw_handle(), &raw const *self.overlapped) };
      }
    }

    fn last_read(&mut self) -> bool {
      self.closing = Closing::LastRead;
      let queued = self.read().is_ok();
      if queued {
        self.cancel();
      }
      queued
    }
  }

  impl Drop for Watch {
    fn drop(&mut self) {
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
        (true, Err(error)) => Packet::Failed(error),
      }
    }
  }

  enum Packet {
    Wake,
    Completed { id: WatchId, result: io::Result<u32> },
    Timeout,
    Failed(io::Error),
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
