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
  tree::{Added, Kind, Tree, push_name},
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
  pub(super) fn is_covered(&self, key: DirKey, before_call: bool) -> bool {
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

  /// Asks the filter, without keeping the answer.
  pub(super) fn ignored(&self, path: &Path, kind: EntryKind) -> bool {
    self.options.ignored.as_ref().is_some_and(|ignored| ignored(path, kind))
  }
}
