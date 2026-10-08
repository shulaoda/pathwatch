//! The data of the engine: a tree of the paths that matter, and the work that waits in it.
//! Nothing here reads the disk.
//!
//! One node is one name. All nodes live in one arena, and only this file knows the links between
//! them and the table that finds a child by its name. The names are in one buffer of their own
//! (`names.rs`), and the small facts of a node are bits of one number (`flags.rs`).

mod flags;
mod names;

use std::{
  ffi::OsStr,
  iter, mem,
  ops::{Index, IndexMut},
  path::{Component, Path, PathBuf},
  time::{Instant, SystemTime},
};

use hashbrown::HashTable;
use rustc_hash::FxHashMap;

use self::{flags::Flags, names::Names};
use crate::{
  backend::{DirKey, Hint, Want},
  fs::{DirId, Meta},
};

/// In a link between nodes: there is no such node.
const NONE: u32 = u32::MAX;

/// One name the tree knows.
///
/// A node is kept for one of four reasons: it is registered (a watched path, or on the way to
/// one), something that counts is at its path, a mark waits on it, or the handler still has to
/// hear that its file is gone. [`Tree::prune`] frees a node that has none of them.
#[derive(Debug)]
pub struct Node {
  /// What the last look found at the path. While the node is held: the look before that.
  pub kind: Kind,

  // Where it is.
  /// The second half of its key. The next node in the same slot gets another one.
  generation: u32,
  /// `NONE` only for the root.
  parent: u32,
  /// The children are a list: the first one here, and each child knows its two neighbours.
  first_child: u32,
  next: u32,
  prev: u32,
  /// How many of its children are watched paths or lead to one.
  registered_children: u32,
  /// Where its name is in [`Tree::names`]. For a top directory the name is its whole path
  /// (`/`, `C:\`).
  name_at: u32,
  name_len: u16,

  /// Held, told, changed, baseline, a watched path, what the running `watch` call did with
  /// it, the answers of the ignore filter, and its mark: see `flags.rs` for each.
  flags: Flags,
}

// One node for each path the tree knows: it has to stay small. The fields above fill 64 bytes
// with no gap.
#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<Node>() == 64 && size_of::<Slot>() == 64);

impl Node {
  fn new(generation: u32, parent: u32, (name_at, name_len): (u32, u16)) -> Self {
    Self {
      kind: Kind::Missing,
      generation,
      parent,
      first_child: NONE,
      next: NONE,
      prev: NONE,
      registered_children: 0,
      name_at,
      name_len,
      flags: Flags::empty(),
    }
  }

  /// Whether a directory is on disk at the path. Not while the node is held.
  pub fn is_dir(&self) -> bool {
    self.dir().is_some()
  }

  /// What is kept for the directory that is on disk at the path.
  pub fn dir(&self) -> Option<&Dir> {
    match &self.kind {
      Kind::Dir(dir) if !self.is_held() => Some(dir),
      _ => None,
    }
  }

  pub fn dir_mut(&mut self) -> Option<&mut Dir> {
    let held = self.is_held();
    match &mut self.kind {
      Kind::Dir(dir) if !held => Some(dir),
      _ => None,
    }
  }

  /// Whether the node tracks every entry of its directory.
  pub fn is_complete(&self) -> bool {
    self.dir().is_some_and(|dir| dir.complete)
  }

  /// How changes in the directory are heard. `Watch::None` for a node that is no directory.
  pub fn watch(&self) -> Watch {
    self.dir().map_or(Watch::None, |dir| dir.watch)
  }

  /// Whether a watched path lies below the path.
  pub fn is_ancestor(&self) -> bool {
    self.registered_children > 0
  }

  /// Whether the path is a watched path or leads to one. Such a node is tracked also in a
  /// partial node, and it stays while nothing is at its path.
  pub fn is_registered(&self) -> bool {
    self.is_path() || self.is_ancestor()
  }

  /// Adds a mark to the node. Two marks for one node become one. `true`: it had none.
  fn add_mark(&mut self, mark: Mark) -> bool {
    let old = self.mark();
    self.set_mark(Some(old.map_or(mark, |old| Mark {
      deep: old.deep || mark.deep,
      written: old.written || mark.written,
      // A check that anything else asked for does not wait with the loss reports.
      lost: old.lost && mark.lost,
    })));
    old.is_none()
  }
}

/// What the tree has at a path.
#[derive(Clone, Copy, Debug)]
pub enum Kind {
  /// Nothing, or nothing that counts.
  Missing,
  File(Fingerprint),
  Dir(Dir),
}

/// What tells one state of a file from the next without reading it: its modification time, its
/// size, and its inode (on Windows: its creation time).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fingerprint {
  /// The modification time in nanoseconds since 1970 (before 1970: below zero).
  pub modified: i64,
  pub size: u64,
  pub id: u64,
}

impl Fingerprint {
  pub fn of(meta: &Meta) -> Self {
    Self { modified: nanos(meta.modified), size: meta.size, id: meta.file_id }
  }
}

/// A time as one number: nanoseconds since 1970. A time that does not fit (before 1678 or after
/// 2262) becomes the nearest one that fits.
fn nanos(time: SystemTime) -> i64 {
  let fit = |nanos: u128| i64::try_from(nanos).unwrap_or(i64::MAX);
  match time.duration_since(SystemTime::UNIX_EPOCH) {
    Ok(after) => fit(after.as_nanos()),
    Err(before) => -fit(before.duration().as_nanos()),
  }
}

/// What the tree keeps for a directory that is on disk. All of it goes away with the directory.
#[derive(Clone, Copy, Debug)]
pub struct Dir {
  /// Tells this directory from another one at the same path.
  pub id: DirId,
  /// The node tracks every entry that is not ignored. If not, it is partial: it tracks only
  /// its registered children.
  pub complete: bool,
  /// The last want given to the backend. `None`: the backend has no watch for this node.
  pub asked: Option<Want>,
  pub watch: Watch,
  /// A scan of this node, or its watch, failed, and the error was reported. While this is set,
  /// the error is not reported again and every poll scans the node.
  pub has_error: bool,
  /// The node is queued for a scan.
  pub scan: Option<Depth>,
}

