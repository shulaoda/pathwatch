//! The OS watches: what each directory asks from the backend, the commit that makes the watches
//! live, and what is polled because no OS watch tells about it.

use std::{
  io,
  path::{Path, PathBuf},
};

use super::{Engine, check::is_missing};
use crate::{
  backend::{Backend, Coverage, DirKey, Want, is_watch_limit},
  config::EntryKind,
  error::ErrorKind,
  fs::{FileType, Fs},
  tree::{Depth, Dir, Kind, Mark, Node, Tree, Watch},
};

/// Asking the backend. A watch is live only after the next commit, so every ask queues a scan
/// for after it: watch, commit, then scan.
impl<B: Backend, F: Fs> Engine<B, F> {
  /// Asks the backend to watch the directory `key` when the node wants more than was asked, or
  /// when `force` says that the watch may be gone. `path` is the path of the directory.
  pub(super) fn arm(&mut self, key: DirKey, path: &mut PathBuf, force: bool) {
    let Some(asked) = self.tree[key].dir().map(|dir| dir.asked) else { return };
    let covered_from_above = self.tree.parent(key).is_some_and(|parent| {
      // A watch for a tree reports the real directories below it. A directory that is reached
      // through a link is not one of them.
      let linked = self.tree.link(key).is_some();
      !linked && matches!(self.tree[parent].watch(), Watch::WatchedTree | Watch::FromAbove)
    });
    if self.backend_failed {
      return self.set_state(key, Watch::Polled);
    }
    // A watch above covers this directory, and the backend holds none for it.
    if asked.is_none() && covered_from_above {
      return self.set_state(key, Watch::FromAbove);
    }
    // For one key the want only goes up (rule 5 of the backend), and no more than the
    // contents can be asked: then nothing has to be worked out, and the disk is not read.
    let want = if asked == Some(Want::Contents) { Want::Contents } else { self.want(key, path) };
    if !force && asked == Some(want) {
      return;
    }
    self.ask(key, path, want);
  }

  /// Asks the backend to watch the directory `key` with `want`, and queues the scan.
  pub(super) fn ask(&mut self, key: DirKey, path: &Path, want: Want) {
    if let Some(dir) = self.tree[key].dir_mut() {
      dir.asked = Some(want);
    }
    let answer = self.backend.watch(key, path, want);
    self.set_watch(key, answer, path);
    // What happened while no watch covered the directory is found by a scan after the commit.
    // No hint and maybe no poll comes for it now.
    self.tree.queue_scan(key, Depth::Shallow);
  }

  /// The directory `key` may want less than the backend was asked for: `unwatch` took a
  /// watched path away.
  ///
  /// A watch for one directory stays as it is: for one key the want only goes up. A watch for
  /// a whole tree that was asked for the contents is given back, and the backend is asked
  /// again, for names. Then everything below is scanned deep: the nodes that relied on the
  /// watch ask for their own.
  ///
  /// `scan_comes`: a directory above gave its watch back in this call, and its deep scan comes
  /// here too. That scan asks the backend and scans everything below, so nothing more is done
  /// here: a tree in which many levels give a watch back is scanned once, not once for each
  /// level. Says whether the watch was given back.
  pub(super) fn narrow(&mut self, key: DirKey, scan_comes: bool) -> bool {
    let is_wide = |dir: &Dir| (dir.watch, dir.asked) == (Watch::WatchedTree, Some(Want::Contents));
    if !self.tree.get(key).and_then(Node::dir).is_some_and(is_wide) {
      return false;
    }
    // Only when the tree knows that names are enough: where it has nothing at a registered
    // name, the watch stays as it is.
    if self.tree.want(key) != Some(Want::Names) {
      return false;
    }
    self.backend.unwatch(key);
    if let Some(dir) = self.tree[key].dir_mut() {
      dir.asked = None;
    }
    if scan_comes {
      // Until that scan, no OS watch covers the directory.
      self.set_state(key, Watch::Polled);
    } else {
      let mut path = self.tree.path(key);
      self.arm(key, &mut path, true);
      self.tree.queue_scan(key, Depth::Deep);
    }
    true
  }

  /// Whether the backend was asked to tell names only for the directory `key`.
  pub(super) fn tells_names_only(&self, key: DirKey) -> bool {
    self.tree[key].dir().is_some_and(|dir| dir.asked == Some(Want::Names))
  }

  /// Whether the next check of the watched path at the node `key` will find a directory there.
  /// Only the kind is read. `path` is the path of the node.
  pub(super) fn is_dir_on_disk(&mut self, key: DirKey, path: &Path) -> bool {
    let kind = self.fs.lstat(path).map(|meta| meta.kind);
    // A watched path is followed when it is a link.
    let kind = if matches!(kind, Ok(FileType::Symlink)) {
      self.fs.stat(path).map(|meta| meta.kind)
    } else {
      kind
    };
    // The rules of `read_entry`: a watched path that the filter ignores counts as missing,
    // but a directory on the way to another watched path is never filtered.
    matches!(kind, Ok(FileType::Dir))
      && (self.tree[key].is_ancestor() || !self.ignores(key, path, EntryKind::Dir))
  }

