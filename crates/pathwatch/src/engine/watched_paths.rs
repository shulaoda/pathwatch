//! The watched paths: `register` makes a path one, and `unregister` takes it back. Also the
//! baseline rule: what a `watch` call finds is known without an event.

use std::{iter, path::Path};

use super::Engine;
use crate::{
  backend::{Backend, DirKey, Want},
  config::EntryKind,
  fs::Fs,
  path::split_top,
  tree::{Added, Kind, Mark, Spelling, Tree},
};

/// A path becomes a watched path, and what the call finds is its baseline.
///
/// The baseline is not reported. But there is no silence for a file that had to be reported
/// before this `watch`: an older watched path, or an older complete node, covered it already,
/// and the handler has not heard of it yet. So while a `watch` call runs, a node keeps what the
/// call did with it (`Node::added`, `Node::baseline`), and `finish` clears both when it ends.
impl<B: Backend, F: Fs> Engine<B, F> {
  /// Puts `path` into the tree: a registered node for each name on the way, and the last one is
  /// a watched path. Then it marks the place where the disk has to be checked.
  ///
  /// A path that the filter ignores both as a file and as a directory can never be reported.
  /// It is not registered: it costs no node, no watch and no look.
  pub(super) fn register(&mut self, path: &Path) {
    let Some((top, rest)) = split_top(path) else { return };
    let ignored_file = self.ignored(path, EntryKind::File);
    if ignored_file && self.ignored(path, EntryKind::Dir) {
      return;
    }

    // The deepest node on the way with only directories on disk above it: the place where
    // the disk has to be checked. Nothing is on disk below a held node, whatever the tree
    // still has there, so the way ends at such a node: its check finds out whether it is back.
    let mut anchor = None;
    let mut below_directories = true;
    let mut at = Tree::ROOT;
    for name in iter::once(top).chain(rest.iter()) {
      below_directories &= self.tree[at].is_dir();
      at = match self.tree.child(at, name) {
        Some(child) => child,
        None => self.tree.insert_child(at, name),
      };
      if below_directories {
        anchor = Some(at);
      }
      let node = &mut self.tree[at];
      if !node.is_registered() {
        node.set_added(Added::Registered);
        self.touched.push(at);
        // Only a registered name is followed when it is a link. The tree has a directory here,
        // but it can be behind the disk: a look tells whether a link took its place.
        if node.is_dir() {
          self.tree.mark(at, Mark::default());
        }
      }
    }

    // The first look does not ask the filter again.
    let node = &mut self.tree[at];
    node.set_ignored(EntryKind::File, ignored_file);
    if ignored_file {
      node.set_ignored(EntryKind::Dir, false);
    }
    if !self.tree.make_path(at) {
      return;
    }
    let node = &mut self.tree[at];
    if node.added() == Added::No {
      node.set_added(Added::Path);
      self.touched.push(at);
    }
    let Some(parent) = self.tree.parent(at) else { return };
    // The directory above was asked for names only. If no directory is at the new watched
    // path, it has to tell more, and it is asked before the path is looked at.
    // Not when the filter ignores that directory: it is no watch target.
    if below_directories
      && self.tells_names_only(parent)
      && self.tree[parent].ignored(EntryKind::Dir) != Some(true)
      && !self.is_dir_on_disk(at, path)
      && let Some(parent_path) = path.parent()
      && !self.is_ignored_target(parent)
    {
      self.ask(parent, parent_path, Want::Contents);
    }
    if let Some(anchor) = anchor
      && let Some(parent) = self.tree.parent(anchor)
      // A directory that is scanned anyway checks the node then.
      && self.tree[parent].dir().is_some_and(|dir| dir.scan.is_none())
    {
      self.tree.mark(anchor, Mark::DEEP);
    }
  }

  /// Whether a new file at the node `key` is part of the baseline of the running `watch`.
  pub(super) fn is_baseline_file(&self, key: DirKey) -> bool {
    !self.reported_before(key)
  }