impl Dir {
  /// A directory that was just found. The backend was not asked yet.
  pub fn new(id: DirId, complete: bool) -> Self {
    Self { id, complete, asked: None, watch: Watch::None, has_error: false, scan: None }
  }

  /// Whether the timer has to look at the directory: no OS watch tells about it, or it had an
  /// error.
  pub fn is_polled(&self) -> bool {
    matches!(self.watch, Watch::Polled | Watch::Failed) || self.has_error
  }
}

/// How changes in a directory are heard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Watch {
  /// The directory was just found, and the backend was not asked yet. Also the answer for a
  /// node that is no directory.
  None,
  /// The backend watches this directory.
  Watched,
  /// The backend watches this directory and every real directory below it.
  WatchedTree,
  /// A node above is `WatchedTree`, and the backend was not asked for this one.
  FromAbove,
  /// No OS watch covers it: the timer scans it.
  Polled,
  /// The backend could not watch it. It is polled, and the backend is asked again at each poll.
  Failed,
}

/// How far a scan goes.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Depth {
  /// Only the names are compared with a listing: a node that the listing names is as it was,
  /// and is not looked at. Where there is no listing, this is a shallow scan.
  Names,
  /// Every name of the directory is checked. A directory below that is still the same one is
  /// left alone.
  Shallow,
  /// Also every directory below is watched again and scanned deep.
  Deep,
}

/// What the running `watch` call did with a node. The end of the call sets it back to `No`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Added {
  /// Nothing. Also: no call is running.
  No,
  /// The node led to a watched path before. The call made it a watched path itself.
  Path,
  /// The call registered the node: before, it was no watched path and led to none. If it is
  /// a link, nobody followed it before.
  Registered,
}

/// The arena of the nodes, the table that finds a child by its name, and the work that waits.
#[derive(Debug)]
pub struct Tree {
  /// The arena. Its last slot is never free: a free end is cut off ([`Tree::give_back`]).
  slots: Vec<Slot>,
  /// The free slots. An index can be stale: its slot was cut off the end of the arena.
  free: Vec<u32>,
  /// The generation of a node in a new slot: above the generation of every slot that was cut
  /// off, so that an old key for such a slot never names a node again.
  first_generation: u32,
  names: Names,
  /// Every node but the root, filed under its parent and its name.
  index: HashTable<u32>,
  /// The registered nodes that were symbolic links at their last check, each with a hash of
  /// the real path it led to.
  links: FxHashMap<DirKey, u64>,
  /// The files whose fingerprint is not trusted at their next check.
  contents: FxHashMap<DirKey, Content>,
  /// The held nodes, each with the time of its second look.
  holds: FxHashMap<DirKey, Instant>,
  /// The times of the directories that nothing but a poll of the directory above looks at.
  times: FxHashMap<DirKey, DirTimes>,
  /// The nodes with a mark. A key can be stale.
  pub marked: Vec<DirKey>,
  /// The nodes that are queued for a scan. A key can be stale or be there twice.
  pub to_scan: Vec<DirKey>,
  /// The directories that the timer looks at ([`Dir::is_polled`]), so that a poll does not walk
  /// every node. The root is always on it. Before [`Tree::tidy_polled`], a key can be stale
  /// or be there twice, and a directory on the list can have an OS watch by now.
  pub polled: Vec<DirKey>,
  /// Notifications were lost: everything has to be scanned. The engine says when.
  pub lost: bool,
  /// The disk takes two spellings of a name for one (`Options::loose_spelling`).
  pub loose_spelling: bool,
}

/// The nodes: finding them, adding and freeing them, their paths, and which are watched paths.
impl Tree {
  /// The one fixed node above the top directories (`/`, `C:\`). It has no path and is never
  /// listed or given to the backend. It always exists and is polled, so every top directory has
  /// a parent that checks it, like any other directory.
  pub const ROOT: DirKey = DirKey::new(0, 0);

  pub fn new() -> Self {
    let mut names = Names::default();
    let mut root = Node::new(0, NONE, names.add(OsStr::new("")));
    root.kind = Kind::Dir(Dir { watch: Watch::Polled, ..Dir::new(DirId::default(), false) });
    Self {
      slots: vec![Slot::Live(root)],
      free: Vec::new(),
      first_generation: 0,
      names,
      index: HashTable::new(),
      links: FxHashMap::default(),
      contents: FxHashMap::default(),
      holds: FxHashMap::default(),
      times: FxHashMap::default(),
      marked: Vec::new(),
      to_scan: Vec::new(),
      polled: vec![Self::ROOT],
      lost: false,
      loose_spelling: false,
    }
  }

  pub fn get(&self, key: DirKey) -> Option<&Node> {
    let node = self.slots.get(key.index() as usize)?.node()?;
    (node.generation == key.generation()).then_some(node)
  }

  pub fn get_mut(&mut self, key: DirKey) -> Option<&mut Node> {
    let node = self.slots.get_mut(key.index() as usize)?.node_mut()?;
    (node.generation == key.generation()).then_some(node)
  }

