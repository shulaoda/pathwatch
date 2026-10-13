//! One entry: `check` reads what is at a path and brings the node in line with it.

use std::{
  ffi::{OsStr, OsString},
  hash::{Hash, Hasher},
  io, mem,
  path::{Path, PathBuf},
  time::Duration,
};

use rustc_hash::FxHasher;

use super::Engine;
use crate::{
  backend::{Backend, DirKey, Want},
  config::EntryKind,
  error::ErrorKind,
  fs::{DirId, Entry, FileType, Fs, Meta, is_delete_pending},
  tree::{
    Added, Content, Depth, Fingerprint, Kind, Mark, Spelling, Watch, nanos, push_name, with_name,
  },
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
  },
  /// The disk did not answer, or the look has to wait. The tree stays as it is.
  Unknown,
}

/// What the disk says of the spelling of a name.
enum Spelled {
  /// As the node has it.
  Same,
  /// Under this other name.
  Other(OsString),
  /// Not now: the node goes, and the listing of its directory that this look queued brings
  /// the name back, as it does every new entry.
  Later,
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
  /// Named, a directory that is no link.
  Dir(DirId),
  /// The whole directory was listed, and the entry was not in it.
  NotNamed,
}

impl Listed {
  pub(super) fn is_named(self) -> bool {
    matches!(self, Self::Named(_) | Self::File(_) | Self::Dir(_))
  }
}

