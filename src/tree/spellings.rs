//! Names that the OS can take for one another (design.md 5.2). The table files a child under a
//! hash that leaves out the case of ASCII letters, so one walk finds a name and its twins; a
//! registered name that is not plain is on a list of its directory.

use std::ffi::OsStr;

use super::{Tree, name_of};
use crate::backend::DirKey;

/// The hash under which the table files the child `name` of `parent`. The case of ASCII letters
/// is left out, so that names that differ only there share a hash and [`Tree::spellings`] finds
/// them in one walk. No ready hasher does that without a copy of the name, so the bytes are
/// mixed here, 16 at a time.
///
/// Only the letters are folded: names that differ in any other byte have other hashes, also
/// when a directory holds very many of them (a listing of names that share a hash costs the
/// square of their number).
#[expect(clippy::cast_possible_truncation, reason = "the two halves of a product are taken")]
pub(super) fn child_hash(parent: u32, name: &OsStr) -> u64 {
  // Digits of pi: two numbers with no pattern.
  const SEEDS: [u64; 2] = [0x243f_6a88_85a3_08d3, 0x1319_8a2e_0370_7344];
  let mix = |a: u64, b: u64| {
    let product = u128::from(a) * u128::from(b);
    (product as u64) ^ ((product >> 64) as u64)
  };
  let word = |bytes: &[u8]| small(u64::from_ne_bytes(*bytes.first_chunk().expect("eight bytes")));
  let half =
    |bytes: &[u8]| small(u64::from(u32::from_ne_bytes(*bytes.first_chunk().expect("four bytes"))));

  let mut rest = name.as_encoded_bytes();
  let mut hash = SEEDS[0] ^ u64::from(parent) ^ ((rest.len() as u64) << 32);
  while rest.len() > 16 {
    hash = mix(hash ^ word(rest), SEEDS[1] ^ word(&rest[8..]));
    rest = &rest[16..];
  }
  // What is left, 16 bytes at most, as two numbers. With fewer bytes the two overlap.
  let (first, last) = match *rest {
    [] => (0, 0),
    [one] => (small(u64::from(one)), 0),
    [one, two] => (small(u64::from(one)), small(u64::from(two))),
    [one, two, three] => {
      (small(u64::from(one)), small(u64::from(u16::from_ne_bytes([two, three]))))
    }
    _ if rest.len() < 8 => (half(rest), half(&rest[rest.len() - 4..])),
    _ => (word(rest), word(&rest[rest.len() - 8..])),
  };
  mix(hash ^ first, SEEDS[1] ^ last)
}

/// The eight bytes with each ASCII capital made small, all at once; every other byte stays.
fn small(bytes: u64) -> u64 {
  const ONES: u64 = u64::from_ne_bytes([1; 8]);
  const HIGH: u64 = ONES * 0x80;
  // The low seven bits of each byte, plus a number that carries into the high bit of the byte
  // from `A` on, and from the byte after `Z` on. No sum leaves its byte.
  let low = bytes & !HIGH;
  let from_a = low + ONES * (0x80 - u64::from(b'A'));
  let after_z = low + ONES * (0x80 - u64::from(b'Z') - 1);
  let capital = from_a & !after_z & !bytes & HIGH;
  // The high bit of a capital, moved to the bit that tells `a` from `A`.
  bytes | (capital >> 2)
}

/// Whether every other spelling of `name` is known: only ASCII and Han ideographs, which have no
/// case and no second form. The few characters that a disk folds to plain ones (`ß`, the Kelvin
/// sign, ligatures, compatibility ideographs) are not covered: design.md 9.
pub(crate) fn is_plain(name: &OsStr) -> bool {
  let is_plain = |letter: char| {
    letter.is_ascii()
      || matches!(letter, '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}')
      || matches!(letter, '\u{20000}'..='\u{2F7FF}' | '\u{30000}'..='\u{3FFFF}')
  };
  name.is_ascii() || name.to_str().is_some_and(|name| name.chars().all(is_plain))
}

impl Tree {
  /// The child `name` of slot `parent`, and whether another child differs from it only in the case
  /// of ASCII letters: one walk of the table finds both, since such names share a hash.
  pub(super) fn spellings(&self, parent: u32, name: &OsStr) -> (Option<u32>, bool) {
    let (mut child, mut has_other) = (None, false);
    for &index in self.index.iter_hash(child_hash(parent, name)) {
      let node = self.slots.linked(index);
      if node.parent != parent {
        continue;
      }
      let other = name_of(&self.names, node);
      if other == name {
        child = Some(index);
      } else {
        has_other |= other.eq_ignore_ascii_case(name);
      }
    }
    (child, has_other)
  }