  /// Makes the watches asked for since the last commit live. `true`: the backend reports more
  /// directories now than its answers said (see `ask_again`).
  pub(super) fn commit(&mut self) -> bool {
    if self.backend_failed {
      return false;
    }
    let started = self.fs.now();
    let committed = self.backend.commit();
    // Making watches live can take long: a new stream of FSEvents takes a quarter of a
    // second. That is paid once, not for every look at the disk, so the pacing leaves it out.
    self.commit_took += self.fs.now().duration_since(started).unwrap_or_default();
    let Err(error) = committed else { return self.backend.covers_more() };
    // None of the watches asked for since the last commit is live. `ask` queued each of those
    // nodes for a scan, so they are all in the queue. A node in the queue that was asked
    // earlier is taken for failed too: it is polled and asked again, which loses nothing.
    let asked = |node: &Node| {
      node.dir().is_some_and(|dir| {
        dir.asked.is_some() && matches!(dir.watch, Watch::Watched | Watch::WatchedTree)
      })
    };
    let queued = self.tree.to_scan.clone();
    for key in queued {
      if self.tree.get(key).is_some_and(asked) {
        let path = self.tree.path(key);
        let error = io::Error::new(error.kind(), error.to_string());
        self.set_watch(key, Coverage::Failed(error), &path);
      }
    }
    false
  }

  /// Asks the backend again for every directory that is polled because the backend said
  /// `NotWatched`: an OS watch that the last commit started may report it now.
  ///
  /// A directory that is reported now is scanned once after the next commit, as after every
  /// ask: watch, commit, then scan. From then on no poll looks at it.
  pub(super) fn ask_again(&mut self) {
    self.tree.tidy_polled();
    // Only the directories that are on the list now: `set_watch` can put one on it.
    for index in 0..self.tree.polled.len() {
      let key = self.tree.polled[index];
      let Some(dir) = self.tree[key].dir() else { continue };
      let (Watch::Polled, Some(want)) = (dir.watch, dir.asked) else { continue };
      let path = self.tree.path(key);
      let answer = self.backend.watch(key, &path, want);
      // Still not watched: nothing changed, and the poll goes on.
      if matches!(answer, Coverage::NotWatched) {
        continue;
      }
      self.set_watch(key, answer, &path);
      self.tree.queue_scan(key, Depth::Shallow);
    }
  }

  /// What the directory `key` asks from the backend: `Contents` when the node is complete, or
  /// when one of its registered children is no directory. `Names` otherwise: a directory at a
  /// registered name asks for its own watch.
  ///
  /// A directory that would ask for the contents because of a registered child is the watch
  /// target of that child. The filter is asked about it first, and an ignored directory asks
  /// for names only: the check of the child drops it. `path` is the path of the directory.
  fn want(&mut self, key: DirKey, path: &mut PathBuf) -> Want {
    let node = &self.tree[key];
    if node.is_complete() {
      return Want::Contents;
    }
    // The filter said before that this directory is no watch target: the disk is not read.
    if node.ignored(EntryKind::Dir) == Some(true) {
      return Want::Names;
    }
    let for_a_child = match self.tree.want(key) {
      Some(want) => want == Want::Contents,
      None => self.has_no_dir_at_a_watched_child(key, path),
    };
    if for_a_child && !self.is_ignored_target(key) { Want::Contents } else { Want::Names }
  }

  /// Whether the disk has a file or nothing at a watched path in the directory `key` where
  /// the tree has nothing. Only the kind is read, and nothing is stored: the look that counts
  /// comes when the watch is live.
  ///
  /// A name on the way to a watched path is not asked: a directory is expected there, and the
  /// check that finds none asks for the contents (see the end of `check`).
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

  /// Notes how changes in the directory `key` are heard when the backend was not asked.
  fn set_state(&mut self, key: DirKey, state: Watch) {
    if let Some(dir) = self.tree[key].dir_mut() {
      dir.watch = state;
    }
    self.tree.list_if_polled(key);
  }

  /// Stores what the backend answered for the directory `key`.
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
    // Nothing tells when a link points somewhere else now, or when its target is replaced.
    // A check of the node is enough: what is inside the target has a watch of its own. A
    // directory that the timer looks at checks its links in its scan.
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

  /// A mark can come through a watch that moved away with its directory, or that stands at a
  /// path where another directory is now. If the parent of that directory is only polled,
  /// nothing says so in time, and the marks would be checked against the wrong directory. So
  /// the parent checks such a directory in this flush too, and first: marks are checked from
  /// the top down.
  pub(super) fn check_moved(&mut self) {
    // Only the marks that are there now: the ones that this loop adds are not looked at.
    for index in 0..self.tree.marked.len() {
      let key = self.tree.marked[index];
      if !self.tree.get(key).is_some_and(Node::is_marked) {
        continue;
      }
      // The directory in which the mark is checked.
      let Some(parent) = self.tree.parent(key) else { continue };
      // A directory with a mark is checked anyway. And a plain mark would take a loss report
      // that waits for the end of a rest out of the wait: no loss scan would ever be paced.
      if let Some(moved) = self.below_polled_parent(parent)
        && !self.tree[moved].is_marked()
      {
        self.tree.mark(moved, Mark::default());
      }
    }
  }

  /// The backend cannot go on: one error, and from now on every node is polled.
  pub(super) fn backend_died(&mut self, error: io::Error) {
    if self.backend_failed {
      return;
    }
    self.backend_failed = true;
    self.report(ErrorKind::Io, None, Some(error));
    self.tree.poll_all();
    // Its last notifications may be lost.
    self.tree.scan_all();
  }

  /// The directory at or above `key` that only a poll of its parent checks: the parent has no
  /// OS watch, or the directory is reached through a link. A top directory does not move, so
  /// the way up ends below the root.
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
