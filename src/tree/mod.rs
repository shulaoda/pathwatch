//! The data of the engine: a tree of the paths that matter, and the work that waits in it.
//! Nothing here reads the disk. One node is one name. All nodes live in one arena, with a table
//! that finds a child by its name; the names are in one buffer (`names.rs`), and the small
//! facts of a node are bits (`flags.rs`).

mod flags;
mod names;
mod slots;

use std::{
  ffi::OsStr,
  iter,
  iter::Peekable,
  mem,
  ops::{Index, IndexMut},
  path::{Component, Components, MAIN_SEPARATOR_STR, Path, PathBuf, is_separator},
  time::{Instant, SystemTime},
};

use hashbrown::HashTable;
use rustc_hash::FxHashMap;

use self::{flags::Flags, names::Names, slots::Slots};
use crate::{
  backend::{DirKey, Hint, Want},
  fs::{DirId, Meta},
  path::split_top,
};

/// In a link between nodes: there is no such node.
const NONE: u32 = u32::MAX;

/// One name the tree knows. It stays while it is registered, something is at its path, a mark
/// waits on it, or a `Delete` is still to be told; [`Tree::prune`] frees it otherwise.
#[derive(Debug)]
pub struct Node {
  pub kind: Kind,

  generation: u32,

  name_at: u32,
  name_len: u16,

  parent: u32,
  next: u32,
  prev: u32,
  first_child: u32,
  registered_children: u32,

  flags: Flags,
}

// One node for each path the tree knows: it has to stay 64 bytes, with no gap.
#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<Node>() == 64 && size_of::<Slot>() == 64);

