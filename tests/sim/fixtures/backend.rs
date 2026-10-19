//! The backend on the disk in memory, and the OS side of its watches.

use std::{
  collections::VecDeque,
  ffi::OsStr,
  mem,
  path::{Path, PathBuf},
};

use pathwatch::backend::{Backend, Committed, Coverage, DirKey, Hint, Want, watch_limit};
use rustc_hash::{FxHashMap, FxHashSet};

use super::fs::SimFs;

/// Which OS watch API the backend plays.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
  /// inotify. One watch for one directory, not for what is below it. The watch belongs to the
  /// inode: it follows a renamed directory and keeps reporting, and it ends when the directory
  /// is deleted. A notification names the watch and the child. A watch asked for as `Names`
  /// does not report writes.
  Inode,
  /// FSEvents. One stream for a real path and everything below it. A stream starts at `commit`,
  /// not at `watch`. A notification carries the real path, without any link in it; the backend
  /// maps it back. A moved directory gives one notification for the directory and none for what
  /// is in it. It never says `Written`. `Names` is answered `NotWatched` (the real backend
  /// answers it with its guard; here the poll stands in), unless a live stream reports the
  /// directory: then the key gets the records of that stream below its directory,
  /// and it stays on that stream when it asks for the contents later.
  ///
  /// A stream stands at the text of its path and hears nothing more when a directory at or
  /// above that path is renamed or removed. The backend of FSEvents keeps each of these
  /// directories open and is told at once: the stream ends, and its keys are told so. Here
  /// the stream remembers the directories that were on its path when it started.
  Prefix,
  /// ReadDirectoryChangesW. One handle on a directory, for everything below it. The handle
  /// belongs to the directory, not to its path. Writes are reported as `Written`; a change of an
  /// entry also reports its directory as "modified". `Names` is answered `NotWatched`.
  Handle,
}

impl Mode {
  pub const ALL: [Mode; 3] = [Mode::Inode, Mode::Prefix, Mode::Handle];
}

/// The backends where one watch covers a whole tree.
pub const RECURSIVE: [Mode; 2] = [Mode::Prefix, Mode::Handle];

/// The backend on a [`SimFs`]. Make one for one `SimFs`.
pub struct SimBackend {
  fs: SimFs,
  mode: Mode,
  /// The last want of each key: for one key it only goes up (rule 5 of the backends).
  wants: FxHashMap<DirKey, Want>,
  bound: FxHashMap<DirKey, u64>,
  keys: FxHashMap<u64, Vec<DirKey>>,
  /// By prefix: the real path of each stream.
  prefixes: FxHashMap<u64, PathBuf>,
  /// By prefix: what `watch` asked for since the last `commit`.
  pending: Vec<(DirKey, PathBuf)>,
  /// By prefix: the real directory of each key that uses a stream above its directory.
  below: FxHashMap<DirKey, PathBuf>,
  /// By prefix: the real directory of each key that got `NotWatched`.
  not_watched: FxHashMap<DirKey, PathBuf>,
}

impl SimBackend {
  pub fn attach(fs: &SimFs, mode: Mode) -> Self {
    fs.lock().kernel.mode = Some(mode);
    Self {
      fs: fs.clone(),
      mode,
      wants: FxHashMap::default(),
      bound: FxHashMap::default(),
      keys: FxHashMap::default(),
      prefixes: FxHashMap::default(),
      pending: Vec::new(),
      below: FxHashMap::default(),
      not_watched: FxHashMap::default(),
    }
  }
}

impl Backend for SimBackend {
  /// inotify says "written" for a file that was created or moved in too.
  fn written_is_a_write(&self) -> bool {
    !matches!(self.mode, Mode::Inode)
  }

  type Raw = SimRaw;

