//! The disk in memory. A test changes it through [`SimFs`], and the engine reads it through
//! [`Fs`]. The OS side of the watches is a part of it (`Kernel` in `backend.rs`).

use std::{
  collections::BTreeMap,
  ffi::{OsStr, OsString},
  hash::Hasher,
  io, mem,
  path::{Component, Path, PathBuf},
  sync::{Arc, Mutex, MutexGuard, PoisonError},
  time::{Duration, SystemTime},
};

use pathwatch::{
  backend::{DirKey, Want},
  fs::{DirId, Entry, FileType, Fs, Meta},
};
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};

use super::backend::{Act, Kernel, Mode, SimRaw, WatchCall, lost_notices};

/// The id of the inode of `/`.
const ROOT: u64 = 0;

/// The disk. Cloning gives another handle to the same disk.
#[derive(Clone)]
pub struct SimFs {
  state: Arc<Mutex<State>>,
}

// The disk as a test makes it and reads it.
impl SimFs {
  pub fn new() -> Self {
    let root = Inode {
      number: 1,
      parent: ROOT,
      name: OsString::new(),
      content: Content::Dir(BTreeMap::new()),
      modified: 0,
      changed: 0,
      born: 0,
      denied: false,
    };
    let state = State {
      inodes: FxHashMap::from_iter([(ROOT, root)]),
      next_id: 1,
      next_number: 1,
      free_numbers: Vec::new(),
      reuse_numbers: true,
      birth_times: false,
      clock: 1_000_000_000_000_000_000,
      step: 1,
      listing_kinds: true,
      listing_meta: false,
      ignore_case: false,
      mount: None,
      other_rule: FxHashSet::default(),
      offline: None,
      calls: Calls::default(),
      watch_calls: Vec::new(),
      seen_calls: 0,
      traps: Vec::new(),
      traps_run: 0,
      quiet: 0,
      kernel: Kernel::default(),
    };
    Self { state: Arc::new(Mutex::new(state)) }
  }

  pub fn mkdir(&self, path: impl AsRef<Path>) -> io::Result<()> {
    let mut state = self.lock();
    let (dir, name) = state.parent_of(path.as_ref())?;
    if state.entries(dir)?.contains_key(&name) {
      return Err(error(io::ErrorKind::AlreadyExists));
    }
    state.create(dir, &name, Content::Dir(BTreeMap::new()));
    Ok(())
  }

  /// `mkdir -p`.
  pub fn mkdir_all(&self, path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    let mut missing: Vec<&Path> = path
      .ancestors()
      .take_while(|ancestor| self.lock().resolve(ancestor, true).is_err())
      .collect();
    missing.reverse();
    missing.into_iter().try_for_each(|dir| self.mkdir(dir))
  }

  /// Creates the file, or writes it in place. A link at the end of the path is followed.
  pub fn write(&self, path: impl AsRef<Path>, content: impl AsRef<[u8]>) -> io::Result<()> {
    let path = path.as_ref();
    let mut state = self.lock();
    if let Ok(id) = state.resolve(path, true) {
      let now = state.stamp();
      let inode = state.inodes.get_mut(&id).ok_or_else(|| error(io::ErrorKind::NotFound))?;
      let Content::File(bytes) = &mut inode.content else {
        return Err(error(io::ErrorKind::IsADirectory));
      };
      *bytes = content.as_ref().to_vec();
      inode.modified = now;
      inode.changed = now;
      let (dir, name) = (inode.parent, inode.name.clone());
      state.notify(dir, &name, Act::Modify, false);
    } else {
      let (dir, name) = state.parent_of(path)?;
      if state.entries(dir)?.contains_key(&name) {
        // A link that points nowhere.
        return Err(error(io::ErrorKind::NotFound));
      }
      state.create(dir, &name, Content::File(content.as_ref().to_vec()));
    }
    Ok(())
  }

  pub fn symlink(&self, target: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    let mut state = self.lock();
    let (dir, name) = state.parent_of(link.as_ref())?;
    if state.entries(dir)?.contains_key(&name) {
      return Err(error(io::ErrorKind::AlreadyExists));
    }
    state.create(dir, &name, Content::Link(target.as_ref().to_path_buf()));
    Ok(())
  }

