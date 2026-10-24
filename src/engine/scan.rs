//! One directory: the scan of its entries, and what is done when a directory appears, is
//! still there, or vanishes.

use std::{mem, path::PathBuf};

use super::{
  Engine,
  check::{Found, Listed},
};
use crate::{
  backend::{Backend, DirKey},
  fs::{DirId, Entry, FileType, Fs, is_missing},
  tree::{Depth, Dir, Kind, Mark, Node, Watch, is_plain},
};

/// One node to check in a scan, with what the listing said about it.
pub(super) type ScanEntry = (DirKey, Listed);

#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<ScanEntry>() == 40);

/// What a scan got from the listing of its directory.
#[derive(PartialEq, Eq)]
enum Listing {
  /// Every entry of the directory was named.
  Whole,
  /// There is none: the node is partial, or the directory cannot be read.
  None,
  /// The scan ends here, because the directory is gone or the disk did not answer.
  Stop,
}

/// The scan of a directory: a check of every node, and a listing that adds what the tree does
/// not know.
impl<B: Backend, F: Fs> Engine<B, F> {
  /// Checks every node of the directory, and what a listing shows on top if the node is complete.
  /// `path` and `entries` are buffers.
  pub(super) fn scan(
    &mut self,
    key: DirKey,
    depth: Depth,
    path: &mut PathBuf,
    entries: &mut Vec<ScanEntry>,
  ) {
    // A node above is held: nothing below it is looked at before its second look.
    if self.tree.frozen(key) {
      return;
    }
    self.tree.path_into(key, path);
    self.scan_had_error = false;
    entries.clear();
    entries.extend(self.tree.children(key).map(|child| (child, Listed::NotAsked)));
    self.listed.clear();
    let listing =
      if self.tree[key].is_complete() { self.list_new(key, path, entries) } else { Listing::None };
    if listing == Listing::Stop {
      return;
    }
    // The listing of a loose directory says how the disk spells each known node, or that it is
    // gone.
    let names_are_known = listing == Listing::Whole && self.tree.loose_spelling;
    let names_only = depth == Depth::Names && names_are_known;

    let why = if depth == Depth::Deep { Mark::DEEP } else { Mark::default() };
    for &(child, listed) in entries.iter() {
      if self.stopped() {
        return;
      }
      let listed = match listed {
        Listed::NotAsked if names_are_known => {
          self.listed.get(&child).copied().unwrap_or(Listed::NotNamed)
        }
        said => said,
      };
      // The tree has the node as an entry (known before, or looked at in this scan), and the
      // listing names it: it is as it was. Not a name that is not plain in a loose directory:
      // the hint that asked for this scan can have meant it, under another spelling that the
      // table does not find.
      if names_only
        && listed.is_named()
        && self.tree.get(child).is_some_and(Node::is_entry)
        && is_plain(self.tree.name(child))
      {
        continue;
      }
      // A check can free a later node, a twin or the name it follows: its key names none then.
      self.check(child, path, listed, why);
    }
    // This runs after the commit, so a watch that is said to have failed did fail.
    if let Some(dir) = self.tree[key].dir_mut() {
      // A scan of the names did not look at every entry: it cannot say that an error is over.
      let still = names_only && dir.has_error;
      dir.has_error = still || self.scan_had_error || dir.watch == Watch::Failed;
    }
  }

