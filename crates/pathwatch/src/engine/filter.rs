//! The questions to the ignore filter (`Options::ignored`): what is asked, when, and what a node
//! keeps of the answer (design.md 5.11).

use std::{
  iter,
  path::{Path, PathBuf},
};

use super::Engine;
use crate::{
  backend::{Backend, DirKey},
  config::EntryKind,
  fs::Fs,
  path::split_top,
  tree::{Kind, Tree, push_name},
};

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
  pub(super) fn is_ignored_above(&mut self, path: &Path) -> bool {
    if self.options.ignored.is_none() {
      return false;
    }
    let Some((top, rest)) = split_top(path) else { return false };
    let above = iter::once(top).chain(rest.iter()).take(rest.iter().count());
    let mut dir = PathBuf::with_capacity(path.as_os_str().len());
    let mut at = Some(Tree::ROOT);
    for name in above {
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

  /// Whether the directory above covers the node: it tracks every entry, and the node is one.
  pub(super) fn is_inside(&self, key: DirKey) -> bool {
    !self.tree[key].is_unlisted()
      && self.tree.parent(key).is_some_and(|parent| self.tracks_every_entry(parent))
  }

  /// The rule for a complete node: a watched path, or inside one and not ignored. The tree can be
  /// behind it until the next look (design.md 5.1).
  pub(super) fn tracks_every_entry(&self, key: DirKey) -> bool {
    match self.tree[key].kind {
      Kind::Dir(dir) if dir.complete => return true,
      Kind::Dir(_) => {}
      Kind::File(_) | Kind::Missing => return false,
    }
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

  /// Whether the filter ignores the directory or one of the `levels - 1` above it, below the
  /// watched path `levels` above. That one is not asked: a watched directory that the filter
  /// ignores counts as missing, and has nothing below it. Asked from the top down, so nothing
  /// is asked below an ignored one.
  pub(super) fn is_ignored_on_the_way(&self, key: DirKey, levels: usize) -> bool {
    let parent = |&at: &DirKey| self.tree.parent(at);
    let way: Vec<DirKey> = iter::successors(Some(key), parent).take(levels).collect();
    way.iter().rev().any(|&at| {
      let kept = self.tree[at].ignored(EntryKind::Dir);
      kept.unwrap_or_else(|| self.ignored(&self.tree.path(at), EntryKind::Dir))
    })
  }

  /// Asks the filter, without keeping the answer.
  pub(super) fn ignored(&self, path: &Path, kind: EntryKind) -> bool {
    self.options.ignored.as_ref().is_some_and(|ignored| ignored(path, kind))
  }
}
