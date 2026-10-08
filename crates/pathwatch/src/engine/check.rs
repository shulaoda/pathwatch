//! One entry: `check` reads what is at a path, compares it with what the tree had there, and
//! changes the node. The `match` in `apply` is the heart of the engine.

use std::{
  hash::{Hash, Hasher},
  io,
  path::{Path, PathBuf},
  time::{Duration, SystemTime},
};

use rustc_hash::FxHasher;

use super::Engine;
use crate::{
  backend::{Backend, DirKey, Want},
  config::EntryKind,
  error::ErrorKind,
  fs::{DirId, FileType, Fs, Meta},
  tree::{Added, Content, Depth, DirTimes, Fingerprint, Kind, Mark, Tree, Watch},
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
  File(Meta),
  Dir {
    id: DirId,
    times: DirTimes,
    complete: bool,
    /// The name is a link now and was none at the last check, or the other way around, or the
    /// link leads to another real path. The directory can be the same one all the same.
    relinked: bool,
  },
  /// The disk did not answer, or the look has to wait. The tree stays as it is.
  Unknown,
}

/// What the tree had at a node before the check, also while the node is held.
enum Had {
  Nothing,
  File,
  Dir { id: DirId, complete: bool },
}

impl<B: Backend, F: Fs> Engine<B, F> {
  /// Brings the tree in line with the disk for the node `key`. `path` is the path of the
  /// directory that holds it, and `listed` is the type that a listing of that directory gave
  /// for it.
  ///
  /// It never goes into a directory: a scan is queued for that. It makes no event: a node
  /// that changed is put on the list `touched`, and `finish` reads it.
  pub(super) fn check(
    &mut self,
    key: DirKey,
    path: &mut PathBuf,
    listed: Option<FileType>,
    why: Mark,
  ) {
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
    // paths at the name and below it are dropped when the flush ends.
    if registered
      && matches!(found, Found::Missing | Found::File(_))
      && self.is_ignored_target(parent)
    {
      self.dropped.push(key);
      path.pop();
      return;
    }
    self.apply(key, path, found, had, why);

    // A registered name with a file or nothing at it makes its directory want the contents: a
    // file can come there, or a directory with all that is below it at once.
    let wants_more = below_names_only && !self.tree[key].is_dir();
    self.tree.prune(key);
    // `pop` does not take a top directory (`/`, `C:\\`) away.
    if !path.pop() {
      path.clear();
    }
    if wants_more && !self.is_ignored_target(parent) {
      // This queues a scan of the directory: the path is looked at once more when the wider
      // watch is live.
      self.ask(parent, path, Want::Contents);
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

      (Found::File(meta), Had::File | Had::Nothing) => {
        self.store_file(key, path, &meta, why.written);
      }
      (Found::File(meta), Had::Dir { .. }) => {
        // A change of kind is never an `Update`: the files below are gone.
        self.vanish(key);
        self.store_file(key, path, &meta, why.written);
      }

      (Found::Dir { id, times, complete, relinked }, Had::Nothing) => {
        let baseline = self.is_baseline_dir(key, relinked);
        self.appear(key, path, id, complete, baseline);
        self.was_renamed(key, times);
      }
      (Found::Dir { id, times, complete, relinked }, Had::File) => {
        let baseline = self.is_baseline_dir(key, relinked);
        // A change of kind is never an `Update`: the file is gone.
        self.forget_file(key);
        self.appear(key, path, id, complete, baseline);
        self.was_renamed(key, times);
      }
      (
        Found::Dir { id, times, complete, relinked },
        Had::Dir { id: old, complete: was_complete },
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
        if complete && !was_complete {
          self.make_complete(key, relinked);
        } else if complete && relinked && self.tree[key].added() == Added::Registered {
          // The running `watch` follows this link for the first time: what is behind it is
          // its baseline. (`register` put the node on the list `touched`.)
          self.tree[key].set_baseline(true);
        }
        // Watch it again and scan it deep when nothing tells what happened in it meanwhile:
        // it was away (held, or renamed), it is another directory, or its link changed.
        let moved = self.was_renamed(key, times) || was_held || id != old || relinked;
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
  fn read_entry(
    &mut self,
    parent: DirKey,
    key: DirKey,
    path: &Path,
    listed: Option<FileType>,
  ) -> Found {
    let node = &self.tree[key];
    let (registered, is_ancestor) = (node.is_registered(), node.is_ancestor());
    let inside = node.is_path() || self.tree[parent].is_complete();

    if !registered {
      if !inside {
        // No watched path can see the entry. Or the directory is to be complete and the tree
        // is behind: then the look that makes it complete comes first, and this one waits.
        return if self.tracks_every_entry(parent) { Found::Unknown } else { Found::Missing };
      }
      // Asked before the `lstat`, so that an ignored entry costs no system call.
      if let Some(kind) = listed.map(entry_kind)
        && self.ignores(key, path, kind)
      {
        return Found::Missing;
      }
    }

    let mut meta = match self.fs.lstat(path) {
      Ok(meta) => meta,
      Err(error) => return self.disk_error(parent, path, error),
    };
    let mut relinked = false;
    if registered {
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
            return found;
          }
        };
      }
    }

    let kind = entry_kind(meta.kind);
    let visible = inside && !self.ignores(key, path, kind);
    match kind {
      // A directory on the way to a watched path is never filtered.
      EntryKind::Dir if is_ancestor || visible => {
        let times = DirTimes::of(&meta);
        Found::Dir { id: meta.dir_id, times, complete: visible, relinked }
      }
      EntryKind::File if visible => Found::File(meta),
      _ => Found::Missing,
    }
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
  fn store_file(&mut self, key: DirKey, path: &Path, meta: &Meta, written: bool) {
    let fingerprint = Fingerprint::of(meta);
    // What the tree has of the file. A held node still has it.
    let old = match self.tree[key].kind {
      Kind::File(old) => Some((old, self.tree.content(key))),
      _ => None,
    };
    let young = meta.kind != FileType::Other && self.is_young(key, meta.modified);
    // A stored hash is compared one last time when the file is not young any more.
    let compare = old.is_some_and(|(old, content)| {
      old == fingerprint && matches!(content, Some(Content::Hash(_)))
    });
    let hash = if young || compare {
      match self.content_hash(path, meta) {
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
  fn content_hash(&self, path: &Path, meta: &Meta) -> io::Result<Option<u64>> {
    match meta.kind {
      FileType::File if meta.size <= HASH_LIMIT => self.fs.hash(path).map(Some),
      FileType::Symlink => self.fs.link_target(path).map(|target| Some(hash_of(&target))),
      _ => Ok(None),
    }
  }

  /// Whether the file at the node `key` is young: a second write can leave the modification
  /// time as it is, so an equal fingerprint proves nothing at the next check.
  ///
  /// The long time is for a time stamp in whole seconds, and for a file that only the poll
  /// looks at: it has no "written" hint to make up for a wrong guess.
  fn is_young(&self, key: DirKey, modified: SystemTime) -> bool {
    // A time in the future: the file is as young as can be.
    let Ok(age) = self.flush_time.duration_since(modified) else { return true };
    if age < YOUNG {
      return true;
    }
    if age >= YOUNG_COARSE {
      return false;
    }
    let whole_seconds =
      modified.duration_since(SystemTime::UNIX_EPOCH).is_ok_and(|time| time.subsec_nanos() == 0);
    whole_seconds || self.only_polled(key)
  }

  /// Keeps the times of the directory at the node `key` when nothing but a poll tells that
  /// the directory itself was renamed. Says whether the times tell so: the change time moved
  /// since the last look, and the modification time did not. A directory that was renamed
  /// away and back is the same one, but a watch by path below it heard nothing meanwhile.
  fn was_renamed(&mut self, key: DirKey, times: DirTimes) -> bool {
    // A top directory does not move. For every other one the times are kept from its first
    // look on, also before any watch stands below it: the first watch can come later in the
    // same flush, and a rename right after that must not be missed.
    let moves = self.tree.parent(key) != Some(Tree::ROOT);
    let new = (moves && self.only_polled(key)).then_some(times);
    let old = self.tree.set_times(key, new);
    old
      .zip(new)
      .is_some_and(|(old, new)| old.changed != new.changed && old.modified == new.modified)
  }

  /// Whether no OS watch tells about the entry at the node `key`: its directory is polled, or
  /// the node is a link, and nothing watches what is behind a link.
  fn only_polled(&self, key: DirKey) -> bool {
    let watch = self.tree.parent(key).map_or(Watch::None, |parent| self.tree[parent].watch());
    !matches!(watch, Watch::Watched | Watch::WatchedTree | Watch::FromAbove)
      || self.tree.link(key).is_some()
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

fn hash_of(path: &Path) -> u64 {
  let mut hasher = FxHasher::default();
  path.hash(&mut hasher);
  hasher.finish()
}

fn entry_kind(kind: FileType) -> EntryKind {
  if kind == FileType::Dir { EntryKind::Dir } else { EntryKind::File }
}
