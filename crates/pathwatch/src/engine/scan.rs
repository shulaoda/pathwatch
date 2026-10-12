//! One directory: the scan of its entries, and what is done when a directory appears, is
//! still there, or vanishes.

use std::path::PathBuf;

use super::{
  Engine,
  check::{Found, Listed, is_missing, swap_case},
};
use crate::{
  backend::{Backend, DirKey},
  fs::{DirId, Entry, FileType, Fs},
  tree::{Depth, Dir, Kind, Mark, Spelling, Watch},
};

/// One node to check in a scan, with what the listing said about it.
pub(super) type ScanEntry = (DirKey, Listed);

const _: () = assert!(size_of::<ScanEntry>() == 40);

impl<B: Backend, F: Fs> Engine<B, F> {
  /// Checks every node of the directory, and what a listing shows on top if the node is complete.
  /// `path` and `names` are buffers.
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
    // A look proved meanwhile that the directory is strict: there are no names to compare.
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
    let kept_names = self.tree.is_loose(key);
    if listing == Listing::Whole && known > 0 && !self.options.loose_spelling {
      self.ask_for_the_rule(key, path, names);
    }
    // The listing of a loose directory says how the disk spells each known node, or that it is
    // gone.
    let names_are_known = listing == Listing::Whole && kept_names;
    let names_only = depth == Depth::Names && names_are_known;

    let why = if depth == Depth::Deep { Mark::DEEP } else { Mark::default() };
    for (index, &(child, listed)) in names.iter().enumerate() {
      let listed = match listed {
        Listed::NotAsked if names_are_known => {
          self.listed.get(&child).copied().unwrap_or(Listed::NotNamed)
        }
        said => said,
      };
      // The tree knew the node, and the listing names it: it is as it was.
      if names_only && index < known && listed.is_named() {
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

  /// Lists the directory and adds a node for each entry the tree does not know. With loose
  /// spelling, `listed` gets what the listing said of the known nodes.
  fn list_new(&mut self, key: DirKey, path: &mut PathBuf, names: &mut Vec<ScanEntry>) -> Listing {
    let known = names.len();
    // A listing that lags behind a write is trusted for a file only where an OS watch reports
    // the write.
    let heard =
      matches!(self.tree[key].watch(), Watch::Watched | Watch::WatchedTree | Watch::FromAbove);
    let lags = self.fs.listing_lags() && !heard;
    let tree = &mut self.tree;
    let mut listed = tree.is_loose(key).then_some(&mut self.listed);
    let result = self.fs.read_dir(path, &mut |name, entry| {
      let entry = match entry {
        Entry::File { .. } if lags => Entry::Kind(FileType::File),
        entry => entry,
      };
      match tree.child(key, name) {
        Some(node) => {
          if let Some(listed) = &mut listed {
            listed.insert(node, entry.into());
          }
        }
        None => names.push((tree.insert_child(key, name), entry.into())),
      }
    });
    let Err(error) = result else { return Listing::Whole };

    // What the listing gave before it failed is forgotten.
    for (new, _) in names.drain(known..) {
      self.tree.prune(new);
    }
    if is_missing(&error) {
      if self.check_from_parent(key, path) {
        // The directory is there after all (away and back while it was read): the deep mark
        // keeps the work for the next flush, as when the disk gives no answer.
        self.tree.mark(key, Mark::DEEP);
      }
      return Listing::Stop;
    }
    if matches!(self.disk_error(key, path, error), Found::Unknown) {
      // No answer: the parent checks it at the next flush, and the deep mark keeps the work.
      if self.tree.parent(key).is_some() {
        self.tree.mark(key, Mark::DEEP);
      }
      return Listing::Stop;
    }
    // Permission denied: nothing in here can be read, so every node is checked as missing.
    Listing::None
  }

  /// Tries one name the listing gave in the other case: whether the directory tells two spellings
  /// apart. A proof of strict is taken back when the other spelling finds the very entry.
  fn ask_for_the_rule(&mut self, key: DirKey, path: &mut PathBuf, names: &[ScanEntry]) {
    let was_strict = !self.tree.is_loose(key);
    let swap = |name: &str| -> Option<String> {
      let has_letter = name.bytes().any(|byte| byte.is_ascii_alphabetic());
      has_letter.then(|| name.chars().map(swap_case).collect())
    };
    // A listing of a strict directory keeps no names: every child is tried there, and one that
    // is gone says nothing.
    let is_listed = |&&(child, listed): &&ScanEntry| {
      let named = was_strict || listed.is_named() || self.listed.contains_key(&child);
      named && !self.tree.has_twin(child)
    };
    let to_try = names.iter().filter(is_listed).filter_map(|&(child, _)| {
      let name = self.tree.name(child);
      Some((name.to_owned(), name.to_str().and_then(swap)?))
    });
    let tries = if was_strict { usize::MAX } else { 1 };
    let (mut tried, mut found) = (false, None);
    for (name, other) in to_try.take(tries) {
      tried = true;
      path.push(&name);
      let entry = self.fs.lstat(path);
      found = match &entry {
        Ok(entry) if was_strict => {
          let other = self.fs.lstat(&path.with_file_name(other));
          other.is_ok_and(|other| other.file_id == entry.file_id).then_some(false)
        }
        Ok(entry) => self.tells_spellings_apart(path, entry),
        Err(_) => None,
      };
      path.pop();
      if entry.is_ok() {
        break;
      }
    }
    match found {
      Some(true) => self.set_spelling(key, Spelling::Strict),
      Some(false) => self.set_spelling(key, Spelling::Loose),
      // No name to try: the looks must not ask for a listing again and again.
      None if !tried && !was_strict => self.set_spelling(key, Spelling::Loose),
      None => {}
    }
  }

  /// The directory is gone: its parent finds out what is at its path now. True: the same
  /// directory is still there, not held, and no scan of it is queued.
  fn check_from_parent(&mut self, key: DirKey, path: &mut PathBuf) -> bool {
    let Some(parent) = self.tree.parent(key) else { return false };
    self.tree.path_into(parent, path);
    self.check(key, path, Listed::NotAsked, Mark::default());
    let Some(node) = self.tree.get(key) else { return false };
    !node.is_held() && node.dir().is_some_and(|dir| dir.scan.is_none())
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

impl<B: Backend, F: Fs> Engine<B, F> {
  /// A directory is on disk where the tree had none. `baseline`: what is in it is the baseline.
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

  /// A directory is still on disk at the node.
  pub(super) fn still_there(&mut self, key: DirKey, path: &mut PathBuf, deep: bool) {
    if deep && self.has_a_watch_to_give_back(key) {
      // A watch above covers it, and a deep scan follows the next commit: its own watch can go
      // then, whichever path was watched first.
      self.covered.push(key);
    } else {
      // Also when not deep: the node may want more, or the watch above may be gone.
      self.arm(key, path, deep);
    }
    if deep {
      self.tree.queue_scan(key, Depth::Deep);
    }
  }

  /// The directory is gone for good, with everything below it: the end of the flush tells the
  /// handler.
  pub(super) fn vanish(&mut self, key: DirKey) {
    let node = &mut self.tree[key];
    let Kind::Dir(dir) = node.kind else { return };
    node.kind = Kind::Missing;
    self.tree.unhold(key);
    if dir.asked.is_some() {
      self.unwatch_dir(key);
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