  fn watch(&mut self, key: DirKey, dir: &Path, want: Want) -> Coverage {
    self.fs.enter(|calls| calls.watch += 1);
    self.fs.lock().log_watch(WatchCall { key, dir: dir.to_path_buf(), want: Some(want) });
    if let Some(had) = self.wants.insert(key, want) {
      assert!(want >= had, "rule 5: the want of {key:?} went down from {had:?} to {want:?}");
    }
    self.not_watched.remove(&key);
    // By prefix, a key that uses a stream above its directory stays on it when it asks for
    // more: that stream reports the whole tree already, as with FSEvents.
    let joins = self.mode == Mode::Prefix && (want == Want::Names || self.below.contains_key(&key));
    if joins && self.join_stream_above(key, dir, want) {
      return Coverage::WatchedTree;
    }
    if self.mode != Mode::Inode && want == Want::Names {
      return Coverage::NotWatched;
    }
    let mut state = self.fs.lock();
    let id = match state.watchable(dir) {
      Ok(id) => id,
      Err(error) => return Coverage::Failed(error),
    };
    if self.mode == Mode::Prefix {
      let real = state.real_path(id);
      drop(state);
      self.pending.retain(|(pending, _)| *pending != key);
      self.pending.push((key, real));
      return Coverage::WatchedTree;
    }
    let kernel = &mut state.kernel;
    if !kernel.watched.contains_key(&id)
      && kernel.limit.is_some_and(|limit| kernel.watched.len() >= limit)
    {
      return Coverage::Failed(watch_limit("no room for one more watch"));
    }
    let has = kernel.watched.entry(id).or_insert(want);
    *has = want.max(*has);
    drop(state);
    self.bind(key, id);
    if self.mode == Mode::Inode { Coverage::Watched } else { Coverage::WatchedTree }
  }

  fn unwatch(&mut self, key: DirKey) {
    self.fs.lock().log_unwatch(key);
    self.wants.remove(&key);
    self.not_watched.remove(&key);
    self.pending.retain(|(pending, _)| *pending != key);
    self.unbind(key);
  }

  fn commit(&mut self) -> Committed {
    self.fs.enter(|calls| calls.commit += 1);
    let pending = mem::take(&mut self.pending);
    let mut state = self.fs.lock();
    let kernel = &mut state.kernel;
    // As with FSEvents, one commit starts all of its new streams or none of them.
    let new: FxHashSet<&PathBuf> = pending
      .iter()
      .map(|(_, real)| real)
      .filter(|real| !kernel.streams.contains_key(*real))
      .collect();
    if kernel.limit.is_some_and(|limit| kernel.streams.len() + new.len() > limit) {
      drop(state);
      let mut failed = Vec::new();
      for (key, _) in pending {
        self.unbind(key);
        failed.push((key, watch_limit("no room for one more stream")));
      }
      return Committed { now_watched: Vec::new(), failed };
    }
    let is_new = |dir: &Path| new.iter().any(|stream| dir.starts_with(stream));
    let now_streamed: Vec<DirKey> =
      self.not_watched.iter().filter(|(_, dir)| is_new(dir)).map(|(&key, _)| key).collect();
    let mut bound = Vec::new();
    let mut started = Vec::new();
    for (key, real) in pending {
      let next = &mut kernel.next_stream;
      let stream = *kernel.streams.entry(real.clone()).or_insert_with(|| {
        *next += 1;
        started.push((*next, real.clone()));
        *next
      });
      bound.push((key, stream, real));
    }
    // The path of a stream was asked at `watch`. A directory on it that moved since then is
    // found out now: the stream ends before it told anything.
    for (stream, real) in started {
      match state.chain(&real) {
        Some(chain) => drop(state.kernel.guards.insert(stream, chain)),
        None => state.kernel.end_stream(stream),
      }
    }
    drop(state);
    // Every key joins its stream before any key leaves one. A stream that one key leaves in
    // this commit can be the stream that another key joins: it must not stop in between.
    let mut left = Vec::new();
    for (key, stream, real) in bound {
      self.prefixes.insert(stream, real);
      // The key has a stream on its own directory now.
      self.below.remove(&key);
      if self.bound.get(&key) == Some(&stream) {
        continue;
      }
      if let Some(old) = self.bound.insert(key, stream) {
        left.push((key, old));
      }
      self.keys.entry(stream).or_default().push(key);
    }
    for (key, old) in left {
      self.move_on(key, old);
    }
    Committed { now_watched: now_streamed, failed: Vec::new() }
  }

