//! The small facts of a node, as bits in one number so that a node stays small.

use bitflags::bitflags;

use super::{Added, Mark, Node};
use crate::config::EntryKind;

bitflags! {
  #[derive(Clone, Copy, PartialEq, Eq, Debug)]
  pub(super) struct Flags: u16 {
    const HELD = 1 << 0;
    const TOLD = 1 << 1;
    const CHANGED = 1 << 2;
    const BASELINE = 1 << 3;
    const IS_PATH = 1 << 4;

    // Whether the filter was asked about a file at the path, and what it said.
    const FILE_ASKED = 1 << 5;
    const FILE_IGNORED = 1 << 6;
    const DIR_ASKED = 1 << 7;
    const DIR_IGNORED = 1 << 8;

    const MARKED = 1 << 9;
    const MARK_DEEP = 1 << 10;
    const MARK_WRITTEN = 1 << 11;
    const MARK_LOST = 1 << 12;

    // `Added::Path` and `Added::Registered`. Neither: `Added::No`.
    const ADDED_PATH = 1 << 13;
    const ADDED_REGISTERED = 1 << 14;

    const UNLISTED = 1 << 15;
  }
}

impl Flags {
  /// Whether the filter was asked about `kind`, and what it said.
  fn of_filter(kind: EntryKind) -> (Self, Self) {
    match kind {
      EntryKind::File => (Self::FILE_ASKED, Self::FILE_IGNORED),
      EntryKind::Dir => (Self::DIR_ASKED, Self::DIR_IGNORED),
    }
  }
}

impl Node {
  /// The last look found nothing where `kind` has a file or a directory. The second look
  /// decides; until then the node and everything below it stay as they are.
  pub fn is_held(&self) -> bool {
    self.flags.contains(Flags::HELD)
  }

  pub(super) fn set_held(&mut self, held: bool) {
    self.flags.set(Flags::HELD, held);
  }

  /// The handler believes that a file is at the path.
  pub fn told(&self) -> bool {
    self.flags.contains(Flags::TOLD)
  }

  pub fn set_told(&mut self, told: bool) {
    self.flags.set(Flags::TOLD, told);
  }

  /// The file is another one than the handler was told about: an `Update` is due.
  pub fn changed(&self) -> bool {
    self.flags.contains(Flags::CHANGED)
  }

  pub fn set_changed(&mut self, changed: bool) {
    self.flags.set(Flags::CHANGED, changed);
  }

  // Both are set only while a `watch` call runs.

  /// What the call did with the node.
  pub fn added(&self) -> Added {
    if self.flags.contains(Flags::ADDED_REGISTERED) {
      Added::Registered
    } else if self.flags.contains(Flags::ADDED_PATH) {
      Added::Path
    } else {
      Added::No
    }
  }

  pub fn set_added(&mut self, added: Added) {
    self.flags.set(Flags::ADDED_PATH, added == Added::Path);
    self.flags.set(Flags::ADDED_REGISTERED, added == Added::Registered);
  }

  /// No event tells about what the call found here: the file at the node, or, for a directory,
  /// every entry that is found in it for the first time.
  pub fn baseline(&self) -> bool {
    self.flags.contains(Flags::BASELINE)
  }

  pub fn set_baseline(&mut self, baseline: bool) {
    self.flags.set(Flags::BASELINE, baseline);
  }

  pub fn is_path(&self) -> bool {
    self.flags.contains(Flags::IS_PATH)
  }

  /// Only [`super::Tree::set_path`] calls it: it keeps the counts of the nodes above right.
  pub(super) fn set_path(&mut self, is_path: bool) {
    self.flags.set(Flags::IS_PATH, is_path);
  }

  /// The directory above lists the entry of this registered name under another spelling: the
  /// node is the path its caller gave, and no entry of that directory.
  pub fn is_unlisted(&self) -> bool {
    self.flags.contains(Flags::UNLISTED)
  }

  pub fn set_unlisted(&mut self, unlisted: bool) {
    self.flags.set(Flags::UNLISTED, unlisted);
  }

  // The filter gives the same answer for the same path and kind, so the node keeps it.

  /// What the filter said about a file, or a directory, at the path. `None`: it was not asked.
  pub fn ignored(&self, kind: EntryKind) -> Option<bool> {
    let (asked, answer) = Flags::of_filter(kind);
    self.flags.contains(asked).then(|| self.flags.contains(answer))
  }

  pub fn set_ignored(&mut self, kind: EntryKind, ignored: bool) {
    let (asked, answer) = Flags::of_filter(kind);
    self.flags.insert(asked);
    self.flags.set(answer, ignored);
  }

  pub fn is_marked(&self) -> bool {
    self.flags.contains(Flags::MARKED)
  }

  pub fn mark(&self) -> Option<Mark> {
    self.is_marked().then(|| Mark {
      deep: self.flags.contains(Flags::MARK_DEEP),
      written: self.flags.contains(Flags::MARK_WRITTEN),
      lost: self.flags.contains(Flags::MARK_LOST),
      since: None,
    })
  }

  pub(super) fn set_mark(&mut self, mark: Option<Mark>) {
    self.flags.set(Flags::MARKED, mark.is_some());
    self.flags.set(Flags::MARK_DEEP, mark.is_some_and(|mark| mark.deep));
    self.flags.set(Flags::MARK_WRITTEN, mark.is_some_and(|mark| mark.written));
    self.flags.set(Flags::MARK_LOST, mark.is_some_and(|mark| mark.lost));
  }
}