  /// Removes a file, a link or an empty directory.
  pub fn remove(&self, path: impl AsRef<Path>) -> io::Result<()> {
    let mut state = self.lock();
    let (dir, name) = state.parent_of(path.as_ref())?;
    let id = *state.entries(dir)?.get(&name).ok_or_else(|| error(io::ErrorKind::NotFound))?;
    if matches!(&state.inodes[&id].content, Content::Dir(entries) if !entries.is_empty()) {
      return Err(error(io::ErrorKind::DirectoryNotEmpty));
    }
    state.delete(dir, &name, said(path.as_ref(), &name));
    Ok(())
  }

  /// `rm -r`: the deepest entries go first.
  pub fn remove_all(&self, path: impl AsRef<Path>) -> io::Result<()> {
    let mut state = self.lock();
    let (dir, name) = state.parent_of(path.as_ref())?;
    if !state.entries(dir)?.contains_key(&name) {
      return Err(error(io::ErrorKind::NotFound));
    }
    state.delete(dir, &name, said(path.as_ref(), &name));
    Ok(())
  }

  /// Moves an entry. A file or an empty directory at `to` is replaced. The entry gets the name
  /// as `to` spells it.
  pub fn rename(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let mut state = self.lock();
    let (from_dir, from_name) = state.parent_of(from.as_ref())?;
    let (to_dir, old_name) = state.parent_of(to.as_ref())?;
    let (from_name, old_name) = (from_name.as_os_str(), old_name.as_os_str());
    let to_name = to.as_ref().file_name().unwrap_or(old_name);
    let id =
      *state.entries(from_dir)?.get(from_name).ok_or_else(|| error(io::ErrorKind::NotFound))?;
    let is_dir = matches!(state.inodes[&id].content, Content::Dir(_));
    if (from_dir, from_name) == (to_dir, to_name) {
      return Ok(());
    }
    if is_dir {
      // A directory cannot move into itself.
      let mut above = to_dir;
      while above != ROOT {
        if above == id {
          return Err(error(io::ErrorKind::InvalidInput));
        }
        above = state.inodes[&above].parent;
      }
    }
    // The entry that is at `to` now. With `ignore_case` it can be the moved entry itself, under
    // another spelling: then nothing is replaced.
    if let Some(&old) = state.entries(to_dir)?.get(old_name)
      && old != id
    {
      match &state.inodes[&old].content {
        Content::Dir(entries) if !is_dir || !entries.is_empty() => {
          return Err(error(io::ErrorKind::DirectoryNotEmpty));
        }
        Content::File(_) | Content::Link(_) if is_dir => {
          return Err(error(io::ErrorKind::NotADirectory));
        }
        _ => {}
      }
      // The OS says nothing about the entry that is replaced.
      state.entries_mut(to_dir).remove(old_name);
      state.forget(old);
    }
    state.entries_mut(from_dir).remove(from_name);
    state.touch(from_dir);
    if is_dir {
      state.kernel.dir_moved(id);
    }
    state.notify(from_dir, said(from.as_ref(), from_name), Act::MoveFrom, is_dir);
    state.entries_mut(to_dir).insert(to_name.to_owned(), id);
    state.touch(to_dir);
    state.touch_entry(id);
    if let Some(inode) = state.inodes.get_mut(&id) {
      inode.parent = to_dir;
      to_name.clone_into(&mut inode.name);
    }
    state.notify(to_dir, to_name, Act::MoveTo, is_dir);
    Ok(())
  }

  /// Takes away or gives back the permission to list a directory, or to read a file.
  pub fn set_denied(&self, path: impl AsRef<Path>, denied: bool) -> io::Result<()> {
    let mut state = self.lock();
    let id = state.resolve(path.as_ref(), true)?;
    if let Some(inode) = state.inodes.get_mut(&id) {
      inode.denied = denied;
    }
    state.touch_entry(id);
    Ok(())
  }

  /// The content of the file that `path` leads to, links followed, also when the engine is not
  /// allowed to read it. `None`: `path` leads to no file.
  pub fn read(&self, path: impl AsRef<Path>) -> Option<Vec<u8>> {
    let state = self.lock();
    let id = state.resolve(path.as_ref(), true).ok()?;
    match &state.inodes[&id].content {
      Content::File(bytes) => Some(bytes.clone()),
      _ => None,
    }
  }

