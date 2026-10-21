//! One entry: `check` reads what is at a path and brings the node in line with it.

use std::{
  ffi::OsString,
  hash::{Hash, Hasher},
  io, mem,
  path::{Path, PathBuf},
  time::{Duration, Instant},
};

use rustc_hash::FxHasher;

use super::Engine;
use crate::{
  backend::{Backend, DirKey, Want},
  config::EntryKind,
  error::ErrorKind,
  fs::{DirId, Entry, FileType, Fs, is_delete_pending, is_missing},
  tree::{Added, Content, Depth, Fingerprint, Kind, Mark, Watch, nanos, push_name},
};

/// A file is young this long: two writes inside it can leave one modification time.
const YOUNG: Duration = Duration::from_millis(20);
/// The same on a file system that counts in whole seconds (FAT: two), and under the poll.
const YOUNG_COARSE: Duration = Duration::from_secs(2);
/// A bigger file is not hashed.
const HASH_LIMIT: u64 = 16 * 1024 * 1024;

/// What is at a path, after the rules of what counts.
pub(super) enum Found {
  Missing,
  /// The disk has the entry under another spelling: it is gone under this name, at once.
  OtherSpelling(OsString),
  File(FileType, Fingerprint),
  Dir {
    id: DirId,
    complete: bool,
    /// The link at the name changed, or came, or went.
    relinked: bool,
    /// Made in the last moments (`young_dir`).
    young: bool,
  },
  /// The disk did not answer, or the look has to wait. The tree stays as it is.
  Unknown,
}

/// What a listing of the directory said about a node whose check runs in a scan.
#[derive(Clone, Copy)]
pub(super) enum Listed {
  /// The check comes from a mark, or the directory was not listed.
  NotAsked,
  /// Named, with its kind if the listing gives one.
  Named(Option<FileType>),
  /// Named, a regular file with all that a look reads.
  File(Fingerprint),
  /// Named, a directory that is no link, and whether it was made in the last moments
  /// (`young_dir`).
  Dir(DirId, bool),
  /// The whole directory was listed, and the entry was not in it.
  NotNamed,
}

impl Listed {
  pub(super) fn is_named(self) -> bool {
    matches!(self, Self::Named(_) | Self::File(_) | Self::Dir(..))
  }
}

impl Listed {
  /// What the listing said, at a flush whose time is `flush_time` (nanoseconds since 1970).
  pub(super) fn of(entry: Entry, flush_time: i64) -> Self {
    match entry {
      Entry::Name => Self::Named(None),
      Entry::Kind(kind) => Self::Named(Some(kind)),
      Entry::File { modified, size, file_id } => {
        Self::File(Fingerprint { modified: nanos(modified), size, id: file_id })
      }
      Entry::Dir { id, modified } => Self::Dir(id, young_dir(flush_time, nanos(modified))),
    }
  }
}

/// What the tree had at a node before the check, also while the node is held.
enum Had {
  Nothing,
  File,
  Dir { id: DirId, complete: bool, unlisted: bool },
}

impl<B: Backend, F: Fs> Engine<B, F> {
  /// Brings the node in line with the disk. `path` holds the directory of the node and is left so;
  /// `listed` is what a listing of it said.
  pub(super) fn check(&mut self, key: DirKey, path: &mut PathBuf, listed: Listed, why: Mark) {
    let mut twins = Vec::new();
    self.check_one(key, path, listed, why, &mut twins, false);
    // The twins of a node that goes are looked at now: their looks were unsure of the spelling
    // because of this node (`Tree::has_twin`). A twin that goes has twins of its own: a list,
    // not a recursion, since a flush can see thousands.
    while let Some(twin) = twins.pop() {
      if let Some(mark) = self.tree.take_mark(twin) {
        self.check_one(twin, path, Listed::NotNamed, mark, &mut twins, false);
      }
    }
  }

