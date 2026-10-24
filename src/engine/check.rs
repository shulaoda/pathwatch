use std::{
  cmp::Reverse,
  ffi::OsString,
  hash::{Hash, Hasher},
  io,
  path::{Path, PathBuf},
  time::{Duration, SystemTime},
};

use rustc_hash::FxHasher;

use super::Engine;
use crate::{
  backend::{Backend, NodeKey, Want},
  config::EntryKind,
  error::ErrorKind,
  fs::{DirId, Entry, FileType, Fs, is_delete_pending, is_link_loop, is_missing},
  path::push_name,
  tree::{
    Added, Content, Depth, Dir, Fingerprint, Kind, Mark, Watch, nanos, nanos_of, room_to_keep,
  },
};

const HASH_LIMIT: u64 = 16 * 1024 * 1024;
const YOUNG: Duration = Duration::from_millis(20);
const YOUNG_COARSE: Duration = Duration::from_secs(2);

pub(super) enum Found {
  Missing,
  OtherSpelling,
  File(FileType, Fingerprint),
  Dir { id: DirId, complete: bool, relinked: bool, young: bool },
  NoAnswer,
}

#[derive(Clone, Copy)]
pub(super) enum Listed {
  NotAsked,
  Named(Option<FileType>),
  File(Fingerprint),
  Dir { id: DirId, young: bool },
  NotNamed,
}

impl Listed {
  pub(super) fn is_named(self) -> bool {
    matches!(self, Self::Named(_) | Self::File(_) | Self::Dir { .. })
  }

  pub(super) fn of(entry: Entry, flush_time: i64) -> Self {
    match entry {
      Entry::Name => Self::Named(None),
      Entry::Kind(kind) => Self::Named(Some(kind)),
      Entry::File { modified, size, file_id } => {
        Self::File(Fingerprint { modified: nanos(modified), size, id: file_id })
      }
      Entry::Dir { id, modified } => {
        Self::Dir { id, young: young_dir(flush_time, nanos(modified)) }
      }
    }
  }
}

impl<B: Backend, F: Fs> Engine<B, F> {
  pub(super) fn check(&mut self, key: NodeKey, path: &mut PathBuf, listed: Listed, why: Mark) {
    let mut twins = Vec::new();
    self.check_one(key, path, listed, why, &mut twins, false);
    while let Some(twin) = twins.pop() {
      if let Some(mark) = self.tree.take_mark(twin) {
        self.check_one(twin, path, Listed::NotNamed, mark, &mut twins, false);
      }
    }
  }

  fn check_one(
    &mut self,
    key: NodeKey,
    path: &mut PathBuf,
    listed: Listed,
    why: Mark,
    twins: &mut Vec<NodeKey>,
    named_by_disk: bool,
  ) {
    let why = match self.tree.take_mark(key) {
      Some(pending) => Mark {
        deep: why.deep || pending.deep,
        written: why.written || pending.written,
        lost: why.lost || pending.lost,
        since: why.since,
      },
      None => why,
    };
    let Some(parent) = self.tree.parent(key) else { return };
    let node = &self.tree[key];
    let registered = node.is_registered();
    let was_unlisted = node.is_unlisted();

    push_name(path, self.tree.name(key));
    let (found, on_disk) = self.read_entry(parent, key, path, listed);
    self.apply(key, path, found, was_unlisted, why);

    let wants_more = registered && !self.tree[key].is_dir() && self.tells_names_only(parent);
    let can_go = !registered && matches!(self.tree[key].kind, Kind::Missing);
    let gone_twins = if can_go { self.tree.marked_twins(key) } else { Vec::new() };
    self.tree.prune(key);
    if self.tree.get(key).is_none() {
      twins.extend(gone_twins);
    }
    if !path.pop() {
      path.clear();
    }
    if wants_more {
      self.ask(parent, path, Want::Contents);
    }
    if let Some(name) = on_disk {
      let node =
        self.tree.child(parent, &name).unwrap_or_else(|| self.tree.insert_child(parent, &name));
      let pending = self.tree.take_mark(node).unwrap_or_default();
      let written = why.written || pending.written;
      let mark = Mark { deep: true, written, lost: false, since: why.since };
      if named_by_disk {
        self.tree.mark(node, mark);
      } else {
        self.check_one(node, path, Listed::NotAsked, mark, twins, true);
      }
    }
  }

