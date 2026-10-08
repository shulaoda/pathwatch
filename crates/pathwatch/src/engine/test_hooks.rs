//! Hooks for tests: the handler's view, the stored fingerprints and the invariants of the tree.

use std::{collections::BTreeSet, iter, path::PathBuf, time::Instant};

use super::{Engine, check::swap_case};
use crate::{
  backend::{Backend, DirKey, Want},
  fs::Fs,
  tree::{Added, Fingerprint, Kind, Node, Spelling, Tree, Watch},
};

impl<B: Backend, F: Fs> Engine<B, F> {
  /// Every file the handler believes to exist: what it was told about, and what was there
  /// when a path was watched. A file whose `Delete` waits for the delete delay is still in it.
  pub fn view(&self) -> BTreeSet<PathBuf> {
    let told = self.tree.nodes().filter(|(_, node)| node.told());
    told.map(|(key, _)| self.tree.path(key)).collect()
  }

  /// Every file the tree holds, with the fingerprint that its last check stored. A test
  /// compares each one with the disk: a file that was looked at too early, and never again,
  /// shows up here and nowhere else.
  pub fn fingerprints(&self) -> Vec<(PathBuf, Fingerprint)> {
    let stored = |(key, node): (DirKey, &Node)| match node.kind {
      Kind::File(fingerprint) => Some((self.tree.path(key), fingerprint)),
      _ => None,
    };
    self.tree.nodes().filter_map(stored).collect()
  }

  /// The paths of the pending deletes: every file the handler believes in at or below a held
  /// node. Its `Delete` waits for the second look.
  pub fn pending_deletes(&self) -> BTreeSet<PathBuf> {
    let waits = |key: DirKey, node: &Node| node.told() && (node.is_held() || self.tree.frozen(key));
    let pending = self.tree.nodes().filter(|(key, node)| waits(*key, node));
    pending.map(|(key, _)| self.tree.path(key)).collect()
  }

  /// Whether a `tick` at `now` runs a poll.
  pub fn poll_due(&self, now: Instant) -> bool {
    self.poll_at.is_some_and(|at| at <= now)
  }

  /// Checks that nothing is left to do but the next poll. The error names what is left.
  pub fn check_idle(&self) -> Result<(), String> {
    let left = [
      (self.flush_at.is_some(), "a flush is due"),
      (self.tree.holds().next().is_some(), "a delete is pending"),
      (!self.hold_queue.is_empty(), "the queue of held nodes is not empty"),
      (self.tree.has_work(), "marks or scans wait"),
      (self.tree.lost, "lost notifications wait for their scan"),
      (!self.errors.is_empty(), "an error waits for the handler"),
      (self.tree.nodes().any(|(key, _)| self.is_behind(key)), "a directory is to be complete"),
    ];
    match left.iter().find(|(is_left, _)| *is_left) {
      Some((_, what)) => Err((*what).to_owned()),
      None => Ok(()),
    }
  }