  /// One check, which puts the twins that are to be looked at next on `twins`. `followed`: the
  /// check came from one that found its entry under this name. A mark that waits on the node is
  /// taken into the check: the look is made now, and what the hint said of the node holds.
  fn check_one(
    &mut self,
    key: DirKey,
    path: &mut PathBuf,
    listed: Listed,
    why: Mark,
    twins: &mut Vec<DirKey>,
    followed: bool,
  ) {
    let why = match self.tree.take_mark(key) {
      // A loss report that asked for this node holds: its hints may be lost (5.8).
      Some(pending) => Mark {
        deep: why.deep || pending.deep,
        written: why.written || pending.written,
        lost: why.lost || pending.lost,
        since: why.since,
      },
      None => why,
    };
    let Some(parent) = self.tree.parent(key) else { return };
    let Some(asked) = self.tree[parent].dir().map(|dir| dir.asked) else {
      return self.tree.prune(key);
    };
    let node = &self.tree[key];
    let registered = node.is_registered();
    let below_names_only = registered && asked == Some(Want::Names);
    let had = match node.kind {
      Kind::Missing => Had::Nothing,
      Kind::File(_) => Had::File,
      Kind::Dir(dir) => {
        Had::Dir { id: dir.id, complete: dir.complete, unlisted: node.is_unlisted() }
      }
    };

    push_name(path, self.tree.name(key));
    let mut listed_as = None;
    let found = self.read_entry(parent, key, path, listed, &mut listed_as);
    // The directory lists the entry under another name: a name that is not registered is gone
    // under this one, a registered name stays, and the other name is looked at below.
    let on_disk = match &found {
      Found::OtherSpelling(name) => Some(name.clone()),
      _ => listed_as,
    };
    self.apply(key, path, found, had, why);

    // A registered name with no directory at it makes its directory want the contents.
    let wants_more = below_names_only && !self.tree[key].is_dir();
    let can_go = !registered && matches!(self.tree[key].kind, Kind::Missing);
    let gone_twins = if can_go { self.tree.marked_twins(key) } else { Vec::new() };
    self.tree.prune(key);
    if self.tree.get(key).is_none() {
      twins.extend(gone_twins);
    }
    // `pop` does not take a top directory (`/`, `C:\`) away.
    if !path.pop() {
      path.clear();
    }
    if wants_more {
      // The ask queues a scan: the name is looked at again when the wider watch is live.
      self.ask(parent, path, Want::Contents);
    }
    // The entry is on the disk under another name: that name is looked at now, with what the
    // hint said of the entry. Once only, so that a disk whose answers do not agree with each
    // other cannot keep the flush busy; a second step waits for the next flush.
    if let Some(name) = on_disk {
      let node = match self.tree.child(parent, &name) {
        Some(node) => node,
        None => self.tree.insert_child(parent, &name),
      };
      let pending = self.tree.take_mark(node).unwrap_or_default();
      let written = why.written || pending.written;
      let mark = Mark { deep: true, written, lost: false, since: why.since };
      if followed {
        self.tree.mark(node, mark);
      } else {
        self.check_one(node, path, Listed::NotAsked, mark, twins, true);
      }
    }
  }