  fn read_entry(
    &mut self,
    parent: NodeKey,
    key: NodeKey,
    path: &Path,
    listed: Listed,
  ) -> (Found, Option<OsString>) {
    if self.tree[key].is_registered() {
      self.read_registered_name(parent, key, path, listed)
    } else if self.tree[parent].is_complete() {
      self.read_listed_name(parent, key, path, listed)
    } else if self.tracks_every_entry(parent) {
      (Found::NoAnswer, None)
    } else {
      (Found::Missing, None)
    }
  }

  fn read_registered_name(
    &mut self,
    parent: NodeKey,
    key: NodeKey,
    path: &Path,
    listed: Listed,
  ) -> (Found, Option<OsString>) {
    let node = &self.tree[key];
    let (is_ancestor, is_path, was_unlisted) =
      (node.is_ancestor(), node.is_path(), node.is_unlisted());
    let in_complete = self.tree[parent].is_complete();
    let unsure = in_complete && self.tree.loose_spelling && self.spelling_is_unsure(key, listed);
    let mut meta = match self.fs.lstat(path) {
      Ok(meta) => meta,
      Err(error) => return (self.look_error(parent, key, path, listed, error), None),
    };
    let other = if unsure {
      match self.other_spelling(key, path) {
        Ok(other) => other,
        Err(error) => return (self.disk_error(parent, path, error), None),
      }
    } else {
      None
    };
    self.tree[key].set_unlisted(other.is_some());
    let link = (meta.kind == FileType::Symlink)
      .then(|| self.fs.real_path(path).map_or(0, |real| hash_of(&real)));
    let old = self.tree.link(key);
    let relinked = old != link;
    self.tree.set_link(key, link);
    if link.is_some() {
      meta = match self.fs.stat(path) {
        Ok(meta) => meta,
        Err(error) => {
          let found = self.disk_error(parent, path, error);
          if matches!(found, Found::NoAnswer) {
            self.tree.set_link(key, old);
            self.tree[key].set_unlisted(was_unlisted);
          }
          return match found {
            Found::Missing if !is_path => respelled(other),
            found => (found, None),
          };
        }
      };
    }

    let inside = is_path || (in_complete && other.is_none());
    let kind = entry_kind(meta.kind);
    let visible = inside && !self.ignores(key, path, kind);
    let found = match kind {
      EntryKind::Dir if is_ancestor || visible => {
        let young = young_dir(self.flush_time, nanos(meta.modified));
        Found::Dir { id: meta.dir_id, complete: visible, relinked, young }
      }
      EntryKind::File if visible => Found::File(meta.kind, Fingerprint::of(&meta)),
      _ => return respelled(other),
    };
    (found, other.filter(|_| !was_unlisted))
  }

  fn read_listed_name(
    &mut self,
    parent: NodeKey,
    key: NodeKey,
    path: &Path,
    listed: Listed,
  ) -> (Found, Option<OsString>) {
    let kind = match listed {
      Listed::Named(kind) => kind,
      Listed::File(_) => Some(FileType::File),
      Listed::Dir { .. } => Some(FileType::Dir),
      Listed::NotAsked | Listed::NotNamed => None,
    };
    if kind.is_some_and(|kind| self.ignores(key, path, entry_kind(kind))) {
      return (Found::Missing, None);
    }
    match listed {
      Listed::File(file) => return (Found::File(FileType::File, file), None),
      Listed::Dir { id, young } => {
        return (Found::Dir { id, complete: true, relinked: false, young }, None);
      }
      _ => {}
    }
    let unsure = self.tree.loose_spelling && self.spelling_is_unsure(key, listed);
    let meta = match self.fs.lstat(path) {
      Ok(meta) => meta,
      Err(error) => return (self.look_error(parent, key, path, listed, error), None),
    };
    let kind = entry_kind(meta.kind);
    if self.ignores(key, path, kind) {
      return (Found::Missing, None);
    }
    if unsure {
      match self.other_spelling(key, path) {
        Ok(None) => {}
        Ok(Some(name)) => return (Found::OtherSpelling, Some(name)),
        Err(error) => return (self.disk_error(parent, path, error), None),
      }
    }
    let found = match kind {
      EntryKind::Dir => {
        let young = young_dir(self.flush_time, nanos(meta.modified));
        Found::Dir { id: meta.dir_id, complete: true, relinked: false, young }
      }
      EntryKind::File => Found::File(meta.kind, Fingerprint::of(&meta)),
    };
    (found, None)
  }

