//! The watched paths: `register` makes a path one, and `unregister` takes it back. Also the
//! baseline rule: what a `watch` call finds is known without an event; what a watched path
//! covers (design.md 5.1); and the questions to the ignore filter (5.11).

use std::{
  iter,
  path::{Path, PathBuf},
};

use super::Engine;
use crate::{
  backend::{Backend, DirKey, Want},
  config::EntryKind,
  fs::Fs,
  path::split_top,
  tree::{Added, Depth, Kind, Mark, Tree, emptied, push_name},
};

/// The baseline: what a `watch` call finds is known without an event, unless an older watched
/// path covered it already (design.md 5.7).
impl<B: Backend, F: Fs> Engine<B, F> {
  /// Puts the path into the tree, a registered node for each name, and marks where the disk has
  /// to be checked. A path below a directory that the filter ignores, and one that it ignores
  /// as a file and as a directory, is left out (design.md 5.11).
  pub(super) fn register(&mut self, path: &Path) {
    let Some((top, rest)) = split_top(path) else { return };
    if self.is_ignored_above(path) {
      return;
    }
    let ignored_file = self.ignored(path, EntryKind::File);
    if ignored_file && self.ignored(path, EntryKind::Dir) {
      return;
    }

    // The anchor: the deepest node with only directories on disk above it, each listed by the
    // directory above it. The way ends at a held node, since nothing is on disk below it, and
    // at a name that the disk spelled in another way at its last look: that answer can be old,
    // and the baseline of what is below is decided by it.
    let mut anchor = Tree::ROOT;
    let mut below_directories = true;
    let mut at = Tree::ROOT;
    for name in iter::once(top).chain(rest.iter()) {
      let node = &mut self.tree[at];
      below_directories &= node.is_dir() && !node.is_unlisted();
      // A directory on the way was asked about, or there is no filter: the node keeps that.
      node.set_ignored(EntryKind::Dir, false);
      at = self.tree.child(at, name).unwrap_or_else(|| self.tree.insert_child(at, name));
      if below_directories {
        anchor = at;
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
    if !self.tree.set_path(at, true) {
      return;
    }
    let node = &mut self.tree[at];
    if node.added() == Added::No {
      node.set_added(Added::Path);
      self.touched.push(at);
    }
    let Some(parent) = self.tree.parent(at) else { return };
    // A directory asked for names only must tell more when no directory is at the new path.
    if below_directories
      && self.tells_names_only(parent)
      && !self.is_dir_on_disk(at, path)
      && let Some(parent_path) = path.parent()
    {
      self.ask(parent, parent_path, Want::Contents);
    }
    if let Some(parent) = self.tree.parent(anchor)
      // A directory whose every name is checked anyway checks the node then. A scan of the
      // names alone skips what the listing names.
      && self.tree[parent].dir().is_some_and(|dir| dir.scan.is_none_or(|depth| depth == Depth::Names))
    {
      self.tree.mark(anchor, Mark::DEEP);
    }
  }

  /// Whether what is in a directory that appears, or becomes complete, is the baseline.
  /// `relinked`: the node is a link that was none at the last check.
  pub(super) fn is_baseline_dir(&self, key: DirKey, relinked: bool) -> bool {
    // A link that the running call follows for the first time: nobody read its target before.
    let new_link = relinked && self.tree[key].added() == Added::Registered;
    new_link || !self.reported_before(key)
  }

  /// Whether what is at the node had to be reported before the running `watch`.
  pub(super) fn reported_before(&self, key: DirKey) -> bool {
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
      // A partial directory that had to track every entry before the call: the tree can be
      // behind while an older look waits for the disk.
      self.is_covered(parent, true)
    }
  }
}

/// What a watched path covers: the rule for a complete node (design.md 5.1).
impl<B: Backend, F: Fs> Engine<B, F> {
  /// Whether the directory above covers the node: it tracks every entry, and the node is one.
  pub(super) fn is_inside(&self, key: DirKey) -> bool {
    !self.tree[key].is_unlisted()
      && self.tree.parent(key).is_some_and(|parent| self.tracks_every_entry(parent))
  }

  /// The rule for a complete node: a watched path, or inside one and not ignored. The tree can be
  /// behind it until the next look (design.md 5.1).
  pub(super) fn tracks_every_entry(&self, key: DirKey) -> bool {
    match self.tree[key].kind {
      Kind::Dir(dir) => dir.complete || self.is_covered(key, false),
      Kind::File(_) | Kind::Missing => false,
    }
  }