  /// Whether what is in a directory that appears at the node `key`, or that becomes complete
  /// there, is the baseline of the running `watch`. `relinked`: the node is a link that was none
  /// at its last check.
  pub(super) fn is_baseline_dir(&self, key: DirKey, relinked: bool) -> bool {
    // A link that was a plain file entry until this call put a watched path at it or behind
    // it: nobody read its target before, whatever the directory above reported for the link.
    let new_link = relinked && self.tree[key].added() == Added::Registered;
    new_link || !self.reported_before(key)
  }

  /// Whether what is at the node `key` had to be reported before the running `watch`: it was a
  /// watched path, or the node of its directory was complete. Between two `watch` calls that
  /// holds for everything that counts.
  fn reported_before(&self, key: DirKey) -> bool {
    let node = &self.tree[key];
    if node.is_path() && node.added() == Added::No {
      return true;
    }
    // A directory above reports its own entries only.
    if node.is_unlisted() {
      return false;
    }
    let Some(parent) = self.tree.parent(key) else { return false };
    if self.tree[parent].is_complete() {
      !self.tree[parent].baseline()
    } else {
      self.tracked_before(parent)
    }
  }

  /// Whether the directory at the node `key`, which is partial in the tree, had to track every
  /// entry before the running `watch`. The tree is behind that rule when the look of an older
  /// `watch` still waits for the disk to answer.
  fn tracked_before(&self, key: DirKey) -> bool {
    // The nearest older watched path at or above the node. There is none above most partial
    // nodes, and then the filter is not asked at all.
    let (mut at, mut levels) = (key, 0);
    while !(self.tree[at].is_path() && self.tree[at].added() == Added::No) {
      if self.tree[at].is_unlisted() {
        return false;
      }
      let Some(parent) = self.tree.parent(at) else { return false };
      (at, levels) = (parent, levels + 1);
    }
    !self.is_ignored_on_the_way(key, levels)
  }
}

/// The way back: what only the path covered is forgotten, and what another watched path still
/// covers stays as it is. `unwatch` reports nothing. A drop reports `Delete` for each forgotten
/// file that the handler knows: nobody asked for the drop, and the file is gone from the disk.
///
/// It can leave work, and the flush that follows does it: a watch for a whole tree that is
/// wider than what its directory wants now is given back, and a link that is not registered
/// any more is a file entry from now on.
impl<B: Backend, F: Fs> Engine<B, F> {
  /// Makes `path` a path like any other again. A path that is not watched is left alone.
  pub(super) fn unregister(&mut self, path: &Path) {
    if let Some(key) = self.find(path) {
      self.forget(key, false);
    }
  }

  /// The filter ignores the watch target of the names in `dropped`: every watched path at or
  /// below one is forgotten, as `unwatch` of it would do it, but with a `Delete` for each file
  /// that the handler knows. `true`: there was a name.
  pub(super) fn drop_ignored(&mut self) -> bool {
    let any = !self.dropped.is_empty();
    // `forget` can find one more.
    while let Some(key) = self.dropped.pop() {
      let mut paths = Vec::new();
      let mut below = vec![key];
      while let Some(at) = below.pop() {
        // No node: the name is on the list twice.
        let Some(node) = self.tree.get(at) else { continue };
        if node.is_path() {
          paths.push(at);
        }
        below.extend(self.tree.children(at));
      }
      for path in paths {
        self.forget(path, true);
      }
    }
    any
  }

  /// The node of `path`, if the tree has one.
  fn find(&self, path: &Path) -> Option<DirKey> {
    let (top, rest) = split_top(path)?;
    let mut at = Tree::ROOT;
    for name in iter::once(top).chain(rest.iter()) {
      at = self.tree.child(at, name)?;
    }
    Some(at)
  }

