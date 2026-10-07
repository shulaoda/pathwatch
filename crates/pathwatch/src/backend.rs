//! The interface between the engine and an OS watch API.
//!
//! The engine asks a backend to watch directories, and the backend tells the engine where to
//! check. A backend never says what happened: the engine reads the disk itself.

use std::{fmt, io, path::Path, sync::Arc};

/// An OS watch API behind the engine. A backend type has its own `new(sink: Sink<Self::Raw>)`.
///
/// The rules for a backend:
///
/// 1. **Threads.** Every method runs on the watcher thread. A backend thread only copies raw
///    notifications into the [`Sink`].
/// 2. **Watch, commit, scan.** The engine reads a directory only after `watch` and then `commit`.
///    So a watch is live when `commit` returns, and `commit` opens no gap in a watch that is live.
/// 3. **`watch` again** for a key means: the watch must stand on the directory that is at `dir`
///    now. If that needs a new OS watch, the new one is live before the old one goes.
/// 4. **The answer is the truth.** [`Coverage`] says which changes make hints from now on. After
///    `NotWatched` or `Failed` nothing is watched for the key. A `commit` can make an OS watch
///    report a directory that got `NotWatched` before: then [`Backend::covers_more`] says so,
///    and the engine asks again for the directories that it polls.
/// 5. **Want.** For one key the [`Want`] only goes up, until `unwatch(key)`. More hints than
///    asked for are fine, fewer are not.
/// 6. **Keys.** One OS watch can serve several keys: two spellings of one directory, or a renamed
///    directory under its old and its new key. The OS watch goes with its last key.
/// 7. **One hint for each key** that can see the change, with `rel` relative to the `dir` given
///    with that key. A backend whose OS reports real paths maps them back.
/// 8. **Where, not what.** `Changed` for a directory makes the engine scan everything below it.
///    So "this directory was touched" is dropped, or sent as `Written`.
/// 9. **`Written` only when the OS says so for each event.** An OS that merges its flags, or
///    repeats them, never sends it.
/// 10. **A dead watch.** The OS dropped the watch: forget it. The directory was moved: keep the
///     watch and its keys. Both times send `Changed` with an empty `rel` for each of its keys.
/// 11. **Lost notifications.** The OS names the place: `Changed` for each key of it, with an empty
///     `rel` or the `rel` of a place below; the watch stands. No place, or unreadable data: `Lost`.
/// 12. **The end.** A backend that cannot go on calls [`Sink::failed`] once. Dropping the backend
///     stops and joins its threads, and the sink is not called after that.
pub trait Backend: Send + 'static {
  /// A raw OS notification. Cheap to make: the backend thread only copies it into the sink.
  type Raw: Send + 'static;

  /// Report changes in `dir` as hints that carry `key`.
  fn watch(&mut self, key: DirKey, dir: &Path, want: Want) -> Coverage;

  /// Forget `key`. No hint should carry it after this; a late one costs the engine one check.
  fn unwatch(&mut self, key: DirKey);

  /// Make every watch asked for since the last call live. On an error none of them is.
  ///
  /// Only a backend that starts one OS watch for many directories (FSEvents) has work here.
  fn commit(&mut self) -> io::Result<()>;

  /// Whether the last `commit` started an OS watch that reports a directory that got
  /// `NotWatched` before. Then the engine calls `watch` again for each directory that it polls,
  /// with the `Want` of before. One that is reported now is not polled any more.
  ///
  /// Only a backend whose answer for a directory depends on the OS watches of other
  /// directories (FSEvents) says yes.
  fn covers_more(&self) -> bool {
    false
  }

  /// Turn one raw notification into hints.
  fn translate(&mut self, raw: Self::Raw, hint: &mut dyn FnMut(Hint<'_>));

  /// How many bytes `raw` keeps on the heap. The watcher drops raw notifications when too many
  /// bytes wait in its queue.
  fn raw_bytes(_raw: &Self::Raw) -> usize {
    0
  }
}

/// Names one directory node of the engine. A backend stores it and puts it into every [`Hint`]
/// for that directory, so no path spelling has to match between the two sides.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DirKey {
  index: u32,
  generation: u32,
}

impl DirKey {
  pub const fn new(index: u32, generation: u32) -> Self {
    Self { index, generation }
  }

  pub(crate) fn index(self) -> u32 {
    self.index
  }

  pub(crate) fn generation(self) -> u32 {
    self.generation
  }
}

/// What the engine asks a backend about a directory.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Want {
  /// Tell when a name in the directory comes, goes or is replaced. Enough for a directory that
  /// only leads to watched paths.
  Names,
  /// Tell that, and also writes to the files in the directory. For a watched directory, and for
  /// a directory that holds a watched path with a file or nothing at it.
  Contents,
}

/// What the backend answers to [`Backend::watch`].
#[derive(Debug)]
pub enum Coverage {
  /// Changes in this directory make hints.
  Watched,
  /// Changes in this directory and in every real directory below it make hints. The engine does
  /// not call `watch` for the directories below.
  WatchedTree,
  /// The backend does not watch this directory. The engine polls it.
  NotWatched,
  /// The backend tried and could not. The engine reports it, polls, and asks again later.
  Failed(io::Error),
}

/// Where the engine has to check. A hint never says what happened.
#[derive(Clone, Copy, Debug)]
pub enum Hint<'a> {
  /// The entry at `rel` below `dir` came, went or was replaced. If it is a directory, anything
  /// below it may have changed too.
  ///
  /// An empty `rel` is a loss report for `dir` itself: the watch is gone, the directory was
  /// moved away, or notifications were lost. The engine checks the directory, watches it again
  /// and scans everything below it. It paces these scans, so a backend may send this often.
  Changed { dir: DirKey, rel: &'a Path },
  /// The content of the file at `rel` was written. For a directory it means nothing.
  Written { dir: DirKey, rel: &'a Path },
  /// Notifications were lost, and the backend cannot say where. Check everything.
  Lost,
}

/// The way from a backend thread into the engine. It never blocks.
pub struct Sink<R> {
  deliver: Arc<dyn Fn(Signal<R>) + Send + Sync>,
}

impl<R> Sink<R> {
  pub fn new(deliver: impl Fn(Signal<R>) + Send + Sync + 'static) -> Self {
    Self { deliver: Arc::new(deliver) }
  }

  pub fn raw(&self, raw: R) {
    (self.deliver)(Signal::Raw(raw));
  }

  /// The backend cannot go on. From now on the engine polls everything.
  pub fn failed(&self, error: io::Error) {
    (self.deliver)(Signal::Failed(error));
  }
}

impl<R> Clone for Sink<R> {
  fn clone(&self) -> Self {
    Self { deliver: Arc::clone(&self.deliver) }
  }
}

impl<R> fmt::Debug for Sink<R> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Sink").finish_non_exhaustive()
  }
}

/// What a [`Sink`] hands to the engine.
#[derive(Debug)]
pub enum Signal<R> {
  /// A raw OS notification, for [`Backend::translate`] on the watcher thread.
  Raw(R),
  /// The backend cannot deliver notifications any more.
  Failed(io::Error),
}

/// The error for "the OS has no room for one more watch". The engine reports it as a watch limit.
pub fn watch_limit(message: &'static str) -> io::Error {
  io::Error::new(io::ErrorKind::StorageFull, message)
}

/// Whether `error` says "the OS has no room for one more watch".
pub fn is_watch_limit(error: &io::Error) -> bool {
  matches!(error.kind(), io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded)
}
