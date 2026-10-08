//! One directory: the scan that checks every entry in it, and what is done when a directory
//! appears, is still there, or vanishes.

use std::path::PathBuf;

use super::{
  Engine,
  check::{Found, Listed, is_missing, swap_case},
};
use crate::{
  backend::{Backend, DirKey},
  fs::{DirId, Fs},
  tree::{Depth, Dir, Kind, Mark, Spelling, Watch},
};

/// One node to check in a scan, with what the listing said about it.
pub(super) type ScanEntry = (DirKey, Listed);

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
    // The names were to be compared because the directory counted as loose. A look proved
    // meanwhile that it is strict: there is nothing to compare.
    if depth == Depth::Names && !self.tree.is_loose(key) {
      return;
    }
    self.tree.path_into(key, path);
    self.scan_had_error = false;
    names.clear();
    names.extend(self.tree.children(key).map(|child| (child, Listed::NotAsked)));
    let known = names.len();
    self.listed.clear();
    let listing =
      if self.tree[key].is_complete() { self.list_new(key, path, names) } else { Listing::None };
    if listing == Listing::Stop {
      return;
    }
    // The listing kept the names of the known nodes if the directory counted as loose.
    let kept_names = self.tree.is_loose(key);
    if listing == Listing::Whole && known > 0 && !self.options.loose_spelling {
      self.ask_for_the_rule(key, path, names);
    }
    // In a directory that counts as loose, a listing of all of it tells how the disk spells
    // each name: a node of the tree is one of them, or the disk has no entry under its name.
    let names_are_known = listing == Listing::Whole && kept_names;
    let names_only = depth == Depth::Names && names_are_known;

    let why = if depth == Depth::Deep { Mark::DEEP } else { Mark::default() };
    for (index, &(child, listed)) in names.iter().enumerate() {
      let listed = match listed {
        Listed::NotAsked if names_are_known && self.listed.contains(&child) => Listed::Named(None),
        Listed::NotAsked if names_are_known => Listed::NotNamed,
        said => said,
      };
      // The tree knew the node, and the listing names it: it is as it was.
      if names_only && index < known && matches!(listed, Listed::Named(_)) {
        continue;
      }
      // A check frees no node but its own.
      self.check(child, path, listed, why);
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
  /// In a directory that counts as loose, `Engine::listed` gets every node that the tree
  /// knew and that the listing named.
  fn list_new(&mut self, key: DirKey, path: &mut PathBuf, names: &mut Vec<ScanEntry>) -> Listing {
    let known = names.len();
    let tree = &mut self.tree;
    let mut listed = tree.is_loose(key).then_some(&mut self.listed);
    let result = self.fs.read_dir(path, &mut |name, kind| match tree.child(key, name) {
      Some(node) => {
        if let Some(listed) = &mut listed {
          listed.insert(node);
        }
      }
      None => names.push((tree.insert_child(key, name), Listed::Named(kind))),
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

  /// Asks the disk whether the directory `key`, which was just listed, tells two spellings of
  /// a name apart (`Tree::is_loose`), with one of its names in the other case of its letters.
  /// The listing gave the name, so its entry is there. (`lstat` alone can find a name that is
  /// gone: see `other_spelling`.)
  ///
  /// A directory that was proved strict is strict no more if that other spelling finds the
  /// very entry: the directory can be another one by now, with the same id.
  fn ask_for_the_rule(&mut self, key: DirKey, path: &mut PathBuf, names: &[ScanEntry]) {
    let was_strict = !self.tree.is_loose(key);
    let swap = |name: &str| -> Option<String> {
      let has_letter = name.bytes().any(|byte| byte.is_ascii_alphabetic());
      has_letter.then(|| name.chars().map(swap_case).collect())
    };
    // A name that the listing gave, with no second name in the tree that can be the same
    // name. A listing of a strict directory keeps no names: there every child is tried.
    let is_listed = |&&(child, listed): &&ScanEntry| {
      let named = was_strict || matches!(listed, Listed::Named(_)) || self.listed.contains(&child);
      named && !self.tree.has_twin(child)
    };
    let tried = names.iter().filter(is_listed).find_map(|&(child, _)| {
      let name = self.tree.name(child);
      Some((name.to_owned(), name.to_str().and_then(swap)?))
    });
    let Some((name, other)) = tried else {
      // No name to try: the looks must not ask for a listing again and again.
      if !was_strict {
        self.set_spelling(key, Spelling::Loose);
      }
      return;
    };
    path.push(&name);
    let found = match self.fs.lstat(path) {
      Ok(entry) if was_strict => {
        let other = self.fs.lstat(&path.with_file_name(other));
        other.is_ok_and(|other| other.file_id == entry.file_id).then_some(false)
      }
      Ok(entry) => self.tells_spellings_apart(path, &entry),
      Err(_) => None,
    };
    path.pop();
    match found {
      Some(true) => self.set_spelling(key, Spelling::Strict),
      Some(false) => self.set_spelling(key, Spelling::Loose),
      None => {}
    }
  }

  /// The directory `key` is gone. Its parent finds out what is at its path now.
  fn check_from_parent(&mut self, key: DirKey, path: &mut PathBuf) {
    let Some(parent) = self.tree.parent(key) else { return };
    self.tree.path_into(parent, path);
    self.check(key, path, Listed::NotAsked, Mark::default());
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
