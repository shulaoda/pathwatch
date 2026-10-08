//! The questions to the ignore filter (`Options::ignored`). Its rules:
//!
//! - An entry in a watched directory is asked with the kind that it has on disk, when it is
//!   found. A path given to `watch` is asked as a file and as a directory.
//! - A node keeps the answer for each kind: the filter is not asked again for that node.
//! - A path that is ignored for both kinds is not registered: no node, no watch, no look.
//! - The watch target of a registered name with a file or nothing at it is the directory that
//!   holds the name. It is asked, as a directory, before the backend is asked for its contents.
//! - A drop: when the target is ignored, the watched paths at and below the name are forgotten,
//!   with a `Delete` for each file that the handler knows.

use std::{
  iter,
  path::{Path, PathBuf},
};

use super::Engine;
use crate::{
  backend::{Backend, DirKey},
  config::EntryKind,
  fs::Fs,
  tree::{Kind, Tree},
};

impl<B: Backend, F: Fs> Engine<B, F> {
  /// Whether the filter ignores an entry of this kind at `path`, the path of the node `key`.
  pub(super) fn ignores(&mut self, key: DirKey, path: &Path, kind: EntryKind) -> bool {
    if let Some(answer) = self.tree[key].ignored(kind) {
      return answer;
    }
    let answer = self.ignored(path, kind);
    self.tree[key].set_ignored(kind, answer);
    answer
  }

  /// Whether the filter ignores the directory `key` as a watch target: the directory whose
  /// watch serves a registered name in it that has a file or nothing at it. A directory that
  /// only leads to watched paths is never asked.
  pub(super) fn is_ignored_target(&mut self, key: DirKey) -> bool {
    // The root node has no path, and is no target.
    if key == Tree::ROOT || self.options.ignored.is_none() {
      return false;
    }
    if let Some(answer) = self.tree[key].ignored(EntryKind::Dir) {
      return answer;
    }
    let mut path = PathBuf::with_capacity(128);
    self.tree.path_into(key, &mut path);
    self.ignores(key, &path, EntryKind::Dir)
  }

  /// Whether the directory above the node `key` covers it: that directory tracks every entry,
  /// and the node is one of its entries. A registered name that the directory lists under
  /// another spelling is none (`Node::is_unlisted`).
  pub(super) fn is_inside(&self, key: DirKey) -> bool {
    !self.tree[key].is_unlisted()
      && self.tree.parent(key).is_some_and(|parent| self.tracks_every_entry(parent))
  }

  /// Whether a directory at the node `key` has to track every entry: the node is a watched
  /// path, or the node above it covers it (`is_inside`); and the filter does not ignore the
  /// directory.
  ///
  /// The tree can be behind this rule: a directory that becomes a watched path is complete in
  /// the tree only after its next look, and that look can wait for the disk.
  pub(super) fn tracks_every_entry(&self, key: DirKey) -> bool {
    match self.tree[key].kind {
      // What the tree has, also at a held node: the filter was asked when it was stored.
      Kind::Dir(dir) if dir.complete => return true,
      Kind::Dir(_) => {}
      // The tree has no directory here: there is nothing to ask the filter about.
      Kind::File(_) | Kind::Missing => return false,
    }
    // The nearest watched path at or above the node. There is none above most partial nodes,
    // and then the filter is not asked at all.
    let (mut at, mut levels) = (key, 0);
    while !self.tree[at].is_path() {
      // No entry of the directory above: nothing further up covers it.
      if self.tree[at].is_unlisted() {
        return false;
      }
      let Some(parent) = self.tree.parent(at) else { return false };
      (at, levels) = (parent, levels + 1);
    }
    !self.is_ignored_on_the_way(key, levels)
  }

  /// Whether the filter ignores the directory at the node `key`, or one of the `levels`
  /// directories above it. The way is walked from the top down, so nothing is asked below an
  /// ignored directory.
  pub(super) fn is_ignored_on_the_way(&self, key: DirKey, levels: usize) -> bool {
    let parent = |&at: &DirKey| self.tree.parent(at);
    let way: Vec<DirKey> = iter::successors(Some(key), parent).take(levels + 1).collect();
    way.iter().rev().any(|&at| {
      let kept = self.tree[at].ignored(EntryKind::Dir);
      kept.unwrap_or_else(|| self.ignored(&self.tree.path(at), EntryKind::Dir))
    })
  }

  /// Asks the filter. `ignores` asks the same with the answer kept on the node.
  pub(super) fn ignored(&self, path: &Path, kind: EntryKind) -> bool {
    self.options.ignored.as_ref().is_some_and(|ignored| ignored(path, kind))
  }
}