  /// Where the link at `path` points. `None`: there is no link at `path`.
  pub fn read_link(&self, path: impl AsRef<Path>) -> Option<PathBuf> {
    let state = self.lock();
    let id = state.resolve(path.as_ref(), false).ok()?;
    match &state.inodes[&id].content {
      Content::Link(target) => Some(target.clone()),
      _ => None,
    }
  }

  /// The directory that `path` leads to, links followed: a number for it and its real path.
  /// Unlike an inode number, the number is never given to another directory. `None`: `path`
  /// leads to no directory.
  pub fn dir_object(&self, path: impl AsRef<Path>) -> Option<(u64, PathBuf)> {
    let state = self.lock();
    let id = state.resolve(path.as_ref(), true).ok()?;
    matches!(state.inodes[&id].content, Content::Dir(_)).then(|| (id, state.real_path(id)))
  }

  /// Every directory on the disk but `/`, by its real path, sorted.
  pub fn dirs(&self) -> Vec<PathBuf> {
    let state = self.lock();
    let mut dirs: Vec<PathBuf> = state
      .inodes
      .iter()
      .filter(|(id, inode)| **id != ROOT && matches!(inode.content, Content::Dir(_)))
      .map(|(&id, _)| state.real_path(id))
      .collect();
    dirs.sort_unstable();
    dirs
  }

  /// Every file and link on the disk, by its real path, sorted.
  pub fn files(&self) -> Vec<PathBuf> {
    let state = self.lock();
    let mut files: Vec<PathBuf> = state
      .inodes
      .iter()
      .filter(|(_, inode)| !matches!(inode.content, Content::Dir(_)))
      .map(|(&id, _)| state.real_path(id))
      .collect();
    files.sort_unstable();
    files
  }
}

// The clock and the switches of the disk.
impl SimFs {
  pub fn advance(&self, by: Duration) {
    let mut state = self.lock();
    state.clock += u64::try_from(by.as_nanos()).expect("a test does not run for 500 years");
  }

  /// Sets how fine modification times are. With one second, two writes inside the same second
  /// leave the same time.
  pub fn set_time_step(&self, step: Duration) {
    self.lock().step = u64::try_from(step.as_nanos()).expect("the step fits").max(1);
  }

  /// Whether a freed inode number goes to the next new inode, as on ext4. On by default.
  pub fn set_reuse_inode_numbers(&self, reuse: bool) {
    self.lock().reuse_numbers = reuse;
  }

  /// Whether the identity of a directory holds its birth time. Off by default, as with musl.
  pub fn set_birth_times(&self, birth_times: bool) {
    self.lock().birth_times = birth_times;
  }

  /// Whether a listing tells the kind of each entry. On by default.
  pub fn set_listing_kinds(&self, kinds: bool) {
    self.lock().listing_kinds = kinds;
  }

  /// Whether a listing tells of each regular file all that `lstat` tells, as on APFS. Off by
  /// default.
  pub fn set_listing_meta(&self, meta: bool) {
    self.lock().listing_meta = meta;
  }

  /// Whether names that differ only in the case of ASCII letters are one name, as on the
  /// usual disks of macOS and Windows. A lookup finds `A.js` as `a.js`. A listing, a real path
  /// and a notification give the name as the disk spells it: the spelling of the call that
  /// made the entry or renamed it last. A notification for a name that is gone (removed, or
  /// renamed away) gives it as the call spelled it, as FSEvents does. Off by default. Set it
  /// before the first entry is made.
  pub fn set_ignore_case(&self, ignore_case: bool) {
    self.lock().ignore_case = ignore_case;
  }

  /// Another disk is mounted at `/name`: a directory that is made there takes names the other
  /// way than `set_ignore_case` says, and so does every directory that is made in it. Set it
  /// before the directory is made.
  pub fn set_mount(&self, name: &str) {
    self.lock().mount = Some(name.into());
  }

  /// The backend makes no more than this many OS watches.
  pub fn set_watch_limit(&self, limit: Option<usize>) {
    self.lock().kernel.limit = limit;
  }

