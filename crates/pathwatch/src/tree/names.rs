//! The names of all nodes, one after the other in one buffer. A freed name leaves its bytes
//! behind; when half of the buffer is dead, the names in use are packed to the front.

use std::ffi::OsStr;

#[derive(Debug, Default)]
pub(super) struct Names {
  bytes: Vec<u8>,
  /// How many of the bytes belong to names that were freed.
  dead: usize,
}

impl Names {
  /// Keeps a name. Gives where it starts and how long it is.
  pub fn add(&mut self, name: &OsStr) -> (u32, u16) {
    let name = name.as_encoded_bytes();
    let start = u32::try_from(self.bytes.len()).expect("all names together are below 4 GiB");
    let len = u16::try_from(name.len()).expect("a name is below 64 KiB: `path::MAX_NAME`");
    if self.bytes.capacity() - self.bytes.len() < name.len() {
      // Grows by an eighth, not to twice its size as a `Vec` does by itself: like the arena.
      self.bytes.reserve_exact(name.len().max(self.bytes.len() / 8 + 256));
    }
    self.bytes.extend_from_slice(name);
    (start, len)
  }

  /// The name that [`Names::add`] put at `start`.
  pub fn get(&self, start: u32, len: u16) -> &OsStr {
    let bytes = &self.bytes[start as usize..][..usize::from(len)];
    // SAFETY: `start` and `len` came from `add` or `pack`, so the bytes are one whole name as
    // `OsStr::as_encoded_bytes` gave it, and nothing else writes to the buffer.
    unsafe { OsStr::from_encoded_bytes_unchecked(bytes) }
  }

  /// A name of this length is not used any more.
  pub fn free(&mut self, len: u16) {
    self.dead += usize::from(len);
  }

  /// Whether it is time to pack: half of the bytes are dead, and at least one for each of the
  /// `slots` that a pass walks, so that a pass never costs more than the names freed for it.
  pub fn is_half_dead(&self, slots: usize) -> bool {
    self.dead >= slots && self.dead >= self.bytes.len() - self.dead
  }

  /// Moves the names in use to the front of a new buffer of the right size. `used` gives each
  /// of them: the place of its start, which is set to the new start, and its length.
  pub fn pack<'a>(&mut self, used: impl Iterator<Item = (&'a mut u32, u16)>) {
    let mut packed = Vec::with_capacity(self.bytes.len() - self.dead);
    for (start, len) in used {
      let name = &self.bytes[*start as usize..][..usize::from(len)];
      *start = u32::try_from(packed.len()).expect("the packed names are fewer than before");
      packed.extend_from_slice(name);
    }
    self.bytes = packed;
    self.dead = 0;
  }

  /// How many bytes the buffer holds, dead ones too.
  #[cfg(test)]
  pub fn len(&self) -> usize {
    self.bytes.len()
  }
}

#[cfg(test)]
mod tests {
  use std::ffi::{OsStr, OsString};

  use super::Names;

  /// A stand-in for the nodes: where each name is, and the name that is expected there.
  struct Kept {
    start: u32,
    len: u16,
    name: OsString,
  }

  fn keep(names: &mut Names, name: &OsStr) -> Kept {
    let (start, len) = names.add(name);
    Kept { start, len, name: name.to_owned() }
  }

  fn check(names: &Names, kept: &[Kept]) {
    for Kept { start, len, name } in kept {
      assert_eq!(names.get(*start, *len), name);
    }
  }

  #[test]
  fn names_come_back_as_they_went_in() {
    let mut names = Names::default();
    let given = ["", "/", "caf\u{e9} \u{1f600}.ts"].map(OsStr::new);
    let kept: Vec<Kept> = given.iter().map(|name| keep(&mut names, name)).collect();
    check(&names, &kept);
  }

  #[cfg(unix)]
  #[test]
  fn a_name_that_is_no_text_comes_back_too() {
    use std::os::unix::ffi::OsStrExt;

    let mut names = Names::default();
    let odd = OsStr::from_bytes(b"\xff\xfe a\x80");
    let (start, len) = names.add(odd);
    assert_eq!(names.get(start, len), odd);
  }

  #[test]
  fn freed_names_are_packed_away() {
    let mut names = Names::default();
    let mut kept: Vec<Kept> =
      (0..100).map(|i| keep(&mut names, OsStr::new(&format!("file{i:04}")))).collect();
    assert_eq!(names.len(), 800);

    // Every second name goes. That is half of the bytes, but not yet a byte for each slot of
    // an arena of 1,000 slots.
    let mut index = 0;
    kept.retain(|name| {
      index += 1;
      let stays = index % 2 == 0;
      if !stays {
        assert!(!names.is_half_dead(100));
        names.free(name.len);
      }
      stays
    });
    assert!(names.is_half_dead(100));
    assert!(!names.is_half_dead(1000));
    check(&names, &kept);

    names.pack(kept.iter_mut().map(|name| (&mut name.start, name.len)));
    assert_eq!(names.len(), 400);
    assert!(!names.is_half_dead(1));
    assert_eq!(kept[0].start, 0);
    check(&names, &kept);
  }
}