  fn look_error(
    &mut self,
    parent: NodeKey,
    key: NodeKey,
    path: &Path,
    listed: Listed,
    error: io::Error,
  ) -> Found {
    if is_delete_pending(&error) {
      self.tree.mark(key, Mark::default());
    } else if listed.is_named()
      && is_missing(&error)
      && let Some(dir) = self.tree[parent].dir_mut()
    {
      dir.relist = true;
      self.tree.mark(parent, Mark::default());
    }
    self.disk_error(parent, path, error)
  }

  fn other_spelling(&self, key: NodeKey, path: &Path) -> io::Result<Option<OsString>> {
    let name = self.fs.name_on_disk(path)?;
    Ok(Some(name).filter(|name| name != self.tree.name(key)))
  }

  fn spelling_is_unsure(&self, key: NodeKey, listed: Listed) -> bool {
    if listed.is_named() {
      return false;
    }
    let node = &self.tree[key];
    let was_entry =
      matches!(listed, Listed::NotAsked) && node.is_entry() && node.added() != Added::Registered;
    self.tree.has_spellings(key) && (!was_entry || self.tree.has_other_spelling(key))
  }

  pub(super) fn disk_error(&mut self, owner: NodeKey, path: &Path, error: io::Error) -> Found {
    if is_missing(&error) {
      return Found::Missing;
    }
    let (kind, found) = match error.kind() {
      io::ErrorKind::PermissionDenied => (Some(ErrorKind::PermissionDenied), Found::Missing),
      io::ErrorKind::InvalidFilename | io::ErrorKind::InvalidInput => {
        (Some(ErrorKind::Io), Found::Missing)
      }
      _ if is_link_loop(&error) => (Some(ErrorKind::Io), Found::Missing),
      io::ErrorKind::ResourceBusy => (None, Found::NoAnswer),
      _ => (Some(ErrorKind::Io), Found::NoAnswer),
    };
    self.scan_had_error = true;
    if let Some(dir) = self.tree[owner].dir_mut()
      && !dir.has_error
    {
      dir.has_error = true;
      self.tree.list_if_polled(owner);
      if let Some(kind) = kind {
        self.report(kind, path, error);
      }
    }
    found
  }

  fn apply(
    &mut self,
    key: NodeKey,
    path: &mut PathBuf,
    found: Found,
    was_unlisted: bool,
    why: Mark,
  ) {
    match (found, self.tree[key].kind) {
      (Found::NoAnswer, _) => self.tree.mark(key, self.retry(why)),

      (Found::Missing, Kind::Missing) => {}
      (Found::Missing, Kind::File(_) | Kind::Dir(_)) => self.missing(key, why.since),
      (Found::OtherSpelling, _) => self.gone(key),

      (Found::File(kind, file), had) => {
        if let Kind::Dir(_) = had {
          self.vanish(key);
        }
        self.store_file(key, path, kind, file, why);
      }

      (Found::Dir { id, complete, relinked, young }, had @ (Kind::Missing | Kind::File(_))) => {
        if let Kind::File(_) = had {
          self.forget_file(key);
        }
        self.appear(key, path, id, complete, relinked);
        self.look_again_at_a_new_dir(key, young);
      }
      (
        Found::Dir { id, complete, relinked, young },
        Kind::Dir(Dir { id: old, complete: was_complete, .. }),
      ) => {
        let was_held = self.tree[key].is_held();
        self.tree.unhold(key);
        if let Some(dir) = self.tree[key].dir_mut() {
          dir.id = id;
        }
        let unlisted = self.tree[key].is_unlisted();
        if complete && !was_complete {
          self.make_complete(key, relinked);
        } else if !complete && unlisted && (was_complete || !was_unlisted) {
          self.uncover(key, true);
        } else if complete && relinked && self.tree[key].added() == Added::Registered {
          self.tree[key].set_baseline(true);
        }
        let moved = was_held || id != old || relinked;
        let named = why.deep && self.tree[key].watch() == Watch::FromAbove && why.since.is_some();
        let away = moved || why.lost || named || self.away_scan;
        self.still_there(key, path, why.deep || moved, away);
        if id != old {
          self.look_again_at_a_new_dir(key, young);
        }
      }
    }
  }

