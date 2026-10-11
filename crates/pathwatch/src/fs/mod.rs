//! How the engine sees the disk.

use std::{
  ffi::{OsStr, OsString},
  fs::{self, File, Metadata},
  hash::Hasher,
  io::{self, Read},
  path::{Path, PathBuf},
  time::SystemTime,
};

use rustc_hash::FxHasher;

/// Every call that the engine makes to the disk. `StdFs` is the real one; the tests have one
/// in memory.
pub trait Fs {
  /// Does not follow a symbolic link at the end of `path`.
  fn lstat(&self, path: &Path) -> io::Result<Meta>;

  /// Follows symbolic links.
  fn stat(&self, path: &Path) -> io::Result<Meta>;

  /// Calls `each` with the name of every entry of `dir`, and what the listing tells of it.
  fn read_dir(&self, dir: &Path, each: &mut dyn FnMut(&OsStr, Entry)) -> io::Result<()>;

  /// A hash of the content of a regular file.
  fn hash(&self, path: &Path) -> io::Result<u64>;

  /// A number that the disk changes with every change of the regular file at `path`, a rename
  /// too; `None` where the disk has none. Where an OS watch reports writes, the engine takes
  /// it in place of the hash, so the file is not opened.
  fn version(&self, _path: &Path) -> Option<u64> {
    None
  }

  /// Where the symbolic link at `path` points: the text that is stored in the link.
  fn link_target(&self, path: &Path) -> io::Result<PathBuf>;

  /// The path that `path` leads to: no link in it, and every name as the disk spells it.
  fn real_path(&self, path: &Path) -> io::Result<PathBuf>;

  /// The last name of `path` as the disk spells it; a link at the end is not followed. Asked
  /// only on a disk that takes two spellings of a name for one.
  fn name_on_disk(&self, path: &Path) -> io::Result<OsString>;

  /// The clock that the modification times come from.
  fn now(&self) -> SystemTime;

  /// Whether a listing can be behind `lstat` for a file that its writer still has open (NTFS).
  /// Then the engine takes a file from a listing only where an OS watch reports the write.
  fn listing_lags(&self) -> bool {
    false
  }
}

/// What one `lstat` or `stat` tells about an entry.
#[derive(Clone, Copy, Debug)]
pub struct Meta {
  pub kind: FileType,
  pub modified: SystemTime,
  /// The change time: it moves with a rename or a chmod too. The modification time where the
  /// platform gives none.
  pub changed: SystemTime,
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
  /// A directory that is no link, with its identity, so no `lstat` is asked.
  Dir {
    id: DirId,
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
    {
      if let Some(volume) = apple::apfs_volume(dir) {
        return apple::read_dir(dir, volume, each);
      }
    }
    #[cfg(windows)]
    {
      if windows::read_dir(dir, each)? {
        return Ok(());
      }
    }
    for entry in fs::read_dir(dir).map_err(told)? {
      let entry = entry.map_err(told)?;
      let kind = entry.file_type().ok().map(|file_type| {
        if file_type.is_dir() {
          FileType::Dir
        } else if file_type.is_symlink() {
          FileType::Symlink
        } else {
          FileType::File
        }
      });
      each(&entry.file_name(), kind.map_or(Entry::Name, Entry::Kind));
    }
    Ok(())
  }

  fn hash(&self, path: &Path) -> io::Result<u64> {
    let mut file = File::open(path).map_err(told)?;
    let mut hasher = FxHasher::default();
    let mut buffer = [0; 16 * 1024];
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
      hasher.write(&buffer[..filled]);
      if filled < buffer.len() {
        return Ok(hasher.finish());
      }
    }
  }

  fn link_target(&self, path: &Path) -> io::Result<PathBuf> {
    fs::read_link(path).map_err(told)
  }

  fn real_path(&self, path: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(path).map_err(told)
  }

  fn name_on_disk(&self, path: &Path) -> io::Result<OsString> {
    name_on_disk(path).map_err(told)
  }

  fn now(&self) -> SystemTime {
    SystemTime::now()
  }

  fn listing_lags(&self) -> bool {
    cfg!(windows)
  }

  #[cfg(windows)]
  fn version(&self, path: &Path) -> Option<u64> {
    windows::version(path).ok().flatten()
  }
}

/// Windows: what the calls of std do not tell, and what they cost too much for.
#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub(crate) use self::windows::{Unknown, kernel32};
#[cfg(windows)]
use self::windows::{name_on_disk, stat, told};

#[cfg(not(windows))]
fn told(error: io::Error) -> io::Error {
  error
}

/// macOS: a listing with what `lstat` tells of each entry, and the name of one entry.
#[cfg(target_vendor = "apple")]
mod apple;
#[cfg(target_vendor = "apple")]
pub(crate) use self::apple::name_and_kind;
#[cfg(target_vendor = "apple")]
use self::apple::name_on_disk;

