//! The inotify backend (Linux, Android).
//!
//! One kernel watch for one directory; it does not reach the directories below. The kernel ties
//! a watch to the inode, not to the path, so one watch can serve several keys.

use std::{
  ffi::{CString, OsStr},
  fs::File,
  io::{self, Read, Write},
  os::{
    fd::{AsRawFd, FromRawFd, RawFd},
    unix::ffi::OsStrExt,
  },
  path::Path,
  sync::Arc,
  thread::{self, JoinHandle},
};

use rustc_hash::FxHashMap;

use crate::backend::{Backend, Coverage, DirKey, Hint, Sink, Want, watch_limit};

/// A name in the directory comes or goes. Every watch asks for these.
const ENTRY_EVENTS: u32 =
  libc::IN_CREATE | libc::IN_DELETE | libc::IN_MOVED_FROM | libc::IN_MOVED_TO;

/// The watched directory itself is moved or deleted. Every watch asks for these, because the
/// parent directory, which would tell the same, does not always have a watch.
const SELF_EVENTS: u32 = libc::IN_MOVE_SELF | libc::IN_DELETE_SELF;

/// A file in the directory is written or touched. Only `Want::Contents` asks for these.
/// OPEN and ACCESS are in no list: a scan or a read by the engine must not come back as an event.
const CONTENT_EVENTS: u32 = libc::IN_MODIFY | libc::IN_ATTRIB | libc::IN_CLOSE_WRITE;

/// - `EXCL_UNLINK`: a write through an old descriptor of a replaced file would be reported under
///   the name of the new file.
/// - `ONLYDIR`: a path that turned into a file fails instead of getting a watch.
/// - `MASK_ADD`: without it a second call for one directory replaces the events of the first.
const WATCH_FLAGS: u32 = libc::IN_EXCL_UNLINK | libc::IN_ONLYDIR | libc::IN_MASK_ADD;

/// Big enough that a flood is drained in few reads. One event needs at most 272 bytes.
const READ_BUFFER_LEN: usize = 64 * 1024;

/// Watches directories with inotify. A `wd` (watch descriptor) is the number that the kernel
/// gives to one watch.
///
/// Only the watcher thread uses the two maps, so nothing needs a lock.
#[derive(Debug)]
pub struct InotifyBackend {
  fds: Arc<Fds>,
  reader: Option<JoinHandle<()>>,
  keys_by_wd: FxHashMap<i32, Vec<DirKey>>,
  wd_by_key: FxHashMap<DirKey, i32>,
}

/// What the watcher thread and the reader thread share.
#[derive(Debug)]
struct Fds {
  inotify: File,
  /// An eventfd. One write to it ends the reader thread.
  stop: File,
}

impl InotifyBackend {
  /// Opens inotify and starts the reader thread.
  pub fn new(sink: Sink<Vec<u8>>) -> io::Result<Self> {
    // SAFETY: the call takes no pointer.
    let inotify = owned(unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) })?;
    // SAFETY: the call takes no pointer.
    let stop = owned(unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) })?;
    let fds = Arc::new(Fds { inotify, stop });
    let reader = thread::Builder::new().name("pathwatch-inotify".to_owned()).spawn({
      let fds = Arc::clone(&fds);
      move || forward(&fds, &sink)
    })?;
    Ok(Self {
      fds,
      reader: Some(reader),
      keys_by_wd: FxHashMap::default(),
      wd_by_key: FxHashMap::default(),
    })
  }
}

/// The reader thread: copies what the kernel queued into the sink until the backend is dropped.
fn forward(fds: &Fds, sink: &Sink<Vec<u8>>) {
  let mut buffer = vec![0; READ_BUFFER_LEN];
  let error = loop {
    let mut polled = [&fds.inotify, &fds.stop].map(|file| libc::pollfd {
      fd: file.as_raw_fd(),
      events: libc::POLLIN,
      revents: 0,
    });
    // SAFETY: `polled` is a live array of two entries.
    if unsafe { libc::poll(polled.as_mut_ptr(), 2, -1) } < 0 {
      let error = io::Error::last_os_error();
      if error.kind() == io::ErrorKind::Interrupted {
        continue;
      }
      break error;
    }
    // The stop signal: the backend is being dropped.
    if polled[1].revents != 0 {
      return;
    }
    // One read for each `poll`, so that a flood cannot keep the thread away from the stop signal.
    match (&fds.inotify).read(&mut buffer) {
      Ok(0) => break io::ErrorKind::UnexpectedEof.into(),
      Ok(len) => sink.raw(buffer[..len].to_vec()),
      Err(error)
        if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
      Err(error) => break error,
    }
  };
  sink.failed(error);
}

impl Backend for InotifyBackend {
  /// The bytes of one `read`: one event or more.
  type Raw = Vec<u8>;

