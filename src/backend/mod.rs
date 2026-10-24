//! The `Backend` trait: what the engine asks of an OS watch API, and the backends, one in each
//! file. The rules for a backend are in `references/design.md`, section 6.

#[cfg(target_os = "macos")]
pub mod fsevents;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub mod inotify;
pub mod poll;
#[cfg(target_os = "windows")]
pub mod windows;

use std::{error, fmt, hash::Hash, io, path::Path, sync::Arc, time::SystemTime};

use rustc_hash::FxHashMap;

/// An OS watch API as the engine sees it. Every method runs on the watcher thread.
pub trait Backend: 'static {
  /// One raw notification, as the thread of the backend copies it into the [`Sink`].
  type Raw: Send + 'static;

  /// Asks for a watch on the directory `dir` under `key`. The watch is live after the next
  /// [`commit`](Backend::commit).
  fn watch(&mut self, key: NodeKey, dir: &Path, want: Want) -> Coverage;

  /// Takes the watch of `key` back. A watch that other keys share stays; where the watches
  /// stand is worked out again at the next [`commit`](Backend::commit).
  fn unwatch(&mut self, key: NodeKey);

  /// Makes the watches asked for since the last commit live. Says which keys got no watch,
  /// and which keys that were answered `NotWatched` before an OS watch reports now.
  fn commit(&mut self) -> Committed {
    Committed::default()
  }

  /// Turns a raw notification into hints: one call of `hint` for each.
  fn translate(&mut self, raw: Self::Raw, hint: &mut dyn FnMut(Hint<'_>));

  /// Asks for a [`Hint::Delivered`] with a time at or after `after`: the thread of the backend
  /// reads the queue of the OS empty then, and sends it after what it read. A later question
  /// takes the place of one that is not answered yet. The default never answers, and a young
  /// file that was not read stays `Pending` until a look reports it (design.md 5.8).
  fn ask_delivered(&mut self, _after: SystemTime) {}

  /// How many bytes `raw` keeps on the heap: the queue of the watcher counts them on top of
  /// the message.
  fn raw_bytes(_raw: &Self::Raw) -> usize {
    0
  }
}

/// The name of a node of the tree: its slot, and a generation that tells it from an earlier node
/// in the slot. A backend gets the keys of directories.
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

/// What a [`commit`](Backend::commit) made live, and what it could not.
#[derive(Debug, Default)]
pub struct Committed {
  /// The keys that were answered `NotWatched` before and that an OS watch reports now.
  pub now_watched: Vec<NodeKey>,
  /// The keys whose watch could not be made live, each with the reason: the engine reports
  /// it, polls the directory, and asks again later.
  pub failed: Vec<(NodeKey, io::Error)>,
}

/// What a directory asks to hear about.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Want {
  /// Entries that come and go. Enough for a directory on the way to a watched path.
  Names,
  /// Also writes to the files in it.
  Contents,
}

/// The answer to [`Backend::watch`]: which changes make hints from now on.
#[derive(Debug)]
pub enum Coverage {
  /// This directory alone.
  Watched,
  /// This directory and every real directory below it.
  WatchedTree,
  /// Nothing. The engine polls the directory.
  NotWatched,
  /// Nothing, because the OS refused. The engine polls the directory and asks again later.
  Failed(io::Error),
}

/// Where to look. A hint never says what happened.
#[derive(Clone, Copy, Debug)]
pub enum Hint<'a> {
  /// Something at `rel` below the directory `dir` changed. An empty `rel`: the directory
  /// itself, or its watch, and the engine scans everything below it.
  Changed { dir: NodeKey, rel: &'a Path },
  /// The file at `rel` below `dir` was written: an `Update`, also with an equal fingerprint,
  /// under every spelling of the name where every disk is loose. `new`: the OS says so for a
  /// file that was created or moved in too (inotify), which holds other content than the name
  /// had before; a twin of such a name is left to its look, as a rename that changes only the
  /// case is the same file.
  Written { dir: NodeKey, rel: &'a Path, new: bool },
  /// Notifications were lost, and the backend cannot say where.
  Lost,
  /// A volume was mounted or unmounted at `path`, a real path: another directory may stand
  /// there now. The backend knows no key for it.
  Mount { path: &'a Path },
  /// Every notification of a change made before `before`, by the clock of the system, came
  /// before this hint: the answer to [`Backend::ask_delivered`].
  Delivered { before: SystemTime },
}

/// The way from a thread of the backend into the engine. It never blocks.
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

/// The error of a limit of the OS on watches: a backend makes it (`watch_limit`), and the
/// watcher tells it as `ErrorKind::WatchLimit`.
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

/// The message of the error, when it is a limit (`Limit`).
fn limit(error: &io::Error) -> Option<&'static str> {
  Some(error.get_ref()?.downcast_ref::<Limit>()?.0)
}

/// A copy of the error, for a backend that tells one error for many keys: an `io::Error` is not
/// `Clone`.
#[cfg(any(target_os = "macos", windows))]
pub(crate) fn copy_error(error: &io::Error) -> io::Error {
  match (limit(error), error.raw_os_error()) {
    (Some(message), _) => watch_limit(message),
    (None, Some(code)) => io::Error::from_raw_os_error(code),
    (None, None) => io::Error::new(error.kind(), error.to_string()),
  }
}

/// Whether a watch of a tree, a stream of FSEvents or a handle of Windows, may stand on `dir`:
/// not on a root, `Users` in it or a home directory, where every program of the user writes.
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

/// Gives the room of a table back once a quarter of it is in use, keeping twice what is left:
/// many entries that went at once leave no room behind for good.
pub(crate) fn give_room_back<K: Eq + Hash, V>(table: &mut FxHashMap<K, V>) {
  let (len, capacity) = (table.len(), table.capacity());
  if capacity > 64 && len * 4 < capacity {
    table.shrink_to(len * 2);
  }
}