  pub(super) fn retry(&self, why: Mark) -> Mark {
    Mark { deep: true, lost: why.lost || self.loss.scanning || self.away_scan, ..why }
  }

  fn look_again_at_a_new_dir(&mut self, key: NodeKey, young: bool) {
    if self.options.two_step_links && young {
      self.tree.mark(key, Mark::default());
    }
  }

  pub(super) fn make_complete(&mut self, key: NodeKey, relinked: bool) {
    let baseline = self.is_baseline_dir(key, relinked);
    let node = &mut self.tree[key];
    node.set_baseline(baseline);
    if let Some(dir) = node.dir_mut() {
      dir.complete = true;
    }
    if baseline {
      self.touched.push(key);
    }
    self.tree.queue_scan(key, Depth::Shallow);
  }

  fn store_file(
    &mut self,
    key: NodeKey,
    path: &Path,
    kind: FileType,
    fingerprint: Fingerprint,
    why: Mark,
  ) {
    let old = match self.tree[key].kind {
      Kind::File(old) => Some(old),
      _ => None,
    };
    let same = old == Some(fingerprint);
    let only_polled = self.only_polled(key);
    let young =
      kind != FileType::Other && self.is_young(key, fingerprint.modified, same, only_polled);
    let trusts_hints = self.options.written_hints && !only_polled;
    let kept = if same { self.tree.content(key) } else { None };
    let hash = if matches!(kept, Some(Content::Hash(_))) || (young && !trusts_hints) {
      match self.content_hash(key, path, kind, fingerprint.size) {
        Ok(hash) => hash,
        Err(error) if is_missing(&error) => {
          self.tree.set_ahead_since(key, None);
          return self.tree.mark(key, Mark::default());
        }
        Err(_) => None,
      }
    } else {
      None
    };
    let update_waits = young && hash.is_none() && only_polled;

    let changed = if old.is_some() {
      why.written
        || !same
        || match kept {
          None => false,
          Some(Content::Hash(old)) => hash != Some(old),
          Some(Content::Pending) if trusts_hints => self.hint_may_be_lost(key, why),
          Some(Content::Unknown | Content::Pending) => !update_waits,
        }
    } else {
      let baseline = !self.watched_before(key);
      let node = &mut self.tree[key];
      node.set_baseline(baseline);
      node.told()
    };
    let node = &mut self.tree[key];
    if changed {
      node.set_changed(true);
    }
    node.kind = Kind::File(fingerprint);
    if changed || old.is_none() {
      self.touched.push(key);
    }
    self.tree.unhold(key);
    let content = if trusts_hints {
      (young || (matches!(kept, Some(Content::Pending)) && !changed)).then_some(Content::Pending)
    } else {
      young.then(|| hash.map_or(Content::Unknown, Content::Hash))
    };
    self.tree.set_content(key, content);
    if young && trusts_hints {
      self.await_delivery(key, fingerprint.modified, only_polled);
    }
  }

  fn hint_may_be_lost(&self, key: NodeKey, why: Mark) -> bool {
    why.lost
      || self.tree[key].is_held()
      || self.away_scan
      || self.loss.scanning
      || self.loss.waits
      || self.tree.ancestors(key).any(|dir| self.tree[dir].dir().is_some_and(|dir| dir.away))
  }

  fn content_hash(
    &self,
    key: NodeKey,
    path: &Path,
    kind: FileType,
    size: u64,
  ) -> io::Result<Option<u64>> {
    match kind {
      FileType::File if size <= HASH_LIMIT => {
        if self.tree.link(key).is_some() {
          let real = self.fs.real_path(path)?;
          self.fs.hash(&real, HASH_LIMIT)
        } else {
          self.fs.hash(path, HASH_LIMIT)
        }
      }
      FileType::Symlink => self.fs.link_target(path).map(|target| Some(hash_of(&target))),
      _ => Ok(None),
    }
  }