  /// Every node with its key, the root first.
  pub fn nodes(&self) -> impl Iterator<Item = (DirKey, &Node)> + '_ {
    self.slots.iter().enumerate().filter_map(|(index, slot)| {
      let index = u32::try_from(index).expect("the arena holds fewer than 2^32 nodes");
      let node = slot.node()?;
      Some((DirKey::new(index, node.generation), node))
    })
  }

  /// The name of a node. Empty for the root.
  pub fn name(&self, key: DirKey) -> &OsStr {
    name_of(&self.names, &self[key])
  }

  /// The node above. `None` for the root.
  pub fn parent(&self, key: DirKey) -> Option<DirKey> {
    let parent = self.get(key)?.parent;
    (parent != NONE).then(|| self.key(parent))
  }

  pub fn child(&self, parent: DirKey, name: &OsStr) -> Option<DirKey> {
    self.get(parent)?;
    self.find(parent.index(), name).map(|index| self.key(index))
  }

  /// The children of a node, the newest first.
  pub fn children(&self, key: DirKey) -> impl Iterator<Item = DirKey> + '_ {
    let at = |index: u32| (index != NONE).then_some(index);
    let first = self.get(key).and_then(|node| at(node.first_child));
    iter::successors(first, move |&index| at(linked(&self.slots, index).next))
      .map(|index| self.key(index))
  }

  /// The child that comes after `child` in the list of its parent. With it a loop can walk the
  /// children and change the tree on the way, as long as it frees no node.
  pub fn next_sibling(&self, child: DirKey) -> Option<DirKey> {
    let next = self.get(child)?.next;
    (next != NONE).then(|| self.key(next))
  }

  /// Adds a node for the child `name` of the node `parent`. The caller has checked with
  /// [`Tree::child`] that there is none.
  pub fn insert_child(&mut self, parent: DirKey, name: &OsStr) -> DirKey {
    let parent = parent.index();
    debug_assert!(self.find(parent, name).is_none(), "the child is there already");
    let name_at = self.names.add(name);
    let slots = self.slots.len();
    let free = iter::from_fn(|| self.free.pop()).find(|&index| (index as usize) < slots);
    let index = if let Some(index) = free {
      let slot = &mut self.slots[index as usize];
      let Slot::Free { generation } = *slot else { unreachable!("the slot is on the free list") };
      *slot = Slot::Live(Node::new(generation, parent, name_at));
      index
    } else {
      let index = u32::try_from(self.slots.len()).expect("the arena holds fewer than 2^32 nodes");
      if self.slots.len() == self.slots.capacity() {
        // The arena is the biggest block of the watcher. It grows by an eighth, not to twice
        // its size as a `Vec` does by itself, so that at most a ninth of it is unused.
        self.slots.reserve_exact(self.slots.len() / 8 + 16);
      }
      self.slots.push(Slot::Live(Node::new(self.first_generation, parent, name_at)));
      index
    };

    // The new node becomes the first child.
    let first = mem::replace(&mut self.linked_mut(parent).first_child, index);
    self.linked_mut(index).next = first;
    if first != NONE {
      self.linked_mut(first).prev = index;
    }

    let Self { slots, names, index: table, .. } = self;
    table.insert_unique(child_hash(parent, name), index, |&other| hash_of(slots, names, other));
    self.key(index)
  }

  /// Frees the node if nothing needs it any more: nothing is at its path, it is not
  /// registered, no mark waits on it, and the handler does not believe in a file there.
  pub fn prune(&mut self, key: DirKey) {
    let Some(node) = self.get(key) else { return };
    let nothing = matches!(node.kind, Kind::Missing);
    let needed = !nothing || node.is_registered() || node.is_marked() || node.told();
    // Such a node has no children. The test is there so that a mistake elsewhere can never
    // leave a child without its parent.
    if !needed && node.first_child == NONE {
      self.remove(key);
    }
  }

  /// Writes the path of a node into `path`: the names from the root down. The root itself has
  /// the empty path.
  pub fn path_into(&self, key: DirKey, path: &mut PathBuf) {
    path.clear();
    if self.get(key).is_some() {
      self.push_names(key.index(), path);
    }
  }

  pub fn path(&self, key: DirKey) -> PathBuf {
    let mut path = PathBuf::new();
    self.path_into(key, &mut path);
    path
  }

  /// How many nodes are above the node. Zero for a key that names no node.
  pub fn level(&self, key: DirKey) -> usize {
    let mut level = 0;
    let mut parent = self.get(key).map_or(NONE, |node| node.parent);
    while parent != NONE {
      level += 1;
      parent = linked(&self.slots, parent).parent;
    }
    level
  }

  /// Makes the node a watched path. `false`: it is one already.
  pub fn make_path(&mut self, key: DirKey) -> bool {
    let node = &mut self[key];
    if node.is_path() {
      return false;
    }
    let was_registered = node.is_registered();
    node.set_path(true);
    let parent = node.parent;
    if parent != NONE && !was_registered {
      self.count_registered(parent);
    }
    true
  }

  /// The node is no watched path any more. `false`: it was none.
  pub fn drop_path(&mut self, key: DirKey) -> bool {
    let node = &mut self[key];
    if !node.is_path() {
      return false;
    }
    node.set_path(false);
    let is_registered = node.is_registered();
    let parent = node.parent;
    if parent != NONE && !is_registered {
      self.uncount_registered(parent);
    }
    true
  }

  /// What the directory at the node asks from the backend, as far as the tree can tell:
  /// `Contents` when the node is complete, or when a registered child of it has a file at it;
  /// `Names` when every registered child is a directory.
  ///
  /// `None`: the disk has to say. The tree has nothing at a registered child: nothing was
  /// there at the last look, or nobody looked yet.
  pub fn want(&self, key: DirKey) -> Option<Want> {
    let node = &self[key];
    if node.is_complete() {
      return Some(Want::Contents);
    }
    if node.registered_children == 0 {
      return Some(Want::Names);
    }
    let mut unknown = false;
    for child in self.children(key) {
      let child = &self[child];
      match child.kind {
        // A partial node can keep a child for a mark only.
        _ if !child.is_registered() => {}
        Kind::Dir(_) if !child.is_held() => {}
        Kind::Missing => unknown = true,
        // A file, or a file or a directory that the last look did not find any more.
        Kind::File(_) | Kind::Dir(_) => return Some(Want::Contents),
      }
    }
    if unknown { None } else { Some(Want::Names) }
  }

  /// The key of the live node in the slot `index`.
  fn key(&self, index: u32) -> DirKey {
    DirKey::new(index, linked(&self.slots, index).generation)
  }

  fn linked_mut(&mut self, index: u32) -> &mut Node {
    self.slots[index as usize].node_mut().expect("a link points to a live node")
  }

  /// The slot of the child `name` of the live node in the slot `parent`.
  fn find(&self, parent: u32, name: &OsStr) -> Option<u32> {
    let is_it = |&index: &u32| {
      let node = linked(&self.slots, index);
      node.parent == parent && name_of(&self.names, node) == name
    };
    self.index.find(child_hash(parent, name), is_it).copied()
  }

  /// Takes a node without children out of the tree. Its key never names a node again.
  fn remove(&mut self, key: DirKey) {
    let slot = &mut self.slots[key.index() as usize];
    let Some(generation) = slot.node().map(|node| node.generation.wrapping_add(1)) else { return };
    let Slot::Live(node) = mem::replace(slot, Slot::Free { generation }) else { return };
    self.free.push(key.index());

    if node.prev == NONE {
      self.linked_mut(node.parent).first_child = node.next;
    } else {
      self.linked_mut(node.prev).next = node.next;
    }
    if node.next != NONE {
      self.linked_mut(node.next).prev = node.prev;
    }

    let hash = child_hash(node.parent, name_of(&self.names, &node));
    if let Ok(entry) = self.index.find_entry(hash, |&index| index == key.index()) {
      entry.remove();
    }
    self.links.remove(&key);
    self.contents.remove(&key);
    self.holds.remove(&key);
    self.times.remove(&key);

    self.give_back();
    self.names.free(node.name_len);
    if self.names.is_half_dead(self.slots.len()) {
      let used = self.slots.iter_mut().filter_map(Slot::node_mut);
      self.names.pack(used.map(|node| (&mut node.name_at, node.name_len)));
    }
  }

  /// Gives back the memory that a tree needs no more after many of its nodes are gone.
  ///
  /// A key is an index into the arena, so a node never moves: only the end of the arena can
  /// go, as far as no node lives there. A block is made smaller only when less than half of it
  /// is in use, so that a tree that grows and shrinks by a little moves no memory. The table
  /// waits until an eighth is in use: it hashes every name it keeps once more.
  fn give_back(&mut self) {
    while let Some(&Slot::Free { generation }) = self.slots.last() {
      self.first_generation = self.first_generation.max(generation);
      self.slots.pop();
    }
    let slots = self.slots.len();
    if self.slots.capacity() > 2 * slots + 16 {
      self.slots.shrink_to(slots + slots / 8 + 16);
    }
    // More than half of the list is stale then.
    if self.free.len() > 2 * slots + 16 {
      self.free.retain(|&index| (index as usize) < slots);
      self.free.shrink_to(slots);
    }
    if self.index.capacity() > 8 * self.index.len() + 16 {
      let Self { slots, names, index, .. } = self;
      index.shrink_to(2 * index.len(), |&other| hash_of(slots, names, other));
    }
  }

  fn push_names(&self, index: u32, path: &mut PathBuf) {
    let node = linked(&self.slots, index);
    if node.parent != NONE {
      self.push_names(node.parent, path);
      path.push(name_of(&self.names, node));
    }
  }

  /// A child of the node in the slot `index` is registered now. That can make the node itself
  /// registered, and so on up to the root.
  fn count_registered(&mut self, mut index: u32) {
    loop {
      let node = self.linked_mut(index);
      let was_registered = node.is_registered();
      node.registered_children += 1;
      if was_registered || node.parent == NONE {
        return;
      }
      index = node.parent;
    }
  }

  /// A child of the node in the slot `index` is not registered any more. That can end the
  /// registration of the node itself, and so on up to the root.
  fn uncount_registered(&mut self, mut index: u32) {
    loop {
      let node = self.linked_mut(index);
      node.registered_children -= 1;
      if node.is_registered() || node.parent == NONE {
        return;
      }
      index = node.parent;
    }
  }
}