  /// Whether the nearest watched path at or above the node covers it (`before_call`: the
  /// nearest that was one before the running `watch`): each node on the way up to it is an
  /// entry of the directory above. The filter ignores none of them as a directory: each has a
  /// directory, or lies on the way to a watched path (design.md 5.1).
  fn is_covered(&self, key: DirKey, before_call: bool) -> bool {
    let mut at = key;
    loop {
      let node = &self.tree[at];
      if node.is_path() && (!before_call || node.added() == Added::No) {
        return true;
      }
      // A node that is no entry of the directory above, or the root, comes first.
      let Some(parent) = self.tree.parent(at).filter(|_| !node.is_unlisted()) else {
        return false;
      };
      at = parent;
    }
  }
}

/// The way back: `unwatch` forgets what only the path covered and reports nothing (design.md
/// 5.7).
impl<B: Backend, F: Fs> Engine<B, F> {
  /// Makes `path` a path like any other again. A path that is not watched is left alone.
  pub(super) fn unregister(&mut self, path: &Path) {
    let Some((top, rest)) = split_top(path) else { return };
    let mut at = Tree::ROOT;
    for name in iter::once(top).chain(rest.iter()) {
      let Some(child) = self.tree.child(at, name) else { return };
      at = child;
    }
    self.forget(at);
  }

  /// Asks each directory that a `forget` of this call left on the list whether it wants less
  /// now: once for each, however many paths went out of it.
  pub(super) fn narrow_all(&mut self) {
    let mut dirs = std::mem::take(&mut self.narrowing);
    dirs.sort_unstable();
    dirs.dedup();
    for dir in dirs.drain(..) {
      self.narrow(dir, false);
    }
    self.narrowing = emptied(dirs);
  }

  /// The node is no watched path any more, and the handler hears nothing of what is forgotten.
  fn forget(&mut self, key: DirKey) {
    if !self.tree.set_path(key, false) {
      return;
    }
    let Some(parent) = self.tree.parent(key) else { return };
    // What is at the path can still count as an entry of a watched directory above: only then it
    // stays.
    if self.tree[key].is_registered() && !self.is_inside(key) {
      self.uncover(key, false);
    }
    // With no watched path below, the node is let go, and the nodes above that only led to it.
    let mut at = key;
    while !self.tree[at].is_registered()
      && let Some(parent) = self.tree.parent(at)
    {
      self.let_go(at);
      at = parent;
    }
    // The directory that held what went may want less now: the one that stays, at the end of
    // the call (`narrow_all`).
    self.narrowing.push(if at == key { parent } else { at });
  }

  /// Forgets what the tree has at the node and below it, down to each watched path, which stays.
  pub(super) fn uncover(&mut self, key: DirKey, tell_delete: bool) {
    // A list, not a recursion: a tree can be deeper than the stack takes. The directories that
    // go vanish afterwards, each after the ones below it.
    let mut gone = Vec::new();
    let mut next = Some((key, false));
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
        Kind::Dir(dir) => dir.complete = false,
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
  fn let_go(&mut self, key: DirKey) {
    // Asked while the node still says that the directory above lists it under another name.
    let inside = self.is_inside(key);
    self.tree[key].set_unlisted(false);
    // Only a registered node is followed: the link itself is the entry now, and a look must see
    // it.
    if self.tree.link(key).is_some() {
      self.tree.set_link(key, None);
      if inside {
        self.tree.mark(key, Mark::default());
      }
    }
    if !inside {
      self.uncover(key, false);
    }
    self.tree.prune(key);
  }
}

/// The questions to the ignore filter (`Options::ignored`): what is asked, when, and what a node
/// keeps of the answer (design.md 5.11).
impl<B: Backend, F: Fs> Engine<B, F> {
  /// Asks the filter about the node, and keeps the answer on it.
  pub(super) fn ignores(&mut self, key: DirKey, path: &Path, kind: EntryKind) -> bool {
    if let Some(answer) = self.tree[key].ignored(kind) {
      return answer;
    }
    let answer = self.ignored(path, kind);
    self.tree[key].set_ignored(kind, answer);
    answer
  }

  /// Whether the filter ignores a directory above the path, as a directory. Asked from the top
  /// down, whether the directory exists or not, so nothing is asked below an ignored one. A
  /// node on the way keeps the answer.
  fn is_ignored_above(&mut self, path: &Path) -> bool {
    if self.options.ignored.is_none() {
      return false;
    }
    let Some((top, rest)) = path.parent().and_then(split_top) else { return false };
    let mut dir = PathBuf::with_capacity(path.as_os_str().len());
    let mut at = Some(Tree::ROOT);
    for name in iter::once(top).chain(rest.iter()) {
      push_name(&mut dir, name);
      at = at.and_then(|at| self.tree.child(at, name));
      let ignored = match at {
        Some(at) => self.ignores(at, &dir, EntryKind::Dir),
        None => self.ignored(&dir, EntryKind::Dir),
      };
      if ignored {
        return true;
      }
    }
    false
  }

  /// Asks the filter, without keeping the answer.
  fn ignored(&self, path: &Path, kind: EntryKind) -> bool {
    self.options.ignored.as_ref().is_some_and(|ignored| ignored(path, kind))
  }
}
