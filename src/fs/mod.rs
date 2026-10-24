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

#[cfg(unix)]
use libc::{ELOOP, O_NOFOLLOW, O_NONBLOCK};
use rustc_hash::FxHasher;

#[cfg(target_vendor = "apple")]
pub(crate) use self::apple::name_and_kind;
#[cfg(target_vendor = "apple")]
use self::apple::{name_on_disk, remote_disk};
#[cfg(windows)]
pub(crate) use self::windows::{SHARE_ALL, is_delete_pending, kernel32, literal, wide_name};
#[cfg(windows)]
use self::windows::{classify, name_on_disk, open_regular, stat};

pub trait Fs {
  fn lstat(&self, path: &Path) -> io::Result<Meta>;

  fn stat(&self, path: &Path) -> io::Result<Meta>;

  fn read_dir(&self, dir: &Path, each: &mut dyn FnMut(&OsStr, Entry)) -> io::Result<()>;

  fn hash(&self, path: &Path, limit: u64) -> io::Result<Option<u64>>;

  fn link_target(&self, path: &Path) -> io::Result<PathBuf>;

  fn real_path(&self, path: &Path) -> io::Result<PathBuf>;

  fn name_on_disk(&self, path: &Path) -> io::Result<OsString>;

  fn now(&self) -> SystemTime;

  fn ticks(&self) -> Duration {
    self.now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default()
  }

  fn remote_disk(&self, _dir: &Path) -> Option<String> {
    None
  }

  fn listing_lags(&self) -> bool {
    false
  }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Meta {
  pub kind: FileType,
  pub modified: SystemTime,
  pub size: u64,
  pub file_id: u64,
  pub dir_id: DirId,
}

#[derive(Clone, Copy, Debug)]
pub enum Entry {
  Name,
  Kind(FileType),
  File { modified: SystemTime, size: u64, file_id: u64 },
  Dir { id: DirId, modified: SystemTime },
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FileType {
  File,
  Dir,
  Symlink,
  Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct DirId {
  pub volume: u64,
  pub inode: u64,
  pub birth: u64,
}

pub(crate) fn is_missing(error: &io::Error) -> bool {
  matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
}

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

#[cfg(not(windows))]
pub(crate) fn is_delete_pending(_error: &io::Error) -> bool {
  false
}

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
    for entry in fs::read_dir(literal(dir)).map_err(classify)? {
      let entry = entry.map_err(classify)?;
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
      let mut filled = 0;
      while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
          Ok(0) => break,
          Ok(count) => filled += count,
          Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
          Err(error) => return Err(error),
        }
      }
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
    fs::read_link(literal(path)).map_err(classify)
  }

  fn real_path(&self, path: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(literal(path)).map_err(classify)
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

  #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
  fn remote_disk(&self, dir: &Path) -> Option<String> {
    remote_disk(dir)
  }

  fn listing_lags(&self) -> bool {
    cfg!(windows)
  }
}

#[cfg(not(windows))]
fn classify(error: io::Error) -> io::Error {
  error
}

#[cfg(not(windows))]
fn literal(path: &Path) -> &Path {
  path
}

#[cfg(not(windows))]
fn stat(path: &Path, follow: bool) -> io::Result<Meta> {
  let metadata = if follow { fs::metadata(path) } else { fs::symlink_metadata(path) };
  match metadata {
    Ok(metadata) => Ok(meta(&metadata, ids(&metadata))),
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

#[cfg(unix)]
fn ids(metadata: &Metadata) -> (u64, DirId) {
  use std::os::unix::fs::MetadataExt;

  let birth = metadata.created().map_or(0, unsigned_nanos);
  (metadata.ino(), DirId { volume: metadata.dev(), inode: metadata.ino(), birth })
}

#[cfg(not(any(unix, windows)))]
fn ids(metadata: &Metadata) -> (u64, DirId) {
  (metadata.created().map_or(0, unsigned_nanos), DirId::default())
}

#[cfg(not(windows))]
pub(crate) fn unsigned_nanos(time: SystemTime) -> u64 {
  time
    .duration_since(SystemTime::UNIX_EPOCH)
    .map_or(0, |since| u64::try_from(since.as_nanos()).unwrap_or(u64::MAX))
}

#[cfg(not(windows))]
fn open_regular(path: &Path) -> io::Result<Option<fs::File>> {
  let mut options = fs::OpenOptions::new();
  #[cfg(unix)]
  std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, O_NOFOLLOW | O_NONBLOCK);
  let file = options.read(true).open(path)?;
  #[expect(clippy::filetype_is_file, reason = "a regular file and nothing else is read")]
  let is_regular = file.metadata()?.file_type().is_file();
  Ok(is_regular.then_some(file))
}

#[cfg(not(any(target_vendor = "apple", windows)))]
fn name_on_disk(path: &Path) -> io::Result<OsString> {
  fs::symlink_metadata(path)?;
  path.file_name().map(OsStr::to_owned).ok_or_else(|| io::ErrorKind::InvalidInput.into())
}

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
  let kind = i128::from(answer.f_type) & 0xffff_ffff;
  KINDS.iter().find(|(magic, _)| *magic == kind).map(|(_, name)| (*name).to_owned())
}