  /// One arm for each case of what was found against what the tree had: the table in
  /// design.md 5.4.
  fn apply(&mut self, key: DirKey, path: &mut PathBuf, found: Found, had: Had, why: Mark) {
    match (found, had) {
      // No answer: the look is tried again, deep, since the tree below can be behind meanwhile.
      // What this look knew of lost hints (5.8) holds for the next one.
      (Found::Unknown, _) => {
        let lost = why.lost || self.loss.scanning || self.away_scan;
        self.tree.mark(key, Mark { deep: true, lost, ..why });
      }

      (Found::Missing, Had::Nothing) => {}
      (Found::Missing, Had::File | Had::Dir { .. }) => self.missing(key, why.since),
      (Found::OtherSpelling(_), _) => self.gone(key),

      (Found::File(kind, file), Had::File | Had::Nothing) => {
        self.store_file(key, path, kind, file, why);
      }
      (Found::File(kind, file), Had::Dir { .. }) => {
        // A change of kind is never an `Update`: the files below are gone.
        self.vanish(key);
        self.store_file(key, path, kind, file, why);
      }

      (Found::Dir { id, complete, relinked, young }, Had::Nothing) => {
        let baseline = self.is_baseline_dir(key, relinked);
        self.appear(key, path, id, complete, baseline);
        self.look_again_at_a_new_dir(key, young);
      }
      (Found::Dir { id, complete, relinked, young }, Had::File) => {
        let baseline = self.is_baseline_dir(key, relinked);
        // A change of kind is never an `Update`: the file is gone.
        self.forget_file(key);
        self.appear(key, path, id, complete, baseline);
        self.look_again_at_a_new_dir(key, young);
      }
      (
        Found::Dir { id, complete, relinked, young },
        Had::Dir { id: old, complete: was_complete, unlisted: was_unlisted },
      ) => {
        let was_held = self.tree[key].is_held();
        if was_held {
          self.tree.unhold(key);
        }
        if id != old
          && let Some(dir) = self.tree[key].dir_mut()
        {
          dir.id = id;
        }
        let unlisted = self.tree[key].is_unlisted();
        if complete && !was_complete {
          self.make_complete(key, relinked);
        } else if !complete && unlisted && (was_complete || !was_unlisted) {
          // The directory above lists it under another name now: only what a watched path covers
          // stays. Also in a partial node, whose entries were covered through this name.
          self.uncover(key, true, false);
        } else if complete && relinked && self.tree[key].added() == Added::Registered {
          // The running `watch` follows this link for the first time: what is behind it is its
          // baseline.
          self.tree[key].set_baseline(true);
        }
        // Scan deep when nothing tells what happened in it meanwhile. The writes in it can have
        // gone unheard: it was away, or lies in a directory that was (`away_scan`), its watch
        // lost records, or a record named it (a hint, with its time; not a `watch` call) while
        // only a watch above covers it, so the records of its files came under another name.
        let moved = was_held || id != old || relinked;
        let from_above = self.tree[key].watch() == Watch::FromAbove;
        let named = why.deep && from_above && why.since.is_some();
        let away = moved || why.lost || named || self.away_scan;
        // Only named: a write while the directory was away hides behind the fingerprint of a
        // file that was young when the record was made, and of no other.
        if away
          && !(moved || why.lost || self.away_scan)
          && let Some(since) = why.since
        {
          self.forget_pending_old_at(key, since);
        }
        self.still_there(key, path, why.deep || moved, away);
        let relist = self.tree[key].dir_mut().is_some_and(|dir| mem::take(&mut dir.relist));
        if relist && !(why.deep || moved) {
          self.tree.queue_scan(key, Depth::Shallow);
        }
        if id != old {
          self.look_again_at_a_new_dir(key, young);
        }
      }
    }
  }

  /// On Windows a junction or a directory link is a plain directory first, and the second step
  /// gives no record that the backend asks for: a directory that the tree did not know, and
  /// that was made in the last moments, is looked at once more at the next flush, so that a
  /// link is seen within the poll interval. An old directory is past its second step: the
  /// directories of a baseline are not looked at again (one `lstat` for each of them at the
  /// first flush after `watch` before, 5,011 of them for 50,000 files in 5,000 directories,
  /// 47 ms, measured).
  fn look_again_at_a_new_dir(&mut self, key: DirKey, young: bool) {
    if self.options.two_step_links && young {
      self.tree.mark(key, Mark::default());
    }
  }

  /// The partial node tracks every entry of its directory from now on.
  fn make_complete(&mut self, key: DirKey, relinked: bool) {
    let baseline = self.is_baseline_dir(key, relinked);
    let node = &mut self.tree[key];
    node.set_baseline(baseline);
    if let Some(dir) = node.dir_mut() {
      dir.complete = true;
    }
    self.touched.push(key);
    self.tree.queue_scan(key, Depth::Shallow);
  }