  fn translate(&mut self, raw: Self::Raw, hint: &mut dyn FnMut(Hint<'_>)) {
    match raw.what {
      What::Dead => {
        self.prefixes.remove(&raw.watch);
        for key in self.keys.remove(&raw.watch).unwrap_or_default() {
          // Not a key that has another stream by now.
          if self.bound.get(&key) == Some(&raw.watch) {
            self.bound.remove(&key);
            self.below.remove(&key);
          }
          hint(Hint::Changed { dir: key, rel: Path::new("") });
        }
        return;
      }
      What::Lost => {
        for &key in self.keys.get(&raw.watch).into_iter().flatten() {
          hint(Hint::Changed { dir: key, rel: Path::new("") });
        }
        return;
      }
      What::Overflow => return hint(Hint::Lost),
      What::Mount => return hint(Hint::Mount { path: &raw.path }),
      What::Entry | What::Moved | What::Written => {}
    }
    let Some(keys) = self.keys.get(&raw.watch) else { return };
    let prefix = self.prefixes.get(&raw.watch);
    if prefix.is_some_and(|prefix| !raw.path.starts_with(prefix)) {
      return hint(Hint::Lost);
    }
    for &key in keys {
      // A key that uses a stream above its directory gets what is at or below its directory.
      let rel = match self.below.get(&key).or(prefix) {
        Some(dir) => match raw.path.strip_prefix(dir) {
          Ok(rel) => rel,
          // The directory of the key went with a directory above it, or came with it.
          Err(_) if raw.what == What::Moved && dir.starts_with(&raw.path) => Path::new(""),
          Err(_) => continue,
        },
        None => &raw.path,
      };
      if raw.what == What::Written {
        hint(Hint::Written { dir: key, rel });
      } else {
        hint(Hint::Changed { dir: key, rel });
      }
    }
  }
}

/// A backend that is dropped gives every OS watch back (rule 12 of the backends).
impl Drop for SimBackend {
  fn drop(&mut self) {
    for key in self.bound.keys().copied().collect::<Vec<DirKey>>() {
      self.unbind(key);
    }
  }
}

impl SimBackend {
  /// By prefix: a live stream at or above the directory reports it too, so the key joins that
  /// stream. No stream is started for a directory that is asked for names only: it would
  /// report the whole tree below it. `false`: there is no such stream.
  fn join_stream_above(&mut self, key: DirKey, dir: &Path, want: Want) -> bool {
    let state = self.fs.lock();
    let Ok(id) = state.watchable(dir) else { return false };
    let real = state.real_path(id);
    let stream = real.ancestors().find_map(|above| state.kernel.streams.get(above).copied());
    drop(state);
    let Some(stream) = stream else {
      if want == Want::Names {
        self.not_watched.insert(key, real);
      }
      return false;
    };
    self.pending.retain(|(pending, _)| *pending != key);
    self.bind(key, stream);
    self.below.insert(key, real);
    true
  }

  fn bind(&mut self, key: DirKey, watch: u64) {
    if self.bound.get(&key) == Some(&watch) {
      return;
    }
    self.below.remove(&key);
    if let Some(old) = self.bound.remove(&key) {
      self.move_on(key, old);
    }
    self.bound.insert(key, watch);
    self.keys.entry(watch).or_default().push(key);
  }

  /// Forgets `key`. The OS watch goes when no other key uses it.
  fn unbind(&mut self, key: DirKey) {
    self.below.remove(&key);
    if let Some(watch) = self.bound.remove(&key) {
      self.leave(key, watch);
    }
  }

