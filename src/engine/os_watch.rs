use std::{
  collections::BinaryHeap,
  io, mem,
  path::{Path, PathBuf},
};

use super::Engine;
use crate::{
  backend::{Backend, Committed, Coverage, NodeKey, Want, is_watch_limit},
  config::EntryKind,
  error::{Error, ErrorKind},
  fs::{FileType, Fs, is_missing},
  path::push_name,
  tree::{Depth, Kind, Mark, Node, Tree, Watch},
};

impl<B: Backend, F: Fs> Engine<B, F> {
  pub(super) fn arm(&mut self, key: NodeKey, path: &mut PathBuf, force: bool) {
    let Some(asked) = self.tree[key].dir().map(|dir| dir.asked) else { return };
    if self.backend.is_none() {
      return self.set_watch(key, Watch::Polled);
    }
    if asked.is_none()
      && self.tree.link(key).is_none()
      && self.tree.parent(key).is_some_and(|parent| self.tree[parent].watch().covers_below())
    {
      return self.set_watch(key, Watch::FromAbove);
    }
    let want = if asked == Some(Want::Contents) { Want::Contents } else { self.want(key, path) };
    if !force && asked == Some(want) {
      return;
    }
    self.ask(key, path, want);
  }

  fn want(&mut self, key: NodeKey, path: &mut PathBuf) -> Want {
    if let Some(want) = self.tree.want(key) {
      return want;
    }
    let mut next = self.tree.children(key).next();
    while let Some(child) = next {
      next = self.tree.next_sibling(child);
      let node = &self.tree[child];
      if !node.is_path() || !matches!(node.kind, Kind::Missing) {
        continue;
      }
      push_name(path, self.tree.name(child));
      let is_dir = self.is_dir_on_disk(child, path);
      path.pop();
      if !is_dir {
        return Want::Contents;
      }
    }
    Want::Names
  }

  pub(super) fn is_dir_on_disk(&mut self, key: NodeKey, path: &Path) -> bool {
    let kind = self.fs.lstat(path).map(|meta| meta.kind);
    let kind = if matches!(kind, Ok(FileType::Symlink)) {
      self.fs.stat(path).map(|meta| meta.kind)
    } else {
      kind
    };
    matches!(kind, Ok(FileType::Dir))
      && (self.tree[key].is_ancestor() || !self.ignores(key, path, EntryKind::Dir))
  }

  pub(super) fn ask(&mut self, key: NodeKey, path: &Path, want: Want) {
    if let Some(dir) = self.tree[key].dir_mut() {
      dir.asked = Some(want);
    }
    let answer = self.watch_dir(key, path, want);
    self.store_answer(key, answer, path);
    self.tree.queue_scan(key, Depth::Shallow);
  }

  fn set_watch(&mut self, key: NodeKey, watch: Watch) {
    if let Some(dir) = self.tree[key].dir_mut()
      && mem::replace(&mut dir.watch, watch) != watch
    {
      self.tree.list_if_polled(key);
    }
  }

  fn store_answer(&mut self, key: NodeKey, answer: Coverage, path: &Path) {
    let Some(dir) = self.tree[key].dir_mut() else { return };
    let (old, volume) = (dir.watch, dir.id.volume);
    let mut to_report = None;
    let new = match answer {
      Coverage::Watched => Watch::Watched,
      Coverage::WatchedTree => Watch::WatchedTree,
      Coverage::NotWatched => Watch::Polled,
      Coverage::Failed(error) => {
        if !dir.has_error && !is_missing(&error) {
          dir.has_error = true;
          to_report = Some(error);
        }
        Watch::Failed
      }
    };
    self.set_watch(key, new);
    if matches!(new, Watch::Watched | Watch::WatchedTree)
      && self.asked_disks.insert(volume)
      && let Some(kind) = self.fs.remote_disk(path)
    {
      let what = format!("a disk of {kind}: polling finds those changes (`with_polling`)");
      self.report(ErrorKind::RemoteDisk, path, io::Error::other(what));
    }
    if let Some(error) = to_report {
      let kind = match error.kind() {
        _ if is_watch_limit(&error) => ErrorKind::WatchLimit,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::Io,
      };
      if kind != ErrorKind::WatchLimit || !mem::replace(&mut self.told_watch_limit, true) {
        self.report(kind, path, error);
      }
    }
    if old == Watch::WatchedTree && new != Watch::WatchedTree {
      self.tree.queue_scan(key, Depth::Deep);
      if let Some(dir) = self.tree[key].dir_mut() {
        dir.away = true;
      }
    }
  }

  fn watch_dir(&mut self, key: NodeKey, path: &Path, want: Want) -> Coverage {
    self.backend.as_mut().map_or(Coverage::NotWatched, |backend| backend.watch(key, path, want))
  }