  /// Reads what is at the node and applies what counts: an entry that no watched path covers, or
  /// that the filter ignores, is missing. `listed_as`: the name under which the directory lists
  /// a registered node from this look on, when it listed it under its own name before.
  fn read_entry(
    &mut self,
    parent: DirKey,
    key: DirKey,
    path: &Path,
    listed: Listed,
    listed_as: &mut Option<OsString>,
  ) -> Found {
    let node = &self.tree[key];
    let (registered, is_ancestor, is_path) =
      (node.is_registered(), node.is_ancestor(), node.is_path());
    let in_complete = self.tree[parent].is_complete();

    if !registered {
      if !in_complete {
        // Unknown while the look that makes the directory complete is still to come.
        return if self.tracks_every_entry(parent) { Found::Unknown } else { Found::Missing };
      }
      // Asked before the `lstat`, so that an ignored entry costs no system call.
      let kind = match listed {
        Listed::Named(kind) => kind,
        Listed::File(_) => Some(FileType::File),
        Listed::Dir(..) => Some(FileType::Dir),
        Listed::NotAsked | Listed::NotNamed => None,
      };
      if kind.is_some_and(|kind| self.ignores(key, path, entry_kind(kind))) {
        return Found::Missing;
      }
    }

    // Only a loose directory is asked how it spells a name.
    let unsure = in_complete && self.tree.loose_spelling && self.spelling_is_unsure(key, listed);
    if let Listed::File(file) = listed
      && !registered
      && !unsure
    {
      return Found::File(FileType::File, file);
    }
    if let Listed::Dir(id, young) = listed
      && !registered
      && !unsure
    {
      return Found::Dir { id, complete: true, relinked: false, young };
    }
    let mut meta = match self.fs.lstat(path) {
      Ok(meta) => meta,
      Err(error) => return self.look_failed(parent, key, path, listed, error),
    };
    // A registered name is an entry of the directory above only under this very name: asked
    // before a link is followed.
    let mut other = None;
    let mut relinked = false;
    if registered {
      if unsure {
        other = match self.other_spelling(key, path) {
          Ok(other) => other,
          Err(error) => return self.disk_error(parent, path, error),
        };
      }
      let was_unlisted = self.tree[key].is_unlisted();
      self.tree[key].set_unlisted(other.is_some());
      // Only a registered node is followed.
      let link = (meta.kind == FileType::Symlink).then(|| self.real_place(path));
      let old = self.tree.link(key);
      relinked = old != link;
      self.tree.set_link(key, link);
      if link.is_some() {
        meta = match self.fs.stat(path) {
          Ok(meta) => meta,
          Err(error) => {
            let found = self.disk_error(parent, path, error);
            if matches!(found, Found::Unknown) {
              // No answer: the tree stays as it was. The stored link stays, or a watch by
              // path is never made again.
              self.tree.set_link(key, old);
              self.tree[key].set_unlisted(was_unlisted);
            }
            return match found {
              // The link itself is an entry of the directory above, under another name.
              Found::Missing if !is_path => other.map_or(Found::Missing, Found::OtherSpelling),
              found => found,
            };
          }
        };
      }
      if !was_unlisted && let Some(name) = &other {
        *listed_as = Some(name.clone());
      }
    }

    let inside = is_path || (in_complete && other.is_none());
    let kind = entry_kind(meta.kind);
    // A watched path that the filter ignores for the kind it has counts as missing (design.md
    // 5.11).
    let visible = inside && !self.ignores(key, path, kind);
    let found = match kind {
      // A directory on the way to a watched path is not asked: `watch` asked about it.
      EntryKind::Dir if is_ancestor || visible => {
        let young = young_dir(nanos(self.flush_time), nanos(meta.modified));
        Found::Dir { id: meta.dir_id, complete: visible, relinked, young }
      }
      EntryKind::File if visible => Found::File(meta.kind, Fingerprint::of(&meta)),
      _ => return other.map_or(Found::Missing, Found::OtherSpelling),
    };
    // Asked after the filter: an ignored entry costs no question.
    if !registered && unsure {
      match self.other_spelling(key, path) {
        Ok(None) => {}
        Ok(Some(name)) => return Found::OtherSpelling(name),
        Err(error) => return self.disk_error(parent, path, error),
      }
    }
    found
  }