  /// `key` has another OS watch than `old` now. A stream that ended and has not said so yet
  /// keeps the key: its directory was at another path for a while, and the backend of
  /// FSEvents reports a key whose directory has another real path than before.
  fn move_on(&mut self, key: DirKey, old: u64) {
    let ended =
      self.prefixes.contains_key(&old) && !self.fs.lock().kernel.guards.contains_key(&old);
    if !ended {
      self.leave(key, old);
    }
  }

  /// Takes `key` off the OS watch `watch`. The OS watch goes when no other key uses it.
  fn leave(&mut self, key: DirKey, watch: u64) {
    let Some(keys) = self.keys.get_mut(&watch) else { return };
    keys.retain(|&other| other != key);
    if keys.is_empty() {
      self.keys.remove(&watch);
      let mut state = self.fs.lock();
      let kernel = &mut state.kernel;
      match self.prefixes.remove(&watch) {
        // Another stream can stand at the path by now, if this one ended by itself.
        Some(prefix) if kernel.streams.get(&prefix) == Some(&watch) => {
          kernel.streams.remove(&prefix);
          kernel.guards.remove(&watch);
        }
        Some(_) => {}
        None => drop(kernel.watched.remove(&watch)),
      }
    }
  }
}

/// One `watch` or `unwatch` call that the engine made to the backend. [`SimFs::watch_calls`]
/// has them all, in the order of the calls.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WatchCall {
  pub key: DirKey,
  /// The directory. For `unwatch`: the one that the last `watch` call for the key named.
  pub dir: PathBuf,
  /// What `watch` asked for. `None`: the call was `unwatch`.
  pub want: Option<Want>,
}

/// One raw notification. A test can clone it to deliver it twice.
#[derive(Clone, Debug)]
pub struct SimRaw {
  watch: u64,
  /// By inode: the child name. By handle: the path below the handle. By prefix: the real path.
  path: PathBuf,
  what: What,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum What {
  /// An entry came, went or was renamed.
  Entry,
  /// By prefix: a directory was renamed, or moved in or out. What is in it moved with it.
  Moved,
  /// The OS says the content was written. For a directory this is noise.
  Written,
  /// The watch is gone.
  Dead,
  /// The watch lost notifications. It still stands. The backend knows where the loss was: at
  /// or below the directory of the watch.
  Lost,
  /// The OS lost notifications of every watch.
  Overflow,
  /// A volume came or went at the path, which no watch tells (inotify).
  Mount,
}

impl SimRaw {
  /// A volume came or went at the real path `path`: the backend tells it by its path.
  pub fn mount(path: &str) -> Self {
    Self { watch: 0, path: path.into(), what: What::Mount }
  }
}

/// The OS side of the watches. It is a part of the disk, not of the backend, because it must see
/// a change at the moment it happens: whether a notification is made is decided then, not when
/// the test delivers it.
#[derive(Default)]
pub(crate) struct Kernel {
  pub(crate) mode: Option<Mode>,
  /// By inode and by handle: the watched directories (inode id) and what they report.
  pub(crate) watched: FxHashMap<u64, Want>,
  /// By path prefix: the real path of each live stream, and its id.
  pub(crate) streams: FxHashMap<PathBuf, u64>,
  /// By path prefix: for each live stream, the directories (inode id) at and above its path.
  pub(crate) guards: FxHashMap<u64, Vec<u64>>,
  next_stream: u64,
  pub(crate) limit: Option<usize>,
  pub(crate) queue: VecDeque<SimRaw>,
}

impl Kernel {
  pub(crate) fn by_inode(&mut self, dir: u64, name: &OsStr, act: Act, is_dir: bool) {
    let Some(&want) = self.watched.get(&dir) else { return };
    if act == Act::Modify && want == Want::Names {
      return;
    }
    // A new or moved-in file holds other content than the name had before, also when size and
    // time say nothing.
    let written = !is_dir && matches!(act, Act::Modify | Act::Create | Act::MoveTo);
    let what = if written { What::Written } else { What::Entry };
    self.queue.push_back(SimRaw { watch: dir, path: name.into(), what });
  }