  fn is_young(&mut self, key: NodeKey, modified: i64, same: bool, only_polled: bool) -> bool {
    if self.is_ahead(modified) {
      let now = self.now.expect("a check runs inside a flush");
      let since = self.tree.ahead_since(key).filter(|_| same).unwrap_or(now);
      self.tree.set_ahead_since(key, Some(since));
      return now.saturating_duration_since(since) < YOUNG_COARSE;
    }
    self.tree.set_ahead_since(key, None);
    self.flush_time.saturating_sub(modified) < nanos_of(youth(modified, only_polled))
  }

  fn is_ahead(&self, modified: i64) -> bool {
    modified.saturating_sub(self.flush_time) >= nanos_of(YOUNG_COARSE)
  }

  fn only_polled(&self, key: NodeKey) -> bool {
    !self.tree.parent(key).is_some_and(|parent| self.tree[parent].watch().is_reported())
      || self.tree.link(key).is_some()
  }

  fn await_delivery(&mut self, key: NodeKey, modified: i64, only_polled: bool) {
    if let Some(old_at) = self.old_at(modified, only_polled) {
      self.pending.push(Reverse((old_at, key)));
    }
    if self.pending.len() > 2 * self.tree.content_count() + 64 {
      let tree = &self.tree;
      self.pending.retain(|&Reverse((_, key))| tree.content(key) == Some(Content::Pending));
    }
    self.ask_delivered();
  }

  fn old_at(&self, modified: i64, only_polled: bool) -> Option<i64> {
    let youth = nanos_of(youth(modified, only_polled));
    (!self.is_ahead(modified)).then(|| modified.saturating_add(youth))
  }

  pub(super) fn forget_delivered(&mut self) {
    if self.tree.has_work() || self.tree.lost || self.loss.waits || self.tree.hold_count() > 0 {
      return;
    }
    let mut later = Vec::new();
    while let Some(&Reverse((old_at, key))) = self.pending.peek()
      && old_at <= self.delivered
    {
      self.pending.pop();
      let Some(Kind::File(file)) = self.tree.get(key).map(|node| node.kind) else { continue };
      if self.tree.content(key) != Some(Content::Pending) {
        continue;
      }
      match self.old_at(file.modified, self.only_polled(key)) {
        Some(old_at) if old_at <= self.delivered => self.tree.set_content(key, None),
        Some(old_at) => later.push(Reverse((old_at, key))),
        None => {}
      }
    }
    later.sort_unstable();
    later.dedup();
    self.pending.extend(later);
    if let Some(room) = room_to_keep(self.pending.len(), self.pending.capacity()) {
      self.pending.shrink_to(room);
    }
    self.ask_delivered();
  }

  fn ask_delivered(&mut self) {
    let Some(&Reverse((old_at, _))) = self.pending.peek() else { return };
    if old_at <= self.delivered || self.asked_until >= old_at {
      return;
    }
    self.asked_until = old_at;
    let at = SystemTime::UNIX_EPOCH + Duration::from_nanos(u64::try_from(old_at).unwrap_or(0));
    if let Some(backend) = &mut self.backend {
      backend.ask_delivered(at);
    }
  }
}

fn respelled(other: Option<OsString>) -> (Found, Option<OsString>) {
  let found = if other.is_some() { Found::OtherSpelling } else { Found::Missing };
  (found, other)
}

fn youth(modified: i64, only_polled: bool) -> Duration {
  let whole_second = modified >= 0 && modified % nanos_of(Duration::from_secs(1)) == 0;
  if whole_second || only_polled { YOUNG_COARSE } else { YOUNG }
}

fn young_dir(flush_time: i64, modified: i64) -> bool {
  flush_time.saturating_sub(modified) < nanos_of(YOUNG_COARSE)
}

fn hash_of(path: &Path) -> u64 {
  let mut hasher = FxHasher::default();
  path.hash(&mut hasher);
  hasher.finish()
}

fn entry_kind(kind: FileType) -> EntryKind {
  if kind == FileType::Dir { EntryKind::Dir } else { EntryKind::File }
}
