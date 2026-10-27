use std::{
  iter,
  path::{Path, PathBuf},
};

use super::Engine;
use crate::{
  backend::{Backend, NodeKey, Want},
  config::EntryKind,
  fs::Fs,
  path::{push_name, split_top},
  tree::{Added, Depth, Kind, Mark, Tree, emptied},
};

impl<B: Backend, F: Fs> Engine<B, F> {
  pub(super) fn register(&mut self, path: &Path) {
    let Some((top, rest)) = split_top(path) else { return };
    if self.is_ignored_above(path) {
      return;
    }
    let ignored_file = self.ask_filter(path, EntryKind::File);
    if ignored_file && self.ask_filter(path, EntryKind::Dir) {
      return;
    }

    let mut mark_at = Tree::ROOT;
    let mut below_directories = true;
    let mut at = Tree::ROOT;
    for name in iter::once(top).chain(rest.iter()) {
      let node = &mut self.tree[at];
      below_directories &= node.is_dir() && !node.is_unlisted();
      node.set_ignored(EntryKind::Dir, false);
      at = self.tree.child(at, name).unwrap_or_else(|| self.tree.insert_child(at, name));
      if below_directories {
        mark_at = at;
      }
      let node = &mut self.tree[at];
      if !node.is_registered() {
        node.set_added(Added::Registered);
        self.touched.push(at);
        if node.is_dir() {
          self.tree.mark(at, Mark::default());
        }
      }
    }

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
    if below_directories
      && self.tells_names_only(parent)
      && !self.counts_as_dir(at, path)
      && let Some(parent_path) = path.parent()
    {
      self.ask(parent, parent_path, Want::Contents);
    }
    if let Some(parent) = self.tree.parent(mark_at)
      && self.tree[parent]
        .dir()
        .is_some_and(|dir| dir.scan.is_none_or(|depth| depth == Depth::Names))
    {
      self.tree.mark(mark_at, Mark::DEEP);
    }
  }

  pub(super) fn is_baseline_dir(&self, key: NodeKey, relinked: bool) -> bool {
    let new_link = relinked && self.tree[key].added() == Added::Registered;
    new_link || !self.watched_before(key)
  }

  pub(super) fn watched_before(&self, key: NodeKey) -> bool {
    let node = &self.tree[key];
    if node.is_path() && node.added() == Added::No {
      return true;
    }
    !node.is_unlisted() && self.tree.link(key).is_none() && self.entry_watched_before(key)
  }

  pub(super) fn entry_watched_before(&self, key: NodeKey) -> bool {
    let Some(parent) = self.tree.parent(key) else { return false };
    if self.tree[parent].is_complete() {
      !self.tree[parent].baseline()
    } else {
      self.is_covered(parent, true)
    }
  }

  pub(super) fn covered_by_parent(&self, key: NodeKey) -> bool {
    !self.tree[key].is_unlisted()
      && self.tree.link(key).is_none()
      && self.tree.parent(key).is_some_and(|parent| self.tracks_every_entry(parent))
  }

  pub(super) fn tracks_every_entry(&self, key: NodeKey) -> bool {
    match self.tree[key].kind {
      Kind::Dir(dir) => dir.complete || self.is_covered(key, false),
      Kind::File(_) | Kind::Missing => false,
    }
  }

  fn is_covered(&self, key: NodeKey, before_call: bool) -> bool {
    let mut at = key;
    loop {
      let node = &self.tree[at];
      if node.is_path() && (!before_call || node.added() == Added::No) {
        return true;
      }
      let followed = self.tree.link(at).is_some();
      let Some(parent) = self.tree.parent(at).filter(|_| !node.is_unlisted() && !followed) else {
        return false;
      };
      at = parent;
    }
  }

  pub(super) fn unregister(&mut self, path: &Path) {
    let Some((top, rest)) = split_top(path) else { return };
    let mut key = Tree::ROOT;
    for name in iter::once(top).chain(rest.iter()) {
      let Some(child) = self.tree.child(key, name) else { return };
      key = child;
    }
    if !self.tree.set_path(key, false) {
      return;
    }
    let Some(parent) = self.tree.parent(key) else { return };
    if self.tree[key].is_registered() && !self.covered_by_parent(key) {
      self.uncover(key, false);
    }
    let mut at = key;
    while !self.tree[at].is_registered()
      && let Some(parent) = self.tree.parent(at)
    {
      self.let_go(at);
      at = parent;
    }
    self.narrowing.push(if at == key { parent } else { at });
  }

  pub(super) fn narrow_all(&mut self) {
    let mut dirs = std::mem::take(&mut self.narrowing);
    dirs.sort_unstable();
    dirs.dedup();
    for dir in dirs.drain(..) {
      self.narrow(dir, false);
    }
    self.narrowing = emptied(dirs);
  }

  pub(super) fn uncover(&mut self, key: NodeKey, tell_delete: bool) {
    let mut gone = Vec::new();
    let mut next = Some((key, false));
    let mut stack = Vec::new();
    let top = key;
    while let Some((key, mut scan_comes)) = next.take().or_else(|| stack.pop()) {
      let keeps_entry = key == top && self.tree.entry(key).is_some();
      let node = &mut self.tree[key];
      if !tell_delete && !keeps_entry {
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
        scan_comes |= self.narrow(key, scan_comes);
      } else {
        gone.push(key);
      }
      let mut child = self.tree.children(key).next();
      while let Some(at) = child {
        child = self.tree.next_sibling(at);
        self.drop_entry(at, tell_delete);
        if !self.tree[at].is_path() {
          stack.push((at, scan_comes));
        }
      }
    }
    for key in gone.into_iter().rev() {
      self.vanish(key);
    }
  }

  fn let_go(&mut self, key: NodeKey) {
    if !self.covered_by_parent(key) {
      self.uncover(key, false);
    }
    self.tree[key].set_unlisted(false);
    if self.tree.link(key).is_some() {
      if let Some(entry) = self.tree.entry(key) {
        self.tree[key].kind = Kind::File(entry.file);
      }
      self.tree.set_link(key, None);
      if self.covered_by_parent(key) {
        self.tree.mark(key, Mark::default());
      }
    }
    self.tree.prune(key);
  }

  pub(super) fn ignores(&mut self, key: NodeKey, path: &Path, kind: EntryKind) -> bool {
    if let Some(answer) = self.tree[key].ignored(kind) {
      return answer;
    }
    let answer = self.ask_filter(path, kind);
    self.tree[key].set_ignored(kind, answer);
    answer
  }

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
        None => self.ask_filter(&dir, EntryKind::Dir),
      };
      if ignored {
        return true;
      }
    }
    false
  }

  fn ask_filter(&self, path: &Path, kind: EntryKind) -> bool {
    self.options.ignored.as_ref().is_some_and(|ignored| ignored(path, kind))
  }
}