  /// With `Some(kind)` every read of the disk by the engine fails with that error, and so does
  /// every new watch: a drive that was unplugged (`NotFound`), or a network disk that is down
  /// (any other kind). The test can still change the disk.
  ///
  /// An unplugged drive takes its watches with it, and the OS says so: by inode and by handle
  /// every watch ends, by prefix every stream reports that it lost track.
  pub fn set_offline(&self, offline: Option<io::ErrorKind>) {
    let mut state = self.lock();
    state.offline = offline;
    if offline == Some(io::ErrorKind::NotFound) {
      state.kernel.unplug();
    }
  }
}

// A change in the middle of an engine call, and reads that do not count.
impl SimFs {
  /// Runs `change` in the middle of an engine call: right before the call to the disk or to the
  /// backend (`lstat`, `stat`, `read_dir`, `hash`, `link_target`, `watch`, `commit`) that comes
  /// `after` such calls from now. Zero is the very next one.
  pub fn at_call(&self, after: u64, change: impl FnOnce(&SimFs) + Send + 'static) {
    let mut state = self.lock();
    let at = state.seen_calls + after + 1;
    state.traps.push((at, Box::new(change)));
  }

  /// Forgets the changes that still wait for a call.
  pub fn clear_traps(&self) {
    self.lock().traps.clear();
  }

  /// How many changes given to [`SimFs::at_call`] have run.
  pub fn traps_run(&self) -> u64 {
    self.lock().traps_run
  }

  /// Runs `read` as a test that reads the disk through [`Fs`]: its calls are not counted and
  /// run no trap.
  pub fn quiet<T>(&self, read: impl FnOnce() -> T) -> T {
    self.lock().quiet += 1;
    let result = read();
    self.lock().quiet -= 1;
    result
  }
}

// What the engine asked for, and the raw notifications that wait.
impl SimFs {
  pub fn calls(&self) -> Calls {
    self.lock().calls
  }

  /// Every `watch` and `unwatch` call that the engine made to the backend so far.
  pub fn watch_calls(&self) -> Vec<WatchCall> {
    self.lock().watch_calls.clone()
  }

  /// Where an OS watch is now, sorted by path: the real path of each watched directory, or of
  /// each live stream, and what the watch reports.
  pub fn os_watches(&self) -> Vec<(PathBuf, Want)> {
    let state = self.lock();
    let kernel = &state.kernel;
    let mut watches: Vec<(PathBuf, Want)> =
      kernel.watched.iter().map(|(&dir, &want)| (state.real_path(dir), want)).collect();
    watches.extend(kernel.streams.keys().map(|real| (real.clone(), Want::Contents)));
    watches.sort();
    watches
  }

  /// How many raw notifications wait.
  pub fn pending_raw(&self) -> usize {
    self.lock().kernel.queue.len()
  }

  /// Takes up to `count` raw notifications out of the queue, the oldest first.
  pub fn take_raw(&self, count: usize) -> Vec<SimRaw> {
    let mut state = self.lock();
    let count = count.min(state.kernel.queue.len());
    state.kernel.queue.drain(..count).collect()
  }

  /// The OS loses up to `count` raw notifications, the oldest first, and says so in its own
  /// way: one "overflow" for everything by inode, one "scan this watch" for each watch that
  /// lost something by prefix and by handle. Says how many were lost.
  pub fn lose_raw(&self, count: usize) -> usize {
    let mut state = self.lock();
    let kernel = &mut state.kernel;
    let count = count.min(kernel.queue.len());
    let lost: Vec<SimRaw> = kernel.queue.drain(..count).collect();
    for notice in lost_notices(kernel.mode, &lost).into_iter().rev() {
      kernel.queue.push_front(notice);
    }
    lost.len()
  }

  pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
    self.state.lock().unwrap_or_else(PoisonError::into_inner)
  }

  /// Counts one call of the engine and runs the traps that wait for it. The disk can change
  /// here, right before the call reads it.
  pub(crate) fn enter(&self, count: impl FnOnce(&mut Calls)) {
    let due: Vec<Trap> = {
      let mut state = self.lock();
      if state.quiet > 0 {
        return;
      }
      count(&mut state.calls);
      state.seen_calls += 1;
      let now = state.seen_calls;
      let (due, later) = mem::take(&mut state.traps).into_iter().partition(|(at, _)| *at <= now);
      state.traps = later;
      due.into_iter().map(|(_, trap)| trap).collect()
    };
    for trap in due {
      self.lock().traps_run += 1;
      self.quiet(|| trap(self));
    }
  }
}

impl Fs for SimFs {
  fn lstat(&self, path: &Path) -> io::Result<Meta> {
    self.enter(|calls| calls.lstat += 1);
    self.lock().look(path, false)
  }

  fn stat(&self, path: &Path) -> io::Result<Meta> {
    self.enter(|calls| calls.stat += 1);
    self.lock().look(path, true)
  }

  fn read_dir(&self, dir: &Path, each: &mut dyn FnMut(&OsStr, Entry)) -> io::Result<()> {
    self.enter(|calls| calls.read_dir += 1);
    let state = self.lock();
    let id = state.look_up(dir, true)?;
    let entries = state.entries(id)?;
    if state.inodes[&id].denied {
      return Err(error(io::ErrorKind::PermissionDenied));
    }
    for (name, child) in entries {
      let meta = state.meta(*child);
      each(
        name,
        if state.listing_meta && meta.kind == FileType::File {
          Entry::File { modified: meta.modified, size: meta.size, file_id: meta.file_id }
        } else if state.listing_kinds {
          Entry::Kind(meta.kind)
        } else {
          Entry::Name
        },
      );
    }
    Ok(())
  }

  fn hash(&self, path: &Path, limit: u64) -> io::Result<Option<u64>> {
    self.enter(|calls| calls.hash += 1);
    let state = self.lock();
    // The real disk opens with `O_NOFOLLOW`: a link at the end of the path is refused.
    let id = state.look_up(path, false)?;
    let inode = &state.inodes[&id];
    match &inode.content {
      Content::Link(_) => Err(error(io::ErrorKind::InvalidInput)),
      Content::File(_) if inode.denied => Err(error(io::ErrorKind::PermissionDenied)),
      Content::File(bytes) if u64::try_from(bytes.len()).is_ok_and(|len| len > limit) => Ok(None),
      Content::File(bytes) => {
        let mut hasher = FxHasher::default();
        hasher.write(bytes);
        Ok(Some(hasher.finish()))
      }
      Content::Dir(_) => Err(error(io::ErrorKind::IsADirectory)),
    }
  }

  fn link_target(&self, path: &Path) -> io::Result<PathBuf> {
    self.enter(|calls| calls.link_target += 1);
    let state = self.lock();
    let id = state.look_up(path, false)?;
    match &state.inodes[&id].content {
      Content::Link(target) => Ok(target.clone()),
      _ => Err(error(io::ErrorKind::InvalidInput)),
    }
  }

  fn real_path(&self, path: &Path) -> io::Result<PathBuf> {
    self.enter(|calls| calls.real_path += 1);
    let state = self.lock();
    state.look_up(path, true).map(|id| state.real_path(id))
  }

  fn name_on_disk(&self, path: &Path) -> io::Result<OsString> {
    self.enter(|calls| calls.name_on_disk += 1);
    let state = self.lock();
    state.look_up(path, false).map(|id| state.inodes[&id].name.clone())
  }

  fn now(&self) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_nanos(self.lock().clock)
  }
}

/// How often each call of [`Fs`] and of the backend was made.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Calls {
  pub lstat: u64,
  pub stat: u64,
  pub read_dir: u64,
  pub hash: u64,
  pub link_target: u64,
  pub real_path: u64,
  pub name_on_disk: u64,
  pub watch: u64,
  pub unwatch: u64,
  pub commit: u64,
}

impl Calls {
  /// The calls that can run a change of [`SimFs::at_call`].
  pub fn total(&self) -> u64 {
    self.lstat
      + self.stat
      + self.read_dir
      + self.hash
      + self.link_target
      + self.real_path
      + self.name_on_disk
      + self.watch
      + self.commit
  }
}

/// A change of the disk that waits for a call of the engine.
type Trap = Box<dyn FnOnce(&SimFs) + Send>;

