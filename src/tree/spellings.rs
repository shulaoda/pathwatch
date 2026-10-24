use std::ffi::OsStr;

use super::{Tree, name_of};
use crate::backend::NodeKey;

#[expect(clippy::cast_possible_truncation, reason = "the two halves of a product are taken")]
pub(super) fn child_hash(parent: u32, name: &OsStr) -> u64 {
  const SEEDS: [u64; 2] = [0x243f_6a88_85a3_08d3, 0x1319_8a2e_0370_7344];
  let mix = |a: u64, b: u64| {
    let product = u128::from(a) * u128::from(b);
    (product as u64) ^ ((product >> 64) as u64)
  };

  let mut rest = name.as_encoded_bytes();
  let mut hash = SEEDS[0] ^ u64::from(parent) ^ ((rest.len() as u64) << 32);
  while rest.len() > 16
    && let Some((one, more)) = rest.split_first_chunk()
    && let Some((two, more)) = more.split_first_chunk()
  {
    hash = mix(hash ^ lower(u64::from_ne_bytes(*one)), SEEDS[1] ^ lower(u64::from_ne_bytes(*two)));
    rest = more;
  }
  let (first, last) = if let Some((one, two)) = rest.first_chunk::<8>().zip(rest.last_chunk()) {
    (u64::from_ne_bytes(*one), u64::from_ne_bytes(*two))
  } else if let Some((one, two)) = rest.first_chunk::<4>().zip(rest.last_chunk()) {
    (u64::from(u32::from_ne_bytes(*one)), u64::from(u32::from_ne_bytes(*two)))
  } else {
    match *rest {
      [one, two, three] => (u64::from(one), u64::from(u16::from_ne_bytes([two, three]))),
      [one, two] => (u64::from(one), u64::from(two)),
      [one] => (u64::from(one), 0),
      _ => (0, 0),
    }
  };
  mix(hash ^ lower(first), SEEDS[1] ^ lower(last))
}

fn lower(bytes: u64) -> u64 {
  const ONES: u64 = u64::from_ne_bytes([1; 8]);
  const HIGH: u64 = ONES * 0x80;
  let low = bytes & !HIGH;
  let from_a = low + ONES * (0x80 - u64::from(b'A'));
  let after_z = low + ONES * (0x80 - u64::from(b'Z') - 1);
  let capital = from_a & !after_z & !bytes & HIGH;
  bytes | (capital >> 2)
}

pub(crate) fn is_plain(name: &OsStr) -> bool {
  name.is_ascii()
    || name.to_str().is_some_and(|name| {
      name.chars().all(|letter: char| {
        letter.is_ascii()
          || matches!(letter, '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}')
          || matches!(letter, '\u{20000}'..='\u{2F7FF}' | '\u{30000}'..='\u{3FFFF}')
      })
    })
}

impl Tree {
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

  pub(super) fn others_meant(&self, at: u32, name: &OsStr, exact: Option<u32>) -> Vec<u32> {
    if self.loose_spelling && !is_plain(name) {
      let listed = self.not_plain.get(&at).map_or(&[][..], Vec::as_slice);
      return listed.iter().copied().filter(|&child| Some(child) != exact).collect();
    }
    let any_child = self.slots.linked(at).is_complete();
    let alike = self.alike(at, name, exact);
    alike.filter(|&child| any_child || self.slots.linked(child).is_registered()).collect()
  }

  pub fn has_spellings(&self, key: NodeKey) -> bool {
    let name = self.name(key);
    !is_plain(name) || name.as_encoded_bytes().iter().any(u8::is_ascii_alphabetic)
  }

  pub fn has_other_spelling(&self, key: NodeKey) -> bool {
    !is_plain(self.name(key)) || self.twins(key).next().is_some()
  }

  pub fn marked_twins(&self, key: NodeKey) -> Vec<NodeKey> {
    let marked = self.twins(key).filter(|&twin| self.slots.linked(twin).is_marked());
    marked.map(|twin| self.key(twin)).collect()
  }

  fn twins(&self, key: NodeKey) -> impl Iterator<Item = u32> + '_ {
    let node = &self[key];
    self.alike(node.parent, name_of(&self.names, node), Some(key.index()))
  }

  fn alike(&self, parent: u32, name: &OsStr, except: Option<u32>) -> impl Iterator<Item = u32> {
    self.index.iter_hash(child_hash(parent, name)).copied().filter(move |&index| {
      let node = self.slots.linked(index);
      Some(index) != except
        && node.parent == parent
        && name_of(&self.names, node).eq_ignore_ascii_case(name)
    })
  }
}