  pub(super) fn unwatch_dir(&mut self, key: NodeKey) {
    if let Some(backend) = &mut self.backend {
      backend.unwatch(key);
      self.gave_back = true;
    }
  }

  pub(super) fn narrow(&mut self, key: NodeKey, scan_comes: bool) -> bool {
    let Some(dir) = self.tree.get(key).and_then(Node::dir) else { return false };
    let covers_a_tree = match (dir.watch, dir.asked) {
      (Watch::WatchedTree, Some(Want::Contents)) => true,
      (Watch::Watched, Some(Want::Contents)) => false,
      _ => return false,
    };
    if self.tree.want(key) != Some(Want::Names) {
      return false;
    }
    self.unwatch_dir(key);
    if let Some(dir) = self.tree[key].dir_mut() {
      dir.asked = None;
    }
    if scan_comes {
      self.set_watch(key, Watch::Polled);
    } else {
      let mut path = self.tree.path(key);
      self.arm(key, &mut path, true);
      if covers_a_tree {
        self.tree.queue_scan(key, Depth::Deep);
      }
    }
    covers_a_tree
  }

  pub(super) fn tells_names_only(&self, key: NodeKey) -> bool {
    self.tree[key].dir().is_some_and(|dir| dir.asked == Some(Want::Names))
  }

  pub(super) fn commit(&mut self) -> Vec<NodeKey> {
    let Some(backend) = &mut self.backend else { return Vec::new() };
    let Committed { now_watched, failed } = backend.commit();
    for (key, error) in failed {
      if self.tree.get(key).and_then(Node::dir).is_some_and(|dir| dir.asked.is_some()) {
        let path = self.tree.path(key);
        self.store_answer(key, Coverage::Failed(error), &path);
      }
    }
    now_watched
  }

  pub(super) fn ask_again(&mut self, keys: Vec<NodeKey>) {
    for key in keys {
      let Some(dir) = self.tree.get(key).and_then(Node::dir) else { continue };
      let (Watch::Polled, Some(want)) = (dir.watch, dir.asked) else { continue };
      let path = self.tree.path(key);
      let answer = self.watch_dir(key, &path, want);
      if matches!(answer, Coverage::NotWatched) {
        continue;
      }
      self.store_answer(key, answer, &path);
      self.tree.queue_scan(key, Depth::Shallow);
    }
  }

  pub(super) fn mark_polled(&mut self) {
    self.tree.tidy_polled();
    let has_to_be_marked = |&key: &NodeKey| {
      let parent = self.tree.parent(key).and_then(|parent| self.tree[parent].dir());
      parent.is_some_and(|parent| !parent.is_polled())
    };
    let links: Vec<NodeKey> = self.tree.links().filter(has_to_be_marked).collect();

    for index in 0..self.tree.polled.len() {
      let key = self.tree.polled[index];
      let Some(dir) = self.tree[key].dir() else { continue };
      if dir.watch == Watch::Failed {
        let mut path = self.tree.path(key);
        self.arm(key, &mut path, true);
      } else {
        self.tree.queue_scan(key, Depth::Shallow);
      }
    }
    for key in links {
      self.tree.mark(key, Mark::default());
    }
  }

  pub(super) fn mark_moved(&mut self, hints_due: bool) {
    for index in 0..self.tree.marked.len() {
      let (key, since) = self.tree.marked[index];
      if (since.is_some() && !hints_due) || !self.tree.get(key).is_some_and(Node::is_marked) {
        continue;
      }
      let Some(parent) = self.tree.parent(key) else { continue };
      if let Some(moved) = self.below_polled_parent(parent)
        && !self.tree[moved].is_marked()
      {
        self.tree.mark(moved, Mark::default());
      }
    }
  }

  fn below_polled_parent(&self, mut key: NodeKey) -> Option<NodeKey> {
    loop {
      let parent = self.tree.parent(key).filter(|&parent| parent != Tree::ROOT)?;
      let linked = self.tree.link(key).is_some();
      if linked || matches!(self.tree[parent].watch(), Watch::Polled | Watch::Failed) {
        return Some(key);
      }
      key = parent;
    }
  }

  pub(super) fn backend_died(&mut self, error: io::Error) {
    if self.backend.take().is_none() {
      return;
    }
    self.errors.push(Error::of_watcher("the OS watcher stopped, so every path is polled", error));
    if let Some(fallback) = self.fallback.take() {
      self.options = fallback;
    }
    self.pending = BinaryHeap::new();
    self.tree.poll_all();
    self.tree.queue_scan(Tree::ROOT, Depth::Deep);
  }
}