  /// The other children of slot `at` that a hint for `name` can mean, besides `exact`: a registered
  /// child that can be the same name, and in a complete node any child that differs only in the
  /// case of ASCII letters. Why: design.md 5.2.
  pub(super) fn others_meant(&self, at: u32, name: &OsStr, exact: Option<u32>) -> Vec<u32> {
    // Where every disk is loose, a name that is not plain can be any registered name that is
    // not plain either; a listing shows the other children (`mark_path`).
    if self.loose_spelling && !is_plain(name) {
      let listed = self.not_plain.get(&at).map_or(&[][..], Vec::as_slice);
      return listed.iter().copied().filter(|&child| Some(child) != exact).collect();
    }
    let any_child = self.slots.linked(at).is_complete();
    let alike = self.alike(at, name, exact);
    alike.filter(|&child| any_child || self.slots.linked(child).is_registered()).collect()
  }

  /// Whether the name of the node `key` has more than one spelling at all: it has an ASCII
  /// letter, or it is not plain.
  pub fn has_spellings(&self, key: DirKey) -> bool {
    let name = self.name(key);
    !is_plain(name) || name.as_encoded_bytes().iter().any(u8::is_ascii_alphabetic)
  }

  /// Whether the tree cannot say that the node has its name as the disk spells it: another node of
  /// the directory can be the same name, or the name is not plain.
  pub fn has_other_spelling(&self, key: DirKey) -> bool {
    !is_plain(self.name(key)) || self.twins(key).next().is_some()
  }

  /// The twins of the node `key` that have a mark.
  pub fn marked_twins(&self, key: DirKey) -> Vec<DirKey> {
    let marked = self.twins(key).filter(|&twin| self.slots.linked(twin).is_marked());
    marked.map(|twin| self.key(twin)).collect()
  }

  /// The other nodes of the directory whose names differ from the name of `key` only in the case
  /// of ASCII letters.
  fn twins(&self, key: DirKey) -> impl Iterator<Item = u32> + '_ {
    let node = &self[key];
    self.alike(node.parent, name_of(&self.names, node), Some(key.index()))
  }

  /// The children of slot `parent` but `except` whose names differ from `name` in the case of
  /// ASCII letters at most: the table files them under one hash.
  fn alike(&self, parent: u32, name: &OsStr, except: Option<u32>) -> impl Iterator<Item = u32> {
    self.index.iter_hash(child_hash(parent, name)).copied().filter(move |&index| {
      let node = self.slots.linked(index);
      Some(index) != except
        && node.parent == parent
        && name_of(&self.names, node).eq_ignore_ascii_case(name)
    })
  }
}

#[cfg(test)]
mod tests {
  use std::ffi::OsStr;

  use super::{child_hash, is_plain};

  /// The table finds a name under every case of its letters, whatever its length.
  #[test]
  fn the_case_of_letters_is_left_out_of_the_hash() {
    let name = "aBcDeFgHiJkLmNoPqRsTuVwXyZ-0123456789.Js";
    let mut hashes = Vec::new();
    for len in 0..=name.len() {
      let (lower, upper) = (name[..len].to_ascii_lowercase(), name[..len].to_ascii_uppercase());
      let hash = child_hash(7, OsStr::new(&lower));
      assert_eq!(hash, child_hash(7, OsStr::new(&upper)), "{len} bytes");
      hashes.extend([hash, child_hash(8, OsStr::new(&lower))]);
    }
    // Another name, or another parent, is another hash.
    hashes.sort_unstable();
    hashes.dedup();
    assert_eq!(hashes.len(), 2 * (name.len() + 1));
  }

  /// Only letters are folded: names that differ in the same bit of another byte, which a
  /// directory can hold in any number, have other hashes.
  #[test]
  fn only_the_case_of_letters_is_left_out_of_the_hash() {
    let hash = |name: &str| child_hash(1, OsStr::new(name));
    for byte in 0..0x80_u8 {
      for len in [1, 2, 3, 5, 8, 13, 16, 17] {
        let [one, two] = [byte, byte ^ 0x20].map(|byte| String::from(char::from(byte)).repeat(len));
        let is_letter = byte.is_ascii_alphabetic();
        assert_eq!(hash(&one) == hash(&two), is_letter, "byte {byte:#x}, {len} bytes");
      }
    }
    // Bytes of other letters that differ in the same bit: `é` and `É`, `Р` and `Ѐ`.
    assert_ne!(hash("café"), hash("cafÉ"));
    assert_ne!(hash("Р"), hash("Ѐ"));
  }

  #[test]
  fn names_that_are_plain() {
    assert!(is_plain(OsStr::new("App.js")) && is_plain(OsStr::new("\u{4e2d}\u{6587}.js")));
    // An ideograph is plain, a compatibility ideograph is not.
    assert!(!is_plain(OsStr::new("caf\u{e9}.js")) && !is_plain(OsStr::new("\u{f967}.js")));
  }
}