  pub(crate) fn by_prefix(&mut self, real: &Path, act: Act, is_dir: bool) {
    let moved = is_dir && matches!(act, Act::MoveFrom | Act::MoveTo);
    let what = if moved { What::Moved } else { What::Entry };
    for prefix in real.ancestors() {
      if let Some(&stream) = self.streams.get(prefix) {
        self.queue.push_back(SimRaw { watch: stream, path: real.to_path_buf(), what });
      }
    }
  }

  /// `direct`: the entry lies in the directory of the handle itself.
  pub(crate) fn by_handle(&mut self, handle: u64, rel: &Path, act: Act, direct: bool) {
    if !self.watched.contains_key(&handle) {
      return;
    }
    let what = if act == Act::Modify { What::Written } else { What::Entry };
    self.queue.push_back(SimRaw { watch: handle, path: rel.to_path_buf(), what });
    if act != Act::Modify
      && !direct
      && let Some(dir) = rel.parent()
    {
      self.queue.push_back(SimRaw { watch: handle, path: dir.to_path_buf(), what: What::Written });
    }
  }

  /// The drive is gone, and every watch with it.
  pub(crate) fn unplug(&mut self) {
    let notice = |watch, what| SimRaw { watch, path: PathBuf::new(), what };
    let mut dead: Vec<u64> = self.watched.drain().map(|(watch, _)| watch).collect();
    dead.sort_unstable();
    self.queue.extend(dead.into_iter().map(|watch| notice(watch, What::Dead)));
    let mut streams: Vec<u64> = self.streams.values().copied().collect();
    streams.sort_unstable();
    self.queue.extend(streams.into_iter().map(|stream| notice(stream, What::Lost)));
  }

  /// A directory was deleted: a watch on the inode ends.
  pub(crate) fn gone(&mut self, dir: u64) {
    if self.watched.remove(&dir).is_some() {
      self.queue.push_back(SimRaw { watch: dir, path: PathBuf::new(), what: What::Dead });
    }
    self.dir_moved(dir);
  }

  /// A directory was renamed or removed: each stream that had it at or above its path ends.
  pub(crate) fn dir_moved(&mut self, dir: u64) {
    let had = |(_, chain): &(&u64, &Vec<u64>)| chain.contains(&dir);
    let mut streams: Vec<u64> = self.guards.iter().filter(had).map(|(stream, _)| *stream).collect();
    streams.sort_unstable();
    for stream in streams {
      self.end_stream(stream);
    }
  }

  /// The stream stands at a path where its directory is no more.
  pub(crate) fn end_stream(&mut self, stream: u64) {
    self.guards.remove(&stream);
    self.streams.retain(|_, live| *live != stream);
    self.queue.push_back(SimRaw { watch: stream, path: PathBuf::new(), what: What::Dead });
  }
}

/// What happened to an entry of a directory.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Act {
  Create,
  Remove,
  MoveFrom,
  MoveTo,
  Modify,
}

/// What the OS puts into the queue in place of the notifications in `lost`.
pub(crate) fn lost_notices(mode: Option<Mode>, lost: &[SimRaw]) -> Vec<SimRaw> {
  let notice = |watch, what| SimRaw { watch, path: PathBuf::new(), what };
  if lost.is_empty() {
    Vec::new()
  } else if mode == Some(Mode::Inode) {
    vec![notice(0, What::Overflow)]
  } else {
    let mut watches: Vec<u64> = lost.iter().map(|raw| raw.watch).collect();
    watches.sort_unstable();
    watches.dedup();
    watches.into_iter().map(|watch| notice(watch, What::Lost)).collect()
  }
}