#[expect(
  clippy::struct_excessive_bools,
  reason = "each one is a switch of the disk that a test sets by itself"
)]
pub(crate) struct State {
  /// Every inode by an id that is never used again.
  inodes: FxHashMap<u64, Inode>,
  next_id: u64,
  next_number: u64,
  free_numbers: Vec<u64>,
  reuse_numbers: bool,
  birth_times: bool,
  /// Nanoseconds since the Unix epoch.
  clock: u64,
  /// Modification times are rounded down to a multiple of this.
  step: u64,
  /// A listing tells the kind of each entry.
  listing_kinds: bool,
  /// A listing tells of each regular file all that `lstat` tells.
  listing_meta: bool,
  /// Names that differ only in the case of ASCII letters are one name.
  ignore_case: bool,
  /// `SimFs::set_mount`.
  mount: Option<OsString>,
  /// The directories that take names the other way than `ignore_case` says.
  other_rule: FxHashSet<u64>,
  /// Every read of the disk fails with this error.
  offline: Option<io::ErrorKind>,
  pub(crate) calls: Calls,
  /// Every `watch` and `unwatch` call of the backend, in the order of the calls.
  watch_calls: Vec<WatchCall>,
  /// Calls of the engine so far, and the traps with the count at which each one runs.
  seen_calls: u64,
  traps: Vec<(u64, Trap)>,
  traps_run: u64,
  /// Above zero: the calls are made by a test, not by the engine. They are not counted and
  /// run no trap.
  quiet: u32,
  pub(crate) kernel: Kernel,
}

impl State {
  fn stamp(&self) -> u64 {
    self.clock / self.step * self.step
  }

  /// The directories at and above `path`, if `path` is the real path of what is at it: no
  /// link on the way, and every name as the disk spells it.
  pub(crate) fn chain(&self, path: &Path) -> Option<Vec<u64>> {
    let id = self.resolve(path, false).ok().filter(|id| self.real_path(*id) == path)?;
    let mut chain = vec![id];
    while let Some(&last) = chain.last().filter(|&&last| last != ROOT) {
      chain.push(self.inodes[&last].parent);
    }
    Some(chain)
  }

  fn entries(&self, dir: u64) -> io::Result<&BTreeMap<OsString, u64>> {
    match &self.inodes[&dir].content {
      Content::Dir(entries) => Ok(entries),
      _ => Err(error(io::ErrorKind::NotADirectory)),
    }
  }

  fn entries_mut(&mut self, dir: u64) -> &mut BTreeMap<OsString, u64> {
    match self.inodes.get_mut(&dir).map(|inode| &mut inode.content) {
      Some(Content::Dir(entries)) => entries,
      _ => unreachable!("the caller checked that this is a directory"),
    }
  }

  /// The entry `name` of the directory `dir`: the name as the disk spells it, and the inode.
  fn entry(&self, dir: u64, name: &OsStr) -> Option<(&OsString, u64)> {
    let entries = self.entries(dir).ok()?;
    let found = if self.ignore_case == self.other_rule.contains(&dir) {
      entries.get_key_value(name)
    } else {
      entries.iter().find(|(spelled, _)| spelled.eq_ignore_ascii_case(name))
    };
    found.map(|(spelled, id)| (spelled, *id))
  }

  fn resolve(&self, path: &Path, follow: bool) -> io::Result<u64> {
    self.walk(ROOT, path, follow, 0)
  }

  /// `resolve` as the engine and the backend see it: nothing is found while the disk is offline.
  fn look_up(&self, path: &Path, follow: bool) -> io::Result<u64> {
    match self.offline {
      Some(kind) => Err(error(kind)),
      None => self.resolve(path, follow),
    }
  }

  /// `lstat` or `stat`: what is at the path.
  fn look(&self, path: &Path, follow: bool) -> io::Result<Meta> {
    if let Some(kind) = self.offline {
      return Err(error(kind));
    }
    Ok(self.meta(self.resolve(path, follow)?))
  }

