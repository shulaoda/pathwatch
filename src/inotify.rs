//! The inotify backend: one kernel watch for each directory. See `references/design.md` 7.1.

use std::{
  collections::hash_map::Entry,
  ffi::OsStr,
  fs::File,
  io::{self, Read, Seek, SeekFrom, Write},
  iter,
  os::{
    fd::{AsRawFd, RawFd},
    unix::ffi::OsStrExt,
  },
  path::Path,
  sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
  },
  thread::{self, JoinHandle},
  time::{Duration, Instant, SystemTime},
};

use rustc_hash::FxHashMap;

use crate::{
  backend::{Backend, Coverage, DirKey, Hint, Sink, Want, give_room_back, watch_limit},
  fs::nanos,
};

const ENTRY_EVENTS: u32 =
  libc::IN_CREATE | libc::IN_DELETE | libc::IN_MOVED_FROM | libc::IN_MOVED_TO;
const SELF_EVENTS: u32 = libc::IN_MOVE_SELF | libc::IN_DELETE_SELF;
const CONTENT_EVENTS: u32 = libc::IN_MODIFY | libc::IN_ATTRIB | libc::IN_CLOSE_WRITE;
const WATCH_FLAGS: u32 = libc::IN_EXCL_UNLINK | libc::IN_ONLYDIR | libc::IN_MASK_ADD;
const WATCH_ENDED: u32 = libc::IN_IGNORED | libc::IN_UNMOUNT | libc::IN_DELETE_SELF;
const NEW_CONTENT: u32 = libc::IN_MODIFY | libc::IN_CREATE | libc::IN_MOVED_TO;
const READ_BUFFER_LEN: usize = 64 * 1024;
const READ_INTERVAL: Duration = Duration::from_millis(1);
/// The kernel tells of each mount and unmount of the namespace by a priority event on it.
const MOUNTS: &str = "/proc/self/mountinfo";
/// The record of a mount point that the reader makes: no watch has this number, and an
/// overflow has -1.
const MOUNT_WD: i32 = -2;
/// The record the reader makes once it read the queue empty: every event of a change before the
/// time in its name (nanoseconds since the epoch, in digits) came before it.
const DELIVERED_WD: i32 = -3;

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
  /// `MOUNTS`, where it can be opened.
  mounts: Option<File>,
  /// Wakes the reader for a question of the engine.
  ask: File,
  /// The time the engine asked about (`Backend::ask_delivered`), in nanoseconds since the epoch.
  /// Zero: none.
  asked: AtomicU64,
}

impl InotifyBackend {
  pub fn new(sink: Sink<Vec<u8>>) -> io::Result<Self> {
    let no_room = |error: io::Error| match error.raw_os_error() {
      Some(libc::EMFILE | libc::ENFILE) => watch_limit(ERROR_INSTANCE_LIMIT),
      _ => error,
    };
    let (inotify, stop) = (ffi::inotify().map_err(no_room)?, ffi::eventfd().map_err(no_room)?);
    let ask = ffi::eventfd().map_err(no_room)?;
    let mounts = File::open(MOUNTS).ok();
    let known = mounts.as_ref().and_then(read_mounts).unwrap_or_default();
    let fds = Arc::new(Fds { inotify, stop, mounts, ask, asked: AtomicU64::new(0) });
    let reader = thread::Builder::new().name("pathwatch-inotify".to_owned()).spawn({
      let fds = Arc::clone(&fds);
      move || forward(&fds, known, &sink)
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
    self.shrink();
  }

  /// The two tables are given back when most of their keys are gone.
  fn shrink(&mut self) {
    give_room_back(&mut self.wd_by_key);
    give_room_back(&mut self.keys_by_wd);
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
      if event.wd == DELIVERED_WD {
        let nanos = str::from_utf8(event.name).ok().and_then(|digits| digits.parse().ok());
        if let Some(nanos) = nanos {
          hint(Hint::Delivered { before: SystemTime::UNIX_EPOCH + Duration::from_nanos(nanos) });
        }
        continue;
      }
      if event.wd == MOUNT_WD {
        hint(Hint::Mount { path: Path::new(OsStr::from_bytes(event.name)) });
        continue;
      }
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
        self.shrink();
        continue;
      }
      let Some(keys) = self.keys_by_wd.get(&event.wd) else { continue };
      let is_dir = event.mask & libc::IN_ISDIR != 0;
      // A directory that was touched (`ATTRIB`) or "modified" (`MODIFY`, which some tools get
      // for a directory): its entries that came or went have records of their own.
      let is_touched_dir = is_dir && event.mask & (libc::IN_ATTRIB | libc::IN_MODIFY) != 0;
      if is_touched_dir {
        continue;
      }
      let is_written = !is_dir && event.mask & NEW_CONTENT != 0;
      let new = event.mask & libc::IN_MODIFY == 0;
      let rel = Path::new(OsStr::from_bytes(event.name));
      let tell = |dir: DirKey| {
        hint(if is_written { Hint::Written { dir, rel, new } } else { Hint::Changed { dir, rel } });
      };
      keys.iter().for_each(tell);
    }
    let is_cut_off = !rest.is_empty();
    if is_cut_off {
      hint(Hint::Lost);
    }
  }

