//! The OS watches: what each directory asks from the backend, the commit that makes the watches
//! live, and what is polled because no OS watch tells about it.

use std::{
  io, mem,
  path::{Path, PathBuf},
};

use super::{Engine, check::is_missing};
use crate::{
  backend::{Backend, Coverage, DirKey, Want, is_watch_limit, same},
  config::EntryKind,
  error::{ErrorKind, context},
  fs::{FileType, Fs},
  tree::{Depth, Kind, Mark, Node, Tree, Watch},
};

/// Watch, commit, then scan: every ask queues a scan for after the next commit.
impl<B: Backend, F: Fs> Engine<B, F> {
  /// Asks the backend for the directory when the node wants more than was asked, or `force`
  /// says the watch may be gone.
  pub(super) fn arm(&mut self, key: DirKey, path: &mut PathBuf, force: bool) {
    let Some(asked) = self.tree[key].dir().map(|dir| dir.asked) else { return };
    if self.backend.is_none() {
      return self.set_state(key, Watch::Polled);
    }
    if asked.is_none() && self.is_covered_from_above(key) {
      return self.set_state(key, Watch::FromAbove);
    }
    // The want only goes up (backend rule 5): with the contents asked, the disk is not read.
    let want = if asked == Some(Want::Contents) { Want::Contents } else { self.want(key, path) };
    if !force && asked == Some(want) {
      return;
    }
    self.ask(key, path, want);
  }

  /// Whether a watch for a tree above reports the directory: not through a link.
  fn is_covered_from_above(&self, key: DirKey) -> bool {
    let linked = self.tree.link(key).is_some();
    let above = self.tree.parent(key).map(|parent| self.tree[parent].watch());
    !linked && matches!(above, Some(Watch::WatchedTree | Watch::FromAbove))
  }

  /// Whether the directory has a watch of its own that a watch above makes needless.
  pub(super) fn has_a_watch_to_give_back(&self, key: DirKey) -> bool {
    let has_its_own = self.tree[key].dir().is_some_and(|dir| dir.asked.is_some());
    has_its_own && self.backend.is_some() && self.is_covered_from_above(key)
  }

  /// Gives the watch of each directory on `covered` back, after the commit: the watch above is
  /// live, and the deep scan that follows finds what the old watch did not deliver.
  pub(super) fn give_back_covered(&mut self) {
    let mut covered = mem::take(&mut self.covered);
    for key in covered.drain(..) {
      // Nothing is on disk below a held node: its second look decides for this one too.
      if self.tree.get(key).is_none() || self.tree.frozen(key) {
        continue;
      }
      if self.has_a_watch_to_give_back(key) {
        self.unwatch_dir(key);
        if let Some(dir) = self.tree[key].dir_mut() {
          dir.asked = None;
        }
        self.set_state(key, Watch::FromAbove);
      } else {
        // The watch above is gone again: ask for an own one, and the scan waits for the next
        // commit.
        let mut path = self.tree.path(key);
        self.arm(key, &mut path, true);
      }
    }
    self.covered = covered;
  }

  /// Asks the backend to watch the directory with `want`, and queues the scan.
  pub(super) fn ask(&mut self, key: DirKey, path: &Path, want: Want) {
    if let Some(dir) = self.tree[key].dir_mut() {
      dir.asked = Some(want);
    }
    let answer = self.watch_dir(key, path, want);
    self.set_watch(key, answer, path);
    self.tree.queue_scan(key, Depth::Shallow);
  }