  /// Checks `Node::is_unlisted` and `Spelling::Strict` against the disk, when the tree has caught
  /// up with it. Both are answers of the disk that the tree keeps. The first says, for a
  /// registered name in a complete node whose entry is on the disk, whether the disk has the
  /// entry under another name. The second says that a directory tells two spellings of a
  /// name apart: then no entry of it is found in the other case of its letters.
  pub fn check_spellings(&self) -> Result<(), String> {
    for (key, node) in self.tree.nodes() {
      if node.dir().is_some_and(|dir| dir.spelling == Spelling::Strict) {
        for child in self.tree.children(key) {
          let path = self.tree.path(child);
          let other: Option<String> = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.chars().map(swap_case).collect());
          let (Ok(meta), Some(other)) = (self.fs.lstat(&path), other) else { continue };
          let folded = self.fs.lstat(&path.with_file_name(&other)).is_ok_and(|found| {
            found.file_id == meta.file_id
              && path.file_name().is_some_and(|name| name != other.as_str())
          });
          if folded {
            return Err(format!(
              "{}: the tree says strict, the disk finds {other}",
              path.display()
            ));
          }
        }
      }
      let Some(parent) = self.tree.parent(key) else { continue };
      let in_complete = self.tree[parent].is_complete() && self.tree.is_loose(parent);
      if !(node.is_registered() && in_complete) {
        continue;
      }
      let path = self.tree.path(key);
      let Ok(name) = self.fs.name_on_disk(&path) else { continue };
      let is_other = name != self.tree.name(key);
      // A node with nothing in the tree is no entry of the directory, whatever its flag says.
      let counts_as_entry = !node.is_unlisted() && !matches!(node.kind, Kind::Missing);
      if (node.is_unlisted() && !is_other) || (counts_as_entry && !node.is_held() && is_other) {
        let flag = if node.is_unlisted() { "another name" } else { "this name" };
        let name = name.display();
        return Err(format!("{}: the tree says {flag}, the disk has {name}", path.display()));
      }
    }
    Ok(())
  }

  /// Whether the node `key` has a directory that has to track every entry, but does not yet
  /// in the tree: the look that makes it complete is still to come.
  fn is_behind(&self, key: DirKey) -> bool {
    matches!(self.tree[key].kind, Kind::Dir(dir) if !dir.complete) && self.tracks_every_entry(key)
  }

  /// Whether a mark waits on the node `key` or on a node above it.
  fn mark_waits_above(&self, key: DirKey) -> bool {
    iter::successors(Some(key), |&at| self.tree.parent(at)).any(|at| self.tree[at].is_marked())
  }

  /// Checks the invariants of the tree and the bookkeeping around them, between two calls of
  /// the engine. The error names the first one that does not hold.
  ///
  /// "Has" is what the tree keeps at a node, also while the node is held.
  pub fn check_invariants(&self) -> Result<(), String> {
    // 1. Every node but the root has a parent that knows it.
    self.tree.check_links()?;
    for (key, node) in self.tree.nodes() {
      let fail = |what: &str| Err(format!("{}: {what}", self.tree.path(key).display()));

      let Some(parent) = self.tree.parent(key) else {
        let fixed = node
          .dir()
          .is_some_and(|dir| !dir.complete && dir.asked.is_none() && dir.watch == Watch::Polled);
        if !fixed || node.is_path() || node.is_marked() || node.told() {
          return fail("the root is not what it always is");
        }
        continue;
      };
      let parent_is_dir = matches!(self.tree[parent].kind, Kind::Dir(_));
      // 2. The tree has nothing below a node that is no directory.
      if !matches!(node.kind, Kind::Missing) && !parent_is_dir {
        return fail("the tree has it below a node that is no directory");
      }
      // 3. What the handler knows is what the tree has as a file. A change that waits for
      // its event, and the bits of the baseline rule, live inside one call.
      if node.told() != matches!(node.kind, Kind::File(_)) {
        return fail("the handler and the tree do not agree about a file here");
      }
      if node.changed() || node.baseline() || node.added() != Added::No {
        return fail("an event, or the baseline, is left over from a call");
      }
      // 4. A held node has a file or a directory, and something brings its second look: the
      // queue, or a mark that waits for the disk to answer.
      if node.is_held() {
        if matches!(node.kind, Kind::Missing) {
          return fail("is held, and the tree has nothing there");
        }
        let queued = self
          .tree
          .held_until(key)
          .is_some_and(|until| self.hold_queue.iter().any(|entry| *entry == (until, key)));
        if !queued && !node.is_marked() {
          return fail("is held, and nothing brings its second look");
        }
      }
      match &node.kind {
        // 5. A node with nothing at its path is kept only while a watched path or a mark
        // needs it.
        Kind::Missing => {
          if !node.is_registered() && !node.is_marked() {
            return fail("nothing is at the path, and nothing needs the node");
          }
        }
        Kind::File(_) => {
          // 5. Where the node above is partial, also a file that it will track when it has
          // caught up with the rule.
          if !node.is_path() && !self.is_inside(key) {
            return fail("a partial node knows a file that is no watched path");
          }
        }
        Kind::Dir(dir) => {
          // `hold` takes the watch away. The backend is asked again when the directory is
          // back, or when the directory of the held node above it is.
          if node.is_held() && (dir.asked.is_some() || dir.watch != Watch::Polled) {
            return fail("is held, and the backend still has its watch");
          }
          if dir.watch == Watch::None {
            return fail("is on disk but has no watch state");
          }
          if parent == Tree::ROOT && dir.complete {
            return fail("a top directory is complete");
          }
          // 5. A partial node with no registered name is a directory that is to be complete.
          if !dir.complete && !node.is_ancestor() && !self.is_behind(key) {
            return fail("is partial and has no registered name");
          }
          if dir.scan.is_some() && !self.tree.to_scan.contains(&key) {
            return fail("waits for a scan but is not on the list");
          }
          if dir.is_polled() && !self.tree.polled.contains(&key) {
            return fail("no OS watch tells about it, and it is not on the list of the timer");
          }
        }
      }
      // 6. A directory that was asked for names only holds no registered name with a file or
      // nothing at it. Nothing is known about what is below a held node. And a name on the
      // way can be without its first check while the disk gives no answer: then a mark waits
      // on the way, and the check that it brings asks the directory for more.
      let frozen = self.tree.frozen(key);
      let waits = self.mark_waits_above(key);
      let names_only = self.tree[parent].dir().is_some_and(|dir| dir.asked == Some(Want::Names));
      if names_only && node.is_registered() && !node.is_dir() && !frozen && !waits {
        return fail("is a registered name with no directory at it, and its directory tells names");
      }
      // 7. A directory that relies on a watch above has one there. While the disk gives no
      // answer it can be without: then a mark waits on the way, and its check asks again.
      let covered = matches!(self.tree[parent].watch(), Watch::WatchedTree | Watch::FromAbove);
      if node.watch() == Watch::FromAbove && !covered && !frozen && !waits {
        return fail("relies on a watch above, and there is none");
      }
      if node.is_marked() {
        if !self.tree.marked.contains(&key) {
          return fail("has a mark but is not on the list of marked nodes");
        }
        if !parent_is_dir {
          return fail("has a mark in a node that is no directory");
        }
      }
    }
    if !self.touched.is_empty() {
      return Err("touched nodes are left over from a flush".to_owned());
    }
    if !self.covered.is_empty() {
      return Err("a directory still waits to give its watch back after a flush".to_owned());
    }
    Ok(())
  }
}