  fn raw_bytes(raw: &Self::Raw) -> usize {
    raw.len()
  }

  fn ask_delivered(&mut self, after: SystemTime) {
    self.fds.asked.fetch_max(nanos(after).max(1), Ordering::Relaxed);
    let _ = (&self.fds.ask).write_all(&1_u64.to_ne_bytes());
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

/// `known`: the mounts when the backend started.
fn forward(fds: &Fds, mut known: Vec<(u64, Vec<u8>)>, sink: &Sink<Vec<u8>>) {
  let mut buffer = vec![0; READ_BUFFER_LEN];
  let mut next_read = Instant::now();
  let error = loop {
    // A question of the engine wakes the reader at its time.
    let asked = fds.asked.load(Ordering::Relaxed);
    let until = (asked != 0).then(|| asked.saturating_sub(nanos(SystemTime::now())));
    let ready =
      ffi::wait_until_ready(&fds.inotify, &fds.stop, fds.mounts.as_ref(), &fds.ask, until);
    let mounted = match ready {
      Ok([_, true, _, _]) => return,
      Ok([_, _, mounted, asks]) => {
        if asks {
          let _ = (&fds.ask).read(&mut [0; 8]);
        }
        mounted
      }
      Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
      Err(error) => break error,
    };
    // inotify says nothing of a volume that is mounted over a watched directory or one above:
    // the mount points that came or went are told by their paths. In every wake-up that says
    // so, as the wait that sees a change of the mounts takes the word, which comes once; and
    // before an answer below, which is about every change before its time.
    if mounted && let Some(mounts) = fds.mounts.as_ref().and_then(read_mounts) {
      let raw = mount_records(&known, &mounts);
      known = mounts;
      if !raw.is_empty() {
        sink.raw(raw);
      }
    }
    // Its time came: the queue is read empty, and a record says so. A later question stays.
    if asked != 0 && nanos(SystemTime::now()) >= asked {
      if let Err(error) = read_empty(&fds.inotify, &mut buffer, sink) {
        break error;
      }
      let _ = fds.asked.compare_exchange(asked, 0, Ordering::Relaxed, Ordering::Relaxed);
      continue;
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

/// Reads until the queue is empty, then sends a record with the time the last read began: the
/// kernel queues an event before the call that made the change returns, the loss report too, so
/// every event of a change before that time is sent (design.md 7.1).
fn read_empty(inotify: &File, buffer: &mut [u8], sink: &Sink<Vec<u8>>) -> io::Result<()> {
  loop {
    let began = nanos(SystemTime::now());
    match (&*inotify).read(buffer) {
      Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
      Ok(len) => sink.raw(buffer[..len].to_vec()),
      Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
        let mut raw = Vec::new();
        push_record(&mut raw, DELIVERED_WD, began.to_string().as_bytes());
        sink.raw(raw);
        return Ok(());
      }
      Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
      Err(error) => return Err(error),
    }
  }
}

/// The mounts of the namespace, each by its id with its mount point, in order. Mount ids and
/// not only points: a volume mounted over another one at the same point is a change too.
// The file stays open for the poll of the mounts, and is read again from its start: a read by
// path (`fs::read`) could open another file under the name.
#[expect(clippy::verbose_file_reads, reason = "the open file is read again, not a path")]
fn read_mounts(mut file: &File) -> Option<Vec<(u64, Vec<u8>)>> {
  let mut text = Vec::new();
  file.seek(SeekFrom::Start(0)).ok()?;
  file.read_to_end(&mut text).ok()?;
  let mut mounts: Vec<(u64, Vec<u8>)> = text
    .split(|&byte| byte == b'\n')
    .filter_map(|line| {
      let mut fields = line.split(|&byte| byte == b' ');
      let id = str::from_utf8(fields.next()?).ok()?.parse().ok()?;
      Some((id, unescape(fields.nth(3)?)))
    })
    .collect();
  mounts.sort_unstable();
  Some(mounts)
}

/// A mount point with its space, tab, newline and backslash as the kernel writes them: `\040`.
fn unescape(field: &[u8]) -> Vec<u8> {
  let mut text = Vec::with_capacity(field.len());
  let mut rest = field;
  while let Some((&byte, tail)) = rest.split_first() {
    let octal = tail
      .first_chunk::<3>()
      .filter(|digits| byte == b'\\' && digits.iter().all(|digit| (b'0'..=b'7').contains(digit)));
    if let Some(digits) = octal {
      text.push(digits.iter().fold(0_u8, |sum, digit| sum.wrapping_mul(8) + (digit - b'0')));
      rest = &tail[3..];
    } else {
      text.push(byte);
      rest = tail;
    }
  }
  text
}

/// One record for each mount point that came or went, laid out as the kernel lays out an event.
fn mount_records(old: &[(u64, Vec<u8>)], new: &[(u64, Vec<u8>)]) -> Vec<u8> {
  let came = new.iter().filter(|mount| old.binary_search(mount).is_err());
  let went = old.iter().filter(|mount| new.binary_search(mount).is_err());
  let mut points: Vec<&[u8]> = came.chain(went).map(|(_, point)| point.as_slice()).collect();
  points.sort_unstable();
  points.dedup();
  let mut raw = Vec::new();
  for point in points {
    push_record(&mut raw, MOUNT_WD, point);
  }
  raw
}

/// A record of the reader, laid out as the kernel lays out an event: a number that no watch has,
/// and a name.
fn push_record(raw: &mut Vec<u8>, wd: i32, name: &[u8]) {
  let name_len = u32::try_from(name.len() + 1).unwrap_or(u32::MAX);
  for field in [wd.to_ne_bytes(), [0; 4], [0; 4], name_len.to_ne_bytes()] {
    raw.extend_from_slice(&field);
  }
  raw.extend_from_slice(name);
  raw.push(0);
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

  /// Whether `inotify` and `stop` are readable, whether the mounts changed, and whether `ask`
  /// is readable. A file of the mounts always reads, and tells of a change by a priority event.
  /// `nanos`: the longest wait (rounded up to a millisecond); `None` waits until one is ready.
  pub(super) fn wait_until_ready(
    inotify: &File,
    stop: &File,
    mounts: Option<&File>,
    ask: &File,
    nanos: Option<u64>,
  ) -> io::Result<[bool; 4]> {
    let wait = |fd: i32, events: i16| libc::pollfd { fd, events, revents: 0 };
    // A negative descriptor is left out.
    let mounts = mounts.map_or(-1, AsRawFd::as_raw_fd);
    let mut polled = [
      wait(inotify.as_raw_fd(), libc::POLLIN),
      wait(stop.as_raw_fd(), libc::POLLIN),
      wait(mounts, libc::POLLPRI),
      wait(ask.as_raw_fd(), libc::POLLIN),
    ];
    let millis = nanos.map(|nanos| nanos.div_ceil(1_000_000));
    let timeout = millis.map_or(-1, |millis| i32::try_from(millis).unwrap_or(i32::MAX));
    // SAFETY: `polled` holds four `pollfd`, and the call writes into these four only.
    if unsafe { libc::poll(polled.as_mut_ptr(), 4, timeout) } < 0 {
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
    sync::{
      atomic::AtomicU64,
      mpsc::{self, TryRecvError},
    },
  };

  use super::{Fds, ffi, forward};
  use crate::backend::{Signal, Sink};

  #[test]
  fn read_error_is_reported_and_ends_the_reader() {
    let dir = tempfile::tempdir().unwrap();
    let (stop, _peer) = UnixStream::pair().unwrap();
    let inotify = File::open(dir.path()).unwrap();
    let (stop, ask) = (File::from(OwnedFd::from(stop)), ffi::eventfd().unwrap());
    let fds = Fds { inotify, stop, mounts: None, ask, asked: AtomicU64::new(0) };
    let (sender, signals) = mpsc::channel();
    let sink = Sink::new(move |signal| {
      let _ = sender.send(signal);
    });

    forward(&fds, Vec::new(), &sink);

    let Ok(Signal::Failed(error)) = signals.try_recv() else { panic!("no failure was reported") };
    assert_eq!(error.raw_os_error(), Some(libc::EISDIR));
    assert!(matches!(signals.try_recv(), Err(TryRecvError::Empty)));
  }
}
