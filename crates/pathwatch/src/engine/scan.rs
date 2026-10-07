//! One directory: the scan that checks every entry in it, and what is done when a directory
//! appears, is still there, or vanishes.

use std::path::PathBuf;

use super::{
  Engine,
  check::{Found, is_missing},
};
use crate::{
  backend::{Backend, DirKey},
  fs::{DirId, FileType, Fs},
  tree::{Depth, Dir, Kind, Mark, Watch},
};

/// One node to check in a scan, with the type that the listing gave for it. `None`: the tree
/// knew the node, and the listing was not asked about it.
pub(super) type ScanEntry = (DirKey, Option<FileType>);

/// The scan of one directory.
impl<B: Backend, F: Fs> Engine<B, F> {
  /// Checks every node of the directory `key`: what the tree knows there, and what a listing
  /// shows on top of that (if the node is complete). A deep scan also makes every directory
  /// below watched again and scanned. A scan of the names checks only the nodes whose names
  /// the tree and the listing do not both have.
  ///
  /// `path` and `names` are buffers. What they hold does not matter.
  pub(super) fn scan(
    &mut self,
    key: DirKey,
    depth: Depth,
    path: &mut PathBuf,
    names: &mut Vec<ScanEntry>,
  ) {
    // A node above is held: nothing below it is looked at before its second look.
    if self.tree.frozen(key) {
      return;
    }
    self.tree.path_into(key, path);
    self.scan_had_error = false;
    names.clear();
    names.extend(self.tree.children(key).map(|child| (child, None)));
    let known = names.len();
    self.listed.clear();
    let listing =
      if self.tree[key].is_complete() { self.list_new(key, path, names) } else { Listing::None };
    if listing == Listing::Stop {
      return;
    }
    let names_are_known = listing == Listing::Whole && self.options.loose_spelling;
    let names_only = depth == Depth::Names && names_are_known;

    let why = if depth == Depth::Deep { Mark::DEEP } else { Mark::default() };
    for (index, &(child, kind)) in names.iter().enumerate() {
      if names_are_known && self.has_another_spelling(child, path) {
        continue;
      }
      // The tree knew the node, and the listing names it: it is as it was.
      if names_only && index < known && self.listed.contains(&child) {
        continue;
      }
      // A check frees no node but its own.
      self.check(child, path, kind, why);
    }
    // This runs after the commit, so a watch that is said to have failed did fail.
    if let Some(dir) = self.tree[key].dir_mut() {
      // A scan of the names did not look at every entry: it cannot say that an error is over.
      let still = names_only && dir.has_error;
      dir.has_error = still || self.scan_had_error || dir.watch == Watch::Failed;
    }
    self.tree.list_if_polled(key);
  }

  /// Lists the directory `key` and adds to `names` a new node for each entry that the tree
  /// does not know yet. The check of such a node frees it again if the entry does not count.
  /// With loose spelling, `Engine::listed` gets every node that the listing named.
  fn list_new(&mut self, key: DirKey, path: &mut PathBuf, names: &mut Vec<ScanEntry>) -> Listing {
    let known = names.len();
    let tree = &mut self.tree;
    let mut listed = self.options.loose_spelling.then_some(&mut self.listed);
    let result = self.fs.read_dir(path, &mut |name, kind| {
      let node = tree.child(key, name).unwrap_or_else(|| {
        let new = tree.insert_child(key, name);
        names.push((new, kind));
        new
      });
      if let Some(listed) = &mut listed {
        listed.insert(node);
      }
    });
    let Err(error) = result else { return Listing::Whole };

    // What the listing gave before it failed is forgotten.
    for (new, _) in names.drain(known..) {
      self.tree.prune(new);
    }
    if is_missing(&error) {
      self.check_from_parent(key, path);
      return Listing::Stop;
    }
    if matches!(self.disk_error(key, path, error), Found::Unknown) {
      // The disk did not answer. The parent checks this directory at the next flush, and
      // then all of it is scanned: what this scan was for is not forgotten.
      if self.tree.parent(key).is_some() {
        self.tree.mark(key, Mark::DEEP);
      }
      return Listing::Stop;
    }
    // Permission denied: nothing in here can be read, so every node is checked as missing.
    Listing::None
  }

  /// The directory `key` is gone. Its parent finds out what is at its path now.
  fn check_from_parent(&mut self, key: DirKey, path: &mut PathBuf) {
    let Some(parent) = self.tree.parent(key) else { return };
    self.tree.path_into(parent, path);
    self.check(key, path, None, Mark::default());
  }