impl Node {
  fn new(generation: u32, parent: u32, (name_at, name_len): (u32, u16)) -> Self {
    Self {
      kind: Kind::Missing,
      generation,
      name_at,
      name_len,
      parent,
      next: NONE,
      prev: NONE,
      first_child: NONE,
      registered_children: 0,
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

  /// Whether the path is a watched path or leads to one. Such a node stays while nothing is at it.
  pub fn is_registered(&self) -> bool {
    self.is_path() || self.is_ancestor()
  }

  /// Whether the node is an entry of its directory as far as the tree knows: something is at it,
  /// no second look waits, and the directory lists it under this spelling.
  pub fn is_entry(&self) -> bool {
    !matches!(self.kind, Kind::Missing) && !self.is_held() && !self.is_unlisted()
  }

  /// Adds a mark to the node. Two marks for one node become one. `true`: it had none.
  fn add_mark(&mut self, mark: Mark) -> bool {
    let old = self.mark();
    self.set_mark(Some(old.map_or(mark, |old| Mark {
      deep: old.deep || mark.deep,
      written: old.written || mark.written,
      // A check that anything else asked for does not wait with the loss reports.
      lost: old.lost && mark.lost,
      since: None,
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

/// What tells one state of a file from the next without reading it.
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

/// Nanoseconds since 1970. A time that does not fit (before 1678, after 2262) becomes the nearest.
pub(crate) fn nanos(time: SystemTime) -> i64 {
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
  /// A scan or the watch failed and was reported once; every poll scans the node until it is over.
  pub has_error: bool,
  /// The node is queued for a scan.
  pub scan: Option<Depth>,
  /// An entry that the listing named was not there when it was looked at: the next check lists
  /// the directory again, since it can have been away and back.
  pub relist: bool,
}

impl Dir {
  /// A directory that was just found. The backend was not asked yet.
  pub fn new(id: DirId, complete: bool) -> Self {
    let (asked, scan) = (None, None);
    let (has_error, relist) = (false, false);
    Self { id, complete, asked, watch: Watch::None, has_error, scan, relist }
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
  /// Just found, the backend not asked yet. Also the answer for a node that is no directory.
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
  /// Only the names are compared with a listing: a node that it names is not looked at. Without
  /// a listing this is a shallow scan.
  Names,
  /// Every name of the directory is checked; a directory below that is still the same one is left.
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
  /// The call registered the node: before, it was no watched path and led to none.
  Registered,
}

/// The arena of the nodes, the table that finds a child by its name, and the work that waits.
#[derive(Debug)]
pub struct Tree {
  /// The arena. Its last slot is never free: a free end is cut off ([`Tree::give_back`]).
  slots: Slots,
  /// The free slots. An index can be stale: its slot was cut off the end of the arena.
  free: Vec<u32>,
  /// The generation of a node in a new slot: above that of every slot that was cut off, so that an
  /// old key never names a node again.
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
  /// The files whose modification time is far ahead of the clock, each with the time of the
  /// first look that found the fingerprint it has now.
  ahead: FxHashMap<DirKey, Instant>,
  /// The registered nodes whose names are not plain, by the slot of their parent: the table does
  /// not find them under another spelling, so a hint looks here ([`Tree::others_meant`]).
  not_plain: FxHashMap<u32, Vec<u32>>,
  /// The nodes with a mark, each with the time of the hint that made it, if a hint did: the
  /// delete delay counts from it. A key can be stale.
  pub marked: Vec<(DirKey, Option<Instant>)>,
  /// The time of the hints that are being turned into marks: the engine sets it meanwhile.
  pub hint_time: Option<Instant>,
  /// The nodes that are queued for a scan. A key can be stale or be there twice.
  pub to_scan: Vec<DirKey>,
  /// The directories the timer looks at ([`Dir::is_polled`]); the root is always on it. Before
  /// [`Tree::tidy_polled`] a key can be stale or there twice.
  pub polled: Vec<DirKey>,
  /// Notifications were lost: everything has to be scanned. The engine says when.
  pub lost: bool,
  /// Every disk takes two spellings of a name for one (`Options::loose_spelling`).
  pub loose_spelling: bool,
  /// A `Written` hint is a write to the one entry that the name and its twins (the registered
  /// names that differ in the case of ASCII letters only) all are: every disk is loose, and the
  /// backend says "written" for a write and nothing else (`Backend::written_is_a_write`).
  pub twins_share_writes: bool,
}

/// The nodes: finding them, adding and freeing them, their paths, and which are watched paths.
impl Tree {
  /// The one node above the top directories. It has no path, is never listed or given to the
  /// backend, and is always polled: so every top directory has a parent that checks it.
  pub const ROOT: DirKey = DirKey::new(0, 0);

  pub fn new() -> Self {
    let mut names = Names::default();
    let mut root = Node::new(0, NONE, names.add(OsStr::new("")));
    root.kind = Kind::Dir(Dir { watch: Watch::Polled, ..Dir::new(DirId::default(), false) });
    Self {
      slots: {
        let mut slots = Slots::default();
        slots.push(Slot::Live(root));
        slots
      },
      free: Vec::new(),
      first_generation: 0,
      names,
      index: HashTable::new(),
      links: FxHashMap::default(),
      contents: FxHashMap::default(),
      holds: FxHashMap::default(),
      ahead: FxHashMap::default(),
      not_plain: FxHashMap::default(),
      marked: Vec::new(),
      hint_time: None,
      to_scan: Vec::new(),
      polled: vec![Self::ROOT],
      lost: false,
      loose_spelling: false,
      twins_share_writes: false,
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

  /// The next child in the list of the parent: a loop can change the tree on the way, as long as it
  /// frees no node.
  pub fn next_sibling(&self, child: DirKey) -> Option<DirKey> {
    let next = self.get(child)?.next;
    (next != NONE).then(|| self.key(next))
  }

  /// Adds a child `name` to `parent`. The caller has checked with [`Tree::child`] that there is
  /// none.
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
      self.slots.push(Slot::Live(Node::new(self.first_generation, parent, name_at)));
      index
    };

    let first = mem::replace(&mut self.linked_mut(parent).first_child, index);
    self.linked_mut(index).next = first;
    if first != NONE {
      self.linked_mut(first).prev = index;
    }

    let Self { slots, names, index: table, .. } = self;
    table.insert_unique(child_hash(parent, name), index, |&other| hash_of(slots, names, other));
    self.key(index)
  }

  /// Frees the node if nothing needs it: nothing at its path, not registered, no mark, not told.
  pub fn prune(&mut self, key: DirKey) {
    let Some(node) = self.get(key) else { return };
    let nothing = matches!(node.kind, Kind::Missing);
    let needed = !nothing || node.is_registered() || node.is_marked() || node.told();
    // A node that nothing needs has no children; the test keeps a mistake elsewhere from leaving
    // a child without its parent.
    if !needed && node.first_child == NONE {
      self.remove(key);
    }
  }

  /// Writes the path of a node into `path`. The root has the empty path.
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
    if !was_registered {
      self.count_registered(key.index());
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
    if !node.is_registered() {
      self.uncount_registered(key.index());
    }
    true
  }

  /// What the directory asks from the backend, as far as the tree can tell: `Contents` when the
  /// node is complete or a registered child has a file at it, `Names` when every registered child
  /// is a directory. `None`: the disk has to say, since the tree has nothing at a registered child.
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
        // A file, or a file or a directory that the last look did not find.
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
    self.ahead.remove(&key);

    self.give_back();
    self.names.free(node.name_len);
    if self.names.is_half_dead(self.slots.len()) {
      let used = self.slots.iter_mut().filter_map(Slot::node_mut);
      self.names.pack(used.map(|node| (&mut node.name_at, node.name_len)));
    }
  }

  /// Gives memory back after many nodes went. A key is an index, so a node never moves: only a free
  /// end of the arena can go. A block shrinks only when less than half is in use, so a tree that
  /// grows and shrinks by a little moves nothing; the table waits for an eighth, since it rehashes.
  fn give_back(&mut self) {
    while let Some(&Slot::Free { generation }) = self.slots.last() {
      self.first_generation = self.first_generation.max(generation);
      self.slots.pop();
    }
    self.slots.shrink();
    let slots = self.slots.len();
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
      push_name(path, name_of(&self.names, node));
    }
  }

  /// The node in slot `child` is registered now, and so is every node above it that was not.
  fn count_registered(&mut self, mut child: u32) {
    loop {
      self.list_not_plain(child, true);
      let parent = linked(&self.slots, child).parent;
      if parent == NONE {
        return;
      }
      let node = self.linked_mut(parent);
      let was_registered = node.is_registered();
      node.registered_children += 1;
      if was_registered {
        return;
      }
      child = parent;
    }
  }

  /// The node in slot `child` is not registered any more, nor is a node above that only led to it.
  fn uncount_registered(&mut self, mut child: u32) {
    loop {
      self.list_not_plain(child, false);
      let parent = linked(&self.slots, child).parent;
      if parent == NONE {
        return;
      }
      let node = self.linked_mut(parent);
      node.registered_children -= 1;
      if node.is_registered() {
        return;
      }
      child = parent;
    }
  }

  /// Puts the node in slot `child` on the `not_plain` list of its parent, or takes it off.
  fn list_not_plain(&mut self, child: u32, registered: bool) {
    let node = linked(&self.slots, child);
    if is_plain(name_of(&self.names, node)) {
      return;
    }
    let parent = node.parent;
    if registered {
      self.not_plain.entry(parent).or_default().push(child);
    } else if let Some(list) = self.not_plain.get_mut(&parent) {
      list.retain(|&other| other != child);
      if list.is_empty() {
        self.not_plain.remove(&parent);
      }
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
  /// The node that was here is gone. The next one gets this generation, one above the old, so an
  /// old key never names it.
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
fn linked(slots: &Slots, index: u32) -> &Node {
  slots[index as usize].node().expect("a link points to a live node")
}

fn name_of<'a>(names: &'a Names, node: &Node) -> &'a OsStr {
  names.get(node.name_at, node.name_len)
}

/// The hash under which the table files the live node in the slot `index`.
fn hash_of(slots: &Slots, names: &Names, index: u32) -> u64 {
  let node = linked(slots, index);
  child_hash(node.parent, name_of(names, node))
}

/// The hash under which the table files the child `name` of `parent`. The case of ASCII letters
/// is left out, so that names that differ only there share a hash and [`Tree::spellings`] finds
/// them in one walk. No ready hasher does that without a copy of the name, so the bytes are
/// mixed here, 16 at a time.
#[expect(clippy::cast_possible_truncation, reason = "the two halves of a product are taken")]
fn child_hash(parent: u32, name: &OsStr) -> u64 {
  // The bit that tells `a` from `A`, set in every byte; other bytes only share a hash more often.
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

/// The most bytes of a list that are kept between two uses.
pub(crate) const KEPT: usize = 16 * 1024;

/// Appends `name` to `path` as one more name, by its text: `PathBuf::push` would start the path
/// anew for a name that reads as a drive or a root (`C:`, `\\a`), which a disk that another system
/// filled can list on Windows. `pop` takes such a name off again; a name with a separator in it
/// never reaches the tree (`fs::read_dir` leaves it out).
pub(crate) fn push_name(path: &mut PathBuf, name: &OsStr) {
  let text = path.as_mut_os_string();
  let last = text.as_encoded_bytes().last().copied();
  if !text.is_empty() && !last.is_some_and(|byte| byte.is_ascii() && is_separator(char::from(byte)))
  {
    text.push(MAIN_SEPARATOR_STR);
  }
  text.push(name);
}

/// Empties a list for its next use and keeps its memory, unless it is long: a `watch` of 20,000
/// paths must not leave its lists behind for ever.
pub(crate) fn emptied<T>(mut list: Vec<T>) -> Vec<T> {
  list.clear();
  if list.capacity() * size_of::<T>() > KEPT { Vec::new() } else { list }
}

/// "Check this name at the next flush."
#[derive(Clone, Copy, Default, Debug)]
pub struct Mark {
  /// This very path changed: a directory that is still the same one is watched again and scanned
  /// deep, since it can have been away and back.
  pub deep: bool,
  /// The backend saw a write to the file: it is an `Update`, also with an equal fingerprint.
  pub written: bool,
  /// Only a loss report asked for this check. The engine paces such checks.
  pub lost: bool,
  /// The time of the hint that asked for the check: a `Delete` counts its delay from it. `None`:
  /// the engine itself asked, and the delay counts from the look.
  pub since: Option<Instant>,
}

impl Mark {
  /// Something said that this very path changed.
  pub const DEEP: Self = Self { deep: true, written: false, lost: false, since: None };
  /// The backend lost notifications of the directory at the path, or its watch.
  pub const LOST: Self = Self { deep: true, written: false, lost: true, since: None };
}

/// The work for the next flush: marks and queued scans, and the way from a hint to a mark.
impl Tree {
  /// Turns a hint into work for the next flush. `false`: no watched path can see the change.
  pub fn hint(&mut self, hint: Hint<'_>) -> bool {
    match hint {
      Hint::Changed { dir, rel } => self.mark_below(dir, rel, false),
      Hint::Written { dir, rel } => self.mark_below(dir, rel, true),
      Hint::Lost => {
        self.lost = true;
        true
      }
      // A node at the path checks it, and finds another directory there by its identity.
      Hint::Mount { path } => {
        let Some((top, rest)) = split_top(path) else { return false };
        self.child(Self::ROOT, top).is_some_and(|top| self.mark_below(top, rest, false))
      }
    }
  }

  /// Marks the place to check for a change at `rel` below `dir`: the name itself, or the highest
  /// name on the way that the tree does not know as a directory. `false`: nothing to do.
  pub fn mark_below(&mut self, dir: DirKey, rel: &Path, written: bool) -> bool {
    let Some(node) = self.get(dir) else { return false };
    if !node.is_dir() {
      return self.mark_from_above(dir);
    }
    if rel.as_os_str().is_empty() {
      // The directory itself: a loss report. Its parent checks it; nothing checks the root.
      let has_parent = node.parent != NONE;
      if has_parent {
        self.mark(dir, Mark::LOST);
      }
      return has_parent;
    }
    self.mark_path(dir.index(), rel.components().peekable(), written)
  }

  /// The walk of `mark_below`, by slots since it runs for every hint. A name of the hint means the
  /// child with that name and every child the OS can take for the same name (design.md 5.2).
  fn mark_path(&mut self, mut at: u32, mut parts: Peekable<Components<'_>>, written: bool) -> bool {
    let mut found = false;
    while let Some(part) = parts.next() {
      let Component::Normal(name) = part else { return found };
      let (child, has_other) = self.spellings(at, name);
      let below = child.map(|child| linked(&self.slots, child));
      let is_dir = below.is_some_and(Node::is_dir);
      let parent = linked(&self.slots, at);
      let is_complete = parent.is_complete();
      let is_seen = below.is_some_and(Node::is_registered) || is_complete;
      // The table finds the other spellings of a plain name only; the registered ones that are not
      // plain are on a list, and a listing shows the rest.
      let not_in_table = self.loose_spelling
        && (!self.not_plain.is_empty() || (is_complete && !below.is_some_and(Node::is_entry)));
      if has_other || (not_in_table && !is_plain(name)) {
        found |= self.mark_others(at, name, child, &parts, written);
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
      let written = last && written;
      let child = match child {
        Some(child) => self.key(child),
        None => self.insert_child(self.key(at), name),
      };
      self.mark(child, Mark { deep: !written, written, lost: false, since: None });
      return true;
    }
    found
  }

  /// The walk of `mark_path` for the other children that `name` can mean. A twin of the name
  /// (`twins_share_writes`) is the entry that was written: its mark says `written` as the hint
  /// does. Every other child is a guess (a name that is not plain), and its look decides.
  #[cold]
  fn mark_others(
    &mut self,
    at: u32,
    name: &OsStr,
    exact: Option<u32>,
    rest: &Peekable<Components<'_>>,
    written: bool,
  ) -> bool {
    let last = rest.clone().peek().is_none();
    let mut found = false;
    let parent = linked(&self.slots, at);
    let is_entry = exact.is_some_and(|exact| linked(&self.slots, exact).is_entry());
    let one_entry = written && self.twins_share_writes;
    if self.loose_spelling && parent.is_complete() && !is_entry && !is_plain(name) {
      // A name that is not plain can be another spelling of each child that is not plain.
      self.queue_scan(self.key(at), Depth::Names);
    }
    for other in self.others_meant(at, name, exact) {
      let node = linked(&self.slots, other);
      let is_dir = node.is_dir();
      let written = one_entry && name_of(&self.names, node).eq_ignore_ascii_case(name);
      if !last && is_dir {
        found |= self.mark_path(other, rest.clone(), written);
      } else {
        let written = written && last;
        self.mark(self.key(other), Mark { deep: !written, written, lost: false, since: None });
        found = true;
      }
    }
    found
  }

  /// Adds a mark to a node. Two marks for one node become one. The time of the mark is the one
  /// it carries, else the time of the hint that is being turned into marks, if one is.
  pub fn mark(&mut self, key: DirKey, mark: Mark) {
    let since = mark.since.or(self.hint_time);
    if self[key].add_mark(mark) {
      self.marked.push((key, since));
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

  /// Takes the marked nodes into `marks`, each with its level, its directory and the time of its
  /// hint, from the top down and one directory after the other. The marks stay on the nodes; a
  /// stale key is left out.
  pub fn take_marked(&mut self, marks: &mut Vec<(usize, DirKey, DirKey, Option<Instant>)>) {
    marks.clear();
    let mut marked = mem::take(&mut self.marked);
    for (key, since) in marked.drain(..) {
      if let Some(parent) = self.parent(key) {
        marks.push((self.level(key), parent, key, since));
      }
    }
    self.marked = emptied(marked);
    marks.sort_unstable_by_key(|&(level, parent, key, _)| (level, parent.index(), key.index()));
  }

  pub fn has_work(&self) -> bool {
    !self.marked.is_empty() || !self.to_scan.is_empty()
  }

  /// Puts the directory on the list of the timer if it has to be polled now. Also while held.
  /// A directory is listed more than once (the answer of the backend, the end of its scan), and
  /// a stale key stays until a tidy: the list is tidied when it outgrows the arena twice over,
  /// so a `watch` and `unwatch` of ever new paths cannot grow it without bound.
  pub fn list_if_polled(&mut self, key: DirKey) {
    if matches!(self[key].kind, Kind::Dir(dir) if dir.is_polled()) {
      self.polled.push(key);
      if self.polled.len() > 2 * self.slots.len() + 16 {
        self.tidy_polled();
      }
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

  /// Puts the list of the timer in order: each polled directory once, and nothing else. A long
  /// list that shrank gives its memory back.
  pub fn tidy_polled(&mut self) {
    let mut polled = mem::take(&mut self.polled);
    polled.sort_unstable_by_key(|key| (key.index(), key.generation()));
    polled.dedup();
    let is_polled = |node: &Node| matches!(node.kind, Kind::Dir(dir) if dir.is_polled());
    polled.retain(|&key| self.get(key).is_some_and(is_polled));
    if polled.capacity() > 2 * polled.len() + 16 {
      polled.shrink_to(polled.len() + polled.len() / 8 + 16);
    }
    self.polled = polled;
  }

  /// No directory is at `dir`: the nearest directory above checks the name on the way to it.
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

  /// The child `name` of slot `parent`, and whether another child differs from it only in the case
  /// of ASCII letters: one walk of the table finds both, since such names share a hash.
  fn spellings(&self, parent: u32, name: &OsStr) -> (Option<u32>, bool) {
    let (mut child, mut has_other) = (None, false);
    for &index in self.index.iter_hash(child_hash(parent, name)) {
      let node = linked(&self.slots, index);
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
  fn others_meant(&self, at: u32, name: &OsStr, exact: Option<u32>) -> Vec<u32> {
    let parent = linked(&self.slots, at);
    let loose = self.loose_spelling;
    let any_child = parent.is_complete();
    let is_meant = |&child: &u32| {
      let node = linked(&self.slots, child);
      Some(child) != exact
        && node.parent == at
        && (any_child || node.is_registered())
        && can_be_one_name(name_of(&self.names, node), name)
    };
    if is_plain(name) {
      self.index.iter_hash(child_hash(at, name)).copied().filter(is_meant).collect()
    } else if loose {
      let listed = self.not_plain.get(&at).map_or(&[][..], Vec::as_slice);
      let not_plain = |&child: &u32| {
        Some(child) != exact && !is_plain(name_of(&self.names, linked(&self.slots, child)))
      };
      listed.iter().copied().filter(not_plain).collect()
    } else {
      Vec::new()
    }
  }

  /// Whether the name of the node `key` is plain: the table finds every other spelling of it.
  pub fn is_plain(&self, key: DirKey) -> bool {
    is_plain(self.name(key))
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
    !is_plain(self.name(key)) || self.has_twin(key)
  }

  /// Whether the table has another node of the same directory that can be the same name as
  /// the node `key`.
  pub fn has_twin(&self, key: DirKey) -> bool {
    self.twins(key).next().is_some()
  }

  /// The nodes that `has_twin` speaks of, if they have a mark.
  pub fn marked_twins(&self, key: DirKey) -> Vec<DirKey> {
    let marked = self.twins(key).filter(|&twin| linked(&self.slots, twin).is_marked());
    marked.map(|twin| self.key(twin)).collect()
  }

  fn twins(&self, key: DirKey) -> impl Iterator<Item = u32> + '_ {
    let node = self.get(key);
    let hash = node.map_or(0, |node| child_hash(node.parent, name_of(&self.names, node)));
    self.index.iter_hash(hash).copied().filter(move |&other| {
      let other_node = linked(&self.slots, other);
      node.is_some_and(|node| {
        other != key.index()
          && other_node.parent == node.parent
          && can_be_one_name(name_of(&self.names, other_node), name_of(&self.names, node))
      })
    })
  }
}

/// Whether `a` and `b` can be one name on a loose disk: equal but for the case of ASCII letters,
/// or neither is plain.
fn can_be_one_name(a: &OsStr, b: &OsStr) -> bool {
  a.eq_ignore_ascii_case(b) || (!is_plain(a) && !is_plain(b))
}

/// Whether every other spelling of `name` is known: only ASCII and Han ideographs, which have no
/// case and no second form. The few characters that a disk folds to plain ones (`ß`, the Kelvin
/// sign, ligatures, compatibility ideographs) are not covered: design.md 9.
fn is_plain(name: &OsStr) -> bool {
  let is_plain = |letter: char| {
    letter.is_ascii()
      || matches!(letter, '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}')
      || matches!(letter, '\u{20000}'..='\u{2F7FF}' | '\u{30000}'..='\u{3FFFF}')
  };
  name.is_ascii() || name.to_str().is_some_and(|name| name.chars().all(is_plain))
}

/// What is kept for a file whose fingerprint is not trusted at its next check.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Content {
  /// This hash of the content decides.
  Hash(u64),
  /// No hash (the file is too big or unreadable): an equal fingerprint counts as a change.
  Unknown,
}

/// What only a few nodes have, kept beside the arena: links, contents and holds.
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

  /// What is kept to compare at the next check. `None`: an equal fingerprint means unchanged.
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

  /// When the first look found the fingerprint the file has now, for a file whose modification
  /// time is far ahead of the clock.
  pub fn ahead_since(&self, key: DirKey) -> Option<Instant> {
    self.ahead.get(&key).copied()
  }

  pub fn set_ahead_since(&mut self, key: DirKey, since: Option<Instant>) {
    if let Some(since) = since {
      self.ahead.insert(key, since);
    } else if !self.ahead.is_empty() {
      self.ahead.remove(&key);
    }
  }

  /// Holds the node: a look found nothing where the tree has something. Until the second look at
  /// `until`, the node and everything below it stay as they are.
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
      // Many holds at once (a tree removed file by file) must not keep their room for ever.
      let (len, capacity) = (self.holds.len(), self.holds.capacity());
      if capacity > 64 && len * 4 < capacity {
        self.holds.shrink_to(len * 2);
      }
    }
  }

  pub fn held_until(&self, key: DirKey) -> Option<Instant> {
    self.holds.get(&key).copied()
  }

  pub fn holds(&self) -> impl Iterator<Item = (DirKey, Instant)> + '_ {
    self.holds.iter().map(|(key, until)| (*key, *until))
  }

  /// Whether a node above is held: then nothing is done here before its second look.
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

  /// For the tests: checks the links, the table and the counts.
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
    if !kept.chain(self.ahead.keys()).all(|key| self.get(*key).is_some()) {
      return Err("something is kept for a node that is gone".to_owned());
    }
    let mut of_files = self.contents.keys().chain(self.ahead.keys());
    if !of_files.all(|key| matches!(self[*key].kind, Kind::File(_))) {
      return Err("the content of a file is kept for a node that has no file".to_owned());
    }
    let live = self.nodes().count();
    if reached != live || self.index.len() + 1 != live {
      return Err("a node is in no list of children, or not in the table".to_owned());
    }
    let listed = |(parent, children): (&u32, &Vec<u32>)| {
      !children.is_empty()
        && children.iter().all(|&child| {
          let node = self.slots.get(child as usize).and_then(Slot::node);
          node.is_some_and(|node| {
            node.parent == *parent && node.is_registered() && !is_plain(name_of(&self.names, node))
          })
        })
    };
    let registered = |node: &Node| node.is_registered() && !is_plain(name_of(&self.names, node));
    let on_lists: usize = self.not_plain.values().map(Vec::len).sum();
    // The names on the list: the registered ones that are not plain.
    if !self.not_plain.iter().all(listed)
      || on_lists != self.nodes().filter(|(_, node)| registered(node)).count()
    {
      return Err("the list of the registered names that are not plain is wrong".to_owned());
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

#[cfg(test)]
mod tests {
  use std::{
    ffi::OsStr,
    path::Path,
    time::{Duration, UNIX_EPOCH},
  };

  use super::{KEPT, Tree, can_be_one_name, child_hash, emptied, nanos};
  use crate::backend::DirKey;

  /// After a pack, every node that stays has its name and is found by it.
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

  /// The arena, the free list and the table shrink, and an old key names no later node.
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
    // Other letters: the rules of the disk are not known, so any two such names can be one.
    // `e` with an accent as one character and as two, and in upper case.
    assert!(same("caf\u{e9}.js", "cafe\u{301}.js"));
    assert!(same("caf\u{e9}.js", "CAF\u{c9}.JS"));
    assert!(same("\u{e9}", "\u{444}"));
    // One of the two is plain: the bytes decide. An ideograph is plain, a compatibility
    // ideograph is not.
    assert!(!same("cafe.js", "caf\u{e9}.js"));
    assert!(!same("caf\u{e9}.js", "cafe.js"));
    assert!(same("\u{4e2d}\u{6587}.js", "\u{4e2d}\u{6587}.JS"));
    assert!(!same("\u{4e2d}.js", "\u{6587}.js"));
    assert!(!same("\u{4e2d}.js", "\u{e9}.js"));
    assert!(same("\u{f967}.js", "\u{e9}.js"));
  }

  /// A time that does not fit becomes the nearest one that does.
  #[test]
  fn a_time_as_one_number() {
    assert_eq!(nanos(UNIX_EPOCH), 0);
    // A multiple of 100 ns: the clock of Windows counts in those.
    assert_eq!(nanos(UNIX_EPOCH + Duration::new(2, 300)), 2_000_000_300);
    assert_eq!(nanos(UNIX_EPOCH - Duration::new(2, 300)), -2_000_000_300);
    // The year 2262 is the last one that fits, and 1677 the first.
    let far = Duration::from_hours(400 * 365 * 24);
    assert_eq!(nanos(UNIX_EPOCH + far), i64::MAX);
    // Three hundred years back: the clock of Windows starts in 1601.
    let back = Duration::from_hours(300 * 365 * 24);
    assert_eq!(nanos(UNIX_EPOCH - back), -i64::MAX);
  }

  /// A list that is used again keeps its memory, but not when it is long.
  #[test]
  fn a_long_list_is_given_back() {
    let short = emptied(vec![1_u64; KEPT / 8]);
    assert!(short.is_empty() && short.capacity() == KEPT / 8);
    let long = emptied(vec![1_u64; KEPT / 8 + 1]);
    assert_eq!(long.capacity(), 0);
  }
}
