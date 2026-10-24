use bitflags::bitflags;

use super::{Added, Mark, Node};
use crate::config::EntryKind;

bitflags! {
  #[derive(Debug)]
  pub(super) struct Flags: u16 {
    const HELD = 1 << 0;
    const TOLD = 1 << 1;
    const CHANGED = 1 << 2;
    const BASELINE = 1 << 3;
    const IS_PATH = 1 << 4;

    const FILE_ASKED = 1 << 5;
    const FILE_IGNORED = 1 << 6;
    const DIR_ASKED = 1 << 7;
    const DIR_IGNORED = 1 << 8;

    const MARKED = 1 << 9;
    const MARK_DEEP = 1 << 10;
    const MARK_WRITTEN = 1 << 11;
    const MARK_LOST = 1 << 12;

    const ADDED_PATH = 1 << 13;
    const ADDED_REGISTERED = 1 << 14;

    const UNLISTED = 1 << 15;
  }
}

impl Flags {
  fn of_filter(kind: EntryKind) -> (Self, Self) {
    match kind {
      EntryKind::File => (Self::FILE_ASKED, Self::FILE_IGNORED),
      EntryKind::Dir => (Self::DIR_ASKED, Self::DIR_IGNORED),
    }
  }
}

impl Node {
  pub fn is_held(&self) -> bool {
    self.flags.contains(Flags::HELD)
  }

  pub(super) fn set_held(&mut self, held: bool) {
    self.flags.set(Flags::HELD, held);
  }

  pub fn told(&self) -> bool {
    self.flags.contains(Flags::TOLD)
  }

  pub fn set_told(&mut self, told: bool) {
    self.flags.set(Flags::TOLD, told);
  }

  pub fn changed(&self) -> bool {
    self.flags.contains(Flags::CHANGED)
  }

  pub fn set_changed(&mut self, changed: bool) {
    self.flags.set(Flags::CHANGED, changed);
  }

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

  pub fn baseline(&self) -> bool {
    self.flags.contains(Flags::BASELINE)
  }

  pub fn set_baseline(&mut self, baseline: bool) {
    self.flags.set(Flags::BASELINE, baseline);
  }

  pub fn is_path(&self) -> bool {
    self.flags.contains(Flags::IS_PATH)
  }

  pub(super) fn set_path(&mut self, is_path: bool) {
    self.flags.set(Flags::IS_PATH, is_path);
  }

  pub fn is_unlisted(&self) -> bool {
    self.flags.contains(Flags::UNLISTED)
  }

  pub fn set_unlisted(&mut self, unlisted: bool) {
    self.flags.set(Flags::UNLISTED, unlisted);
  }

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
