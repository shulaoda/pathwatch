use std::{
  ffi::OsStr,
  iter::Peekable,
  mem,
  path::{Component, Components, Path},
  time::Instant,
};

use super::{
  Content, Depth, Kind, NONE, Node, Tree, Watch, emptied, is_plain, name_of, room_to_keep,
};
use crate::{
  backend::{Hint, NodeKey},
  path::split_top,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Said {
  Changed,
  Written,
  New,
}

pub type Marked = (usize, usize, NodeKey, NodeKey, Option<Instant>);

#[derive(Clone, Copy, Default, Debug)]
pub struct Mark {
  pub deep: bool,
  pub written: bool,
  pub lost: bool,
  pub since: Option<Instant>,
}

impl Mark {
  pub const DEEP: Self = Self { deep: true, written: false, lost: false, since: None };
  pub const LOST: Self = Self { deep: true, written: false, lost: true, since: None };
}

impl Node {
  fn add_mark(&mut self, mark: Mark) -> bool {
    if mark.lost
      && let Some(dir) = self.dir_mut()
    {
      dir.away = true;
    }
    let old = self.mark();
    self.set_mark(Some(old.map_or(mark, |old| Mark {
      deep: old.deep || mark.deep,
      written: old.written || mark.written,
      lost: old.lost && mark.lost,
      since: None,
    })));
    old.is_none()
  }
}

impl Tree {
  pub fn hint(&mut self, hint: Hint<'_>) -> bool {
    match hint {
      Hint::Changed { dir, rel } => self.mark_below(dir, rel, Said::Changed),
      Hint::Written { dir, rel, new: true } => self.mark_below(dir, rel, Said::New),
      Hint::Written { dir, rel, new: false } => self.mark_below(dir, rel, Said::Written),
      Hint::Mount { path } => {
        let Some((top, rest)) = split_top(path) else { return false };
        self.child(Self::ROOT, top).is_some_and(|top| self.mark_below(top, rest, Said::Changed))
      }
      Hint::Lost => {
        self.lost = true;
        true
      }
      Hint::Delivered { .. } => false,
    }
  }

  fn mark_below(&mut self, dir: NodeKey, rel: &Path, said: Said) -> bool {
    let Some(node) = self.get(dir) else { return false };
    if !node.is_dir() {
      let mut child = dir;
      while let Some(parent) = self.parent(child) {
        if self[parent].is_dir() {
          self.mark(child, Mark::DEEP);
          return true;
        }
        child = parent;
      }
      return false;
    }
    if rel.as_os_str().is_empty() {
      let has_parent = node.parent != NONE;
      if has_parent {
        self.mark(dir, Mark::LOST);
      }
      return has_parent;
    }
    self.mark_path(dir.index(), rel.components().peekable(), said)
  }

  fn mark_path(&mut self, mut at: u32, mut parts: Peekable<Components<'_>>, said: Said) -> bool {
    let mut found = false;
    while let Some(part) = parts.next() {
      let Component::Normal(name) = part else { return found };
      let (child, has_other) = self.spellings(at, name);
      let below = child.map(|child| self.slots.linked(child));
      let is_dir = below.is_some_and(Node::is_dir);
      let parent = self.slots.linked(at);
      let is_complete = parent.is_complete();
      let is_seen = below.is_some_and(Node::is_registered) || is_complete;
      if self.loose_spelling && is_complete && !below.is_some_and(Node::is_entry) && !is_plain(name)
      {
        self.queue_scan(self.key(at), Depth::Names);
      }
      if has_other || (self.loose_spelling && !self.not_plain.is_empty() && !is_plain(name)) {
        found |= self.mark_others(at, name, child, &parts, said);
      }
      if !is_seen {
        return found;
      }
      let last = parts.peek().is_none();
      if !last
        && let Some(child) = child
        && is_dir
      {
        at = child;
        continue;
      }
      let child = match child {
        Some(child) => self.key(child),
        None => self.insert_child(self.key(at), name),
      };
      let written =
        last && (said != Said::Changed || self.content(child) == Some(Content::Pending));
      self.mark(child, Mark { deep: !written, written, lost: false, since: None });
      return true;
    }
    found
  }

  #[cold]
  fn mark_others(
    &mut self,
    at: u32,
    name: &OsStr,
    exact: Option<u32>,
    rest: &Peekable<Components<'_>>,
    said: Said,
  ) -> bool {
    let last = rest.clone().peek().is_none();
    let mut found = false;
    let one_entry = said == Said::Written && self.loose_spelling;
    for other in self.others_meant(at, name, exact) {
      let node = self.slots.linked(other);
      let written = one_entry && name_of(&self.names, node).eq_ignore_ascii_case(name);
      if !last && node.is_dir() {
        let said = if written { said } else { Said::Changed };
        found |= self.mark_path(other, rest.clone(), said);
      } else {
        let written = written && last;
        self.mark(self.key(other), Mark { deep: !written, written, lost: false, since: None });
        found = true;
      }
    }
    found
  }

  pub fn mark(&mut self, key: NodeKey, mark: Mark) {
    if self[key].add_mark(mark) {
      self.marked.push((key, mark.since));
    }
  }

  pub fn take_mark(&mut self, key: NodeKey) -> Option<Mark> {
    let node = self.get_mut(key)?;
    let mark = node.mark();
    node.set_mark(None);
    mark
  }

  pub fn queue_scan(&mut self, key: NodeKey, depth: Depth) {
    let Some(dir) = self[key].dir_mut() else { return };
    if let Some(old) = dir.scan {
      dir.scan = Some(old.max(depth));
    } else {
      dir.scan = Some(depth);
      self.to_scan.push(key);
    }
  }

  pub fn take_scans(&mut self, scans: &mut Vec<(NodeKey, Depth, bool)>) {
    scans.clear();
    let mut queue = mem::take(&mut self.to_scan);
    scans.extend(queue.drain(..).filter_map(|key| {
      let dir = self.get_mut(key)?.dir_mut()?;
      Some((key, dir.scan.take()?, mem::take(&mut dir.away)))
    }));
    self.to_scan = emptied(queue);
  }

  pub fn take_marked(&mut self, marks: &mut Vec<Marked>) {
    marks.clear();
    let mut marked = mem::take(&mut self.marked);
    for (newer, (key, since)) in marked.drain(..).rev().enumerate() {
      if let Some(parent) = self.parent(key)
        && self[key].is_marked()
      {
        marks.push((self.ancestor_slots(key).count(), newer, parent, key, since));
      }
    }
    self.marked = emptied(marked);
    marks.sort_unstable_by_key(|&(level, newer, parent, key, _)| {
      (level, parent.index(), key.index(), newer)
    });
    marks.dedup_by_key(|&mut (.., key, _)| key);
  }

  pub fn tidy_marked(&mut self) {
    let mut marked = mem::take(&mut self.marked);
    marked.retain(|&(key, _)| self.get(key).is_some_and(Node::is_marked));
    self.marked = marked;
  }

  pub fn has_work(&self) -> bool {
    !self.marked.is_empty() || !self.to_scan.is_empty()
  }

  pub fn list_if_polled(&mut self, key: NodeKey) {
    if matches!(self[key].kind, Kind::Dir(dir) if dir.is_polled()) {
      self.polled.push(key);
      if self.polled.len() > 2 * self.slots.len() + 16 {
        self.tidy_polled();
      }
    }
  }

  pub fn poll_all(&mut self) {
    for slot in self.slots.iter_mut() {
      if let Some(Node { kind: Kind::Dir(dir), .. }) = slot.node_mut() {
        (dir.watch, dir.asked) = (Watch::Polled, None);
      }
    }
    let dirs = self.nodes().filter(|(_, node)| matches!(node.kind, Kind::Dir(_)));
    self.polled = dirs.map(|(key, _)| key).collect();
  }

  pub fn tidy_polled(&mut self) {
    let mut polled = mem::take(&mut self.polled);
    polled.sort_unstable();
    polled.dedup();
    polled.retain(|&key| {
      self
        .get(key)
        .is_some_and(|node: &Node| matches!(node.kind, Kind::Dir(dir) if dir.is_polled()))
    });
    if let Some(room) = room_to_keep(polled.len(), polled.capacity()) {
      polled.shrink_to(room);
    }
    self.polled = polled;
  }
}
