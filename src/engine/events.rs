//! The events of one flush, and the delete delay. A check makes no event: `finish` reads each
//! touched node once, so a path gets one event for a flush.

use std::{cmp::Reverse, time::Instant};

use super::{Engine, Output};
use crate::{
  backend::{Backend, DirKey},
  event::{Event, EventKind},
  fs::Fs,
  tree::{Added, Dir, Kind, Mark, Node, Watch, emptied, room_to_keep},
};

/// The end of a flush: one event for each node it changed. And what the tree forgets of a node
/// whose file or directory is gone.
impl<B: Backend, F: Fs> Engine<B, F> {
  /// Hands the events of the touched nodes over as one batch, and ends the baseline of a `watch`.
  pub(super) fn finish(&mut self, out: &mut Output) {
    let mut touched = std::mem::take(&mut self.touched);
    for key in touched.drain(..) {
      // No node: it was freed, or it is on the list twice.
      let Some(node) = self.tree.get_mut(key) else { continue };
      let kind = event_of(node);
      node.set_told(matches!(node.kind, Kind::File(_)));
      node.set_changed(false);
      node.set_added(Added::No);
      node.set_baseline(false);
      if let Some(kind) = kind {
        // Built in the buffer: the path of the event is one allocation of its size.
        self.tree.path_into(key, &mut self.path);
        out.events.push(Event { path: self.path.clone(), kind });
      }
      self.tree.prune(key);
    }
    self.touched = emptied(touched);
    out.errors.append(&mut self.errors);
    // A delivery that came while hints had left work is taken up when it is done.
    self.forget_delivered();
  }

  /// What the tree has at the node is gone for good.
  pub(super) fn gone(&mut self, key: DirKey) {
    match self.tree[key].kind {
      Kind::File(_) => self.forget_file(key),
      Kind::Dir(_) => self.vanish(key),
      Kind::Missing => {}
    }
  }

  /// Forgets the file at the node; the end of the flush tells the handler.
  pub(super) fn forget_file(&mut self, key: DirKey) {
    self.tree[key].kind = Kind::Missing;
    self.tree.set_content(key, None);
    self.tree.set_ahead_since(key, None);
    self.tree.unhold(key);
    self.touched.push(key);
  }
}

/// The delete delay (design.md 5.9): the first look that finds nothing holds the node, and the
/// second look decides.
impl<B: Backend, F: Fs> Engine<B, F> {
  /// Nothing is at the node, and the tree has something: the first look holds it, the second look
  /// decides (design.md 5.9). The delay counts from the hint that asked for the look, `since`.
  pub(super) fn missing(&mut self, key: DirKey, since: Option<Instant>) {
    let now = self.now.expect("a check runs inside a flush");
    // The time of this look, not of the start of the flush: a flush can take long before it
    // reaches the node.
    let looked = now + self.fs.ticks().saturating_sub(self.flush_ticks);
    let delay = self.options.delete_delay;
    let node = &self.tree[key];
    // The handler does not know the file, so there is no `Delete` to hold back.
    let unknown_file = matches!(node.kind, Kind::File(_)) && !node.told();
    match self.tree.held_until(key) {
      Some(until) if until > looked => {}
      None if !delay.is_zero() && !unknown_file => {
        // A first look that came late gets its second look at once, in the next flush.
        self.hold(key, (since.unwrap_or(looked) + delay).max(looked));
      }
      _ => self.gone(key),
    }
  }

  /// Holds the node until `until`. A directory gives its watch back meanwhile.
  fn hold(&mut self, key: DirKey, until: Instant) {
    if let Kind::Dir(dir) = &mut self.tree[key].kind {
      let asked = dir.asked.is_some();
      // Polled from now on: a hold can end without a look when a node above is held too.
      *dir = Dir { watch: Watch::Polled, ..Dir::new(dir.id, dir.complete) };
      if asked {
        self.unwatch_dir(key);
      }
    }
    self.tree.list_if_polled(key);
    self.tree.hold(key, until);
    self.hold_queue.push(Reverse((until, key)));
    // A node that is back, or held again, leaves its entry until that time comes. With a long
    // delete delay the stale entries are dropped once they are most of the queue.
    if self.hold_queue.len() > 2 * self.tree.hold_count() + 64 {
      let tree = &self.tree;
      self.hold_queue.retain(|&Reverse((until, key))| tree.held_until(key) == Some(until));
    }
  }

  /// Marks each held node whose time is over. `true`: a flush has to run.
  pub(super) fn mark_due_holds(&mut self, now: Instant) -> bool {
    while let Some(&Reverse((until, key))) = self.hold_queue.peek()
      && until <= now
    {
      self.hold_queue.pop();
      // Another time, or none: the hold ended, or the node was held again later.
      if self.tree.held_until(key) == Some(until) {
        self.tree.mark(key, Mark::default());
        self.due_holds.push(key);
      }
    }
    // Many holds at once (a tree removed file by file) must not keep their room for ever.
    if let Some(room) = room_to_keep(self.hold_queue.len(), self.hold_queue.capacity()) {
      self.hold_queue.shrink_to(room);
    }
    !self.due_holds.is_empty()
  }
}

/// The event due for a node: what the handler believes against what is there now.
fn event_of(node: &Node) -> Option<EventKind> {
  // A held file is still a file here: its `Delete` waits.
  let is_file = matches!(node.kind, Kind::File(_));
  match (node.told(), is_file) {
    (true, false) => Some(EventKind::Delete),
    (false, true) if !node.baseline() => Some(EventKind::Create),
    (true, true) if node.changed() => Some(EventKind::Update),
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use std::ffi::OsStr;

  use super::event_of;
  use crate::{
    event::EventKind::{Create, Delete, Update},
    fs::DirId,
    tree::{Dir, Fingerprint, Kind, Tree},
  };

  /// What the changes of one path inside one flush come to.
  #[test]
  fn the_event_of_a_node() {
    let file = Kind::File(Fingerprint { modified: 0, size: 1, id: 1 });
    let dir = Kind::Dir(Dir::new(DirId::default(), true));
    // What the handler believed, what is there now, `changed`, `baseline`.
    let table = [
      (false, file, false, false, Some(Create), "a file came"),
      (false, file, true, false, Some(Create), "it came and was written: it is still new"),
      (false, Kind::Missing, false, false, None, "it came and went"),
      (false, Kind::Missing, true, false, None, "it came, was written and went"),
      (false, file, false, true, None, "a file of the baseline"),
      (false, file, true, true, None, "a file of the baseline was written"),
      (false, Kind::Missing, false, true, None, "a file of the baseline went away again"),
      (false, dir, false, true, None, "a directory of the baseline"),
      (true, Kind::Missing, false, false, Some(Delete), "a known file went"),
      (true, Kind::Missing, true, false, Some(Delete), "it was written and went"),
      (true, dir, false, false, Some(Delete), "a directory took its place"),
      (true, file, true, false, Some(Update), "it was written, or it went and came"),
      (true, file, false, false, None, "it is the same file"),
      (true, file, true, true, Some(Update), "the baseline hides no change of a known file"),
      (true, Kind::Missing, false, true, Some(Delete), "and no `Delete` of one"),
    ];
    let mut tree = Tree::new();
    let key = tree.insert_child(Tree::ROOT, OsStr::new("/"));
    for (told, kind, changed, baseline, expected, what) in table {
      let node = &mut tree[key];
      node.kind = kind;
      node.set_told(told);
      node.set_changed(changed);
      node.set_baseline(baseline);
      assert_eq!(event_of(node), expected, "{what}");
    }
  }
}
