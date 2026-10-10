//! One entry: `check` reads what is at a path, compares it with what the tree had there, and
//! changes the node. The `match` in `apply` is the heart of the engine.

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
  fs::{DirId, Entry, FileType, Fs, Meta},
  tree::{Added, Content, Depth, Fingerprint, Kind, Mark, Spelling, Watch, nanos},
};

/// How long a file is young: two writes inside this time can leave the same modification time.
/// Most file systems count in steps far below 20 ms.
const YOUNG: Duration = Duration::from_millis(20);
/// The same for a file system that counts in whole seconds (FAT: two of them), and for a file
/// that only the poll looks at.
const YOUNG_COARSE: Duration = Duration::from_secs(2);
/// A bigger file is not hashed.
const HASH_LIMIT: u64 = 16 * 1024 * 1024;

/// What is at a path, after the rules of what counts.
pub(super) enum Found {
  Missing,
  /// The disk has the entry under this other spelling of the name: the directory has no
  /// entry with the name of the node. A second look would say the same, so what the tree has
  /// here is gone at once.
  OtherSpelling(OsString),
  File(FileType, Fingerprint),
  Dir {
    id: DirId,
    complete: bool,
    /// The name is a link now and was none at the last check, or the other way around, or the
    /// link leads to another real path. The directory can be the same one all the same.
    relinked: bool,
  },
  /// The disk did not answer, or the look has to wait. The tree stays as it is.
  Unknown,
}

/// What a listing of the directory said about a node whose check runs in a scan.
#[derive(Clone, Copy)]
pub(super) enum Listed {
  /// Nothing: the check comes from a mark, or the directory was not listed.
  NotAsked,
  /// The listing named the entry, with its kind if a listing gives one.
  Named(Option<FileType>),
  /// The listing named the entry, a regular file, with all that a look at it reads.
  File(Fingerprint),
  /// The whole directory was listed, and the entry was not named.
  NotNamed,
}

impl Listed {
  pub(super) fn is_named(self) -> bool {
    matches!(self, Self::Named(_) | Self::File(_))
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
    }
  }
}

/// What the tree had at a node before the check, also while the node is held.
enum Had {
  Nothing,
  File,
  Dir { id: DirId, complete: bool },
}

impl<B: Backend, F: Fs> Engine<B, F> {
  /// Brings the tree in line with the disk for the node `key`. `path` is the path of the
  /// directory that holds it, and `listed` is what a listing of that directory said about it.
  ///
  /// It never goes into a directory: a scan is queued for that. It makes no event: a node
  /// that changed is put on the list `touched`, and `finish` reads it.
  pub(super) fn check(&mut self, key: DirKey, path: &mut PathBuf, listed: Listed, why: Mark) {
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
      Kind::Dir(dir) => Had::Dir { id: dir.id, complete: dir.complete },
    };

    path.push(self.tree.name(key));
    let found = self.read_entry(parent, key, path, listed);
    // A registered name with a file or nothing at it is served by the watch of the directory
    // above: that one is its watch target. If the filter ignores the target, the watched
    // paths at the name and below it are dropped when the flush ends. The target is asked
    // from the disk: this look can come before the one that finds the directory gone, and
    // then the target is a directory further up.
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

