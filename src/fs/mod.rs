//! How the engine sees the disk: the trait `Fs`, and `StdFs`, the real disk. What only macOS
//! has is in `apple.rs`, and what only Windows has in `windows.rs`. The end of this file holds
//! the calls of the systems other than Windows.

#[cfg(target_vendor = "apple")]
mod apple;
#[cfg(windows)]
mod windows;

use std::{
  ffi::{OsStr, OsString},
  fs::{self, Metadata},
  hash::Hasher,
  io::{self, Read},
  path::{Path, PathBuf},
  sync::OnceLock,
  time::{Duration, Instant, SystemTime},
};

// The numbers differ between systems (`0x0100` is `O_CREAT` on illumos); `apple.rs` has the
// ones of macOS written out.
#[cfg(all(unix, not(target_vendor = "apple")))]
use libc::{ELOOP, O_NOFOLLOW, O_NONBLOCK};
use rustc_hash::FxHasher;

#[cfg(target_vendor = "apple")]
pub(crate) use self::apple::name_and_kind;
#[cfg(target_vendor = "apple")]
use self::apple::{ELOOP, O_NOFOLLOW, O_NONBLOCK, name_on_disk, remote_disk};
#[cfg(windows)]
pub(crate) use self::windows::{SHARE_ALL, is_delete_pending, kernel32, literal, wide_name};
#[cfg(windows)]
use self::windows::{name_on_disk, open_regular, stat, told};

/// Every call that the engine makes to the disk. `StdFs` is the real one; the tests have one
/// in memory.
pub trait Fs {
  /// Does not follow a symbolic link at the end of `path`.
  fn lstat(&self, path: &Path) -> io::Result<Meta>;

  /// Follows symbolic links.
  fn stat(&self, path: &Path) -> io::Result<Meta>;

  /// Calls `each` with the name of every entry of `dir`, and what the listing tells of it.
  fn read_dir(&self, dir: &Path, each: &mut dyn FnMut(&OsStr, Entry)) -> io::Result<()>;

  /// A hash of the content of the regular file at `path`, which reads up to `limit` bytes.
  /// `None`: the file holds more than `limit` bytes, or no regular file is at the path (a link,
  /// which is never followed; a FIFO or a device, which the open does not wait for).
  fn hash(&self, path: &Path, limit: u64) -> io::Result<Option<u64>>;

  /// Where the symbolic link at `path` points: the text that is stored in the link.
  fn link_target(&self, path: &Path) -> io::Result<PathBuf>;

  /// The path that `path` leads to: no link in it, and every name as the disk spells it.
  fn real_path(&self, path: &Path) -> io::Result<PathBuf>;

  /// The last name of `path` as the disk spells it; a link at the end is not followed. Asked
  /// only where every disk takes two spellings of a name for one (macOS, Windows).
  fn name_on_disk(&self, path: &Path) -> io::Result<OsString>;

  /// The clock that the modification times come from.
  fn now(&self) -> SystemTime;

  /// A clock that only goes forward, counted from any start: the engine reads from it how long
  /// its looks at the disk took. The clock of the modification times can be set, and a step of
  /// it in the middle of a flush would count as time that the flush took.
  fn ticks(&self) -> Duration {
    self.now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default()
  }

  /// The kind of the file system of `dir` (`nfs`), when other machines can change it too: the OS
  /// reports only what changes on this machine there.
  fn remote_disk(&self, _dir: &Path) -> Option<String> {
    None
  }

  /// Whether a listing can be behind `lstat` for a file that its writer still has open (NTFS).
  /// Then the engine takes a file from a listing only where an OS watch reports the write.
  fn listing_lags(&self) -> bool {
    false
  }
}

/// What one `lstat` or `stat` tells about an entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Meta {
  pub kind: FileType,
  pub modified: SystemTime,
  pub size: u64,
  /// The inode on Unix, the file index on Windows.
  pub file_id: u64,
  pub dir_id: DirId,
}

/// What a listing gives of an entry besides its name.
#[derive(Clone, Copy, Debug)]
pub enum Entry {
  Name,
  /// Only "directory or not" is read from it; the exact kind comes from an `lstat`.
  Kind(FileType),
  /// A regular file with what `lstat` would tell of it, so no `lstat` is asked.
  File {
    modified: SystemTime,
    size: u64,
    file_id: u64,
  },
  /// A directory that is no link, with its identity and its modification time, so no `lstat`
  /// is asked.
  Dir {
    id: DirId,
    modified: SystemTime,
  },
}

/// What kind of entry is at a path.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FileType {
  /// A regular file. Only this kind is ever opened.
  File,
  Dir,
  Symlink,
  /// A pipe, a socket, a device.
  Other,
}