impl Index<DirKey> for Tree {
  type Output = Node;

  fn index(&self, key: DirKey) -> &Node {
    self.get(key).expect("the key names a live node")
  }
}

impl IndexMut<DirKey> for Tree {
  fn index_mut(&mut self, key: DirKey) -> &mut Node {
    self.get_mut(key).expect("the key names a live node")
  }
}

/// One place in the arena.
#[derive(Debug)]
enum Slot {
  Live(Node),
  /// The node that was here is gone. The next one gets this generation: it goes up with each
  /// node that is removed, so an old key never names the next node of the slot.
  Free {
    generation: u32,
  },
}

impl Slot {
  fn node(&self) -> Option<&Node> {
    match self {
      Self::Live(node) => Some(node),
      Self::Free { .. } => None,
    }
  }

  fn node_mut(&mut self) -> Option<&mut Node> {
    match self {
      Self::Live(node) => Some(node),
      Self::Free { .. } => None,
    }
  }
}

/// The node in a slot that a link or the table points to.
fn linked(slots: &[Slot], index: u32) -> &Node {
  slots[index as usize].node().expect("a link points to a live node")
}

/// The name of a live node.
fn name_of<'a>(names: &'a Names, node: &Node) -> &'a OsStr {
  names.get(node.name_at, node.name_len)
}

/// The hash under which the table files the live node in the slot `index`.
fn hash_of(slots: &[Slot], names: &Names, index: u32) -> u64 {
  let node = linked(slots, index);
  child_hash(node.parent, name_of(names, node))
}