  fn walk(&self, from: u64, path: &Path, follow: bool, links: u32) -> io::Result<u64> {
    let mut at = from;
    let mut parts = path.components().peekable();
    while let Some(part) = parts.next() {
      match part {
        Component::RootDir => at = ROOT,
        Component::CurDir => {}
        Component::ParentDir => at = self.inodes[&at].parent,
        Component::Prefix(_) => return Err(error(io::ErrorKind::InvalidInput)),
        Component::Normal(name) => {
          self.entries(at)?;
          if self.inodes[&at].denied {
            return Err(error(io::ErrorKind::PermissionDenied));
          }
          let (_, child) = self.entry(at, name).ok_or_else(|| error(io::ErrorKind::NotFound))?;
          at = match &self.inodes[&child].content {
            Content::Link(target) if follow || parts.peek().is_some() => {
              if links >= 32 {
                return Err(link_loop());
              }
              self.walk(at, target, true, links + 1)?
            }
            _ => child,
          };
        }
      }
    }
    Ok(at)
  }

  /// The directory that holds the last name of `path`, and that name: as the disk spells it
  /// if the entry is there, as `path` spells it if not.
  fn parent_of(&self, path: &Path) -> io::Result<(u64, OsString)> {
    let name = path.file_name().ok_or_else(|| error(io::ErrorKind::InvalidInput))?;
    let dir = self.resolve(path.parent().unwrap_or(Path::new("/")), true)?;
    self.entries(dir)?;
    if self.inodes[&dir].denied {
      return Err(error(io::ErrorKind::PermissionDenied));
    }
    let spelled = self.entry(dir, name).map_or(name, |(spelled, _)| spelled.as_os_str());
    Ok((dir, spelled.to_owned()))
  }

  /// The directory at `dir` as a backend finds it when it asks the OS for a watch.
  pub(crate) fn watchable(&self, dir: &Path) -> io::Result<u64> {
    let id = self.look_up(dir, true)?;
    self.entries(id)?;
    if self.inodes[&id].denied { Err(error(io::ErrorKind::PermissionDenied)) } else { Ok(id) }
  }

  /// The path of an inode without any link in it.
  pub(crate) fn real_path(&self, mut id: u64) -> PathBuf {
    let mut names = Vec::new();
    while id != ROOT {
      let inode = &self.inodes[&id];
      names.push(inode.name.as_os_str());
      id = inode.parent;
    }
    let mut path = PathBuf::from("/");
    path.extend(names.into_iter().rev());
    path
  }

  fn meta(&self, id: u64) -> Meta {
    let inode = &self.inodes[&id];
    let (kind, size) = match &inode.content {
      Content::File(bytes) => (FileType::File, bytes.len()),
      Content::Dir(_) => (FileType::Dir, 0),
      Content::Link(target) => (FileType::Symlink, target.as_os_str().len()),
    };
    let birth = if self.birth_times { inode.born } else { 0 };
    Meta {
      kind,
      modified: SystemTime::UNIX_EPOCH + Duration::from_nanos(inode.modified),
      changed: SystemTime::UNIX_EPOCH + Duration::from_nanos(inode.changed),
      size: size as u64,
      file_id: inode.number,
      dir_id: DirId { volume: 1, inode: inode.number, birth },
    }
  }

  fn create(&mut self, dir: u64, name: &OsStr, content: Content) -> u64 {
    let id = self.next_id;
    self.next_id += 1;
    let number = self.free_numbers.pop().unwrap_or_else(|| {
      self.next_number += 1;
      self.next_number
    });
    let is_dir = matches!(content, Content::Dir(_));
    let is_mount = dir == ROOT && self.mount.as_deref() == Some(name);
    if is_dir && (is_mount || self.other_rule.contains(&dir)) {
      self.other_rule.insert(id);
    }
    let now = self.stamp();
    self.inodes.insert(
      id,
      Inode {
        number,
        parent: dir,
        name: name.to_owned(),
        content,
        modified: now,
        changed: now,
        born: now,
        denied: false,
      },
    );
    self.entries_mut(dir).insert(name.to_owned(), id);
    self.touch(dir);
    self.notify(dir, name, Act::Create, is_dir);
    id
  }

  /// The content of the inode changed: for a directory, an entry came or went. That moves the
  /// modification time and the change time.
  fn touch(&mut self, id: u64) {
    let now = self.stamp();
    if let Some(inode) = self.inodes.get_mut(&id) {
      inode.modified = now;
      inode.changed = now;
    }
  }

