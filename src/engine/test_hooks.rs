//! Hooks for tests: the handler's view, the stored fingerprints and the invariants of the tree.

use std::{collections::BTreeSet, iter, path::PathBuf, time::Instant};

use super::Engine;
use crate::{
  backend::{Backend, DirKey},
  config::EntryKind,
  fs::Fs,
  tree::{Added, Fingerprint, Kind, Node, Tree, Watch},
};

impl<B: Backend, F: Fs> Engine<B, F> {
  /// How long the list of the directories that the timer looks at is, stale keys included.
  pub fn polled_len(&self) -> usize {
    self.tree.polled.len()
  }

  /// The room that the young files take: the table of what is kept to compare, and the queue of
  /// the files that wait for a delivery.
  pub fn young_room(&self) -> usize {
    self.tree.content_room() + self.pending.capacity()
  }

  /// Every file the handler believes to exist, a file whose `Delete` waits too.
  pub fn view(&self) -> BTreeSet<PathBuf> {
    let told = self.tree.nodes().filter(|(_, node)| node.told());
    told.map(|(key, _)| self.tree.path(key)).collect()
  }

  /// Every file the tree holds, with its stored fingerprint, for a test to compare with the disk.
  pub fn fingerprints(&self) -> Vec<(PathBuf, Fingerprint)> {
    let stored = |(key, node): (DirKey, &Node)| match node.kind {
      Kind::File(fingerprint) => Some((self.tree.path(key), fingerprint)),
      _ => None,
    };
    self.tree.nodes().filter_map(stored).collect()
  }

  /// The files whose `Delete` waits for a second look.
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
      (self.tree.hold_count() > 0, "a delete is pending"),
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

  /// Checks the kept answer of the disk, `Node::is_unlisted`, against the disk once the tree has
  /// caught up.
  pub fn check_spellings(&self) -> Result<(), String> {
    if !self.tree.loose_spelling {
      return Ok(());
    }
    for (key, node) in self.tree.nodes() {
      let Some(parent) = self.tree.parent(key) else { continue };
      if !(node.is_registered() && self.tree[parent].is_complete()) {
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

  /// Whether a directory is to be complete, and the look that makes it so is still to come.
  fn is_behind(&self, key: DirKey) -> bool {
    matches!(self.tree[key].kind, Kind::Dir(dir) if !dir.complete) && self.tracks_every_entry(key)
  }

  /// Whether a mark waits on the node `key` or on a node above it.
  fn mark_waits_above(&self, key: DirKey) -> bool {
    iter::successors(Some(key), |&at| self.tree.parent(at)).any(|at| self.tree[at].is_marked())
  }

  /// Checks the invariants of design.md 5.1 between two calls of the engine. The error names the
  /// first one that does not hold.
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
      // 3. The handler knows exactly the files the tree has, and the bits of one call are cleared.
      if node.told() != matches!(node.kind, Kind::File(_)) {
        return fail("the handler and the tree do not agree about a file here");
      }
      if node.changed() || node.baseline() || node.added() != Added::No {
        return fail("an event, or the baseline, is left over from a call");
      }
      // 4. A held node has something, and its second look comes: by the queue, or by a mark.
      if node.is_held() {
        if matches!(node.kind, Kind::Missing) {
          return fail("is held, and the tree has nothing there");
        }
        let queued = self
          .tree
          .held_until(key)
          .is_some_and(|until| self.hold_queue.iter().any(|entry| entry.0 == (until, key)));
        if !queued && !node.is_marked() {
          return fail("is held, and nothing brings its second look");
        }
      }
      let frozen = self.tree.frozen(key);
      let waits = self.mark_waits_above(key);
      match &node.kind {
        // 5. A node with nothing at its path is kept only for a watched path or a mark.
        Kind::Missing => {
          if !node.is_registered() && !node.is_marked() {
            return fail("nothing is at the path, and nothing needs the node");
          }
        }
        Kind::File(_) => {
          // 5. A partial node knows a file only if the rule makes it complete.
          if !node.is_path() && !self.is_inside(key) {
            return fail("a partial node knows a file that is no watched path");
          }
        }
        Kind::Dir(dir) => {
          // `hold` takes the watch away; the backend is asked again when the directory is back.
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
          // A scan that is not deep sets `away` below it only while a lost mark waits above: the
          // deep scan of that mark takes it.
          if dir.away && dir.scan.is_none() && !frozen && !waits {
            return fail("was away, and no scan comes that knows it");
          }
          if dir.is_polled() && !self.tree.polled.contains(&key) {
            return fail("no OS watch tells about it, and it is not on the list of the timer");
          }
        }
      }
      // 6. A directory asked for names only holds no registered name without a directory, unless
      // it is frozen or a mark waits above it.
      let names_only = self.tells_names_only(parent);
      if names_only && node.is_registered() && !node.is_dir() && !frozen && !waits {
        return fail("is a registered name with no directory at it, and its directory tells names");
      }
      // 7. A directory that relies on a watch above has one, unless it is frozen or a mark waits
      // above it.
      let covered = self.tree[parent].watch().covers_below();
      if node.watch() == Watch::FromAbove && !covered && !frozen && !waits {
        return fail("relies on a watch above, and there is none");
      }
      // 8. Every node but the root that has a file or a directory keeps the answer "not
      // ignored" for that kind.
      if !keeps_not_ignored(node) {
        return fail("has what the filter ignores, or what it was never asked about");
      }
      if node.is_marked() {
        if !self.tree.marked.iter().any(|&(marked, _)| marked == key) {
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
    // A key whose node has no mark keeps the timer of the poll for nothing (`has_poll_work`).
    if let Some(&(key, _)) =
      self.tree.marked.iter().find(|&&(key, _)| !self.tree.get(key).is_some_and(Node::is_marked))
    {
      return Err(format!("{key:?} is on the list of marked nodes and has no mark"));
    }
    Ok(())
  }
}

/// Whether the node keeps the answer "not ignored" of the filter for the file or the directory
/// at it. A node with nothing at it can keep any answer.
fn keeps_not_ignored(node: &Node) -> bool {
  match node.kind {
    Kind::File(_) => node.ignored(EntryKind::File) == Some(false),
    Kind::Dir(_) => node.ignored(EntryKind::Dir) == Some(false),
    Kind::Missing => true,
  }
}
