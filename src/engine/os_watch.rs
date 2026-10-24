//! The OS watches: what each directory asks from the backend, the commit that makes the watches
//! live, and what is polled because no OS watch tells about it.

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
  /// Asks the backend for the directory when the node wants more than was asked, or `force`
  /// says the watch may be gone.
  pub(super) fn arm(&mut self, key: NodeKey, path: &mut PathBuf, force: bool) {
    let Some(asked) = self.tree[key].dir().map(|dir| dir.asked) else { return };
    if self.backend.is_none() {
      return self.set_watch(key, Watch::Polled);
    }
    // A watch for a tree above reports the directory: not through a link.
    if asked.is_none()
      && self.tree.link(key).is_none()
      && self.tree.parent(key).is_some_and(|parent| self.tree[parent].watch().covers_below())
    {
      return self.set_watch(key, Watch::FromAbove);
    }
    // The want only goes up (backend rule 5): with the contents asked, the disk is not read.
    let want = if asked == Some(Want::Contents) { Want::Contents } else { self.want(key, path) };
    if !force && asked == Some(want) {
      return;
    }
    self.ask(key, path, want);
  }

  /// Asks the backend to watch the directory with `want`, and queues the scan.
  pub(super) fn ask(&mut self, key: NodeKey, path: &Path, want: Want) {
    if let Some(dir) = self.tree[key].dir_mut() {
      dir.asked = Some(want);
    }
    let answer = self.watch_dir(key, path, want);
    self.store_answer(key, answer, path);
    self.tree.queue_scan(key, Depth::Shallow);
  }

  /// After `unwatch` the directory may want names only: its watch for the contents is given back
  /// and asked again. `scan_comes`: a deep scan from above comes here anyway. Says whether a
  /// watch for a whole tree went.
  pub(super) fn narrow(&mut self, key: NodeKey, scan_comes: bool) -> bool {
    let Some(dir) = self.tree.get(key).and_then(Node::dir) else { return false };
    let covers_a_tree = match (dir.watch, dir.asked) {
      (Watch::WatchedTree, Some(Want::Contents)) => true,
      (Watch::Watched, Some(Want::Contents)) => false,
      _ => return false,
    };
    // Where the tree has nothing at a registered name, the watch stays.
    if self.tree.want(key) != Some(Want::Names) {
      return false;
    }
    self.unwatch_dir(key);
    if let Some(dir) = self.tree[key].dir_mut() {
      dir.asked = None;
    }
    if scan_comes {
      // No OS watch covers it until that scan.
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

  /// Whether a directory is at the watched path now. Only the kind is read, nothing is stored.
  pub(super) fn is_dir_on_disk(&mut self, key: NodeKey, path: &Path) -> bool {
    let kind = self.fs.lstat(path).map(|meta| meta.kind);
    let kind = if matches!(kind, Ok(FileType::Symlink)) {
      self.fs.stat(path).map(|meta| meta.kind)
    } else {
      kind
    };
    // As in `read_entry`: an ignored watched path counts as missing, a name on the way never.
    matches!(kind, Ok(FileType::Dir))
      && (self.tree[key].is_ancestor() || !self.ignores(key, path, EntryKind::Dir))
  }

  /// Makes the watches asked for since the last commit live. Gives the directories that an OS
  /// watch reports now (see `ask_again`). A key that got no watch is polled and asked again.
  pub(super) fn commit(&mut self) -> Vec<NodeKey> {
    let Some(backend) = &mut self.backend else { return Vec::new() };
    let Committed { now_watched, failed } = backend.commit();
    for (key, error) in failed {
      let asked = self.tree.get(key).and_then(Node::dir).is_some_and(|dir| dir.asked.is_some());
      if asked {
        let path = self.tree.path(key);
        self.store_answer(key, Coverage::Failed(error), &path);
      }
    }
    now_watched
  }

  /// Asks again for the directories that got `NotWatched` and that an OS watch reports now.
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

  /// What the directory asks from the backend (design.md 5.10). Where the tree has nothing at a
  /// watched path in it, the disk says: the contents when no directory is there. A name on the
  /// way is not asked: a directory is expected there.
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

  /// Notes how changes in the directory are heard when the backend was not asked.
  fn set_watch(&mut self, key: NodeKey, watch: Watch) {
    if let Some(dir) = self.tree[key].dir_mut() {
      dir.watch = watch;
    }
    self.tree.list_if_polled(key);
  }

  /// Stores what the backend answered for the directory.
  fn store_answer(&mut self, key: NodeKey, answer: Coverage, path: &Path) {
    let Some(dir) = self.tree[key].dir_mut() else { return };
    let (old, volume) = (dir.watch, dir.id.volume);
    let mut to_report = None;
    let new = match answer {
      Coverage::Watched => Watch::Watched,
      Coverage::WatchedTree => Watch::WatchedTree,
      Coverage::NotWatched => Watch::Polled,
      Coverage::Failed(error) => {
        // A directory that is gone is no error: the scan that follows finds it missing.
        if !dir.has_error && !is_missing(&error) {
          dir.has_error = true;
          to_report = Some(error);
        }
        Watch::Failed
      }
    };
    dir.watch = new;
    self.tree.list_if_polled(key);
    // An OS watch reports only what changes on this machine: on a disk that other machines
    // change too, the handler is told once for the disk.
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
      // A tree past the limit finds no room for one directory after the other.
      if kind != ErrorKind::WatchLimit || !mem::replace(&mut self.told_watch_limit, true) {
        self.report(kind, path, error);
      }
    }
    if old == Watch::WatchedTree && new != Watch::WatchedTree {
      // The nodes below relied on this watch. A deep scan asks for a watch for each of them. It
      // is away (5.5): the records that the old watch had not given yet are gone with it.
      self.tree.queue_scan(key, Depth::Deep);
      if let Some(dir) = self.tree[key].dir_mut() {
        dir.away = true;
      }
    }
  }

  /// What the backend answers for the directory. A backend that is gone watches nothing.
  fn watch_dir(&mut self, key: NodeKey, path: &Path, want: Want) -> Coverage {
    let backend = self.backend.as_mut();
    backend.map_or(Coverage::NotWatched, |backend| backend.watch(key, path, want))
  }

  pub(super) fn unwatch_dir(&mut self, key: NodeKey) {
    if let Some(backend) = &mut self.backend {
      backend.unwatch(key);
      self.gave_back = true;
    }
  }

  /// The work of one poll: everything that no OS watch tells about.
  pub(super) fn mark_polled(&mut self) {
    self.tree.tidy_polled();
    // Nothing tells when a link leads somewhere else: each poll checks the registered links whose
    // directory is not polled anyway.
    let has_to_be_marked = |&key: &NodeKey| {
      let parent = self.tree.parent(key).and_then(|parent| self.tree[parent].dir());
      parent.is_some_and(|parent| !parent.is_polled())
    };
    let links: Vec<NodeKey> = self.tree.links().filter(has_to_be_marked).collect();

    // Only the directories that are on the list now: `arm` can put one on it.
    for index in 0..self.tree.polled.len() {
      let key = self.tree.polled[index];
      // A held directory is not looked at before its second look.
      let Some(dir) = self.tree[key].dir() else { continue };
      if dir.watch == Watch::Failed {
        // The backend may have room now. `arm` queues the scan.
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

  /// A mark can come through a watch whose directory moved. Where only a poll checks the parent,
  /// the parent checks that directory in this flush too, before the mark (design.md 5.10).
  pub(super) fn mark_moved(&mut self, hints_due: bool) {
    // Only the marks that are there now: the ones that this loop adds are not looked at.
    for index in 0..self.tree.marked.len() {
      let (key, since) = self.tree.marked[index];
      if (since.is_some() && !hints_due) || !self.tree.get(key).is_some_and(Node::is_marked) {
        continue;
      }
      let Some(parent) = self.tree.parent(key) else { continue };
      // A plain mark on a node that has a loss mark would take it out of the paced wait.
      if let Some(moved) = self.below_polled_parent(parent)
        && !self.tree[moved].is_marked()
      {
        self.tree.mark(moved, Mark::default());
      }
    }
  }

  /// The backend cannot go on: it is dropped, and every node is polled.
  pub(super) fn backend_died(&mut self, error: io::Error) {
    if self.backend.take().is_none() {
      return;
    }
    // It comes outside a flush, where no other error waits.
    self.errors.push(Error::of_watcher("the OS watcher stopped, so every path is polled", error));
    // The poll is all there is now: the watcher works as one that only polls, at its interval
    // and with no delete delay. A hold that waits keeps its time.
    if let Some(fallback) = self.fallback.take() {
      self.options = fallback;
    }
    // No backend answers a question about a delivery any more: the scans below look at every
    // young file that was not read, and count it as written (5.8).
    self.pending = BinaryHeap::new();
    self.tree.poll_all();
    // Its last notifications may be lost.
    self.tree.queue_scan(Tree::ROOT, Depth::Deep);
  }

  /// The directory at or above the node that only a poll of its parent checks.
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
}