  /// The entry itself changed, not what is in it: it has another name or other permissions.
  /// That moves only the change time.
  fn touch_entry(&mut self, id: u64) {
    let now = self.stamp();
    if let Some(inode) = self.inodes.get_mut(&id) {
      inode.changed = now;
    }
  }

  /// Takes the entry `name` out of `dir` and frees its inode, with everything below it first.
  /// The OS tells about it under the name `said`.
  fn delete(&mut self, dir: u64, name: &OsStr, said: &OsStr) {
    let Some(id) = self.entries_mut(dir).get(name).copied() else { return };
    let below: Vec<OsString> = match &self.inodes[&id].content {
      Content::Dir(entries) => entries.keys().cloned().collect(),
      _ => Vec::new(),
    };
    for child in &below {
      self.delete(id, child, child);
    }
    self.entries_mut(dir).remove(name);
    self.touch(dir);
    let is_dir = self.forget(id);
    self.notify(dir, said, Act::Remove, is_dir);
  }

  /// Frees an inode that no directory lists any more. Says whether it was a directory.
  fn forget(&mut self, id: u64) -> bool {
    let Some(inode) = self.inodes.remove(&id) else { return false };
    if self.reuse_numbers {
      self.free_numbers.push(inode.number);
    }
    let is_dir = matches!(inode.content, Content::Dir(_));
    if is_dir {
      self.kernel.gone(id);
      self.other_rule.remove(&id);
    }
    is_dir
  }

  pub(crate) fn log_watch(&mut self, call: WatchCall) {
    self.watch_calls.push(call);
  }

  pub(crate) fn log_unwatch(&mut self, key: DirKey) {
    self.calls.unwatch += 1;
    let asked = self.watch_calls.iter().rev().find(|call| call.key == key && call.want.is_some());
    let dir = asked.map(|call| call.dir.clone()).unwrap_or_default();
    self.watch_calls.push(WatchCall { key, dir, want: None });
  }

  fn notify(&mut self, dir: u64, name: &OsStr, act: Act, is_dir: bool) {
    match self.kernel.mode {
      None => {}
      Some(Mode::Inode) => self.kernel.by_inode(dir, name, act, is_dir),
      Some(Mode::Prefix) => {
        let real = self.real_path(dir).join(name);
        self.kernel.by_prefix(&real, act, is_dir);
      }
      Some(Mode::Handle) => {
        // A handle reports everything below its directory, with the path from there.
        let mut rel = PathBuf::from(name);
        let mut at = dir;
        loop {
          self.kernel.by_handle(at, &rel, act, at == dir);
          if at == ROOT {
            break;
          }
          let inode = &self.inodes[&at];
          rel = Path::new(&inode.name).join(rel);
          at = inode.parent;
        }
      }
    }
  }
}

/// The name under which the OS tells that the entry `name` is gone: the last name of the path
/// of the call. An entry that is gone has no spelling of its own any more.
fn said<'a>(path: &'a Path, name: &'a OsStr) -> &'a OsStr {
  path.file_name().unwrap_or(name)
}

struct Inode {
  /// The number programs see. It is given to a new inode as soon as this one is gone.
  number: u64,
  parent: u64,
  name: OsString,
  content: Content,
  modified: u64,
  /// The change time. It moves with `modified`, and also when the entry itself is renamed or
  /// gets other permissions.
  changed: u64,
  born: u64,
  denied: bool,
}

enum Content {
  File(Vec<u8>),
  Dir(BTreeMap<OsString, u64>),
  Link(PathBuf),
}

fn error(kind: io::ErrorKind) -> io::Error {
  io::Error::from(kind)
}

/// The error of a link that leads back to itself. Its kind has no stable name, so it is made
/// from the error number where that is known.
fn link_loop() -> io::Error {
  if cfg!(any(target_os = "linux", target_os = "android")) {
    io::Error::from_raw_os_error(40)
  } else if cfg!(any(target_vendor = "apple", target_os = "freebsd")) {
    io::Error::from_raw_os_error(62)
  } else {
    io::Error::other("too many levels of symbolic links")
  }
}