  fn watch(&mut self, key: DirKey, dir: &Path, want: Want) -> Coverage {
    match self.add_watch(dir, want) {
      Ok(wd) => {
        self.bind(key, wd);
        Coverage::Watched
      }
      Err(error) => {
        // `Failed` says that nothing is watched for the key. A watch that it still has is on a
        // directory that is no longer at `dir`, or that cannot be read any more.
        self.unbind(key);
        Coverage::Failed(error)
      }
    }
  }

  fn unwatch(&mut self, key: DirKey) {
    self.unbind(key);
  }

  /// A watch is live when `watch` returns.
  fn commit(&mut self) -> io::Result<Vec<DirKey>> {
    Ok(Vec::new())
  }

  fn translate(&mut self, raw: Self::Raw, hint: &mut dyn FnMut(Hint<'_>)) {
    let mut rest = raw.as_slice();
    while let Some((event, tail)) = split_event(rest) {
      rest = tail;
      self.translate_event(&event, hint);
    }
    if !rest.is_empty() {
      // A cut-off event. The kernel never hands one out, but if it did, events would be missing.
      hint(Hint::Lost);
    }
  }

  fn raw_bytes(raw: &Self::Raw) -> usize {
    raw.len()
  }
}

impl InotifyBackend {
  /// Asks the kernel to watch the directory at `dir`. For a directory that has a watch, the
  /// kernel gives the same `wd` again.
  fn add_watch(&self, dir: &Path, want: Want) -> io::Result<i32> {
    let path = CString::new(dir.as_os_str().as_bytes())?;
    let events = match want {
      Want::Names => ENTRY_EVENTS | SELF_EVENTS,
      Want::Contents => ENTRY_EVENTS | SELF_EVENTS | CONTENT_EVENTS,
    };
    // SAFETY: `path` is a NUL-terminated string that lives until the call returns.
    let wd = unsafe {
      libc::inotify_add_watch(self.fds.inotify.as_raw_fd(), path.as_ptr(), events | WATCH_FLAGS)
    };
    if wd < 0 { Err(watch_error(io::Error::last_os_error())) } else { Ok(wd) }
  }

  /// Puts `key` on the watch `wd`. If it was on another watch, another directory is at its path
  /// now, and the key leaves the old watch.
  fn bind(&mut self, key: DirKey, wd: i32) {
    if self.wd_by_key.get(&key) == Some(&wd) {
      return;
    }
    self.unbind(key);
    self.wd_by_key.insert(key, wd);
    self.keys_by_wd.entry(wd).or_default().push(key);
  }

  /// Forgets `key`. The kernel watch goes with its last key.
  fn unbind(&mut self, key: DirKey) {
    let Some(wd) = self.wd_by_key.remove(&key) else { return };
    let Some(keys) = self.keys_by_wd.get_mut(&wd) else { return };
    keys.retain(|&other| other != key);
    if !keys.is_empty() {
      return;
    }
    self.keys_by_wd.remove(&wd);
    #[cfg(target_os = "android")]
    let wd = wd.cast_unsigned();
    // The result is not checked: when the directory was removed, the kernel has dropped the
    // watch already.
    // SAFETY: the call takes no pointer.
    unsafe { libc::inotify_rm_watch(self.fds.inotify.as_raw_fd(), wd) };
  }

  /// The hints for one event of the kernel.
  fn translate_event(&mut self, event: &Event<'_>, hint: &mut dyn FnMut(Hint<'_>)) {
    if event.mask & libc::IN_Q_OVERFLOW != 0 {
      hint(Hint::Lost);
      return;
    }
    if event.mask & (libc::IN_IGNORED | libc::IN_UNMOUNT | libc::IN_DELETE_SELF) != 0 {
      // The kernel dropped the watch, and it never gives this number out again. IGNORED comes
      // right after each of the other two, and finds no key then.
      for key in self.keys_by_wd.remove(&event.wd).unwrap_or_default() {
        self.wd_by_key.remove(&key);
        hint(Hint::Changed { dir: key, rel: Path::new("") });
      }
      return;
    }
    // No keys: the event was in the queue when the last key of its watch went.
    let Some(keys) = self.keys_by_wd.get(&event.wd) else { return };
    if event.mask & libc::IN_MOVE_SELF != 0 {
      // The directory is no longer at the path of its keys. The watch stays: it follows the
      // directory, so its events still come for these keys until the engine calls `watch`
      // again. Then `bind` takes the key to the directory that is at the path now.
      for &key in keys {
        hint(Hint::Changed { dir: key, rel: Path::new("") });
      }
      return;
    }
    let is_dir = event.mask & libc::IN_ISDIR != 0;
    if is_dir && event.mask & libc::IN_ATTRIB != 0 {
      // A chmod or touch of a directory. `Changed` for it would make the engine scan
      // everything below it.
      return;
    }
    // After these the name holds other content, even when size and times are the same.
    let written =
      !is_dir && event.mask & (libc::IN_MODIFY | libc::IN_CREATE | libc::IN_MOVED_TO) != 0;
    let rel = Path::new(OsStr::from_bytes(event.name));
    for &key in keys {
      if written {
        hint(Hint::Written { dir: key, rel });
      } else {
        hint(Hint::Changed { dir: key, rel });
      }
    }
  }
}

