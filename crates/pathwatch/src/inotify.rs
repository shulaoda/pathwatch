//! The inotify backend: one kernel watch for each directory. See `docs/design.md` 7.1.

use std::{
  collections::hash_map::Entry,
  ffi::OsStr,
  fs::File,
  io::{self, Read, Write},
  iter,
  os::{
    fd::{AsRawFd, RawFd},
    unix::ffi::OsStrExt,
  },
  path::Path,
  sync::Arc,
  thread::{self, JoinHandle},
  time::{Duration, Instant},
};

use rustc_hash::FxHashMap;

use crate::backend::{Backend, Coverage, DirKey, Hint, Sink, Want, watch_limit};

const ENTRY_EVENTS: u32 =
  libc::IN_CREATE | libc::IN_DELETE | libc::IN_MOVED_FROM | libc::IN_MOVED_TO;
const SELF_EVENTS: u32 = libc::IN_MOVE_SELF | libc::IN_DELETE_SELF;
const CONTENT_EVENTS: u32 = libc::IN_MODIFY | libc::IN_ATTRIB | libc::IN_CLOSE_WRITE;
const WATCH_FLAGS: u32 = libc::IN_EXCL_UNLINK | libc::IN_ONLYDIR | libc::IN_MASK_ADD;
const WATCH_ENDED: u32 = libc::IN_IGNORED | libc::IN_UNMOUNT | libc::IN_DELETE_SELF;
const NEW_CONTENT: u32 = libc::IN_MODIFY | libc::IN_CREATE | libc::IN_MOVED_TO;
const READ_BUFFER_LEN: usize = 64 * 1024;
const READ_INTERVAL: Duration = Duration::from_millis(1);

const ERROR_WATCH_LIMIT: &str =
  "inotify has no watch left for this user (raise fs.inotify.max_user_watches)";
const ERROR_INSTANCE_LIMIT: &str = "inotify has no instance left for this user (raise \
                                    fs.inotify.max_user_instances or the limit of open files)";

#[derive(Debug)]
pub struct InotifyBackend {
  fds: Arc<Fds>,
  reader: Option<JoinHandle<()>>,
  keys_by_wd: FxHashMap<i32, Keys>,
  wd_by_key: FxHashMap<DirKey, i32>,
}

#[derive(Debug)]
struct Keys {
  first: DirKey,
  more: Vec<DirKey>,
}

impl Keys {
  fn iter(&self) -> impl Iterator<Item = DirKey> {
    iter::once(self.first).chain(self.more.iter().copied())
  }
}

#[derive(Debug)]
struct Fds {
  inotify: File,
  stop: File,
}

impl InotifyBackend {
  pub fn new(sink: Sink<Vec<u8>>) -> io::Result<Self> {
    let no_room = |error: io::Error| match error.raw_os_error() {
      Some(libc::EMFILE | libc::ENFILE) => watch_limit(ERROR_INSTANCE_LIMIT),
      _ => error,
    };
    let (inotify, stop) = (ffi::inotify().map_err(no_room)?, ffi::eventfd().map_err(no_room)?);
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

  pub fn inotify_fd(&self) -> RawFd {
    self.fds.inotify.as_raw_fd()
  }

  pub fn wd(&self, key: DirKey) -> Option<i32> {
    self.wd_by_key.get(&key).copied()
  }

  pub fn keys(&self, wd: i32) -> Vec<DirKey> {
    self.keys_by_wd.get(&wd).into_iter().flat_map(Keys::iter).collect()
  }

  pub fn watch_count(&self) -> usize {
    self.keys_by_wd.len()
  }

  fn unbind(&mut self, key: DirKey) {
    let Some(wd) = self.wd_by_key.remove(&key) else { return };
    let Entry::Occupied(mut keys) = self.keys_by_wd.entry(wd) else { return };
    let Keys { first, more } = keys.get_mut();
    if *first != key {
      more.retain(|&other| other != key);
    } else if let Some(next) = more.pop() {
      *first = next;
    } else {
      keys.remove();
      ffi::remove_watch(&self.fds.inotify, wd);
    }
    if self.wd_by_key.len() < self.wd_by_key.capacity() / 4 {
      self.wd_by_key.shrink_to_fit();
      self.keys_by_wd.shrink_to_fit();
    }
  }
}

impl Backend for InotifyBackend {
  type Raw = Vec<u8>;

  fn watch(&mut self, key: DirKey, dir: &Path, want: Want) -> Coverage {
    let events = match want {
      Want::Names => ENTRY_EVENTS | SELF_EVENTS,
      Want::Contents => ENTRY_EVENTS | SELF_EVENTS | CONTENT_EVENTS,
    };
    match ffi::add_watch(&self.fds.inotify, dir, events | WATCH_FLAGS) {
      Ok(wd) if self.wd_by_key.get(&key) == Some(&wd) => Coverage::Watched,
      // A kernel watch belongs to the inode: a second add for it gives the old `wd`.
      Ok(wd) => {
        self.unbind(key);
        self.wd_by_key.insert(key, wd);
        match self.keys_by_wd.entry(wd) {
          Entry::Occupied(mut keys) => keys.get_mut().more.push(key),
          Entry::Vacant(free) => {
            free.insert(Keys { first: key, more: Vec::new() });
          }
        }
        Coverage::Watched
      }
      Err(error) => {
        self.unbind(key);
        let is_limit = error.raw_os_error() == Some(libc::ENOSPC);
        Coverage::Failed(if is_limit { watch_limit(ERROR_WATCH_LIMIT) } else { error })
      }
    }
  }

