use std::{cmp::Reverse, time::Instant};

use super::{Engine, Output};
use crate::{
  backend::{Backend, NodeKey},
  event::{Event, EventKind},
  fs::Fs,
  tree::{Added, Dir, Kind, Mark, Node, Watch, emptied, room_to_keep},
};

impl<B: Backend, F: Fs> Engine<B, F> {
  pub(super) fn finish(&mut self, out: &mut Output) {
    let mut touched = std::mem::take(&mut self.touched);
    for key in touched.drain(..) {
      let Some(node) = self.tree.get_mut(key) else { continue };
      let kind = event_of(node);
      node.set_told(matches!(node.kind, Kind::File(_)));
      node.set_changed(false);
      node.set_added(Added::No);
      node.set_baseline(false);
      if let Some(kind) = kind {
        self.tree.path_into(key, &mut self.path);
        out.events.push(Event { path: self.path.clone(), kind });
      }
      self.tree.prune(key);
    }
    out.errors.append(&mut self.errors);
    self.touched = emptied(touched);
    self.forget_delivered();
  }

  pub(super) fn gone(&mut self, key: NodeKey) {
    match self.tree[key].kind {
      Kind::File(_) => self.forget_file(key),
      Kind::Dir(_) => self.vanish(key),
      Kind::Missing => {}
    }
  }

  pub(super) fn forget_file(&mut self, key: NodeKey) {
    self.tree[key].kind = Kind::Missing;
    self.tree.set_content(key, None);
    self.tree.set_ahead_since(key, None);
    self.tree.unhold(key);
    self.touched.push(key);
  }

  pub(super) fn missing(&mut self, key: NodeKey, since: Option<Instant>) {
    let now = self.now.expect("a check runs inside a flush");
    let looked = now + self.fs.ticks().saturating_sub(self.flush_ticks);
    let delay = self.options.delete_delay;
    let node = &self.tree[key];
    let unknown_file = matches!(node.kind, Kind::File(_)) && !node.told();
    match self.tree.held_until(key) {
      Some(until) if until > looked => return,
      None if !delay.is_zero() && !unknown_file => {}
      _ => return self.gone(key),
    }
    let until = (since.unwrap_or(looked) + delay).max(looked);
    if let Kind::Dir(dir) = &mut self.tree[key].kind {
      let asked = dir.asked.is_some();
      *dir = Dir { watch: Watch::Polled, ..Dir::new(dir.id, dir.complete) };
      if asked {
        self.unwatch_dir(key);
      }
    }
    self.tree.list_if_polled(key);
    self.tree.hold(key, until);
    self.hold_queue.push(Reverse((until, key)));
    if self.hold_queue.len() > 2 * self.tree.hold_count() + 64 {
      let tree = &self.tree;
      self.hold_queue.retain(|&Reverse((until, key))| tree.held_until(key) == Some(until));
    }
  }

  pub(super) fn mark_due_holds(&mut self, now: Instant) -> bool {
    while let Some(&Reverse((until, key))) = self.hold_queue.peek()
      && until <= now
    {
      self.hold_queue.pop();
      if self.tree.held_until(key) == Some(until) {
        self.tree.mark(key, Mark::default());
        self.due_holds.push(key);
      }
    }
    if let Some(room) = room_to_keep(self.hold_queue.len(), self.hold_queue.capacity()) {
      self.hold_queue.shrink_to(room);
    }
    !self.due_holds.is_empty()
  }
}

fn event_of(node: &Node) -> Option<EventKind> {
  match (node.told(), matches!(node.kind, Kind::File(_))) {
    (true, false) => Some(EventKind::Delete),
    (true, true) if node.changed() => Some(EventKind::Update),
    (false, true) if !node.baseline() => Some(EventKind::Create),
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

  #[test]
  fn the_event_of_a_node() {
    let file = Kind::File(Fingerprint { modified: 0, size: 1, id: 1 });
    let dir = Kind::Dir(Dir::new(DirId::default(), true));
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
