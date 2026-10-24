#[cfg(target_os = "macos")]
pub mod fsevents;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub mod inotify;
pub mod poll;
#[cfg(target_os = "windows")]
pub mod windows;

use std::{error, fmt, hash::Hash, io, path::Path, sync::Arc, time::SystemTime};

use rustc_hash::FxHashMap;

pub trait Backend: 'static {
  type Raw: Send + 'static;

  fn watch(&mut self, key: NodeKey, dir: &Path, want: Want) -> Coverage;

  fn unwatch(&mut self, key: NodeKey);

  fn commit(&mut self) -> Committed {
    Committed::default()
  }

  fn translate(&mut self, raw: Self::Raw, hint: &mut dyn FnMut(Hint<'_>));

  fn ask_delivered(&mut self, _after: SystemTime) {}

  fn raw_bytes(_raw: &Self::Raw) -> usize {
    0
  }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NodeKey {
  index: u32,
  generation: u32,
}

impl NodeKey {
  pub const fn new(index: u32, generation: u32) -> Self {
    Self { index, generation }
  }

  pub fn index(self) -> u32 {
    self.index
  }

  pub fn generation(self) -> u32 {
    self.generation
  }
}

#[derive(Debug, Default)]
pub struct Committed {
  pub now_watched: Vec<NodeKey>,
  pub failed: Vec<(NodeKey, io::Error)>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Want {
  Names,
  Contents,
}

#[derive(Debug)]
pub enum Coverage {
  Watched,
  WatchedTree,
  NotWatched,
  Failed(io::Error),
}

#[derive(Clone, Copy, Debug)]
pub enum Hint<'a> {
  Changed { dir: NodeKey, rel: &'a Path },
  Written { dir: NodeKey, rel: &'a Path, new: bool },
  Lost,
  Mount { path: &'a Path },
  Delivered { before: SystemTime },
}

pub struct Sink<R> {
  inner: Arc<dyn Fn(Signal<R>) + Send + Sync>,
}

impl<R> Sink<R> {
  pub fn new(deliver: impl Fn(Signal<R>) + Send + Sync + 'static) -> Self {
    Self { inner: Arc::new(deliver) }
  }

  pub fn raw(&self, raw: R) {
    (self.inner)(Signal::Raw(raw));
  }

  pub fn failed(&self, error: io::Error) {
    (self.inner)(Signal::Failed(error));
  }
}

impl<R> Clone for Sink<R> {
  fn clone(&self) -> Self {
    Self { inner: Arc::clone(&self.inner) }
  }
}

impl<R> fmt::Debug for Sink<R> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Sink").finish_non_exhaustive()
  }
}

#[derive(Debug)]
pub enum Signal<R> {
  Raw(R),
  Failed(io::Error),
}

#[derive(Debug)]
struct Limit(&'static str);

impl fmt::Display for Limit {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.0)
  }
}

impl error::Error for Limit {}

pub fn watch_limit(message: &'static str) -> io::Error {
  io::Error::other(Limit(message))
}

pub fn is_watch_limit(error: &io::Error) -> bool {
  limit(error).is_some()
}

fn limit(error: &io::Error) -> Option<&'static str> {
  Some(error.get_ref()?.downcast_ref::<Limit>()?.0)
}

#[cfg(any(target_os = "macos", windows))]
pub(crate) fn copy_error(error: &io::Error) -> io::Error {
  match (limit(error), error.raw_os_error()) {
    (Some(message), _) => watch_limit(message),
    (None, Some(code)) => io::Error::from_raw_os_error(code),
    (None, None) => io::Error::new(error.kind(), error.to_string()),
  }
}

#[cfg(any(target_os = "macos", windows))]
pub(crate) fn may_watch_a_tree(dir: &Path) -> bool {
  use std::path::Component;

  let mut names = dir.components().filter_map(|part| match part {
    Component::Normal(name) => Some(name),
    _ => None,
  });
  match (names.next(), names.next(), names.next()) {
    (None, ..) => false,
    (Some(first), _, None) => !first.eq_ignore_ascii_case("Users"),
    _ => true,
  }
}

pub(crate) fn give_room_back<K: Eq + Hash, V>(table: &mut FxHashMap<K, V>) {
  let (len, capacity) = (table.len(), table.capacity());
  if capacity > 64 && len * 4 < capacity {
    table.shrink_to(len * 2);
  }
}