/// The hash under which the table files the child `name` of the node `parent`.
///
/// The case of ASCII letters is left out of it: two names that differ in nothing else have one
/// hash, and so [`Tree::other_spelling`] finds them in the table. No ready hasher does that
/// without a copy of the name, so the bytes are mixed here: 16 at a time, as two numbers that
/// are multiplied, and the two halves of the product are folded into one.
#[expect(clippy::cast_possible_truncation, reason = "the two halves of a product are taken")]
fn child_hash(parent: u32, name: &OsStr) -> u64 {
  // The one bit that tells `a` from `A`, set in every byte. Bytes that are no letters change
  // too, which only means that a few more names share a hash.
  const CASE: u64 = u64::from_ne_bytes([0x20; 8]);
  // Digits of pi: two numbers with no pattern.
  const SEEDS: [u64; 2] = [0x243f_6a88_85a3_08d3, 0x1319_8a2e_0370_7344];
  let mix = |a: u64, b: u64| {
    let product = u128::from(a) * u128::from(b);
    (product as u64) ^ ((product >> 64) as u64)
  };
  let word = |bytes: &[u8]| u64::from_ne_bytes(*bytes.first_chunk().expect("eight bytes")) | CASE;
  let half =
    |bytes: &[u8]| u64::from(u32::from_ne_bytes(*bytes.first_chunk().expect("four bytes"))) | CASE;

  let mut rest = name.as_encoded_bytes();
  let mut hash = SEEDS[0] ^ u64::from(parent) ^ ((rest.len() as u64) << 32);
  while rest.len() > 16 {
    hash = mix(hash ^ word(rest), SEEDS[1] ^ word(&rest[8..]));
    rest = &rest[16..];
  }
  // What is left, 16 bytes at most, as two numbers. With fewer bytes the two overlap.
  let (first, last) = match *rest {
    [] => (0, 0),
    [one] => (u64::from(one) | CASE, 0),
    [one, two] => (u64::from(one) | CASE, u64::from(two) | CASE),
    [one, two, three] => {
      (u64::from(one) | CASE, u64::from(u16::from_ne_bytes([two, three])) | CASE)
    }
    _ if rest.len() < 8 => (half(rest), half(&rest[rest.len() - 4..])),
    _ => (word(rest), word(&rest[rest.len() - 8..])),
  };
  mix(hash ^ first, SEEDS[1] ^ last)
}

/// The longest list that is kept for its memory between two uses.
const KEPT: usize = 1024;

/// Empties a list for its next use. It keeps its memory, so that a flush for a few changes asks
/// for none. A long list is given back: a `watch` of 20,000 paths does not leave its lists
/// behind for ever.
pub(crate) fn emptied<T>(mut list: Vec<T>) -> Vec<T> {
  list.clear();
  if list.capacity() > KEPT { Vec::new() } else { list }
}

/// "Check this name at the next flush."
#[derive(Clone, Copy, Default, Debug)]
pub struct Mark {
  /// Something said that this very path changed. If a directory is there and it is still the
  /// same one, it is watched again and scanned deep: it can have been away and back.
  pub deep: bool,
  /// The backend saw a write to the file: it is an `Update`, also with an equal fingerprint.
  pub written: bool,
  /// Only a loss report asked for this check. The engine paces such checks.
  pub lost: bool,
}

impl Mark {
  /// Something said that this very path changed.
  pub const DEEP: Self = Self { deep: true, written: false, lost: false };
  /// The backend lost notifications of the directory at the path, or its watch.
  pub const LOST: Self = Self { deep: true, written: false, lost: true };
}

/// The work for the next flush: marks and queued scans, and the way from a hint to a mark.
impl Tree {
  /// Turns a hint of the backend into work for the next flush: a mark, as a rule. `false`: the
  /// hint left no work, because no watched path can see the change.
  pub fn hint(&mut self, hint: Hint<'_>) -> bool {
    match hint {
      Hint::Changed { dir, rel } => self.mark_below(dir, rel, false),
      Hint::Written { dir, rel } => self.mark_below(dir, rel, true),
      Hint::Lost => {
        self.lost = true;
        true
      }
    }
  }

  /// Marks the place to check for a change at `rel` below the node `dir`: the name itself, or
  /// the highest name on the way to it that the tree does not know as a directory. `false`:
  /// no watched path can see the change, or the key names no node.
  pub fn mark_below(&mut self, dir: DirKey, rel: &Path, written: bool) -> bool {
    let Some(node) = self.get(dir) else { return false };
    if !node.is_dir() {
      return self.mark_from_above(dir);
    }
    let mut parts = rel.components().peekable();
    if parts.peek().is_none() {
      // The directory itself: the backend lost its watch, or notifications of it. Its parent
      // checks it. Nothing checks the root.
      let has_parent = node.parent != NONE;
      if has_parent {
        self.mark(dir, Mark::LOST);
      }
      return has_parent;
    }
    // The walk is done for every hint, so it goes by slots: no key is made on the way.
    let mut at = dir.index();
    while let Some(part) = parts.next() {
      let Component::Normal(name) = part else { return false };
      let child = self.find(at, name);
      let below = child.map(|child| linked(&self.slots, child));
      if !below.is_some_and(Node::is_registered) && !linked(&self.slots, at).is_complete() {
        return self.other_spelling(at, name);
      }
      let last = parts.peek().is_none();
      if !last
        && let Some(child) = child
        && below.is_some_and(Node::is_dir)
      {
        at = child;
        continue;
      }
      let written = last && written;
      let child = match child {
        Some(child) => self.key(child),
        None => self.insert_child(self.key(at), name),
      };
      self.mark(child, Mark { deep: !written, written, lost: false });
      return true;
    }
    false
  }

  /// Adds a mark to a node. Two marks for one node become one.
  pub fn mark(&mut self, key: DirKey, mark: Mark) {
    if self[key].add_mark(mark) {
      self.marked.push(key);
    }
  }

