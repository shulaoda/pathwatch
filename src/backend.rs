//! The `Backend` trait: what the engine asks of an OS watch API. The rules for a backend are in
//! `references/design.md`, section 6.

use std::{error, fmt, io, path::Path, sync::Arc, time::SystemTime};

/// An OS watch API as the engine sees it. Every method runs on the watcher thread.
pub trait Backend: Send + 'static {
  /// One raw notification, as the thread of the backend copies it into the [`Sink`].
  type Raw: Send + 'static;

  /// Asks for a watch on the directory `dir` under `key`. The watch is live after the next
  /// [`commit`](Backend::commit).
  fn watch(&mut self, key: DirKey, dir: &Path, want: Want) -> Coverage;

  /// Takes the watch of `key` back. A watch that other keys share stays; where the watches
  /// stand is worked out again at the next [`commit`](Backend::commit).
  fn unwatch(&mut self, key: DirKey);

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

  /// How many bytes `raw` counts for in the queue of the watcher.
  fn raw_bytes(_raw: &Self::Raw) -> usize {
    0
  }
}

/// The name of a directory between the engine and a backend: its slot in the tree, and a
/// generation that tells it from an earlier node in the slot.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct DirKey {
  index: u32,
  generation: u32,
}

impl DirKey {
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
  pub now_watched: Vec<DirKey>,
  /// The keys whose watch could not be made live, each with the reason: the engine reports
  /// it, polls the directory, and asks again later.
  pub failed: Vec<(DirKey, io::Error)>,
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
  Changed { dir: DirKey, rel: &'a Path },
  /// The file at `rel` below `dir` was written: an `Update`, also with an equal fingerprint,
  /// under every spelling of the name where every disk is loose. `new`: the OS says so for a
  /// file that was created or moved in too (inotify), which holds other content than the name
  /// had before; a twin of such a name is left to its look, as a rename that changes only the
  /// case is the same file.
  Written { dir: DirKey, rel: &'a Path, new: bool },
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

pub(crate) fn same(error: &io::Error) -> io::Error {
  match (limit(error), error.raw_os_error()) {
    (Some(message), _) => watch_limit(message),
    (None, Some(code)) => io::Error::from_raw_os_error(code),
    (None, None) => io::Error::new(error.kind(), error.to_string()),
  }
}

fn limit(error: &io::Error) -> Option<&'static str> {
  Some(error.get_ref()?.downcast_ref::<Limit>()?.0)
}