impl From<Entry> for Listed {
  fn from(entry: Entry) -> Self {
    match entry {
      Entry::Name => Self::Named(None),
      Entry::Kind(kind) => Self::Named(Some(kind)),
      Entry::File { modified, size, file_id } => {
        Self::File(Fingerprint { modified: nanos(modified), size, id: file_id })
      }
      Entry::Dir { id } => Self::Dir(id),
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
      Some(pending) => Mark {
        deep: why.deep || pending.deep,
        written: why.written || pending.written,
        lost: why.lost && pending.lost,
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
    let found = self.read_entry(parent, key, path, listed, !followed);
    // An ignored watch target drops the paths below the name at the end of the flush, if the
    // directory is still there (design.md 5.11).
    if registered
      && matches!(found, Found::Missing | Found::File(..))
      && self.is_ignored_target(parent)
      && self.is_on_disk(parent, path)
    {
      self.dropped.push(key);
      path.pop();
      return;
    }
    let on_disk = match &found {
      Found::OtherSpelling(name) => Some(name.clone()),
      _ => None,
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
    if wants_more && !self.is_ignored_target(parent) {
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
      (Found::Unknown, _) => self.tree.mark(key, Mark { deep: true, ..why }),

      (Found::Missing, Had::Nothing) => {}
      (Found::Missing, Had::File | Had::Dir { .. }) => self.missing(key, why.since),
      (Found::OtherSpelling(_), _) => self.gone(key),

      (Found::File(kind, file), Had::File | Had::Nothing) => {
        self.store_file(key, path, kind, file, why.written);
      }
      (Found::File(kind, file), Had::Dir { .. }) => {
        // A change of kind is never an `Update`: the files below are gone.
        self.vanish(key);
        self.store_file(key, path, kind, file, why.written);
      }

      (Found::Dir { id, complete, relinked }, Had::Nothing) => {
        let baseline = self.is_baseline_dir(key, relinked);
        self.appear(key, path, id, complete, baseline);
        self.look_again_at_a_new_dir(key);
      }
      (Found::Dir { id, complete, relinked }, Had::File) => {
        let baseline = self.is_baseline_dir(key, relinked);
        // A change of kind is never an `Update`: the file is gone.
        self.forget_file(key);
        self.appear(key, path, id, complete, baseline);
        self.look_again_at_a_new_dir(key);
      }
      (
        Found::Dir { id, complete, relinked },
        Had::Dir { id: old, complete: was_complete, unlisted: was_unlisted },
      ) => {
        let was_held = self.tree[key].is_held();
        if was_held {
          self.tree.unhold(key);
        }
        if id != old
          && let Some(dir) = self.tree[key].dir_mut()
        {
          // Another directory, and maybe on another disk.
          (dir.id, dir.spelling) = (id, Spelling::Unknown);
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
        // Scan deep when nothing tells what happened in it meanwhile.
        let moved = was_held || id != old || relinked;
        self.still_there(key, path, why.deep || moved);
        let relist = self.tree[key].dir_mut().is_some_and(|dir| mem::take(&mut dir.relist));
        if relist && !(why.deep || moved) {
          self.tree.queue_scan(key, Depth::Shallow);
        }
        if id != old {
          self.look_again_at_a_new_dir(key);
        }
      }
    }
  }

  /// On Windows a junction or a directory link is a plain directory first, and the second step
  /// gives no record that the backend asks for: a directory that the tree did not know is
  /// looked at once more at the next flush, so that a link is seen within the poll interval.
  fn look_again_at_a_new_dir(&mut self, key: DirKey) {
    if self.options.two_step_links {
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
  /// that the filter ignores, is missing. `may_wait`: a new name may wait for a listing of its
  /// directory instead of asking the disk how it is spelled.
  fn read_entry(
    &mut self,
    parent: DirKey,
    key: DirKey,
    path: &Path,
    listed: Listed,
    may_wait: bool,
  ) -> Found {
    let node = &self.tree[key];
    let (registered, is_ancestor, is_path) =
      (node.is_registered(), node.is_ancestor(), node.is_path());
    let is_new = matches!(listed, Listed::NotAsked) && matches!(node.kind, Kind::Missing);
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
        Listed::Dir(_) => Some(FileType::Dir),
        Listed::NotAsked | Listed::NotNamed => None,
      };
      if kind.is_some_and(|kind| self.ignores(key, path, entry_kind(kind))) {
        return Found::Missing;
      }
    }

    // In a strict directory only a twin in another case puts the proof in doubt.
    let unsure = in_complete
      && if self.tree.is_loose(parent) {
        self.spelling_is_unsure(key, listed)
      } else {
        self.tree.has_case_twin(key) || matches!(listed, Listed::NotNamed)
      };
    if let Listed::File(file) = listed
      && !registered
      && !unsure
    {
      return Found::File(FileType::File, file);
    }
    if let Listed::Dir(id) = listed
      && !registered
      && !unsure
    {
      return Found::Dir { id, complete: true, relinked: false };
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
        other = match self.other_spelling(parent, key, path, &meta, is_new, false) {
          Ok(Spelled::Same | Spelled::Later) => None,
          Ok(Spelled::Other(name)) => Some(name),
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
    }

    let inside = is_path || (in_complete && other.is_none());
    let kind = entry_kind(meta.kind);
    let visible = inside && !self.ignores(key, path, kind);
    let found = match kind {
      // A directory on the way to a watched path is never filtered.
      EntryKind::Dir if is_ancestor || visible => {
        Found::Dir { id: meta.dir_id, complete: visible, relinked }
      }
      EntryKind::File if visible => Found::File(meta.kind, Fingerprint::of(&meta)),
      _ => return other.map_or(Found::Missing, Found::OtherSpelling),
    };
    // Asked after the filter: an ignored entry costs no question.
    if !registered && unsure {
      match self.other_spelling(parent, key, path, &meta, is_new, may_wait) {
        Ok(Spelled::Same) => {}
        Ok(Spelled::Other(name)) => return Found::OtherSpelling(name),
        Ok(Spelled::Later) => return Found::Missing,
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

  /// The name the disk has for the entry, when it is another one than the node has. On a disk that
  /// can be strict, a new name with no twin proves the rule first (design.md 5.2).
  fn other_spelling(
    &mut self,
    parent: DirKey,
    key: DirKey,
    path: &Path,
    meta: &Meta,
    is_new: bool,
    may_wait: bool,
  ) -> io::Result<Spelled> {
    let spelling = self.tree[parent].dir().map(|dir| dir.spelling);
    let can_be_strict = !self.options.loose_spelling;
    if can_be_strict && spelling != Some(Spelling::Loose) {
      let can_tell = is_new && !self.tree.has_twin(key);
      match can_tell.then(|| self.tells_spellings_apart(path, meta)).flatten() {
        Some(true) => {
          self.set_spelling(parent, Spelling::Strict);
          return Ok(Spelled::Same);
        }
        Some(false) => self.set_spelling(parent, Spelling::Loose),
        None if spelling == Some(Spelling::Unknown) => {
          self.tree.queue_scan(parent, Depth::Names);
          self.set_spelling(parent, Spelling::Asked);
        }
        None => {}
      }
    }
    // A burst of new names in a loose directory: the disk is asked how it spells the first one
    // (on Linux the question lists the directory), and one listing of the directory spells the
    // others. Not inside a scan of the directory: it listed it, or it could not.
    if may_wait
      && is_new
      && self.scanning != Some(parent)
      && self.tree[parent].is_complete()
      && self.tree.is_loose(parent)
    {
      if self.spelling_asked == Some(parent) {
        self.tree.queue_scan(parent, Depth::Names);
        return Ok(Spelled::Later);
      }
      self.spelling_asked = Some(parent);
    }
    let name = self.fs.name_on_disk(path)?;
    let other = Some(name).filter(|name| name != self.tree.name(key));
    if can_be_strict && other.is_some() {
      self.set_spelling(parent, Spelling::Loose);
    }
    Ok(other.map_or(Spelled::Same, Spelled::Other))
  }

  /// Keeps what a look found out. A proof of strict that is taken back costs one listing: the
  /// looks before took the disk for a strict one. A proof of loose is kept.
  pub(super) fn set_spelling(&mut self, dir: DirKey, spelling: Spelling) {
    let Some(kept) = self.tree[dir].dir_mut() else { return };
    if kept.spelling == Spelling::Loose {
      return;
    }
    let was = mem::replace(&mut kept.spelling, spelling);
    if was == Spelling::Strict && spelling == Spelling::Loose {
      self.tree.queue_scan(dir, Depth::Names);
    }
  }

  /// Whether the directory of `path` tells two spellings of a name apart, tried with the other
  /// case of the ASCII letters of the entry. `Some(true)` needs a third look that finds the entry
  /// as it was: a change between the looks is no proof. `None`: nothing could be tried, or the
  /// other spelling found another entry: a twin on a strict disk, or a disk that gives each
  /// spelling of one entry a number of its own (vboxsf, sshfs), which is no proof either way.
  pub(super) fn tells_spellings_apart(&self, path: &Path, meta: &Meta) -> Option<bool> {
    let name = path.file_name().and_then(OsStr::to_str)?;
    if !name.bytes().any(|byte| byte.is_ascii_alphabetic()) {
      return None;
    }
    let other: String = name.chars().map(swap_case).collect();
    match self.fs.lstat(&with_name(path, other)) {
      Ok(found) => return (found.file_id == meta.file_id).then_some(false),
      Err(error) if error.kind() == io::ErrorKind::NotFound => {}
      Err(_) => return None,
    }
    let same = |again: Meta| {
      (again.kind, again.file_id, again.modified, again.changed, again.size)
        == (meta.kind, meta.file_id, meta.modified, meta.changed, meta.size)
    };
    self.fs.lstat(path).is_ok_and(same).then_some(true)
  }

  /// Whether the disk has to say how it spells the name, in a complete directory that counts as
  /// loose (design.md 5.2).
  fn spelling_is_unsure(&self, key: DirKey, listed: Listed) -> bool {
    let node = &self.tree[key];
    let was_entry = match listed {
      Listed::Named(_) | Listed::File(_) | Listed::Dir(_) => return false,
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
    written: bool,
  ) {
    // A held node still has its file.
    let old = match self.tree[key].kind {
      Kind::File(old) => Some((old, self.tree.content(key))),
      _ => None,
    };
    let same = old.is_some_and(|(old, _)| old == fingerprint);
    let young = kind != FileType::Other && self.is_young(key, fingerprint.modified, same);
    // Compared when the fingerprint is the same, and one last time when the file is old.
    let kept = old.and_then(|(old, content)| content.filter(|_| old == fingerprint));
    let compare = matches!(kept, Some(Content::Hash(_)));
    // The version of the disk stands in for the hash where an OS watch reports writes. Not
    // under the poll: the version also moves with what is no write.
    let asks_version = kind == FileType::File
      && (young || matches!(kept, Some(Content::Version(_))))
      && !self.only_polled(key);
    let version = if asks_version { self.fs.version(path) } else { None };
    let hash = if (young && version.is_none()) || compare {
      // The reads are timed apart: the rest after a poll counts them once (design.md 5.13).
      let started = self.fs.ticks();
      let hashed = self.content_hash(path, kind, fingerprint.size);
      self.read_took += self.fs.ticks().saturating_sub(started);
      match hashed {
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
    let update_waits = young && hash.is_none() && self.only_polled(key);

    let changed = if let Some((old, content)) = old {
      written
        || old != fingerprint
        || match content {
          None => false,
          Some(Content::Hash(old)) => hash != Some(old),
          Some(Content::Version(old)) => version != Some(old),
          Some(Content::Unknown) => !update_waits,
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
    if node.is_held() {
      self.tree.unhold(key);
    }
    let content = match (version, hash) {
      _ if !young => None,
      (Some(version), _) => Some(Content::Version(version)),
      (None, Some(hash)) => Some(Content::Hash(hash)),
      (None, None) => Some(Content::Unknown),
    };
    self.tree.set_content(key, content);
  }

  /// A hash of the content of a regular file, or of the target of a link. `None`: nothing to
  /// compare.
  fn content_hash(&self, path: &Path, kind: FileType, size: u64) -> io::Result<Option<u64>> {
    match kind {
      FileType::File if size <= HASH_LIMIT => self.fs.hash(path, HASH_LIMIT),
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
    if age < nanos_of(YOUNG) {
      return true;
    }
    if age >= nanos_of(YOUNG_COARSE) {
      return false;
    }
    let whole_seconds = modified >= 0 && modified % nanos_of(Duration::from_secs(1)) == 0;
    whole_seconds || self.only_polled(key)
  }

  /// Whether the directory the tree has at the node is on disk now. `path` names an entry in it.
  fn is_on_disk(&self, key: DirKey, path: &Path) -> bool {
    let id = self.tree[key].dir().map(|dir| dir.id);
    let now = path.parent().and_then(|dir| self.fs.stat(dir).ok());
    now.is_some_and(|now| now.kind == FileType::Dir && Some(now.dir_id) == id)
  }

  /// Whether only the poll looks at the entry: its directory is polled, or it is a link.
  fn only_polled(&self, key: DirKey) -> bool {
    let watch = self.tree.parent(key).map_or(Watch::None, |parent| self.tree[parent].watch());
    !matches!(watch, Watch::Watched | Watch::WatchedTree | Watch::FromAbove)
      || self.tree.link(key).is_some()
  }
}

/// The letter in the other case, if it is an ASCII letter.
pub(super) fn swap_case(letter: char) -> char {
  if letter.is_ascii_lowercase() {
    letter.to_ascii_uppercase()
  } else {
    letter.to_ascii_lowercase()
  }
}

/// Whether an error of the disk says "nothing is at this path".
pub(super) fn is_missing(error: &io::Error) -> bool {
  matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
}

/// Whether the error is a link loop. Its kind (`FilesystemLoop`) has no stable name, so the
/// number of the OS is read where it is known.
fn is_link_loop(error: &io::Error) -> bool {
  #[cfg(any(target_os = "linux", target_os = "android"))]
  const LINK_LOOP: Option<i32> = Some(libc::ELOOP);
  #[cfg(target_vendor = "apple")]
  const LINK_LOOP: Option<i32> = Some(62);
  #[cfg(windows)]
  const LINK_LOOP: Option<i32> =
    Some(windows_sys::Win32::Foundation::ERROR_CANT_RESOLVE_FILENAME.cast_signed());
  #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple", windows)))]
  const LINK_LOOP: Option<i32> = None;

  match LINK_LOOP {
    Some(code) => error.raw_os_error() == Some(code),
    None => format!("{:?}", error.kind()) == "FilesystemLoop",
  }
}

fn nanos_of(time: Duration) -> i64 {
  i64::try_from(time.as_nanos()).unwrap_or(i64::MAX)
}

fn hash_of(path: &Path) -> u64 {
  let mut hasher = FxHasher::default();
  path.hash(&mut hasher);
  hasher.finish()
}

fn entry_kind(kind: FileType) -> EntryKind {
  if kind == FileType::Dir { EntryKind::Dir } else { EntryKind::File }
}