  /// Lists the directory and adds a node for each entry the tree does not know. With loose
  /// spelling, `listed` gets what the listing said of the known nodes.
  fn list_new(&mut self, key: DirKey, path: &mut PathBuf, entries: &mut Vec<ScanEntry>) -> Listing {
    let known = entries.len();
    // A listing that lags behind a write is trusted for a file only where an OS watch reports
    // the write.
    let lags = self.fs.listing_lags() && !self.tree[key].watch().is_heard();
    let flush_time = self.flush_time;
    let tree = &mut self.tree;
    let mut listed = tree.loose_spelling.then_some(&mut self.listed);
    let result = self.fs.read_dir(path, &mut |name, entry| {
      let entry = match entry {
        Entry::File { .. } if lags => Entry::Kind(FileType::File),
        entry => entry,
      };
      match tree.child(key, name) {
        Some(node) => {
          if let Some(listed) = &mut listed {
            listed.insert(node, Listed::of(entry, flush_time));
          }
        }
        None => entries.push((tree.insert_child(key, name), Listed::of(entry, flush_time))),
      }
    });
    // A listing follows a link at the path: one that took the place of the directory since the
    // look would be read as the directory. So it counts only while the same directory is at the
    // path when it is done (one look, 1 µs); else it is read again at the next flush. A
    // registered link is followed as the look followed it.
    let error = match result {
      Ok(()) => {
        let is_link = self.tree.link(key).is_some();
        let still = if is_link { self.fs.stat(path) } else { self.fs.lstat(path) };
        match still {
          Ok(meta)
            if meta.kind == FileType::Dir
              && self.tree[key].dir().is_some_and(|dir| dir.id == meta.dir_id) =>
          {
            return Listing::Whole;
          }
          Ok(_) => None,
          Err(error) => Some(error),
        }
      }
      Err(error) => Some(error),
    };

    // What the listing gave is forgotten.
    for (new, _) in entries.drain(known..) {
      self.tree.prune(new);
    }
    let read_again = match error {
      // Something else took the place of the directory.
      None => true,
      // Gone: the parent looks. The directory can be there after all (away and back while it
      // was read): then the deep mark keeps the work for the next flush, as when the disk gives
      // no answer.
      Some(error) if is_missing(&error) => {
        let parent = self.tree.parent(key).expect("a complete node has a parent");
        self.tree.path_into(parent, path);
        self.check(key, path, Listed::NotAsked, Mark::default());
        // The same directory is still there, not held (`Node::dir`), and no scan of it is queued.
        self.tree.get(key).and_then(Node::dir).is_some_and(|dir| dir.scan.is_none())
      }
      // No answer: the parent checks it at the next flush, and the deep mark keeps the work,
      // with what this scan knew of lost hints (5.8). Only the root has no parent, and it is
      // never complete.
      Some(error) => match self.disk_error(key, path, error) {
        Found::Unknown => true,
        // Permission denied: nothing in here can be read, so every node is checked as missing.
        _ => return Listing::None,
      },
    };
    if read_again {
      self.tree.mark(key, self.retry(Mark::default()));
    }
    Listing::Stop
  }
}

/// What a directory that appears, is still there, or vanishes, does to the tree.
impl<B: Backend, F: Fs> Engine<B, F> {
  /// A directory is on disk where the tree had none. `relinked`: as for `is_baseline_dir`.
  pub(super) fn appear(
    &mut self,
    key: DirKey,
    path: &mut PathBuf,
    id: DirId,
    complete: bool,
    relinked: bool,
  ) {
    self.tree[key].kind = Kind::Dir(Dir::new(id, false));
    if complete {
      self.make_complete(key, relinked);
    }
    self.arm(key, path, false);
    // Also when a watch above covers it and none was asked for.
    self.tree.queue_scan(key, Depth::Shallow);
  }

  /// A directory is still on disk at the node. `away`: the writes in it can have gone unheard
  /// (`Dir::away`).
  pub(super) fn still_there(&mut self, key: DirKey, path: &mut PathBuf, deep: bool, away: bool) {
    // Also when not deep: the node may want more, or the watch above may be gone.
    self.arm(key, path, deep);
    let Some(dir) = self.tree[key].dir_mut() else { return };
    dir.away |= away;
    // A deep scan lists the directory again too (`Dir::relist`).
    let relist = mem::take(&mut dir.relist);
    if deep {
      self.tree.queue_scan(key, Depth::Deep);
    } else if relist {
      self.tree.queue_scan(key, Depth::Shallow);
    }
  }

  /// The directory is gone for good, with everything below it: the end of the flush tells the
  /// handler.
  pub(super) fn vanish(&mut self, key: DirKey) {
    // A list, not a recursion: a tree can be deeper than the stack takes. Nothing is asked for
    // when the node is no directory, or an empty one.
    let first = self.touched.len();
    let mut next = Some(key);
    let mut stack = Vec::new();
    while let Some(key) = next.take().or_else(|| stack.pop()) {
      let node = &mut self.tree[key];
      let Kind::Dir(dir) = node.kind else { continue };
      node.kind = Kind::Missing;
      self.tree.unhold(key);
      if dir.asked.is_some() {
        self.unwatch_dir(key);
      }
      self.touched.push(key);

      let mut next_child = self.tree.children(key).next();
      while let Some(child) = next_child {
        next_child = self.tree.next_sibling(child);
        // Nothing can be checked in a directory that is gone: a mark in it is dropped.
        self.tree.take_mark(child);
        match self.tree[child].kind {
          Kind::File(_) => self.forget_file(child),
          Kind::Dir(_) => stack.push(child),
          Kind::Missing => self.tree.prune(child),
        }
      }
    }
    // Each node came after the ones above it: the other way round, `finish` can free them from
    // the bottom up.
    self.touched[first..].reverse();
  }
}
