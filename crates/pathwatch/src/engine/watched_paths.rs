//! The watched paths: `register` makes a path one, and `unregister` takes it back. Also the
//! baseline rule: what a `watch` call finds is known without an event.

use std::{iter, path::Path};

use super::Engine;
use crate::{
  backend::{Backend, DirKey, Want},
  config::EntryKind,
  fs::Fs,
  path::split_top,
  tree::{Added, Depth, Kind, Mark, Spelling, Tree},
};

/// The baseline: what a `watch` call finds is known without an event, unless an older watched
/// path covered it already (design.md 5.7).
impl<B: Backend, F: Fs> Engine<B, F> {
  /// Puts the path into the tree, a registered node for each name, and marks where the disk has
  /// to be checked. A path ignored as a file and as a directory is left out.
  pub(super) fn register(&mut self, path: &Path) {
    let Some((top, rest)) = split_top(path) else { return };
    let ignored_file = self.ignored(path, EntryKind::File);
    if ignored_file && self.ignored(path, EntryKind::Dir) {
      return;
    }

    // The anchor: the deepest node with only directories on disk above it, each listed by the
    // directory above it. The way ends at a held node, since nothing is on disk below it, and
    // at a name that the disk spelled in another way at its last look: that answer can be old,
    // and the baseline of what is below is decided by it.
    let mut anchor = None;
    let mut below_directories = true;
    let mut at = Tree::ROOT;
    for name in iter::once(top).chain(rest.iter()) {
      let node = &self.tree[at];
      below_directories &= node.is_dir() && !node.is_unlisted();
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
        // A look tells whether a link took the place of the directory the tree has here.
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
    // A directory asked for names only must tell more when no directory is at the new path.
    // Not when the filter ignores it: it is no watch target.
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
      // A directory whose every name is checked anyway checks the node then. A scan of the
      // names alone skips what the listing names.
      && self.tree[parent].dir().is_some_and(|dir| dir.scan.is_none_or(|depth| depth == Depth::Names))
    {
      self.tree.mark(anchor, Mark::DEEP);
    }
  }

  /// Whether a new file at the node is part of the baseline of the running `watch`.
  pub(super) fn is_baseline_file(&self, key: DirKey) -> bool {
    !self.reported_before(key)
  }

  /// Whether what is in a directory that appears, or becomes complete, is the baseline.
  /// `relinked`: the node is a link that was none at the last check.
  pub(super) fn is_baseline_dir(&self, key: DirKey, relinked: bool) -> bool {
    // A link that the running call follows for the first time: nobody read its target before.
    let new_link = relinked && self.tree[key].added() == Added::Registered;
    new_link || !self.reported_before(key)
  }

  /// Whether what is at the node had to be reported before the running `watch`.
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

  /// Whether the partial directory had to track every entry before the running `watch`: the
  /// tree can be behind while an older look waits for the disk.
  fn tracked_before(&self, key: DirKey) -> bool {
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

/// The way back: `unwatch` forgets what only the path covered and reports nothing. A drop by
/// the filter reports a `Delete` for each forgotten file the handler knows (design.md 5.7).
impl<B: Backend, F: Fs> Engine<B, F> {
  /// Makes `path` a path like any other again. A path that is not watched is left alone.
  pub(super) fn unregister(&mut self, path: &Path) {
    if let Some(key) = self.find(path) {
      self.forget(key, false);
    }
  }

  /// Forgets every watched path at or below a dropped name, with a `Delete` for each file the
  /// handler knows. `true`: there was one.
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

  /// The node is no watched path any more. `tell_delete`: the handler hears of each forgotten
  /// file.
  fn forget(&mut self, key: DirKey, tell_delete: bool) {
    if !self.tree.drop_path(key) {
      return;
    }
    let Some(parent) = self.tree.parent(key) else { return };
    // What is at the path can still count as an entry of a watched directory above: only then it
    // stays.
    if self.tree[key].is_registered() && !self.is_inside(key) {
      self.uncover(key, tell_delete, false);
    }
    // With no watched path below, the node is let go, and the nodes above that only led to it.
    let mut at = key;
    while !self.tree[at].is_registered()
      && let Some(parent) = self.tree.parent(at)
    {
      self.let_go(at, tell_delete);
      at = parent;
    }
    // The walk ended at a watched path that the filter ignores as a directory: its node was kept
    // for what was below, and counts as missing now.
    let node = &self.tree[at];
    let kept_for_below = node.is_path() && !node.is_ancestor() && matches!(node.kind, Kind::Dir(_));
    if kept_for_below && !self.tracks_every_entry(at) {
      self.uncover(at, tell_delete, false);
      // A watched path with nothing at it makes its directory tell more; an ignored directory
      // drops the path instead.
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

  /// Forgets what the tree has at the node and below it, down to each watched path, which stays.
  /// `scan_comes`: a deep scan comes from a directory above that gave its watch back.
  pub(super) fn uncover(&mut self, key: DirKey, tell_delete: bool, scan_comes: bool) {
    // A list, not a recursion: a tree can be deeper than the stack takes. The directories that
    // go vanish afterwards, each after the ones below it.
    let mut gone = Vec::new();
    let mut next = Some((key, scan_comes));
    let mut stack = Vec::new();
    while let Some((key, mut scan_comes)) = next.take().or_else(|| stack.pop()) {
      let node = &mut self.tree[key];
      if !tell_delete {
        // Not told: the end of the call makes no `Delete` of it.
        node.set_told(false);
      }
      let stays = node.is_ancestor();
      match &mut node.kind {
        Kind::Missing => continue,
        Kind::File(_) => {
          self.forget_file(key);
          continue;
        }
        // Nothing is asked in a partial directory, so nothing would take a wrong proof back.
        Kind::Dir(dir) => (dir.complete, dir.spelling) = (false, Spelling::Unknown),
      }
      if stays {
        // Before the directories below, so that they know of the deep scan that comes from here.
        scan_comes |= self.narrow(key, scan_comes);
      } else {
        gone.push(key);
      }
      let below = self.tree.children(key).filter(|&child| !self.tree[child].is_path());
      stack.extend(below.map(|child| (child, scan_comes)));
    }
    for key in gone.into_iter().rev() {
      self.vanish(key);
    }
  }

  /// The node is not registered any more: an entry like the others inside a watched directory,
  /// and nothing anywhere else.
  fn let_go(&mut self, key: DirKey, tell_delete: bool) {
    let Some(parent) = self.tree.parent(key) else { return };
    let inside = self.is_inside(key);
    self.tree[key].set_unlisted(false);
    // Only a registered node is followed: the link itself is the entry now, and a look must see
    // it.
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