  /// Takes the mark off a node. `None`: it has none, or the key names no node.
  pub fn take_mark(&mut self, key: DirKey) -> Option<Mark> {
    let node = self.get_mut(key)?;
    let mark = node.mark();
    node.set_mark(None);
    mark
  }

  /// Queues a scan of the directory `key`: once, with the deepest depth asked for.
  pub fn queue_scan(&mut self, key: DirKey, depth: Depth) {
    let Some(dir) = self[key].dir_mut() else { return };
    let queued = dir.scan.is_some();
    dir.scan = Some(dir.scan.map_or(depth, |old| old.max(depth)));
    if !queued {
      self.to_scan.push(key);
    }
  }

  /// Queues a scan of everything.
  pub fn scan_all(&mut self) {
    self.queue_scan(Self::ROOT, Depth::Deep);
  }

  /// Takes the queue into `scans`: every queued node, with its depth.
  pub fn take_scans(&mut self, scans: &mut Vec<(DirKey, Depth)>) {
    scans.clear();
    let mut queue = mem::take(&mut self.to_scan);
    let scan = |key| Some((key, self.get_mut(key)?.dir_mut()?.scan.take()?));
    scans.extend(queue.drain(..).filter_map(scan));
    self.to_scan = emptied(queue);
  }

  /// Takes the list of the marked nodes into `marks`, each with its level and the directory
  /// that holds it. The marks themselves stay on the nodes. A stale key is left out.
  ///
  /// The order is from the top down, so that a directory comes before the names in it, and the
  /// nodes of one directory come one after the other.
  pub fn take_marked(&mut self, marks: &mut Vec<(usize, DirKey, DirKey)>) {
    marks.clear();
    let mut marked = mem::take(&mut self.marked);
    for key in marked.drain(..) {
      if let Some(parent) = self.parent(key) {
        marks.push((self.level(key), parent, key));
      }
    }
    self.marked = emptied(marked);
    marks.sort_unstable_by_key(|&(level, parent, key)| (level, parent.index(), key.index()));
  }

  pub fn has_work(&self) -> bool {
    !self.marked.is_empty() || !self.to_scan.is_empty()
  }

  /// The watch or the error of the directory `key` was just written: if the timer has to look
  /// at the directory now, it goes on the list. Also while it is held: the hold can end.
  pub fn list_if_polled(&mut self, key: DirKey) {
    if matches!(self[key].kind, Kind::Dir(dir) if dir.is_polled()) {
      self.polled.push(key);
    }
  }

  /// The backend is gone: no directory has an OS watch any more, and the timer looks at all.
  pub fn poll_all(&mut self) {
    self.polled.clear();
    for (index, slot) in self.slots.iter_mut().enumerate() {
      let Some(node) = slot.node_mut() else { continue };
      let index = u32::try_from(index).expect("the arena holds fewer than 2^32 nodes");
      if let Kind::Dir(dir) = &mut node.kind {
        dir.watch = Watch::Polled;
        dir.asked = None;
        self.polled.push(DirKey::new(index, node.generation));
      }
    }
  }

  /// Puts the list of the timer in order: each directory that it has to look at once, in the
  /// order of the arena, and nothing else.
  pub fn tidy_polled(&mut self) {
    let mut polled = mem::take(&mut self.polled);
    polled.sort_unstable_by_key(|key| (key.index(), key.generation()));
    polled.dedup();
    let is_polled = |node: &Node| matches!(node.kind, Kind::Dir(dir) if dir.is_polled());
    polled.retain(|&key| self.get(key).is_some_and(is_polled));
    self.polled = polled;
  }

  /// No directory is at the node `dir`, so nothing can be checked inside it: the nearest
  /// directory above checks the name on the way to `dir`. `false`: there is none.
  fn mark_from_above(&mut self, dir: DirKey) -> bool {
    let mut child = dir;
    while let Some(parent) = self.parent(child) {
      if self[parent].is_dir() {
        self.mark(child, Mark::DEEP);
        return true;
      }
      child = parent;
    }
    false
  }

  /// A hint names the entry `name` of the partial node in the slot `at`, and no registered
  /// child has that name. No watched path can see the entry, unless the disk takes two
  /// spellings of a name for one: then a registered child can be the entry in another spelling.
  /// Each such child is checked, and deep, because the names below it can be spelled in
  /// another way too. `false`: there is no such child.
  fn other_spelling(&mut self, at: u32, name: &OsStr) -> bool {
    if !self.loose_spelling {
      return false;
    }
    let first_child = linked(&self.slots, at).first_child;
    let Self { slots, names, index, marked, .. } = self;
    let mut found = false;
    // Marks the node in the slot `child` if it can be the entry. Gives the child after it.
    let mut mark_if_it_can_be = |child: u32| {
      let node = slots[child as usize].node_mut().expect("a link points to a live node");
      let can_be =
        node.parent == at && node.is_registered() && can_be_one_name(name_of(names, node), name);
      if can_be {
        found = true;
        if node.add_mark(Mark::DEEP) {
          marked.push(DirKey::new(child, node.generation));
        }
      }
      node.next
    };
    if name.is_ascii() {
      // Only the same letters in another case can be this name, and the table files those
      // under the hash of this name. So a flood of names that nobody watches costs one more
      // lookup for each, however many registered children the node has.
      for &child in index.iter_hash(child_hash(at, name)) {
        mark_if_it_can_be(child);
      }
    } else {
      // Every name with a byte outside ASCII can be this name: each child is looked at.
      let mut child = first_child;
      while child != NONE {
        child = mark_if_it_can_be(child);
      }
    }
    found
  }
}