    // A registered name with a file or nothing at it makes its directory want the contents: a
    // file can come there, or a directory with all that is below it at once.
    let wants_more = below_names_only && !self.tree[key].is_dir();
    // The node goes if nothing is at it and nothing needs it. A marked node that can be the
    // same name was unsure of its spelling because the tree had this one too
    // (`Tree::has_twin`): its look must not come after this node is gone.
    let can_go = !registered && matches!(self.tree[key].kind, Kind::Missing);
    let twins = if can_go { self.tree.marked_twins(key) } else { Vec::new() };
    self.tree.prune(key);
    let twins = if self.tree.get(key).is_none() { twins } else { Vec::new() };
    // `pop` does not take a top directory (`/`, `C:\\`) away.
    if !path.pop() {
      path.clear();
    }
    if wants_more && !self.is_ignored_target(parent) {
      // This queues a scan of the directory: the path is looked at once more when the wider
      // watch is live.
      self.ask(parent, path, Want::Contents);
    }
    // The entry is on the disk under another name. A node that the tree has for that name has
    // a look of its own. If there is none, nothing else brings one in this flush.
    if let Some(name) = on_disk
      && self.tree.child(parent, &name).is_none()
    {
      let node = self.tree.insert_child(parent, &name);
      self.check(node, path, Listed::NotAsked, Mark::DEEP);
    }
    for twin in twins {
      if let Some(mark) = self.tree.take_mark(twin) {
        self.check(twin, path, Listed::NotNamed, mark);
      }
    }
  }

  /// What was found on disk against what the tree had: one arm for each case. `path` is the
  /// path of the node `key`, and `why` is the mark that asked for the check.
  fn apply(&mut self, key: DirKey, path: &mut PathBuf, found: Found, had: Had, why: Mark) {
    match (found, had) {
      // The disk did not answer. Nothing is done, and the same mark waits for the next flush.
      (Found::Unknown, _) => self.tree.mark(key, why),

      (Found::Missing, Had::Nothing) => {}
      // Held first; gone for good when the second look finds nothing too.
      (Found::Missing, Had::File | Had::Dir { .. }) => self.missing(key),
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
      }
      (Found::Dir { id, complete, relinked }, Had::File) => {
        let baseline = self.is_baseline_dir(key, relinked);
        // A change of kind is never an `Update`: the file is gone.
        self.forget_file(key);
        self.appear(key, path, id, complete, baseline);
      }
      (Found::Dir { id, complete, relinked }, Had::Dir { id: old, complete: was_complete }) => {
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
        if complete && !was_complete {
          self.make_complete(key, relinked);
        } else if was_complete && !complete && self.tree[key].is_unlisted() {
          // The directory above lists the entry under another name now. Only what a watched
          // path below covers stays, and the files that were its entries are gone.
          self.uncover(key, true, false);
        } else if complete && relinked && self.tree[key].added() == Added::Registered {
          // The running `watch` follows this link for the first time: what is behind it is
          // its baseline. (`register` put the node on the list `touched`.)
          self.tree[key].set_baseline(true);
        }
        // Watch it again and scan it deep when nothing tells what happened in it meanwhile:
        // it was away (held), it is another directory, or its link changed.
        let moved = was_held || id != old || relinked;
        self.still_there(key, path, why.deep || moved);
      }
    }
  }

  /// A path above became a watched path: from now on the partial node `key` tracks every
  /// entry of its directory.
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

  /// Reads what is at `path`, the path of the node `key` in the directory `parent`, and applies
  /// what counts: an entry that no watched path covers, or that the filter ignores, is missing.
  fn read_entry(&mut self, parent: DirKey, key: DirKey, path: &Path, listed: Listed) -> Found {
    let node = &self.tree[key];
    let (registered, is_ancestor, is_path) =
      (node.is_registered(), node.is_ancestor(), node.is_path());
    let is_new = matches!(listed, Listed::NotAsked) && matches!(node.kind, Kind::Missing);
    let in_complete = self.tree[parent].is_complete();

    if !registered {
      if !in_complete {
        // No watched path can see the entry. Or the directory is to be complete and the tree
        // is behind: then the look that makes it complete comes first, and this one waits.
        return if self.tracks_every_entry(parent) { Found::Unknown } else { Found::Missing };
      }
      // Asked before the `lstat`, so that an ignored entry costs no system call.
      let kind = match listed {
        Listed::Named(kind) => kind,
        Listed::File(_) => Some(FileType::File),
        Listed::NotAsked | Listed::NotNamed => None,
      };
      if kind.is_some_and(|kind| self.ignores(key, path, entry_kind(kind))) {
        return Found::Missing;
      }
    }

    // In a directory that was proved strict, only a second name in another case of its
    // letters puts the proof in doubt: one that the tree has now, or one that it had when
    // this look was asked for (`check` gives `NotNamed` then).
    let unsure = in_complete
      && if self.tree.is_loose(parent) {
        self.spelling_is_unsure(key, listed)
      } else {
        self.tree.has_case_twin(key) || matches!(listed, Listed::NotNamed)
      };
    // The listing told all of the file, and nothing else is to ask about its name.
    if let Listed::File(file) = listed
      && !registered
      && !unsure
    {
      return Found::File(FileType::File, file);
    }
    let mut meta = match self.fs.lstat(path) {
      Ok(meta) => meta,
      Err(error) => return self.disk_error(parent, path, error),
    };
    // A registered name is the path that its caller gave. It is also an entry of the complete
    // directory above only if that directory lists it under this very name. That is asked
    // about the entry itself, so before a link is followed.
    let mut other = None;
    let mut relinked = false;
    if registered {
      if unsure {
        other = match self.other_spelling(parent, key, path, &meta, is_new) {
          Ok(other) => other,
          Err(error) => return self.disk_error(parent, path, error),
        };
      }
      self.tree[key].set_unlisted(other.is_some());
      // Only a registered node is followed. Nothing watches a link, so every poll checks it.
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
              // No answer: the tree stays as it is, the link too. The next look must still
              // see that the link changed, or a watch by path is never made again.
              self.tree.set_link(key, old);
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
    // A name that is not registered is asked after the filter: an ignored entry costs no
    // question.
    if !registered && unsure {
      match self.other_spelling(parent, key, path, &meta, is_new) {
        Ok(None) => {}
        Ok(Some(name)) => return Found::OtherSpelling(name),
        Err(error) => return self.disk_error(parent, path, error),
      }
    }
    found
  }

  /// The name that the disk has for the entry at `path`, when it is not the name of the node
  /// `key`. `meta` is what `lstat` found there. `is_new`: the tree had nothing at the name,
  /// and no listing was asked about it.
  ///
  /// Where a directory can be strict, the look tries to find that out first
  /// (`Tree::is_loose`). Only a look at a new name with no second spelling in the tree can:
  /// such a name is there because something made it. A name that the tree knew can be there
  /// for `lstat` alone. Measured on a folder that a Mac shares with Linux: after a rename
  /// away through another spelling, `lstat` still finds the old name, and nothing in the
  /// other case, as on a strict disk. When this look cannot tell and nothing is known, a
  /// listing of the directory is asked (`ask_for_the_rule`).
  fn other_spelling(
    &mut self,
    parent: DirKey,
    key: DirKey,
    path: &Path,
    meta: &Meta,
    is_new: bool,
  ) -> io::Result<Option<OsString>> {
    let can_be_strict = !self.options.loose_spelling;
    if can_be_strict {
      let can_tell = is_new && !self.tree.has_twin(key);
      match can_tell.then(|| self.tells_spellings_apart(path, meta)).flatten() {
        Some(true) => {
          self.set_spelling(parent, Spelling::Strict);
          return Ok(None);
        }
        Some(false) => self.set_spelling(parent, Spelling::Loose),
        None if self.tree[parent].dir().is_some_and(|dir| dir.spelling == Spelling::Unknown) => {
          self.tree.queue_scan(parent, Depth::Names);
        }
        None => {}
      }
    }
    let name = self.fs.name_on_disk(path)?;
    let other = Some(name).filter(|name| name != self.tree.name(key));
    if can_be_strict && other.is_some() {
      self.set_spelling(parent, Spelling::Loose);
    }
    Ok(other)
  }

  /// Keeps what a look found out about the directory `dir`. When a proof is taken back, the
  /// looks before this one took the disk for a strict one: the names of the directory are
  /// compared with one listing.
  pub(super) fn set_spelling(&mut self, dir: DirKey, spelling: Spelling) {
    let Some(kept) = self.tree[dir].dir_mut() else { return };
    let was = mem::replace(&mut kept.spelling, spelling);
    if was == Spelling::Strict && spelling == Spelling::Loose {
      self.tree.queue_scan(dir, Depth::Names);
    }
  }

  /// Whether the directory of `path` tells two spellings of a name apart. `meta` is the entry
  /// at `path`. The name of that entry is tried with the other case of its ASCII letters.
  /// `Some(false)`: the same entry is there. `Some(true)`: no entry, or another one, and the
  /// entry at `path` is still as it was at one more look (a change of the disk between the
  /// looks must not pass for a proof). `None`: nothing could be tried, or the disk changed.
  pub(super) fn tells_spellings_apart(&self, path: &Path, meta: &Meta) -> Option<bool> {
    let name = path.file_name().and_then(OsStr::to_str)?;
    if !name.bytes().any(|byte| byte.is_ascii_alphabetic()) {
      return None;
    }
    let other: String = name.chars().map(swap_case).collect();
    match self.fs.lstat(&path.with_file_name(other)) {
      Ok(found) if found.file_id == meta.file_id => return Some(false),
      Ok(_) => {}
      Err(error) if error.kind() == io::ErrorKind::NotFound => {}
      Err(_) => return None,
    }
    let same = |again: Meta| {
      (again.kind, again.file_id, again.modified, again.changed, again.size)
        == (meta.kind, meta.file_id, meta.modified, meta.changed, meta.size)
    };
    self.fs.lstat(path).is_ok_and(same).then_some(true)
  }

  /// Whether the disk has to say how it spells the name of the node `key`, where `lstat` just
  /// found an entry in a complete directory that counts as loose (`Tree::is_loose`).
  ///
  /// On such a disk `lstat` finds an entry under every spelling of its name, and the OS names
  /// an entry that is gone as the caller spelled it. So an entry that was found is under this
  /// name only if the disk spells it so, and that is not sure for a name that was no entry
  /// of the directory at the last look (`Node::is_entry`), and while the tree has two
  /// spellings of the name. A listing names an entry as the disk spells it: a node that it
  /// named is sure, and one that it did not name is not. A name with one spelling only
  /// (`Tree::has_spellings`) is always sure.
  ///
  /// An entry of the directory that is spelled in another way is not there under this name
  /// (`Found::OtherSpelling`). A registered name is the path of its caller and is there all
  /// the same, but then the directory above does not cover it (`Node::is_unlisted`).
  fn spelling_is_unsure(&self, key: DirKey, listed: Listed) -> bool {
    let node = &self.tree[key];
    let was_entry = match listed {
      Listed::Named(_) | Listed::File(_) => return false,
      Listed::NotNamed => false,
      Listed::NotAsked => node.is_entry(),
    };
    self.tree.has_spellings(key) && (!was_entry || self.tree.has_other_spelling(key))
  }

  /// A number for the real path behind the link at `path`.
  fn real_place(&self, path: &Path) -> u64 {
    self.fs.real_path(path).map_or(0, |real| hash_of(&real))
  }

  /// Turns an error of the disk into what it means for `path`. `owner` is the node that is
  /// polled until the error is gone.
  pub(super) fn disk_error(&mut self, owner: DirKey, path: &Path, error: io::Error) -> Found {
    if is_missing(&error) {
      return Found::Missing;
    }
    let (kind, found) = match error.kind() {
      // The path cannot be read. For events it is missing.
      io::ErrorKind::PermissionDenied => (ErrorKind::PermissionDenied, Found::Missing),
      // The same for a link loop. `io::ErrorKind::FilesystemLoop` cannot be named on stable
      // Rust, and the error number differs between systems.
      kind if format!("{kind:?}") == "FilesystemLoop" => (ErrorKind::Io, Found::Missing),
      _ => (ErrorKind::Io, Found::Unknown),
    };
    self.scan_had_error = true;
    if let Some(dir) = self.tree[owner].dir_mut()
      && !dir.has_error
    {
      dir.has_error = true;
      self.tree.list_if_polled(owner);
      self.report(kind, Some(path.to_path_buf()), Some(error));
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
    // What the tree has of the file. A held node still has it.
    let old = match self.tree[key].kind {
      Kind::File(old) => Some((old, self.tree.content(key))),
      _ => None,
    };
    let young = kind != FileType::Other && self.is_young(key, fingerprint.modified);
    // A stored hash is compared one last time when the file is not young any more.
    let compare = old.is_some_and(|(old, content)| {
      old == fingerprint && matches!(content, Some(Content::Hash(_)))
    });
    let hash = if young || compare {
      match self.content_hash(path, kind, fingerprint.size) {
        Ok(hash) => hash,
        // The entry went away after the `lstat`, or it is of another kind now. This check
        // says nothing, and the next flush looks again.
        Err(error) if is_missing(&error) => return self.tree.mark(key, Mark::default()),
        // A file that cannot be read has nothing to compare.
        Err(_) => None,
      }
    } else {
      None
    };
    // The file is young, nothing of it can be compared, and a poll looks at it again by
    // itself. Then a look that finds the same time stamp says nothing, and the first look
    // that finds the file old reports one `Update` for all of them. Where no poll looks, a
    // later look is not sure to come, so every look reports.
    let update_waits = young && hash.is_none() && self.only_polled(key);

    let changed = if let Some((old, content)) = old {
      written
        || old != fingerprint
        || match content {
          None => false,
          Some(Content::Hash(old)) => hash != Some(old),
          Some(Content::Unknown) => !update_waits,
        }
    } else {
      // The tree had no file here. If the handler believes in one all the same, the tree
      // forgot it earlier in this flush, and nothing says that this is the same file.
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
    // The file is there: if the node was held, its `Delete` is off.
    if node.is_held() {
      self.tree.unhold(key);
    }
    let content = match hash {
      Some(hash) if young => Some(Content::Hash(hash)),
      None if young => Some(Content::Unknown),
      _ => None,
    };
    self.tree.set_content(key, content);
  }

  /// A hash of what can be compared of the entry at `path`: the content of a regular file
  /// that is not too big, or the target of a link (a link is never opened). `None`: there is
  /// nothing to compare.
  fn content_hash(&self, path: &Path, kind: FileType, size: u64) -> io::Result<Option<u64>> {
    match kind {
      FileType::File if size <= HASH_LIMIT => self.fs.hash(path).map(Some),
      FileType::Symlink => self.fs.link_target(path).map(|target| Some(hash_of(&target))),
      _ => Ok(None),
    }
  }

  /// Whether the file at the node `key` is young: a second write can leave the modification
  /// time as it is, so an equal fingerprint proves nothing at the next check.
  ///
  /// The long time is for a time stamp in whole seconds, and for a file that only the poll
  /// looks at: it has no "written" hint to make up for a wrong guess.
  fn is_young(&self, key: DirKey, modified: i64) -> bool {
    // Below zero for a time in the future: the file is as young as can be.
    let age = nanos(self.flush_time).saturating_sub(modified);
    if age < nanos_of(YOUNG) {
      return true;
    }
    if age >= nanos_of(YOUNG_COARSE) {
      return false;
    }
    let whole_seconds = modified >= 0 && modified % nanos_of(Duration::from_secs(1)) == 0;
    whole_seconds || self.only_polled(key)
  }

  /// Whether the directory that the tree has at the node `key` is on disk now. `path` is the
  /// path of a name in it.
  fn is_on_disk(&self, key: DirKey, path: &Path) -> bool {
    let id = self.tree[key].dir().map(|dir| dir.id);
    let now = path.parent().and_then(|dir| self.fs.stat(dir).ok());
    now.is_some_and(|now| now.kind == FileType::Dir && Some(now.dir_id) == id)
  }

  /// Whether no OS watch tells about the entry at the node `key`: its directory is polled, or
  /// the node is a link, and nothing watches what is behind a link.
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
  matches!(
    error.kind(),
    io::ErrorKind::NotFound
      | io::ErrorKind::NotADirectory
      | io::ErrorKind::InvalidFilename
      | io::ErrorKind::InvalidInput
  )
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