  fn unwatch(&mut self, key: DirKey) {
    self.unbind(key);
  }

  fn translate(&mut self, raw: Self::Raw, hint: &mut dyn FnMut(Hint<'_>)) {
    let mut rest = raw.as_slice();
    while let Some((event, tail)) = split_event(rest) {
      rest = tail;
      if event.mask & libc::IN_Q_OVERFLOW != 0 {
        hint(Hint::Lost);
        continue;
      }
      if event.mask & WATCH_ENDED != 0 {
        let ended = self.keys_by_wd.remove(&event.wd);
        for key in ended.iter().flat_map(Keys::iter) {
          self.wd_by_key.remove(&key);
          hint(Hint::Changed { dir: key, rel: Path::new("") });
        }
        continue;
      }
      let Some(keys) = self.keys_by_wd.get(&event.wd) else { continue };
      let is_dir = event.mask & libc::IN_ISDIR != 0;
      let is_touched_dir = is_dir && event.mask & libc::IN_ATTRIB != 0;
      if is_touched_dir {
        continue;
      }
      let is_written = !is_dir && event.mask & NEW_CONTENT != 0;
      let rel = Path::new(OsStr::from_bytes(event.name));
      let mut tell = |dir: DirKey| {
        hint(if is_written { Hint::Written { dir, rel } } else { Hint::Changed { dir, rel } });
      };
      tell(keys.first);
      keys.more.iter().copied().for_each(tell);
    }
    let is_cut_off = !rest.is_empty();
    if is_cut_off {
      hint(Hint::Lost);
    }
  }

  fn raw_bytes(raw: &Self::Raw) -> usize {
    raw.len()
  }
}

impl Drop for InotifyBackend {
  fn drop(&mut self) {
    let Some(reader) = self.reader.take() else { return };
    let told_to_stop = (&self.fds.stop).write_all(&1_u64.to_ne_bytes()).is_ok();
    if told_to_stop {
      let _ = reader.join();
    }
  }
}

fn forward(fds: &Fds, sink: &Sink<Vec<u8>>) {
  let mut buffer = vec![0; READ_BUFFER_LEN];
  let mut next_read = Instant::now();
  let error = loop {
    match ffi::wait_until_readable([&fds.inotify, &fds.stop]) {
      Ok([_, true]) => return,
      Ok(_) => {}
      Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
      Err(error) => break error,
    }
    // At most one read a millisecond, unless the last read filled over half of the buffer:
    // a flood of small writes would wake the reader for each.
    thread::sleep(next_read.saturating_duration_since(Instant::now()));
    match (&fds.inotify).read(&mut buffer) {
      Ok(0) => break io::ErrorKind::UnexpectedEof.into(),
      Ok(len) => {
        sink.raw(buffer[..len].to_vec());
        let is_behind = len > READ_BUFFER_LEN / 2;
        next_read = Instant::now() + if is_behind { Duration::ZERO } else { READ_INTERVAL };
      }
      Err(error)
        if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
      Err(error) => break error,
    }
  };
  sink.failed(error);
}

struct Event<'a> {
  wd: i32,
  mask: u32,
  name: &'a [u8],
}

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

mod ffi {
  use std::{
    ffi::CString,
    fs::File,
    io,
    os::{
      fd::{AsRawFd, FromRawFd},
      unix::ffi::OsStrExt,
    },
    path::Path,
  };

  pub(super) fn inotify() -> io::Result<File> {
    // SAFETY: the call takes no memory of ours.
    owned(unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) })
  }

  pub(super) fn eventfd() -> io::Result<File> {
    // SAFETY: the call takes no memory of ours.
    owned(unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) })
  }

  pub(super) fn add_watch(inotify: &File, dir: &Path, mask: u32) -> io::Result<i32> {
    let path = CString::new(dir.as_os_str().as_bytes())?;
    // SAFETY: `path` ends with a NUL, and the call only reads it.
    let wd = unsafe { libc::inotify_add_watch(inotify.as_raw_fd(), path.as_ptr(), mask) };
    if wd < 0 { Err(io::Error::last_os_error()) } else { Ok(wd) }
  }

  pub(super) fn remove_watch(inotify: &File, wd: i32) {
    // The libc of Android takes the descriptor as u32.
    #[cfg(target_os = "android")]
    let wd = wd.cast_unsigned();
    // SAFETY: the call takes no memory of ours.
    unsafe { libc::inotify_rm_watch(inotify.as_raw_fd(), wd) };
  }

  pub(super) fn wait_until_readable(files: [&File; 2]) -> io::Result<[bool; 2]> {
    let mut polled =
      files.map(|file| libc::pollfd { fd: file.as_raw_fd(), events: libc::POLLIN, revents: 0 });
    // SAFETY: `polled` holds two `pollfd`, and the call writes into these two only.
    if unsafe { libc::poll(polled.as_mut_ptr(), 2, -1) } < 0 {
      return Err(io::Error::last_os_error());
    }
    Ok(polled.map(|file| file.revents != 0))
  }

  fn owned(fd: libc::c_int) -> io::Result<File> {
    if fd < 0 {
      return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just opened by the caller, and nothing else owns it.
    Ok(unsafe { File::from_raw_fd(fd) })
  }
}

#[cfg(test)]
mod tests {
  use std::{
    fs::File,
    os::{fd::OwnedFd, unix::net::UnixStream},
    sync::mpsc::{self, TryRecvError},
  };

  use super::{Fds, forward};
  use crate::backend::{Signal, Sink};

  #[test]
  fn read_error_is_reported_and_ends_the_reader() {
    let dir = tempfile::tempdir().unwrap();
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