/// Whether `a` and `b` can be one name on a disk that takes two spellings of a name for one:
/// they are equal when the case of ASCII letters is left out. How such a disk folds other
/// letters is not known here, so two names that both have a byte outside ASCII can be one.
fn can_be_one_name(a: &OsStr, b: &OsStr) -> bool {
  let (a, b) = (a.as_encoded_bytes(), b.as_encoded_bytes());
  a.eq_ignore_ascii_case(b) || (!a.is_ascii() && !b.is_ascii())
}

/// What the tree keeps for a file whose fingerprint cannot be trusted at its next check: the
/// file was young when it was stored, or every content is compared.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Content {
  /// This hash of the content decides.
  Hash(u64),
  /// There is no hash (the file is too big or not readable): an equal fingerprint counts as
  /// a change.
  Unknown,
}

/// When a directory changed last, in nanoseconds since 1970. A rename of the directory itself moves `changed` alone; an
/// entry that comes or goes in it moves both.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DirTimes {
  /// The change time: of what the directory holds, and of the entry that it is.
  pub changed: i64,
  /// The modification time: of what it holds.
  pub modified: i64,
}

impl DirTimes {
  pub fn of(meta: &Meta) -> Self {
    Self { changed: nanos(meta.changed), modified: nanos(meta.modified) }
  }
}

/// What only a few nodes have, kept beside the arena: links, contents, times and holds.
impl Tree {
  /// A hash of the real path behind the node, if it was a symbolic link at its last check.
  pub fn link(&self, key: DirKey) -> Option<u64> {
    self.links.get(&key).copied()
  }

  pub fn set_link(&mut self, key: DirKey, link: Option<u64>) {
    match link {
      Some(real_place) => self.links.insert(key, real_place),
      None => self.links.remove(&key),
    };
  }

  pub fn links(&self) -> impl Iterator<Item = DirKey> + '_ {
    self.links.keys().copied()
  }

  /// What is kept for the file at the node when its fingerprint is not trusted at the next
  /// check. `None`: an equal fingerprint means "not changed".
  pub fn content(&self, key: DirKey) -> Option<Content> {
    self.contents.get(&key).copied()
  }

  pub fn set_content(&mut self, key: DirKey, content: Option<Content>) {
    if let Some(content) = content {
      self.contents.insert(key, content);
    } else if !self.contents.is_empty() {
      // Most of the time no file is young, and a check has nothing to take away.
      self.contents.remove(&key);
    }
  }

  /// Keeps `times` for the directory at the node, or nothing. Gives what was kept before.
  pub fn set_times(&mut self, key: DirKey, times: Option<DirTimes>) -> Option<DirTimes> {
    match times {
      Some(times) => self.times.insert(key, times),
      // Most of the time nothing is kept for any node, and a check has nothing to take away.
      None if self.times.is_empty() => None,
      None => self.times.remove(&key),
    }
  }

  /// Holds the node: a look found nothing where the tree has a file or a directory. Until
  /// the second look, at `until` or later, the node and everything below it stay as they are.
  pub fn hold(&mut self, key: DirKey, until: Instant) {
    self[key].set_held(true);
    self.holds.insert(key, until);
  }

  pub fn unhold(&mut self, key: DirKey) {
    if let Some(node) = self.get_mut(key)
      && node.is_held()
    {
      node.set_held(false);
      self.holds.remove(&key);
    }
  }

  pub fn held_until(&self, key: DirKey) -> Option<Instant> {
    self.holds.get(&key).copied()
  }

  pub fn holds(&self) -> impl Iterator<Item = (DirKey, Instant)> + '_ {
    self.holds.iter().map(|(key, until)| (*key, *until))
  }

  /// Whether a node above `key` is held. Then nothing is known about what is at the node
  /// now, and nothing is done there before the node above had its second look.
  pub fn frozen(&self, key: DirKey) -> bool {
    if self.holds.is_empty() {
      return false;
    }
    let mut parent = self.get(key).map_or(NONE, |node| node.parent);
    while parent != NONE {
      let node = linked(&self.slots, parent);
      if node.is_held() {
        return true;
      }
      parent = node.parent;
    }
    false
  }

  /// For the tests: checks the links, the table and the counts. The error names the first
  /// thing that is wrong.
  pub fn check_links(&self) -> Result<(), String> {
    let mut reached = 1;
    for (key, node) in self.nodes() {
      let fail = |what: &str| Err(format!("{}: {what}", self.path(key).display()));
      if (node.parent == NONE) != (key == Self::ROOT) {
        return fail("only the root has no parent");
      }
      if node.parent != NONE && self.child(self.key(node.parent), self.name(key)) != Some(key) {
        return fail("the table does not find the node");
      }
      let (mut registered, mut before) = (0, NONE);
      for child in self.children(key) {
        let below = &self[child];
        if below.parent != key.index() || below.prev != before {
          return fail("the list of children is broken");
        }
        registered += u32::from(below.is_registered());
        before = child.index();
        reached += 1;
      }
      if registered != node.registered_children {
        return fail("counts its registered children wrong");
      }
      if self.links.contains_key(&key) && !node.is_registered() {
        return fail("is a link but is not registered");
      }
      if node.is_held() != self.holds.contains_key(&key) {
        return fail("is held, or has a time for its second look, but not both");
      }
    }
    let kept = self.links.keys().chain(self.contents.keys()).chain(self.holds.keys());
    if !kept.chain(self.times.keys()).all(|key| self.get(*key).is_some()) {
      return Err("something is kept for a node that is gone".to_owned());
    }
    if !self.times.keys().all(|key| matches!(self[*key].kind, Kind::Dir(_))) {
      return Err("the times of a directory are kept for a node that has none".to_owned());
    }
    if !self.contents.keys().all(|key| matches!(self[*key].kind, Kind::File(_))) {
      return Err("the content of a file is kept for a node that has no file".to_owned());
    }
    let live = self.nodes().count();
    if reached != live || self.index.len() + 1 != live {
      return Err("a node is in no list of children, or not in the table".to_owned());
    }
    let free = self.free.iter().filter(|&&index| (index as usize) < self.slots.len()).count();
    if free + live != self.slots.len()
      || self.slots.last().is_some_and(|last| last.node().is_none())
    {
      return Err("a free slot is not on the list once, or the arena ends with one".to_owned());
    }
    Ok(())
  }
}