  /// What an error of the `lstat` at the node means.
  fn look_failed(
    &mut self,
    parent: DirKey,
    key: DirKey,
    path: &Path,
    listed: Listed,
    error: io::Error,
  ) -> Found {
    if is_delete_pending(&error) {
      // The deletion can be taken back, and the OS says nothing then: the node is looked at
      // again at the next flush, until the name is gone or the file is there.
      self.tree.mark(key, Mark::default());
    } else if listed.is_named()
      && is_missing(&error)
      && let Some(dir) = self.tree[parent].dir_mut()
    {
      // The listing named the entry a moment ago: the directory itself can be away, renamed
      // away and back while it is read, which nothing else would report. It is listed again.
      dir.relist = true;
      self.tree.mark(parent, Mark::default());
    }
    self.disk_error(parent, path, error)
  }

  /// The name the disk has for the entry, when it is another one than the node has.
  fn other_spelling(&self, key: DirKey, path: &Path) -> io::Result<Option<OsString>> {
    let name = self.fs.name_on_disk(path)?;
    Ok(Some(name).filter(|name| name != self.tree.name(key)))
  }

  /// Whether the disk has to say how it spells the name, in a complete directory that counts as
  /// loose (design.md 5.2).
  fn spelling_is_unsure(&self, key: DirKey, listed: Listed) -> bool {
    let node = &self.tree[key];
    let was_entry = match listed {
      Listed::Named(_) | Listed::File(_) | Listed::Dir(..) => return false,
      Listed::NotNamed => false,
      // A name that the running call registered is spelled by its caller, whatever the tree
      // knew at the name.
      Listed::NotAsked => node.is_entry() && node.added() != Added::Registered,
    };
    self.tree.has_spellings(key) && (!was_entry || self.tree.has_other_spelling(key))
  }

  /// A number for the real path behind the link at `path`.
  fn real_place(&self, path: &Path) -> u64 {
    self.fs.real_path(path).map_or(0, |real| hash_of(&real))
  }

  /// What an error of the disk means for the path. `owner` is polled until the error is gone.
  pub(super) fn disk_error(&mut self, owner: DirKey, path: &Path, error: io::Error) -> Found {
    if is_missing(&error) {
      return Found::Missing;
    }
    let (kind, found) = match error.kind() {
      io::ErrorKind::PermissionDenied => (Some(ErrorKind::PermissionDenied), Found::Missing),
      // A path that the OS does not take (longer than it allows): nothing can ever be at it.
      io::ErrorKind::InvalidFilename | io::ErrorKind::InvalidInput => {
        (Some(ErrorKind::Io), Found::Missing)
      }
      _ if is_link_loop(&error) => (Some(ErrorKind::Io), Found::Missing),
      // Another program holds the entry for a moment (Windows: a sharing violation): nothing is
      // wrong with the disk, so nothing is reported, and the next look decides.
      io::ErrorKind::ResourceBusy => (None, Found::Unknown),
      _ => (Some(ErrorKind::Io), Found::Unknown),
    };
    self.scan_had_error = true;
    if let Some(dir) = self.tree[owner].dir_mut()
      && !dir.has_error
    {
      dir.has_error = true;
      self.tree.list_if_polled(owner);
      if let Some(kind) = kind {
        self.report(kind, Some(path.to_path_buf()), Some(error));
      }
    }
    found
  }

