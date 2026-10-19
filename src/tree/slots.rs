//! The arena of the nodes, in blocks of a fixed size. A full block never moves: the arena grows by
//! one block, with no copy of the slots before it, so it is never held twice while it grows.

use std::ops::{Index, IndexMut};

use super::{Slot, room_to_keep};

/// Slots in a block: 64 KiB.
const BLOCK: usize = 1024;

#[derive(Debug, Default)]
pub(super) struct Slots {
  /// Full blocks, and a last one that grows like a `Vec` up to the size of a block.
  blocks: Vec<Vec<Slot>>,
  len: usize,
}

impl Slots {
  pub fn len(&self) -> usize {
    self.len
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

  pub fn last(&self) -> Option<&Slot> {
    self.blocks.last()?.last()
  }

  pub fn push(&mut self, slot: Slot) {
    if self.len.is_multiple_of(BLOCK) {
      self.blocks.push(Vec::new());
    }
    self.blocks.last_mut().expect("a block").push(slot);
    self.len += 1;
  }

  /// Takes the last slot away, and the block if that leaves it empty.
  pub fn pop(&mut self) {
    let Some(block) = self.blocks.last_mut() else { return };
    block.pop();
    self.len -= 1;
    if block.is_empty() {
      self.blocks.pop();
    }
  }

  /// Gives back the room of the last block when less than half of it is in use.
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