/// Tells one directory from another at the same path. It is used to find more changes, never
/// to skip a check.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct DirId {
  pub volume: u64,
  pub inode: u64,
  /// Birth time in nanoseconds, zero where the platform gives none.
  pub birth: u64,
}

/// The real file system.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdFs;

impl Fs for StdFs {
  fn lstat(&self, path: &Path) -> io::Result<Meta> {
    stat(path, false)
  }

  fn stat(&self, path: &Path) -> io::Result<Meta> {
    stat(path, true)
  }

  fn read_dir(&self, dir: &Path, each: &mut dyn FnMut(&OsStr, Entry)) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    if apple::read_dir(dir, each)? {
      return Ok(());
    }
    #[cfg(windows)]
    if windows::read_dir(dir, each)? {
      return Ok(());
    }
    for entry in fs::read_dir(literal(dir)).map_err(told)? {
      let entry = entry.map_err(told)?;
      let name = entry.file_name();
      #[cfg(windows)]
      if windows::has_separator(&name) {
        continue;
      }
      let listed = match entry.file_type() {
        Ok(kind) if kind.is_dir() => Entry::Kind(FileType::Dir),
        Ok(_) => Entry::Kind(FileType::File),
        Err(_) => Entry::Name,
      };
      each(&name, listed);
    }
    Ok(())
  }

  fn hash(&self, path: &Path, limit: u64) -> io::Result<Option<u64>> {
    let Some(mut file) = open_regular(path)? else { return Ok(None) };
    let mut hasher = FxHasher::default();
    let mut buffer = [0; 16 * 1024];
    let mut total: u64 = 0;
    loop {
      // The buffer is filled before it is hashed: the hash must not depend on how many bytes
      // one `read` call gives.
      let mut filled = 0;
      while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
          Ok(0) => break,
          Ok(count) => filled += count,
          Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
          Err(error) => return Err(error),
        }
      }
      // The limit ends the read, not the end of the file: a file that grew since the look, or
      // one without an end, is not read whole.
      total = total.saturating_add(u64::try_from(filled).unwrap_or(u64::MAX));
      if total > limit {
        return Ok(None);
      }
      hasher.write(&buffer[..filled]);
      if filled < buffer.len() {
        return Ok(Some(hasher.finish()));
      }
    }
  }

  fn link_target(&self, path: &Path) -> io::Result<PathBuf> {
    fs::read_link(literal(path)).map_err(told)
  }

  fn real_path(&self, path: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(literal(path)).map_err(told)
  }

  fn name_on_disk(&self, path: &Path) -> io::Result<OsString> {
    name_on_disk(path)
  }

  fn now(&self) -> SystemTime {
    SystemTime::now()
  }

  fn ticks(&self) -> Duration {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed()
  }

  // Windows: a server of SMB tells of its changes, and the handle reports them.
  #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
  fn remote_disk(&self, dir: &Path) -> Option<String> {
    remote_disk(dir)
  }

  fn listing_lags(&self) -> bool {
    cfg!(windows)
  }
}

/// A `Meta` from what std tells of an entry, with its file id and its directory identity.
fn meta(metadata: &Metadata, (file_id, dir_id): (u64, DirId)) -> Meta {
  let kind = if metadata.is_dir() {
    FileType::Dir
  } else if metadata.is_symlink() {
    FileType::Symlink
  } else if metadata.is_file() {
    FileType::File
  } else {
    FileType::Other
  };
  let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
  Meta { kind, modified, size: metadata.len(), file_id, dir_id }
}

/// Whether an error of the disk says "nothing is at this path".
pub(crate) fn is_missing(error: &io::Error) -> bool {
  matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
}

/// Whether the error is a link loop. Its kind (`FilesystemLoop`) is not stable yet, so the
/// number of the OS is read where it is known; elsewhere a loop is an error like any other.
pub(crate) fn is_link_loop(error: &io::Error) -> bool {
  #[cfg(unix)]
  const LINK_LOOP: Option<i32> = Some(ELOOP);
  #[cfg(windows)]
  const LINK_LOOP: Option<i32> =
    Some(windows_sys::Win32::Foundation::ERROR_CANT_RESOLVE_FILENAME.cast_signed());
  #[cfg(not(any(unix, windows)))]
  const LINK_LOOP: Option<i32> = None;

  LINK_LOOP.is_some_and(|code| error.raw_os_error() == Some(code))
}

/// Only Windows has errors that std does not tell apart.
#[cfg(not(windows))]
fn told(error: io::Error) -> io::Error {
  error
}

/// Whether the error says that the entry is being deleted (Windows): its name goes when the
/// last handle on it is closed, unless the holder takes the deletion back.
#[cfg(not(windows))]
pub(crate) fn is_delete_pending(_error: &io::Error) -> bool {
  false
}