  /// Stores the file that is at the node `key`, and notes what that means for the handler.
  fn store_file(
    &mut self,
    key: DirKey,
    path: &Path,
    kind: FileType,
    fingerprint: Fingerprint,
    why: Mark,
  ) {
    let written = why.written;
    // A held node still has its file: this is its second look.
    let was_held = self.tree[key].is_held();
    let old = match self.tree[key].kind {
      Kind::File(old) => Some((old, self.tree.content(key))),
      _ => None,
    };
    let same = old.is_some_and(|(old, _)| old == fingerprint);
    let young = kind != FileType::Other && self.is_young(key, fingerprint.modified, same);
    // Where every write gives a `written` hint, a young file is not read: the hint tells of a
    // write that leaves the fingerprint as it is (5.8). Not where only the poll looks at it.
    let only_polled = self.only_polled(key);
    let trusts_hints = self.options.written_hints && !only_polled;
    // Compared when the fingerprint is the same, and one last time when the file is old.
    let kept = old.and_then(|(old, content)| content.filter(|_| old == fingerprint));
    let compare = matches!(kept, Some(Content::Hash(_)));
    let hash = if compare || (young && !trusts_hints) {
      match self.content_hash(key, path, kind, fingerprint.size) {
        Ok(hash) => hash,
        // Gone after the `lstat`: this check says nothing, and the next flush looks again.
        Err(error) if is_missing(&error) => {
          // Nothing was stored: the node may have no file to keep the time for.
          self.tree.set_ahead_since(key, None);
          return self.tree.mark(key, Mark::default());
        }
        Err(_) => None,
      }
    } else {
      None
    };
    // Under the poll a young file with nothing to compare waits: the first look that finds it
    // old reports one `Update` for all. Where no poll comes, every look reports.
    let update_waits = young && hash.is_none() && only_polled;

    let changed = if let Some((old, content)) = old {
      written
        || old != fingerprint
        || match content {
          None => false,
          Some(Content::Hash(old)) => hash != Some(old),
          Some(Content::Unknown) => !update_waits,
          Some(Content::Pending) => {
            if trusts_hints {
              self.hint_may_be_lost(key, why, was_held)
            } else {
              !update_waits
            }
          }
        }
    } else {
      // The handler may believe in a file the tree forgot earlier in this flush: nothing says it
      // is the same one.
      let baseline = self.is_baseline_file(key);
      let node = &mut self.tree[key];
      node.set_baseline(baseline);
      node.told()
    };
    let node = &mut self.tree[key];
    if changed {
      node.set_changed(true);
    }
    node.kind = Kind::File(fingerprint);
    if node.changed() || old.is_none() {
      self.touched.push(key);
    }
    if was_held {
      self.tree.unhold(key);
    }
    let content = if trusts_hints {
      young.then_some(Content::Pending)
    } else {
      young.then(|| hash.map_or(Content::Unknown, Content::Hash))
    };
    self.tree.set_content(key, content);
  }

  /// Whether the hint of a write that left the fingerprint as it is can have gone by since the
  /// last look at the file, so that a young file that was not read counts as written (5.8): a
  /// loss report asked for the look, or one waits; the look is one of the scan of a directory
  /// that was away (`Dir::away`), or such a scan is queued above; or the file was held (it was
  /// away itself, or its hint went into the look that found it missing).
  fn hint_may_be_lost(&self, key: DirKey, why: Mark, was_held: bool) -> bool {
    if why.lost
      || was_held
      || self.away_scan
      || self.loss.scanning
      || self.tree.lost
      || self.loss.waits
    {
      return true;
    }
    let mut above = self.tree.parent(key);
    while let Some(dir) = above {
      if self.tree[dir].dir().is_some_and(|dir| dir.away) {
        return true;
      }
      above = self.tree.parent(dir);
    }
    false
  }

