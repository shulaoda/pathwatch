//! How the engine sees the disk.

use std::{
  ffi::OsStr,
  fs::{self, File, Metadata},
  hash::Hasher,
  io::{self, Read},
  path::{Path, PathBuf},
  time::SystemTime,
};

use rustc_hash::FxHasher;

/// Every call that the engine makes to the disk. `StdFs` is the real disk, and the tests
/// have one in memory.
pub trait Fs {
  /// Does not follow a symbolic link at the end of `path`.
  fn lstat(&self, path: &Path) -> io::Result<Meta>;

  /// Follows symbolic links.
  fn stat(&self, path: &Path) -> io::Result<Meta>;

  /// Calls `each` with the name of every entry of `dir`, and with its kind when the listing gives
  /// it for free.
  fn read_dir(&self, dir: &Path, each: &mut dyn FnMut(&OsStr, Option<FileType>)) -> io::Result<()>;

  /// A hash of the content of a regular file.
  fn hash(&self, path: &Path) -> io::Result<u64>;

  /// Where the symbolic link at `path` points: the text that is stored in the link.
  fn link_target(&self, path: &Path) -> io::Result<PathBuf>;

  /// The path that `path` leads to, without any symbolic link in it, and with every name as
  /// the disk spells it.
  fn real_path(&self, path: &Path) -> io::Result<PathBuf>;

  /// The clock that the modification times come from.
  fn now(&self) -> SystemTime;
}

/// What one `lstat` or `stat` tells about an entry.
#[derive(Clone, Copy, Debug)]
pub struct Meta {
  pub kind: FileType,
  pub modified: SystemTime,
  /// The change time: it moves when the content changes, and also when the entry itself is
  /// renamed or gets other permissions. Where the platform gives none: the modification time.
  pub changed: SystemTime,
  pub size: u64,
  /// The inode on Unix, the creation time on Windows.
  pub file_id: u64,
  pub dir_id: DirId,
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

/// Tells one directory from another one at the same path. The engine uses it only to find more
/// changes, never to skip a check.
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
    fs::symlink_metadata(path).map(|metadata| meta(&metadata))
  }

  fn stat(&self, path: &Path) -> io::Result<Meta> {
    fs::metadata(path).map(|metadata| meta(&metadata))
  }

  fn read_dir(&self, dir: &Path, each: &mut dyn FnMut(&OsStr, Option<FileType>)) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
      let entry = entry?;
      let kind = entry.file_type().ok().map(|file_type| {
        if file_type.is_dir() {
          FileType::Dir
        } else if file_type.is_symlink() {
          FileType::Symlink
        } else {
          // The engine only needs "directory or not" from a listing. The exact kind comes from
          // the `lstat` that follows.
          FileType::File
        }
      });
      each(&entry.file_name(), kind);
    }
    Ok(())
  }

  fn hash(&self, path: &Path) -> io::Result<u64> {
    let mut file = File::open(path)?;
    let mut hasher = FxHasher::default();
    // On the stack: a young file is hashed at every look, and most files are small.
    let mut buffer = [0; 16 * 1024];
    loop {
      // Fill the whole buffer before it is hashed: the hash must not depend on how many bytes
      // one `read` call happens to return.
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
    fs::read_link(path)
  }

  fn real_path(&self, path: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(path)
  }

  fn now(&self) -> SystemTime {
    SystemTime::now()
  }
}

fn meta(metadata: &Metadata) -> Meta {
  let kind = if metadata.is_dir() {
    FileType::Dir
  } else if metadata.is_symlink() {
    FileType::Symlink
  } else if metadata.is_file() {
    FileType::File
  } else {
    FileType::Other
  };
  let (file_id, dir_id) = ids(metadata);
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

  // A time before 1970 is no time a watcher has to tell from another one.
  let seconds = u64::try_from(metadata.ctime()).unwrap_or(0);
  let nanos = u32::try_from(metadata.ctime_nsec()).unwrap_or(0);
  SystemTime::UNIX_EPOCH + Duration::new(seconds, nanos)
}

/// Stable std gives no change time here: the modification time stands in for it.
#[cfg(not(unix))]
fn changed(_metadata: &Metadata, modified: SystemTime) -> SystemTime {
  modified
}

/// The file id and the directory identity.
#[cfg(unix)]
fn ids(metadata: &Metadata) -> (u64, DirId) {
  use std::os::unix::fs::MetadataExt;

  // The birth time tells a new directory from a removed one whose inode number it got. Not every
  // libc gives it (musl does not).
  let birth = metadata.created().map_or(0, nanos);
  (metadata.ino(), DirId { volume: metadata.dev(), inode: metadata.ino(), birth })
}

#[cfg(windows)]
fn ids(metadata: &Metadata) -> (u64, DirId) {
  use std::os::windows::fs::MetadataExt;

  // Stable std gives no volume serial number and no file index.
  let created = metadata.creation_time();
  (created, DirId { volume: 0, inode: 0, birth: created })
}

#[cfg(not(any(unix, windows)))]
fn ids(metadata: &Metadata) -> (u64, DirId) {
  // There is no inode here (WASI). A directory has no identity: a replaced one is found by the
  // listing only.
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

  /// A rename of a directory, or other permissions for it, moves its change time and leaves
  /// its modification time. A new entry in it moves both.
  #[test]
  fn the_change_time_of_a_directory() {
    // Longer than one step of the clock that the file systems of the tests stamp with.
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
}