  /// After `unwatch` the directory may want names only: its watch for the contents is given back
  /// and asked again. `scan_comes`: a deep scan from above comes here anyway. Says whether a
  /// watch for a whole tree went.
  pub(super) fn narrow(&mut self, key: DirKey, scan_comes: bool) -> bool {
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
      self.set_state(key, Watch::Polled);
    } else {
      let mut path = self.tree.path(key);
      self.arm(key, &mut path, true);
      if covers_a_tree {
        self.tree.queue_scan(key, Depth::Deep);
      }
    }
    covers_a_tree
  }

  pub(super) fn tells_names_only(&self, key: DirKey) -> bool {
    self.tree[key].dir().is_some_and(|dir| dir.asked == Some(Want::Names))
  }

  /// Whether a directory is at the watched path now. Only the kind is read, nothing is stored.
  pub(super) fn is_dir_on_disk(&mut self, key: DirKey, path: &Path) -> bool {
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
  /// watch reports now (see `ask_again`).
  pub(super) fn commit(&mut self) -> Vec<DirKey> {
    let Some(backend) = &mut self.backend else {
      self.covered.clear();
      return Vec::new();
    };
    let started = self.fs.ticks();
    let committed = backend.commit();
    // A commit can take long (an FSEvents stream: a quarter of a second); the pacing leaves it
    // out.
    self.commit_took += self.fs.ticks().saturating_sub(started);
    let error = match committed {
      Ok(now_watched) => return now_watched,
      Err(error) => error,
    };
    // A watch above a covered directory may not be live: it keeps its own and is asked again.
    self.covered.clear();
    // Every node asked since the last commit is in the scan queue: all of them count as failed.
    let asked = |node: &Node| {
      node.dir().is_some_and(|dir| {
        dir.asked.is_some() && matches!(dir.watch, Watch::Watched | Watch::WatchedTree)
      })
    };
    let queued = self.tree.to_scan.clone();
    for key in queued {
      if self.tree.get(key).is_some_and(asked) {
        let path = self.tree.path(key);
        self.set_watch(key, Coverage::Failed(same(&error)), &path);
      }
    }
    Vec::new()
  }

  /// Asks again for the directories that got `NotWatched` and that an OS watch reports now.
  pub(super) fn ask_again(&mut self, keys: Vec<DirKey>) {
    for key in keys {
      let Some(dir) = self.tree.get(key).and_then(Node::dir) else { continue };
      let (Watch::Polled, Some(want)) = (dir.watch, dir.asked) else { continue };
      let path = self.tree.path(key);
      let answer = self.watch_dir(key, &path, want);
      if matches!(answer, Coverage::NotWatched) {
        continue;
      }
      self.set_watch(key, answer, &path);
      self.tree.queue_scan(key, Depth::Shallow);
    }
  }

  /// What the directory asks from the backend (design.md 5.10). An ignored watch target asks
  /// for names only: the check of the child drops it.
  fn want(&mut self, key: DirKey, path: &mut PathBuf) -> Want {
    let node = &self.tree[key];
    if node.is_complete() {
      return Want::Contents;
    }
    // Known to be no watch target: the disk is not read.
    if node.ignored(EntryKind::Dir) == Some(true) {
      return Want::Names;
    }
    let for_a_child = match self.tree.want(key) {
      Some(want) => want == Want::Contents,
      None => self.has_no_dir_at_a_watched_child(key, path),
    };
    if for_a_child && !self.is_ignored_target(key) { Want::Contents } else { Want::Names }
  }

  /// Whether a watched path in the directory, where the tree has nothing, has no directory on
  /// disk. A name on the way is not asked: a directory is expected there.
  fn has_no_dir_at_a_watched_child(&mut self, key: DirKey, path: &mut PathBuf) -> bool {
    let mut next = self.tree.children(key).next();
    while let Some(child) = next {
      next = self.tree.next_sibling(child);
      let node = &self.tree[child];
      if !node.is_path() || !matches!(node.kind, Kind::Missing) {
        continue;
      }
      path.push(self.tree.name(child));
      let is_dir = self.is_dir_on_disk(child, path);
      path.pop();
      if !is_dir {
        return true;
      }
    }
    false
  }

  /// Notes how changes in the directory are heard when the backend was not asked.
  fn set_state(&mut self, key: DirKey, state: Watch) {
    if let Some(dir) = self.tree[key].dir_mut() {
      dir.watch = state;
    }
    self.tree.list_if_polled(key);
  }

  /// Stores what the backend answered for the directory.
  fn set_watch(&mut self, key: DirKey, answer: Coverage, path: &Path) {
    let Some(dir) = self.tree[key].dir_mut() else { return };
    let old = dir.watch;
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
    if let Some(error) = to_report {
      let kind = match error.kind() {
        _ if is_watch_limit(&error) => ErrorKind::WatchLimit,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::Io,
      };
      self.report(kind, Some(path.to_path_buf()), Some(error));
    }
    if old == Watch::WatchedTree && new != Watch::WatchedTree {
      // The nodes below relied on this watch. A deep scan asks for a watch for each of them.
      self.tree.queue_scan(key, Depth::Deep);
    }
  }
}

/// Where no OS watch tells: the poll, a directory that may have moved, a dead backend.
impl<B: Backend, F: Fs> Engine<B, F> {
  /// The work of one poll: everything that no OS watch tells about.
  pub(super) fn mark_polled(&mut self) {
    self.tree.tidy_polled();
    // Nothing tells when a link leads somewhere else: each poll checks the registered links whose
    // directory is not polled anyway.
    let has_to_be_marked = |&key: &DirKey| {
      let parent = self.tree.parent(key).and_then(|parent| self.tree[parent].dir());
      parent.is_some_and(|parent| !parent.is_polled())
    };
    let links: Vec<DirKey> = self.tree.links().filter(has_to_be_marked).collect();

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
  pub(super) fn check_moved(&mut self) {
    // Only the marks that are there now: the ones that this loop adds are not looked at.
    for index in 0..self.tree.marked.len() {
      let (key, _) = self.tree.marked[index];
      if !self.tree.get(key).is_some_and(Node::is_marked) {
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

  /// What the backend answers for the directory. A backend that is gone watches nothing.
  fn watch_dir(&mut self, key: DirKey, path: &Path, want: Want) -> Coverage {
    let backend = self.backend.as_mut();
    backend.map_or(Coverage::NotWatched, |backend| backend.watch(key, path, want))
  }

  pub(super) fn unwatch_dir(&mut self, key: DirKey) {
    if let Some(backend) = &mut self.backend {
      backend.unwatch(key);
    }
  }

  /// The backend cannot go on: it is dropped, and every node is polled.
  pub(super) fn backend_died(&mut self, error: &io::Error) {
    if self.backend.take().is_none() {
      return;
    }
    let error = context("the OS watcher stopped, so every path is polled", error);
    self.report(ErrorKind::Io, None, Some(error));
    // The poll is all there is now: it runs as often as a watcher that only polls.
    if let Some(alone) = self.options.poll_interval_alone {
      self.options.poll_interval = alone;
    }
    self.tree.poll_all();
    // Its last notifications may be lost.
    self.tree.scan_all();
  }

  /// The directory at or above the node that only a poll of its parent checks.
  fn below_polled_parent(&self, mut key: DirKey) -> Option<DirKey> {
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