  /// A hash of the content of a regular file, or of the target of a link. `None`: nothing to
  /// compare.
  fn content_hash(
    &self,
    key: DirKey,
    path: &Path,
    kind: FileType,
    size: u64,
  ) -> io::Result<Option<u64>> {
    match kind {
      FileType::File if size <= HASH_LIMIT => {
        // A registered link is followed for its fingerprint, and so for its content: the open
        // refuses a link, so the file is read by its real path.
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

  /// Whether the file is young: an equal fingerprint proves nothing at the next check. The long
  /// time is for whole seconds, and under the poll, which has no `Written` hint.
  ///
  /// A time far ahead of the clock (the clock of another machine, a time that a tool set) never
  /// gets old by the clock, and the file would be read at every look for ever. A write after
  /// the first look gives it another time, so it is young for the long time after the first
  /// look that found this fingerprint (`same`: the tree has it). Then it is old.
  fn is_young(&mut self, key: DirKey, modified: i64, same: bool) -> bool {
    // Below zero for a time in the future.
    let age = nanos(self.flush_time).saturating_sub(modified);
    if age <= -nanos_of(YOUNG_COARSE) {
      let now = self.now.expect("a check runs inside a flush");
      let since = self.tree.ahead_since(key).filter(|_| same).unwrap_or(now);
      self.tree.set_ahead_since(key, Some(since));
      return now.saturating_duration_since(since) < YOUNG_COARSE;
    }
    self.tree.set_ahead_since(key, None);
    age < nanos_of(self.youth(key, modified))
  }

  /// How long the file is young for (5.8): the short time with a fine time stamp under an OS
  /// watch, the long one with a whole second, or where only the poll looks at it.
  fn youth(&self, key: DirKey, modified: i64) -> Duration {
    let whole_seconds = modified >= 0 && modified % nanos_of(Duration::from_secs(1)) == 0;
    if whole_seconds || self.only_polled(key) { YOUNG_COARSE } else { YOUNG }
  }

  /// Forgets the `Pending` content of the files below `dir` that were old at `since`, the time
  /// of the record that named the directory: a write while it was away could hide behind the
  /// fingerprint of a file that was young then, and of no other (5.8).
  fn forget_pending_old_at(&mut self, dir: DirKey, since: Instant) {
    let now = self.now.expect("a check runs inside a flush");
    let passed = nanos_of(now.saturating_duration_since(since));
    let flush_time = nanos(self.flush_time);
    for key in self.tree.pending_below(dir) {
      let Kind::File(file) = self.tree[key].kind else { continue };
      // Below zero for a time in the future: young.
      let age = flush_time.saturating_sub(file.modified).saturating_sub(passed);
      if age >= nanos_of(self.youth(key, file.modified)) {
        self.tree.set_content(key, None);
      }
    }
  }

  /// Whether only the poll looks at the entry: its directory is polled, or it is a link.
  fn only_polled(&self, key: DirKey) -> bool {
    let watch = self.tree.parent(key).map_or(Watch::None, |parent| self.tree[parent].watch());
    !matches!(watch, Watch::Watched | Watch::WatchedTree | Watch::FromAbove)
      || self.tree.link(key).is_some()
  }
}

/// Whether the error is a link loop. Its kind (`FilesystemLoop`) is not stable yet, so the
/// number of the OS is read where it is known; elsewhere a loop is an error like any other.
fn is_link_loop(error: &io::Error) -> bool {
  #[cfg(all(unix, not(target_vendor = "apple")))]
  const LINK_LOOP: Option<i32> = Some(libc::ELOOP);
  #[cfg(target_vendor = "apple")]
  const LINK_LOOP: Option<i32> = Some(62);
  #[cfg(windows)]
  const LINK_LOOP: Option<i32> =
    Some(windows_sys::Win32::Foundation::ERROR_CANT_RESOLVE_FILENAME.cast_signed());
  #[cfg(not(any(unix, windows)))]
  const LINK_LOOP: Option<i32> = None;

  LINK_LOOP.is_some_and(|code| error.raw_os_error() == Some(code))
}

fn nanos_of(time: Duration) -> i64 {
  i64::try_from(time.as_nanos()).unwrap_or(i64::MAX)
}

/// A directory made in the last moments: by its modification time, which its making sets,
/// within the coarse youth of a file (5.8) of the time of the flush, which is longer than any
/// flush waits and any listing takes. A time ahead of the clock counts as young. Only NTFS and
/// ReFS have reparse points, and their clocks are fine, so no rounding of a coarse clock is
/// allowed for.
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