  /// With loose spelling, `lstat` finds an entry under every spelling of its name, but a
  /// listing names it in one way only: as the disk spells it. So the listing is the truth for
  /// the name of an entry that was found (a registered name keeps the spelling of the caller,
  /// and `lstat` is asked about it).
  ///
  /// `true`: the listing did not name the entry at the node `child`, the disk still finds one
  /// there, and says that it spells the name in another way. Under this name the entry is gone
  /// at once. It is not held, because the second look would find it again.
  ///
  /// `path` is the path of the directory. It is the same after the call.
  fn has_another_spelling(&mut self, child: DirKey, path: &mut PathBuf) -> bool {
    let node = &self.tree[child];
    if node.is_registered() || self.listed.contains(&child) {
      return false;
    }
    path.push(self.tree.name(child));
    let another = match self.fs.lstat(path) {
      // The real path of a link is the path of its target, so it does not tell how the disk
      // spells the link: the listing is believed.
      Ok(meta) if meta.kind == FileType::Symlink => true,
      // The disk is asked for its spelling: an entry that is found under its own name was
      // missed by the listing, or it came after the listing. That one is there.
      Ok(_) => {
        self.fs.real_path(path).is_ok_and(|real| real.file_name() != Some(self.tree.name(child)))
      }
      Err(_) => false,
    };
    path.pop();
    if another {
      // A mark would ask `lstat` about this name once more.
      self.tree.take_mark(child);
      self.gone(child);
      self.tree.prune(child);
    }
    another
  }
}

/// What a scan got from the listing of its directory.
#[derive(PartialEq, Eq)]
enum Listing {
  /// Every entry of the directory was named.
  Whole,
  /// There is none: the node is partial, or the directory cannot be read.
  None,
  /// The scan ends here, because the directory is gone or the disk did not answer.
  Stop,
}

/// What `check` does with a directory that it found, or that is gone.
impl<B: Backend, F: Fs> Engine<B, F> {
  /// A directory is on disk at the node `key`, and the tree had none there. `baseline`: what
  /// is in it is the baseline of the running `watch` call.
  pub(super) fn appear(
    &mut self,
    key: DirKey,
    path: &mut PathBuf,
    id: DirId,
    complete: bool,
    baseline: bool,
  ) {
    let node = &mut self.tree[key];
    node.kind = Kind::Dir(Dir::new(id, complete));
    // Only a complete node finds entries by itself.
    node.set_baseline(complete && baseline);
    if node.baseline() {
      self.touched.push(key);
    }
    self.arm(key, path, false);
    // Also when a watch above covers it and none was asked for.
    self.tree.queue_scan(key, Depth::Shallow);
  }

  /// A directory is still on disk at the node `key`.
  pub(super) fn still_there(&mut self, key: DirKey, path: &mut PathBuf, deep: bool) {
    if deep && self.has_a_watch_to_give_back(key) {
      // A watch above covers this directory, and everything below it is read again after the
      // next commit. So its own watch can go then: where the OS watches are does not depend on
      // which of the two paths was watched first.
      self.covered.push(key);
    } else {
      // Also when not `deep`: the node may want more now, or the watch above that covered it
      // may be gone or may not reach it any more.
      self.arm(key, path, deep);
    }
    if deep {
      self.tree.queue_scan(key, Depth::Deep);
    }
  }

  /// The directory at the node `key` is gone for good, or a file took its place. So is
  /// everything below it: the end of the flush tells the handler about each file it knew
  /// there, and frees the nodes that nothing needs any more.
  pub(super) fn vanish(&mut self, key: DirKey) {
    let node = &mut self.tree[key];
    let Kind::Dir(dir) = node.kind else { return };
    node.kind = Kind::Missing;
    self.tree.unhold(key);
    self.tree.set_times(key, None);
    if dir.asked.is_some() && !self.backend_failed {
      self.backend.unwatch(key);
    }

    let children: Vec<DirKey> = self.tree.children(key).collect();
    for child in children {
      // Nothing can be checked in a directory that is gone: a mark in it is dropped.
      self.tree.take_mark(child);
      match self.tree[child].kind {
        Kind::File(_) => self.forget_file(child),
        Kind::Dir(_) => self.vanish(child),
        Kind::Missing => self.tree.prune(child),
      }
    }
    // After the nodes below it, so that `finish` can free them from the bottom up.
    self.touched.push(key);
  }
}
