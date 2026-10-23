use std::ops::{Index, IndexMut};

use super::{Node, room_to_keep};

const BLOCK: usize = 1024;

#[derive(Debug, Default)]
pub(super) struct Slots {
  blocks: Vec<Vec<Slot>>,
}

impl Slots {
  pub fn len(&self) -> usize {
    self.blocks.last().map_or(0, |last| (self.blocks.len() - 1) * BLOCK + last.len())
  }

  #[cfg(test)]
  pub fn capacity(&self) -> usize {
    self.blocks.iter().map(Vec::capacity).sum()
  }

  pub fn get(&self, index: usize) -> Option<&Slot> {
    self.blocks.get(index / BLOCK)?.get(index % BLOCK)
  }

  pub fn get_mut(&mut self, index: usize) -> Option<&mut Slot> {
    self.blocks.get_mut(index / BLOCK)?.get_mut(index % BLOCK)
  }

  pub fn linked(&self, index: u32) -> &Node {
    self[index as usize].node().expect("a link points to a live node")
  }

  pub fn linked_mut(&mut self, index: u32) -> &mut Node {
    self[index as usize].node_mut().expect("a link points to a live node")
  }

  pub fn last(&self) -> Option<&Slot> {
    self.blocks.last()?.last()
  }

  pub fn push(&mut self, slot: Slot) {
    if self.blocks.last().is_none_or(|block| block.len() == BLOCK) {
      self.blocks.push(Vec::new());
    }
    self.blocks.last_mut().expect("a block").push(slot);
  }

  pub fn pop(&mut self) {
    let Some(block) = self.blocks.last_mut() else { return };
    block.pop();
    if block.is_empty() {
      self.blocks.pop();
    }
  }

  pub fn shrink(&mut self) {
    if let Some(block) = self.blocks.last_mut()
      && let Some(room) = room_to_keep(block.len(), block.capacity())
    {
      block.shrink_to(room);
    }
  }

  pub fn iter(&self) -> impl Iterator<Item = &Slot> {
    self.blocks.iter().flatten()
  }

  pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Slot> {
    self.blocks.iter_mut().flatten()
  }
}

impl Index<usize> for Slots {
  type Output = Slot;

  fn index(&self, index: usize) -> &Slot {
    &self.blocks[index / BLOCK][index % BLOCK]
  }
}

impl IndexMut<usize> for Slots {
  fn index_mut(&mut self, index: usize) -> &mut Slot {
    &mut self.blocks[index / BLOCK][index % BLOCK]
  }
}

#[derive(Debug)]
pub(super) enum Slot {
  Live(Node),
  Free(u32),
}

impl Slot {
  pub fn node(&self) -> Option<&Node> {
    match self {
      Self::Live(node) => Some(node),
      Self::Free(_) => None,
    }
  }

  pub fn node_mut(&mut self) -> Option<&mut Node> {
    match self {
      Self::Live(node) => Some(node),
      Self::Free(_) => None,
    }
  }
}