#[cfg(all(test, unix))]
mod tests {
  use std::{
    ffi::OsStr,
    path::Path,
    time::{Duration, UNIX_EPOCH},
  };

  use super::{KEPT, Tree, can_be_one_name, child_hash, emptied, nanos};
  use crate::backend::DirKey;

  /// The names are in one buffer. When many nodes go, the buffer is packed, and every node that
  /// stays still has its name and is still found by it.
  #[test]
  fn the_names_are_packed_when_many_nodes_go() {
    let mut tree = Tree::new();
    let top = tree.insert_child(Tree::ROOT, OsStr::new("/"));
    let name = |i: usize| format!("module-{i:04}.js");
    let keys: Vec<DirKey> =
      (0..1000).map(|i| tree.insert_child(top, OsStr::new(&name(i)))).collect();
    let full = tree.names.len();
    assert_eq!(full, 1 + 1000 * name(0).len());

    // Three of four nodes go: a node with nothing at its path that nothing needs is freed.
    let stays = |i: usize| i.is_multiple_of(4);
    for (i, key) in keys.iter().enumerate() {
      if !stays(i) {
        tree.prune(*key);
      }
    }
    assert!(tree.names.len() <= full / 2, "{} of {full} bytes", tree.names.len());
    tree.check_links().unwrap();
    for (i, key) in keys.iter().enumerate() {
      let found = tree.child(top, OsStr::new(&name(i)));
      if stays(i) {
        assert_eq!(found, Some(*key));
        assert_eq!(tree.name(*key), OsStr::new(&name(i)));
        assert_eq!(tree.path(*key), Path::new("/").join(name(i)));
      } else {
        assert_eq!(found, None);
      }
    }

    // New names go on behind the packed ones.
    let new = tree.insert_child(top, OsStr::new("new.js"));
    assert_eq!(tree.name(new), OsStr::new("new.js"));
    assert_eq!(tree.name(top), OsStr::new("/"));
    tree.check_links().unwrap();
  }

  /// When the nodes go, the arena, the list of free slots and the table get small again, and
  /// an old key does not name a node that comes later.
  #[test]
  fn the_memory_is_given_back_when_the_nodes_go() {
    let mut tree = Tree::new();
    let top = tree.insert_child(Tree::ROOT, OsStr::new("/"));
    let keys: Vec<DirKey> =
      (0..10_000).map(|i| tree.insert_child(top, OsStr::new(&i.to_string()))).collect();
    let (last, others) = keys.split_last().unwrap();
    let table = tree.index.capacity();

    // A node lives at the end of the arena: no slot can be cut off, and each free one is listed.
    for key in others {
      tree.prune(*key);
    }
    assert_eq!((tree.slots.len(), tree.free.len()), (keys.len() + 2, others.len()));
    assert!(tree.index.capacity() < table / 100);
    tree.check_links().unwrap();

    // The end is free now, and so is everything down to the top directory.
    tree.prune(*last);
    assert_eq!((tree.slots.len(), tree.free.len()), (2, 0));
    assert!(tree.slots.capacity() < 100 && tree.free.capacity() < 100);
    tree.check_links().unwrap();

    // A new node gets the first slot that was cut off, under a generation it never had.
    let new = tree.insert_child(top, OsStr::new("new"));
    assert_eq!(new.index(), keys[0].index());
    assert!(keys.iter().all(|key| tree.get(*key).is_none()));
    tree.check_links().unwrap();
  }

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

  #[test]
  fn two_names_that_can_be_one_name() {
    let same = |a: &str, b: &str| can_be_one_name(OsStr::new(a), OsStr::new(b));
    assert!(same("App.js", "App.js"));
    assert!(same("App.js", "aPP.JS"));
    assert!(!same("App.js", "App.jsx"));
    assert!(!same("App.js", "Abb.js"));
    assert!(!same("a-b", "a_b"));
    // Letters outside ASCII: the rules of the disk are not known, so any two such names can
    // be one. `e` with an accent as one character and as two, and in upper case.
    assert!(same("caf\u{e9}.js", "cafe\u{301}.js"));
    assert!(same("caf\u{e9}.js", "CAF\u{c9}.JS"));
    assert!(same("\u{e9}", "\u{4e2d}"));
    // One of the two is plain ASCII: the bytes decide.
    assert!(!same("cafe.js", "caf\u{e9}.js"));
    assert!(!same("caf\u{e9}.js", "cafe.js"));
  }

  /// A time is kept as one number of nanoseconds. A time that the number cannot hold becomes
  /// the nearest one that it can.
  #[test]
  fn a_time_as_one_number() {
    assert_eq!(nanos(UNIX_EPOCH), 0);
    assert_eq!(nanos(UNIX_EPOCH + Duration::new(2, 3)), 2_000_000_003);
    assert_eq!(nanos(UNIX_EPOCH - Duration::new(2, 3)), -2_000_000_003);
    // The year 2262 is the last one that fits.
    let far = Duration::from_secs(400 * 365 * 24 * 3600);
    assert_eq!(nanos(UNIX_EPOCH + far), i64::MAX);
    assert_eq!(nanos(UNIX_EPOCH - far), -i64::MAX);
  }

  /// A list that is used again keeps its memory, but not when it is long.
  #[test]
  fn a_long_list_is_given_back() {
    let short = emptied(vec![1; KEPT]);
    assert!(short.is_empty() && short.capacity() == KEPT);
    let long = emptied(vec![1; KEPT + 1]);
    assert_eq!(long.capacity(), 0);
  }
}
