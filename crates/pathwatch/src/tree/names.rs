//! The names of all nodes, one after the other in one block of memory.
//!
//! A node keeps where its name starts and how long it is. A name that is freed leaves its
//! bytes behind. When half of the buffer is such dead bytes, the tree packs it: the names in
//! use move to the front, and each node is told the new place of its name.

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
    // SAFETY: `start` and `len` come from `add` or `pack`, so the bytes are one whole name,
    // as `OsStr::as_encoded_bytes` gave it to `add`. Nothing else writes to the buffer.
    unsafe { OsStr::from_encoded_bytes_unchecked(bytes) }
  }

  /// A name of this length is not used any more.
  pub fn free(&mut self, len: u16) {
    self.dead += usize::from(len);
  }

  /// Whether it is time to pack: half of the bytes are dead. A pass walks every slot of the
  /// arena, so it also waits for one dead byte for each of the `slots`: then a pass never
  /// costs more than the names that were freed for it.
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
  use std::ffi::OsStr;

  use super::Names;

  /// A stand-in for the nodes: where each name is, and the name that is expected there.
  struct Kept {
    start: u32,
    len: u16,
    name: String,
  }

  fn keep(names: &mut Names, name: &str) -> Kept {
    let (start, len) = names.add(OsStr::new(name));
    Kept { start, len, name: name.to_owned() }
  }

  fn check(names: &Names, kept: &[Kept]) {
    for Kept { start, len, name } in kept {
      assert_eq!(names.get(*start, *len), OsStr::new(name));
    }
  }

  #[test]
  fn names_come_back_as_they_went_in() {
    let mut names = Names::default();
    let given = ["", "/", "a.js", "", "node_modules", "caf\u{e9} \u{1f600}.ts", "a.js"];
    let kept: Vec<Kept> = given.iter().map(|name| keep(&mut names, name)).collect();
    check(&names, &kept);
    assert_eq!(names.len(), given.iter().map(|name| name.len()).sum::<usize>());

    // The empty name of the root takes no room, and two equal names are kept twice.
    assert_eq!((kept[0].start, kept[0].len), (0, 0));
    assert_ne!(kept[2].start, kept[6].start);
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
    let mut kept: Vec<Kept> = (0..100).map(|i| keep(&mut names, &format!("file{i:04}"))).collect();
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

    // The buffer is used on after a pass.
    kept.push(keep(&mut names, "new.js"));
    assert_eq!(kept.last().unwrap().start, 400);
    check(&names, &kept);
  }

  #[test]
  fn a_buffer_with_no_name_left_packs_to_nothing() {
    let mut names = Names::default();
    let (_, len) = names.add(OsStr::new("only.js"));
    names.free(len);
    assert!(names.is_half_dead(1));
    names.pack(std::iter::empty());
    assert_eq!(names.len(), 0);
    let kept = keep(&mut names, "next.js");
    assert_eq!(kept.start, 0);
    check(&names, &[kept]);
  }

  #[test]
  fn a_buffer_that_grows() {
    let mut names = Names::default();
    let mut kept = Vec::new();
    let mut capacities = Vec::new();
    for i in 0..20_000 {
      kept.push(keep(&mut names, &format!("module-{i}.js")));
      if capacities.last() != Some(&names.bytes.capacity()) {
        capacities.push(names.bytes.capacity());
      }
    }
    check(&names, &kept);
    // It grew in many small steps: by an eighth, never to twice its size.
    assert!(capacities.len() > 10, "{capacities:?}");
    assert!(capacities.windows(2).all(|pair| pair[1] <= pair[0] + pair[0] / 8 + 256));
    assert!(names.bytes.capacity() <= names.len() + names.len() / 8 + 256);

    // A name that is longer than the usual step still fits.
    let long = "x".repeat(60_000);
    kept.push(keep(&mut names, &long));
    check(&names, &kept);
  }
}