/// Linux has no call for the name of one entry, so the listing is read (1.8 ms for 2,000
/// entries). The entry is the one with the name, else one in another ASCII case, else one with
/// the same inode; none of these means no such entry, whatever `lstat` said: on a folder that
/// a Mac shares, Linux still finds a name that was renamed away through another spelling.
#[cfg(all(unix, not(target_vendor = "apple")))]
fn name_on_disk(path: &Path) -> io::Result<OsString> {
  use std::os::unix::fs::{DirEntryExt, MetadataExt};

  let inode = fs::symlink_metadata(path)?.ino();
  let (Some(given), Some(dir)) = (path.file_name(), path.parent()) else {
    return Err(io::ErrorKind::InvalidInput.into());
  };
  // A listing that cannot be read says nothing against the name.
  let Ok(entries) = fs::read_dir(dir) else { return Ok(given.to_owned()) };
  let (mut other_case, mut same_inode) = (None, None);
  for entry in entries.flatten() {
    let name = entry.file_name();
    if name == given {
      return Ok(name);
    }
    if name.eq_ignore_ascii_case(given) {
      other_case = Some(name);
    } else if entry.ino() == inode {
      same_inode = Some(name);
    }
  }
  other_case.or(same_inode).ok_or_else(|| io::ErrorKind::NotFound.into())
}

/// Everywhere else a name has one spelling.
#[cfg(not(any(unix, windows)))]
fn name_on_disk(path: &Path) -> io::Result<OsString> {
  fs::symlink_metadata(path)?;
  path.file_name().map(OsStr::to_owned).ok_or_else(|| io::ErrorKind::InvalidInput.into())
}

#[cfg(not(windows))]
fn stat(path: &Path, follow: bool) -> io::Result<Meta> {
  let metadata = if follow { fs::metadata(path) } else { fs::symlink_metadata(path) }?;
  Ok(meta(&metadata, ids(&metadata)))
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
  Meta {
    kind,
    modified,
    changed: changed(metadata, modified),
    size: metadata.len(),
    file_id,
    dir_id,
  }
}

#[cfg(unix)]
fn changed(metadata: &Metadata, _modified: SystemTime) -> SystemTime {
  use std::{os::unix::fs::MetadataExt, time::Duration};

  // A time before 1970 counts as 1970.
  let seconds = u64::try_from(metadata.ctime()).unwrap_or(0);
  let nanos = u32::try_from(metadata.ctime_nsec()).unwrap_or(0);
  SystemTime::UNIX_EPOCH + Duration::new(seconds, nanos)
}

/// Stable std gives no change time here.
#[cfg(not(unix))]
fn changed(_metadata: &Metadata, modified: SystemTime) -> SystemTime {
  modified
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
  // WASI has no inode, so a directory has no identity: a replaced one is found by the listing.
  (metadata.created().map_or(0, nanos), DirId::default())
}

#[cfg(not(windows))]
fn nanos(time: SystemTime) -> u64 {
  time
    .duration_since(SystemTime::UNIX_EPOCH)
    .map_or(0, |since| u64::try_from(since.as_nanos()).unwrap_or(u64::MAX))
}

#[cfg(all(test, unix))]
mod tests {
  use std::{fs, os::unix::fs::PermissionsExt, thread, time::Duration};

  use super::{Fs, StdFs};

  /// A rename or a chmod moves the change time and not the modification time; a new entry
  /// moves both.
  #[test]
  fn the_change_time_of_a_directory() {
    // Longer than one step of the clock of the file system.
    let wait = || thread::sleep(Duration::from_millis(50));
    let top = tempfile::tempdir().unwrap();
    let dir = top.path().join("dir");
    fs::create_dir(&dir).unwrap();
    let made = StdFs.lstat(&dir).unwrap();

    wait();
    let moved = top.path().join("moved");
    fs::rename(&dir, &moved).unwrap();
    let renamed = StdFs.lstat(&moved).unwrap();
    assert_eq!(renamed.modified, made.modified);
    assert!(renamed.changed > made.changed);

    wait();
    fs::set_permissions(&moved, fs::Permissions::from_mode(0o700)).unwrap();
    let permitted = StdFs.lstat(&moved).unwrap();
    assert_eq!(permitted.modified, made.modified);
    assert!(permitted.changed > renamed.changed);

    wait();
    fs::write(moved.join("file"), "1").unwrap();
    let filled = StdFs.lstat(&moved).unwrap();
    assert!(filled.modified > made.modified);
    assert!(filled.changed > permitted.changed);
  }

  /// On APFS the listing tells of a file what `lstat` tells, also while it is open and written.
  #[cfg(target_vendor = "apple")]
  #[test]
  fn the_listing_tells_of_a_file_what_lstat_tells() {
    use std::{
      io::{self, Write},
      os::unix::fs::symlink,
      process::Command,
    };

    use super::{Entry, FileType};

    let top = tempfile::tempdir().unwrap();
    fs::create_dir(top.path().join("dir")).unwrap();
    symlink("file", top.path().join("link")).unwrap();
    let mut file = fs::File::create(top.path().join("file")).unwrap();
    file.write_all(b"written").unwrap();

    let mut seen = Vec::new();
    StdFs.read_dir(top.path(), &mut |name, entry| seen.push((name.to_owned(), entry))).unwrap();
    seen.sort_by(|a, b| a.0.cmp(&b.0));
    let [
      (_, Entry::Kind(FileType::Dir)),
      (_, Entry::File { modified, size, file_id }),
      (_, Entry::Kind(FileType::Symlink)),
    ] = seen[..]
    else {
      panic!("{seen:?}");
    };
    let meta = StdFs.lstat(&top.path().join("file")).unwrap();
    assert_eq!((modified, size, file_id), (meta.modified, meta.size, meta.file_id));

    // A pipe is no directory: the listing fails and does not wait for a writer.
    let pipe = top.path().join("pipe");
    assert!(Command::new("mkfifo").arg(&pipe).status().unwrap().success());
    let error = StdFs.read_dir(&pipe, &mut |_, _| {}).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotADirectory);
  }
}
