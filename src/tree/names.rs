use std::ffi::OsStr;

const BLOCK: usize = 1 << 16;

#[derive(Debug, Default)]
pub(super) struct Names {
  blocks: Vec<Vec<u8>>,
  used: usize,
  dead: usize,
}

impl Names {
  pub fn add(&mut self, name: &OsStr) -> (u32, u16) {
    let name = name.as_encoded_bytes();
    let len = u16::try_from(name.len()).expect("a name is below 64 KiB: `path::MAX_NAME`");
    if self.blocks.last().is_none_or(|block| block.len() + name.len() > BLOCK) {
      self.blocks.push(Vec::new());
    }
    let full = (self.blocks.len() - 1) * BLOCK;
    let block = self.blocks.last_mut().expect("a block");
    if block.capacity() - block.len() < name.len() {
      block.reserve_exact(name.len().max(block.len() / 8 + 256).min(BLOCK - block.len()));
    }
    let start = u32::try_from(full + block.len()).expect("all names together are below 4 GiB");
    block.extend_from_slice(name);
    self.used += name.len();
    (start, len)
  }

  pub fn get(&self, start: u32, len: u16) -> &OsStr {
    let start = start as usize;
    let bytes = &self.blocks[start / BLOCK][start % BLOCK..][..usize::from(len)];
    // SAFETY: `start` and `len` come from `add`: the bytes are one name from `as_encoded_bytes`.
    unsafe { OsStr::from_encoded_bytes_unchecked(bytes) }
  }

  pub fn free(&mut self, len: u16) {
    self.dead += usize::from(len);
  }

  pub fn is_half_dead(&self, slots: usize) -> bool {
    self.dead >= slots && self.dead >= self.used - self.dead
  }

  pub fn pack<'a>(&mut self, used: impl Iterator<Item = (&'a mut u32, u16)>) {
    let mut packed = Self::default();
    for (start, len) in used {
      (*start, _) = packed.add(self.get(*start, len));
    }
    *self = packed;
  }

  #[cfg(test)]
  pub fn len(&self) -> usize {
    self.used
  }
}