/// Only Windows reads a name as another one than the path says.
#[cfg(not(windows))]
fn literal(path: &Path) -> &Path {
  path
}

#[cfg(not(windows))]
fn stat(path: &Path, follow: bool) -> io::Result<Meta> {
  let metadata = if follow { fs::metadata(path) } else { fs::symlink_metadata(path) };
  match metadata {
    Ok(metadata) => Ok(meta(&metadata, ids(&metadata))),
    // WASI: the directories that the program may use (the preopens) lie below `/`, which is
    // none of them as a rule. It is the top directory of every path all the same.
    #[cfg(target_os = "wasi")]
    Err(_) if path == Path::new("/") => Ok(Meta {
      kind: FileType::Dir,
      modified: SystemTime::UNIX_EPOCH,
      size: 0,
      file_id: 0,
      dir_id: DirId::default(),
    }),
    Err(error) => Err(error),
  }
}

/// The file id and the directory identity.
#[cfg(unix)]
fn ids(metadata: &Metadata) -> (u64, DirId) {
  use std::os::unix::fs::MetadataExt;

  // The birth time tells a new directory from a removed one that had the same inode number.
  // Not every libc gives it (musl does not).
  let birth = metadata.created().map_or(0, nanos);
  (metadata.ino(), DirId { volume: metadata.dev(), inode: metadata.ino(), birth })
}

#[cfg(not(any(unix, windows)))]
fn ids(metadata: &Metadata) -> (u64, DirId) {
  // The inode is not in stable std here (WASI), so a directory has no identity: a replaced
  // one is found by the listing. A file is told from the next one at its path by its birth.
  (metadata.created().map_or(0, nanos), DirId::default())
}

/// Nanoseconds since 1970, 0 for a time before.
#[cfg(not(windows))]
pub(crate) fn nanos(time: SystemTime) -> u64 {
  time
    .duration_since(SystemTime::UNIX_EPOCH)
    .map_or(0, |since| u64::try_from(since.as_nanos()).unwrap_or(u64::MAX))
}

/// Opens the regular file at `path` for reading, and nothing else: a link that took the place of
/// the file since the look is not followed, and a FIFO or a device is not waited for. `None`: no
/// regular file is at the path.
#[cfg(not(windows))]
fn open_regular(path: &Path) -> io::Result<Option<fs::File>> {
  let mut options = fs::OpenOptions::new();
  // `O_NOFOLLOW`: the open fails on a link. `O_NONBLOCK`: the open of a FIFO does not wait for
  // a writer, and a read of a regular file is as always.
  #[cfg(unix)]
  std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, O_NOFOLLOW | O_NONBLOCK);
  let file = options.read(true).open(path)?;
  // A FIFO or a device is no file to read: a regular file and nothing else is.
  #[expect(clippy::filetype_is_file, reason = "a regular file and nothing else is read")]
  let is_regular = file.metadata()?.file_type().is_file();
  Ok(is_regular.then_some(file))
}

/// The other systems: every disk tells two spellings of a name apart, and the engine never asks.
#[cfg(not(any(target_vendor = "apple", windows)))]
fn name_on_disk(path: &Path) -> io::Result<OsString> {
  fs::symlink_metadata(path)?;
  path.file_name().map(OsStr::to_owned).ok_or_else(|| io::ErrorKind::InvalidInput.into())
}

/// Linux: the type of the file system tells, not its name (one shared folder of a VM was
/// `fuseblk`, and later `virtiofs`). FUSE counts, as a folder of the host in a VM is one; so
/// does a local disk through FUSE, which only costs it one error too many.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn remote_disk(dir: &Path) -> Option<String> {
  use std::{ffi::CString, mem, os::unix::ffi::OsStrExt};

  const KINDS: [(i128, &str); 6] = [
    (0x6969, "nfs"),
    (0x517b, "smb"),
    (0xff53_4d42, "cifs"),
    (0xfe53_4d42, "smb2"),
    (0x0102_1997, "9p"),
    (0x6573_5546, "fuse"),
  ];
  let path = CString::new(dir.as_os_str().as_bytes()).ok()?;
  // SAFETY: all zeros is a valid value of this plain struct of numbers.
  let mut answer: libc::statfs = unsafe { mem::zeroed() };
  // SAFETY: `path` ends with a NUL, and the call writes one whole `struct statfs`.
  if unsafe { libc::statfs(path.as_ptr(), &raw mut answer) } != 0 {
    return None;
  }
  // The type is 32 bits, in a field that is wider, or signed, on some targets.
  let kind = i128::from(answer.f_type) & 0xffff_ffff;
  KINDS.iter().find(|(magic, _)| *magic == kind).map(|(_, name)| (*name).to_owned())
}
