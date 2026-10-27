use std::{mem, path::PathBuf};

use super::{
  Engine,
  check::{Found, Listed},
};
use crate::{
  backend::{Backend, NodeKey},
  fs::{DirId, Entry, FileType, Fs, is_missing},
  tree::{Depth, Dir, Kind, Mark, Node, Watch, is_plain},
};

pub(super) type ScanEntry = (NodeKey, Listed);

#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<ScanEntry>() == 40);

#[derive(PartialEq, Eq)]
enum Listing {
  Whole,
  None,
  Stop,
}

impl<B: Backend, F: Fs> Engine<B, F> {
  pub(super) fn scan(
    &mut self,
    key: NodeKey,
    path: &mut PathBuf,
    depth: Depth,
    entries: &mut Vec<ScanEntry>,
  ) {
    if self.tree.frozen(key) {
      return;
    }
    self.tree.path_into(key, path);
    self.scan_had_error = false;
    entries.clear();
    entries.extend(self.tree.children(key).map(|child| (child, Listed::NotAsked)));
    self.in_listing.clear();
    let listing =
      if self.tree[key].is_complete() { self.list_new(key, path, entries) } else { Listing::None };
    if listing == Listing::Stop {
      return;
    }
    let names_are_known = listing == Listing::Whole && self.tree.loose_spelling;
    let names_only = depth == Depth::Names && names_are_known;

    let why = if depth == Depth::Deep { Mark::DEEP } else { Mark::default() };
    for &(child, listed) in entries.iter() {
      if self.stopped() {
        return;
      }
      let listed = match listed {
        Listed::NotAsked if names_are_known => {
          self.in_listing.get(&child).copied().unwrap_or(Listed::NotNamed)
        }
        said => said,
      };
      if names_only
        && listed.is_named()
        && self.tree.get(child).is_some_and(Node::is_entry)
        && is_plain(self.tree.name(child))
      {
        continue;
      }
      self.check(child, path, listed, why);
    }
    if let Some(dir) = self.tree[key].dir_mut() {
      let still = names_only && dir.has_error;
      dir.has_error = still || self.scan_had_error || dir.watch == Watch::Failed;
    }
  }

  fn list_new(
    &mut self,
    key: NodeKey,
    path: &mut PathBuf,
    entries: &mut Vec<ScanEntry>,
  ) -> Listing {
    let known = entries.len();
    let lags = self.fs.listing_lags() && !self.tree[key].watch().is_reported();
    let flush_time = self.flush_time;
    let tree = &mut self.tree;
    let mut in_listing = tree.loose_spelling.then_some(&mut self.in_listing);
    let listed = self.fs.read_dir(path, &mut |name, entry| {
      let entry = match entry {
        Entry::File { .. } if lags => Entry::Kind(FileType::File),
        entry => entry,
      };
      match tree.child(key, name) {
        Some(node) => {
          if let Some(in_listing) = &mut in_listing {
            in_listing.insert(node, Listed::of(entry, flush_time));
          }
        }
        None => entries.push((tree.insert_child(key, name), Listed::of(entry, flush_time))),
      }
    });
    let is_link = self.tree.link(key).is_some();
    let still =
      listed.and_then(|()| if is_link { self.fs.stat(path) } else { self.fs.lstat(path) });
    let error = match still {
      Ok(meta)
        if meta.kind == FileType::Dir
          && self.tree[key].dir().is_some_and(|dir| dir.id == meta.dir_id) =>
      {
        return Listing::Whole;
      }
      Ok(_) => None,
      Err(error) => Some(error),
    };

    for (new, _) in entries.drain(known..) {
      self.tree.prune(new);
    }
    let read_again = match error {
      None => true,
      Some(error) if is_missing(&error) => {
        let parent = self.tree.parent(key).expect("a complete node has a parent");
        self.tree.path_into(parent, path);
        self.check(key, path, Listed::NotAsked, Mark::default());
        self.tree.get(key).and_then(Node::dir).is_some_and(|dir| dir.scan.is_none())
      }
      Some(error) => match self.disk_error(key, path, error) {
        Found::NoAnswer => true,
        _ => return Listing::None,
      },
    };
    if read_again {
      self.tree.mark(key, self.retry(Mark::default()));
    }
    Listing::Stop
  }

  pub(super) fn appear(
    &mut self,
    key: NodeKey,
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
    self.tree.queue_scan(key, Depth::Shallow);
  }

  pub(super) fn still_there(&mut self, key: NodeKey, path: &mut PathBuf, deep: bool, away: bool) {
    self.arm(key, path, deep);
    let Some(dir) = self.tree[key].dir_mut() else { return };
    dir.away |= away;
    let relist = mem::take(&mut dir.relist);
    if deep {
      self.tree.queue_scan(key, Depth::Deep);
    } else if relist {
      self.tree.queue_scan(key, Depth::Shallow);
    }
  }

  pub(super) fn vanish(&mut self, key: NodeKey) {
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
        self.tree.take_mark(child);
        self.drop_entry(child, true);
        match self.tree[child].kind {
          Kind::File(_) => self.forget_file(child),
          Kind::Dir(_) => stack.push(child),
          Kind::Missing => self.tree.prune(child),
        }
      }
    }
    self.touched[first..].reverse();
  }
}