  /// The watched path at the node `key` is no watched path any more. `tell_delete`: the handler
  /// hears of each file that is forgotten.
  fn forget(&mut self, key: DirKey, tell_delete: bool) {
    if !self.tree.drop_path(key) {
      return;
    }
    let Some(parent) = self.tree.parent(key) else { return };
    // The node stays when a watched path lies below it. What is at the path counted because
    // the path was watched, or because it is inside a watched directory. Only the second
    // reason can be left.
    if self.tree[key].is_registered() && !self.is_inside(key) {
      self.uncover(key, tell_delete, false);
    }
    // When no watched path lies below it, the node is let go, and so are the nodes above it
    // that only led to this path. The root always stays.
    let mut at = key;
    while !self.tree[at].is_registered()
      && let Some(parent) = self.tree.parent(at)
    {
      self.let_go(at, tell_delete);
      at = parent;
    }
    // The walk ended at a watched path. If the filter ignores the directory there, the node
    // was kept as a partial one for what was below it. It counts as missing, like every
    // watched path that the filter ignores.
    let node = &self.tree[at];
    let kept_for_below = node.is_path() && !node.is_ancestor() && matches!(node.kind, Kind::Dir(_));
    if kept_for_below && !self.tracks_every_entry(at) {
      self.uncover(at, tell_delete, false);
      // A watched path with nothing at it: its directory has to tell more than names.
      // Not when the filter ignores that directory: then the path is dropped too.
      if let Some(parent) = self.tree.parent(at) {
        if self.is_ignored_target(parent) {
          self.dropped.push(at);
        } else if self.tells_names_only(parent) {
          let path = self.tree.path(parent);
          self.ask(parent, &path, Want::Contents);
        }
      }
    }
    // The directory that held the path may want less now.
    self.narrow(parent, false);
  }

  /// The node `key` is no watched path, and no watched directory covers it any more. What the
  /// tree has there is forgotten, and the handler is told only if `tell_delete` says so. The
  /// same for everything below it, down to each watched path: that one stays, with all that it
  /// covers. The node itself stays, as a partial one, while a watched path lies below it.
  ///
  /// `scan_comes`: a directory above gave its watch for a whole tree back, so a deep scan
  /// comes here from there (see `narrow`).
  pub(super) fn uncover(&mut self, key: DirKey, tell_delete: bool, mut scan_comes: bool) {
    let node = &mut self.tree[key];
    if !tell_delete {
      // Not told: the end of the call makes no `Delete` of it.
      node.set_told(false);
    }
    let stays = node.is_ancestor();
    match &mut node.kind {
      Kind::Missing => return,
      Kind::File(_) => return self.forget_file(key),
      // Nothing is asked in a partial directory, so nothing would take a wrong proof back.
      Kind::Dir(dir) => (dir.complete, dir.spelling) = (false, Spelling::Unknown),
    }
    if stays {
      // Before the directories below, so that they know of the deep scan that comes from
      // here. What this directory wants does not depend on what is forgotten below it.
      scan_comes |= self.narrow(key, scan_comes);
    }
    let children: Vec<DirKey> = self.tree.children(key).collect();
    for child in children {
      if !self.tree[child].is_path() {
        self.uncover(child, tell_delete, scan_comes);
      }
    }
    if !stays {
      // Nothing is left below it. The backend forgets its watch.
      self.vanish(key);
    }
  }

  /// The node `key` is not registered any more: it is no watched path and leads to none.
  /// Inside a watched directory it is an entry like the others from now on. Anywhere else
  /// nothing needs it.
  fn let_go(&mut self, key: DirKey, tell_delete: bool) {
    let Some(parent) = self.tree.parent(key) else { return };
    let inside = self.is_inside(key);
    self.tree[key].set_unlisted(false);
    // Only a registered node is followed when it is a link. Now the link itself is the
    // entry, and a look has to find that.
    if self.tree.link(key).is_some() {
      self.tree.set_link(key, None);
      if inside && matches!(self.tree[parent].kind, Kind::Dir(_)) {
        self.tree.mark(key, Mark::default());
      }
    }
    let is_entry = inside
      && match self.tree[key].kind {
        Kind::Missing => false,
        Kind::File(_) => true,
        // Not when the filter ignores the directory: its node was kept, as a partial one,
        // for the watched path below it.
        Kind::Dir(_) => self.tracks_every_entry(key),
      };
    if !is_entry {
      self.uncover(key, tell_delete, false);
    }
    self.tree.prune(key);
  }
}