impl Drop for InotifyBackend {
  fn drop(&mut self) {
    let Some(reader) = self.reader.take() else { return };
    // The reader sits in `poll`, and closing the inotify descriptor would not wake it. If the
    // stop signal cannot be sent, the thread is left alone: a join would never return.
    if (&self.fds.stop).write_all(&1_u64.to_ne_bytes()).is_ok() {
      let _ = reader.join();
    }
  }
}

/// Hooks for tests: what the backend knows.
impl InotifyBackend {
  /// The inotify descriptor. `/proc/self/fdinfo` shows the kernel watches of it.
  pub fn inotify_fd(&self) -> RawFd {
    self.fds.inotify.as_raw_fd()
  }

  /// The kernel watch that `key` is on.
  pub fn wd(&self, key: DirKey) -> Option<i32> {
    self.wd_by_key.get(&key).copied()
  }

  /// The keys of the kernel watch `wd`.
  pub fn keys(&self, wd: i32) -> &[DirKey] {
    self.keys_by_wd.get(&wd).map_or(&[], Vec::as_slice)
  }

  /// How many kernel watches the backend knows.
  pub fn watch_count(&self) -> usize {
    self.keys_by_wd.len()
  }
}

/// One event of the kernel.
struct Event<'a> {
  wd: i32,
  mask: u32,
  /// The name of the entry in the watched directory. Empty when the event is about the
  /// directory itself.
  name: &'a [u8],
}

/// Splits the first event off a buffer that `read` filled: a 16 byte header (wd, mask, cookie,
/// name length), then the name, padded with NUL bytes. `None` at the end and for a cut-off event.
fn split_event(raw: &[u8]) -> Option<(Event<'_>, &[u8])> {
  let (wd, rest) = raw.split_first_chunk()?;
  let (mask, rest) = rest.split_first_chunk()?;
  let (_cookie, rest) = rest.split_first_chunk::<4>()?;
  let (name_len, rest) = rest.split_first_chunk()?;
  let (name, rest) = rest.split_at_checked(usize::try_from(u32::from_ne_bytes(*name_len)).ok()?)?;
  let name_end = name.iter().position(|&byte| byte == 0).unwrap_or(name.len());
  let event =
    Event { wd: i32::from_ne_bytes(*wd), mask: u32::from_ne_bytes(*mask), name: &name[..name_end] };
  Some((event, rest))
}

/// Takes over a descriptor that a system call just returned, or reads its error.
fn owned(fd: libc::c_int) -> io::Result<File> {
  if fd < 0 {
    return Err(io::Error::last_os_error());
  }
  // SAFETY: both callers pass what a system call just returned. It is not negative, so it is an
  // open descriptor, and nothing else owns it yet.
  Ok(unsafe { File::from_raw_fd(fd) })
}

/// The error of a failed `inotify_add_watch`. At the watch limit the OS says "No space left on
/// device", which points at the disk, so that case gets a text of its own.
fn watch_error(error: io::Error) -> io::Error {
  if error.raw_os_error() == Some(libc::ENOSPC) {
    return watch_limit("the inotify watch limit is reached (fs.inotify.max_user_watches)");
  }
  error
}

#[cfg(test)]
mod tests {
  use std::{
    fs::File,
    io,
    os::{fd::OwnedFd, unix::net::UnixStream},
    sync::mpsc::{self, TryRecvError},
  };

  use super::{Fds, forward, watch_error};
  use crate::backend::{Signal, Sink, is_watch_limit};

  #[test]
  fn watch_limit_error_names_the_sysctl() {
    let error = watch_error(io::Error::from_raw_os_error(libc::ENOSPC));
    assert!(is_watch_limit(&error));
    assert!(error.to_string().contains("fs.inotify.max_user_watches"), "{error}");

    let other = watch_error(io::Error::from_raw_os_error(libc::EACCES));
    assert_eq!(other.raw_os_error(), Some(libc::EACCES));
  }

  #[test]
  fn read_error_is_reported_and_ends_the_reader() {
    let dir = tempfile::tempdir().unwrap();
    // A directory stands in for a broken inotify descriptor: `poll` calls it readable and
    // `read` fails. The silent end of a socket pair stands in for the eventfd.
    let (stop, _peer) = UnixStream::pair().unwrap();
    let fds =
      Fds { inotify: File::open(dir.path()).unwrap(), stop: File::from(OwnedFd::from(stop)) };
    let (sender, signals) = mpsc::channel();
    let sink = Sink::new(move |signal| {
      let _ = sender.send(signal);
    });

    forward(&fds, &sink);

    let Ok(Signal::Failed(error)) = signals.try_recv() else { panic!("no failure was reported") };
    assert_eq!(error.raw_os_error(), Some(libc::EISDIR));
    assert!(matches!(signals.try_recv(), Err(TryRecvError::Empty)));
  }
}
