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

/// Every call that the engine makes to the disk. `StdFs` is the real disk, and the tests
/// have one in memory.
pub trait Fs {
  /// Does not follow a symbolic link at the end of `path`.
  fn lstat(&self, path: &Path) -> io::Result<Meta>;

  /// Follows symbolic links.
  fn stat(&self, path: &Path) -> io::Result<Meta>;

  /// Calls `each` with the name of every entry of `dir`, and with what the listing gives of it
  /// for free.
  fn read_dir(&self, dir: &Path, each: &mut dyn FnMut(&OsStr, Entry)) -> io::Result<()>;

  /// A hash of the content of a regular file.
  fn hash(&self, path: &Path) -> io::Result<u64>;

  /// Where the symbolic link at `path` points: the text that is stored in the link.
  fn link_target(&self, path: &Path) -> io::Result<PathBuf>;

  /// The path that `path` leads to, without any symbolic link in it, and with every name as
  /// the disk spells it.
  fn real_path(&self, path: &Path) -> io::Result<PathBuf>;

  /// The last name of `path` as the disk spells it. A link at the end is not followed.
  ///
  /// It is asked only on a disk that takes two spellings of a name for one. There `lstat` finds
  /// an entry under every spelling of its name, and this call tells which one the entry has.
  fn name_on_disk(&self, path: &Path) -> io::Result<OsString>;

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

/// What a listing gives of an entry besides its name.
#[derive(Clone, Copy, Debug)]
pub enum Entry {
  Name,
  /// The engine only needs "directory or not" from it. The exact kind comes from an `lstat`.
  Kind(FileType),
  /// A regular file, with what an `lstat` would tell of it: the engine asks none then.
  File {
    modified: SystemTime,
    size: u64,
    file_id: u64,
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

  fn read_dir(&self, dir: &Path, each: &mut dyn FnMut(&OsStr, Entry)) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
      if let Some(volume) = apple::apfs_volume(dir) {
        return apple::read_dir(dir, volume, each);
      }
    }
    for entry in fs::read_dir(dir)? {
      let entry = entry?;
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

  fn name_on_disk(&self, path: &Path) -> io::Result<OsString> {
    name_on_disk(path)
  }

  fn now(&self) -> SystemTime {
    SystemTime::now()
  }
}

/// macOS: `getattrlistbulk` gives the entries of a directory with what `lstat` tells of each,
/// many of them in one call. Only a regular file on APFS is taken from it: there it was
/// measured to tell the same as `lstat`, and on other disks not (`docs/design.md` 5.4). Of a
/// directory where a disk is mounted it tells what is under the mount.
#[cfg(target_vendor = "apple")]
mod apple {
  use std::{
    ffi::{CString, OsStr, c_char, c_void},
    fs::OpenOptions,
    io, mem,
    os::{
      fd::AsRawFd,
      unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    },
    path::Path,
    time::{Duration, SystemTime},
  };

  use super::{Entry, FileType};

  /// `struct attrlist`: the attributes that are asked for.
  #[repr(C)]
  pub struct AttrList {
    pub bitmap_count: u16,
    pub reserved: u16,
    pub common: u32,
    pub volume: u32,
    pub dir: u32,
    pub file: u32,
    pub fork: u32,
  }
  /// `struct statfs`. Only the device and the name of the file system are read.
  #[repr(C)]
  struct StatFs {
    sizes_and_counts: [u64; 6],
    id: [i32; 2],
    owner_type_flags_subtype: [u32; 4],
    type_name: [u8; 16],
    rest: [u8; 2 * 1024 + 4 + 7 * 4],
  }
  pub const ATTR_BIT_MAP_COUNT: u16 = 5;
  pub const ATTR_CMN_NAME: u32 = 0x1;
  const ATTR_CMN_DEVID: u32 = 0x2;
  const ATTR_CMN_OBJTYPE: u32 = 0x8;
  const ATTR_CMN_MODTIME: u32 = 0x400;
  const ATTR_CMN_FLAGS: u32 = 0x4_0000;
  const ATTR_CMN_FILEID: u32 = 0x0200_0000;
  const ATTR_CMN_ERROR: u32 = 0x2000_0000;
  const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
  const ATTR_FILE_DATALENGTH: u32 = 0x200;
  /// What is read of a regular file, in the common attributes.
  const FILE: u32 = ATTR_CMN_DEVID | ATTR_CMN_MODTIME | ATTR_CMN_FLAGS | ATTR_CMN_FILEID;
  /// The content of the file is not on this disk (a file in a cloud).
  const SF_DATALESS: u32 = 0x4000_0000;
  const O_DIRECTORY: i32 = 0x0010_0000;
  const VREG: u32 = 1;
  const VDIR: u32 = 2;
  const VLNK: u32 = 5;
  unsafe extern "C" {
    #[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), link_name = "statfs$INODE64")]
    fn statfs(path: *const c_char, answer: *mut StatFs) -> i32;
    fn getattrlistbulk(
      dir: i32,
      wanted: *const AttrList,
      answer: *mut c_void,
      size: usize,
      options: u64,
    ) -> i32;
  }

  /// The device number of the disk under `dir`, if it is an APFS disk.
  pub fn apfs_volume(dir: &Path) -> Option<i32> {
    let c_path = CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: all zeros is a valid value of this plain struct of numbers.
    let mut answer: StatFs = unsafe { mem::zeroed() };
    // SAFETY: `c_path` ends with a NUL, and the call writes one whole `struct statfs`.
    let result = unsafe { statfs(c_path.as_ptr(), &raw mut answer) };
    (result == 0 && answer.type_name.starts_with(b"apfs\0")).then_some(answer.id[0])
  }

  /// `volume` is what `apfs_volume` gave for `dir`. An entry on another device is not taken
  /// from the listing: the directory can be another one by now.
  pub fn read_dir(dir: &Path, volume: i32, each: &mut dyn FnMut(&OsStr, Entry)) -> io::Result<()> {
    // Not a pipe, which would wait for its writer.
    let dir = OpenOptions::new().read(true).custom_flags(O_DIRECTORY).open(dir)?;
    let wanted = AttrList {
      bitmap_count: ATTR_BIT_MAP_COUNT,
      reserved: 0,
      common: ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_ERROR | ATTR_CMN_NAME | ATTR_CMN_OBJTYPE | FILE,
      volume: 0,
      dir: 0,
      file: ATTR_FILE_DATALENGTH,
      fork: 0,
    };
    // On the stack, as in `hash`. The longest entry takes less than 1 KiB.
    let mut buffer = [0_u8; 16 * 1024];
    loop {
      // SAFETY: `dir` is open, `wanted` is a whole `attrlist`, and the call writes at most
      // `buffer.len()` bytes into `buffer`.
      let count = unsafe {
        getattrlistbulk(
          dir.as_raw_fd(),
          &raw const wanted,
          buffer.as_mut_ptr().cast(),
          buffer.len(),
          0,
        )
      };
      if count < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
          continue;
        }
        return Err(error);
      }
      if count == 0 {
        return Ok(());
      }
      let mut rest = &buffer[..];
      for _ in 0..count {
        let entry = Fields { bytes: rest, at: 0 }.u32().and_then(|len| rest.get(..len as usize));
        let entry = entry.ok_or_else(|| io::Error::other("getattrlistbulk gave no entry"))?;
        rest = &rest[entry.len()..];
        // An entry without a name is left out, as a plain listing would leave it out.
        if let Some((name, told)) = read(entry, volume) {
          each(name, told);
        }
      }
    }
  }

  /// One entry of the answer: its length, the set of the attributes that follow, and these
  /// attributes in the order of their bits.
  fn read(entry: &[u8], volume: i32) -> Option<(&OsStr, Entry)> {
    let mut fields = Fields { bytes: entry, at: size_of::<u32>() };
    let [common, _, _, file, _] = [(); 5].map(|()| fields.u32());
    let (common, file) = (common?, file?);
    let has = |attribute: u32| common & attribute != 0;
    let failed = has(ATTR_CMN_ERROR) && fields.u32()? != 0;
    if !has(ATTR_CMN_NAME) {
      return None;
    }
    // Where the name starts, counted from this field, and how long it is with its NUL.
    let (reference, offset, len) = (fields.at, fields.i32()?, fields.u32()?);
    let start = reference.checked_add_signed(isize::try_from(offset).ok()?)?;
    let end = start.checked_add((len as usize).checked_sub(1)?)?;
    let name = OsStr::from_bytes(entry.get(start..end)?);
    let device = if has(ATTR_CMN_DEVID) { Some(fields.i32()?) } else { None };
    let kind = if has(ATTR_CMN_OBJTYPE) { Some(fields.u32()?) } else { None };
    let kind = match kind {
      None => return Some((name, Entry::Name)),
      Some(VREG) => FileType::File,
      Some(VDIR) => FileType::Dir,
      Some(VLNK) => FileType::Symlink,
      Some(_) => FileType::Other,
    };
    let whole = common & FILE == FILE && file & ATTR_FILE_DATALENGTH != 0;
    if failed || kind != FileType::File || !whole || device != Some(volume) {
      return Some((name, Entry::Kind(kind)));
    }
    let (modified, flags, file_id, size) =
      (fields.time(), fields.u32()?, fields.u64()?, fields.u64()?);
    // A time before 1970 is left to `lstat`, and so is a file in a cloud: not measured.
    match modified {
      Some(modified) if flags & SF_DATALESS == 0 => {
        Some((name, Entry::File { modified, size, file_id }))
      }
      _ => Some((name, Entry::Kind(kind))),
    }
  }

  struct Fields<'a> {
    bytes: &'a [u8],
    at: usize,
  }

  impl Fields<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
      let field = self.bytes.get(self.at..)?.first_chunk::<N>()?;
      self.at += N;
      Some(*field)
    }

    fn u32(&mut self) -> Option<u32> {
      self.take().map(u32::from_ne_bytes)
    }

    fn i32(&mut self) -> Option<i32> {
      self.take().map(i32::from_ne_bytes)
    }

    fn u64(&mut self) -> Option<u64> {
      self.take().map(u64::from_ne_bytes)
    }

    /// A `struct timespec`, if it is no time before 1970.
    fn time(&mut self) -> Option<SystemTime> {
      let [seconds, nanos] = [(); 2].map(|()| self.take().map(i64::from_ne_bytes));
      let since = u64::try_from(seconds?).ok().zip(u32::try_from(nanos?).ok());
      since.map(|(seconds, nanos)| SystemTime::UNIX_EPOCH + Duration::new(seconds, nanos))
    }
  }
}

/// macOS: `getattrlist` gives the name that the directory holds for the entry, in one call.
#[cfg(target_vendor = "apple")]
fn name_on_disk(path: &Path) -> io::Result<OsString> {
  use std::{
    ffi::{CString, c_char, c_void},
    mem,
    os::unix::ffi::OsStrExt,
  };

  use self::apple::{ATTR_BIT_MAP_COUNT, ATTR_CMN_NAME, AttrList};

  /// The answer for the name alone: the length of all of it, where the name starts, counted
  /// from the field `name_offset`, how long it is with its NUL, and then its bytes.
  #[repr(C)]
  struct Answer {
    length: u32,
    name_offset: i32,
    name_length: u32,
    bytes: [u8; NAME_BYTES],
  }
  const FSOPT_NOFOLLOW: u32 = 0x1;
  /// A name is at most 255 UTF-16 units, each at most three bytes here, and a NUL.
  const NAME_BYTES: usize = 255 * 3 + 1;
  unsafe extern "C" {
    fn getattrlist(
      path: *const c_char,
      wanted: *mut c_void,
      answer: *mut c_void,
      size: usize,
      options: u32,
    ) -> i32;
  }

  let c_path = CString::new(path.as_os_str().as_bytes())?;
  let wanted = AttrList {
    bitmap_count: ATTR_BIT_MAP_COUNT,
    reserved: 0,
    common: ATTR_CMN_NAME,
    volume: 0,
    dir: 0,
    file: 0,
    fork: 0,
  };
  let mut answer = Answer { length: 0, name_offset: 0, name_length: 0, bytes: [0; NAME_BYTES] };
  // SAFETY: `c_path` ends with a NUL, `wanted` is a whole `attrlist` that the call only reads,
  // and the call writes at most `size_of::<Answer>()` bytes into `answer`.
  let result = unsafe {
    getattrlist(
      c_path.as_ptr(),
      (&raw const wanted).cast_mut().cast(),
      (&raw mut answer).cast(),
      size_of::<Answer>(),
      FSOPT_NOFOLLOW,
    )
  };
  if result != 0 {
    return Err(io::Error::last_os_error());
  }
  let start = usize::try_from(answer.name_offset).ok().and_then(|from_the_field| {
    (mem::offset_of!(Answer, name_offset) + from_the_field)
      .checked_sub(mem::offset_of!(Answer, bytes))
  });
  let len = usize::try_from(answer.name_length).ok().and_then(|with_nul| with_nul.checked_sub(1));
  let name = start.zip(len).and_then(|(start, len)| answer.bytes.get(start..start + len));
  let name = name.ok_or_else(|| io::Error::other("getattrlist gave no name"))?;
  Ok(OsStr::from_bytes(name).to_owned())
}

/// Windows: a search for the one path gives the entry as the directory holds it.
#[cfg(windows)]
fn name_on_disk(path: &Path) -> io::Result<OsString> {
  use std::{
    mem,
    os::windows::ffi::{OsStrExt, OsStringExt},
  };

  use windows_sys::Win32::{
    Foundation::INVALID_HANDLE_VALUE,
    Storage::FileSystem::{FindClose, FindFirstFileW, WIN32_FIND_DATAW},
  };

  let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
  // SAFETY: all zeros is a valid value of this plain struct of numbers.
  let mut found: WIN32_FIND_DATAW = unsafe { mem::zeroed() };
  // SAFETY: `wide` ends with a NUL, and the call writes one whole struct into `found`.
  let search = unsafe { FindFirstFileW(wide.as_ptr(), &raw mut found) };
  if search == INVALID_HANDLE_VALUE {
    return Err(io::Error::last_os_error());
  }
  // SAFETY: `search` is the open handle that the call above gave, closed once.
  unsafe { FindClose(search) };
  let len = found.cFileName.iter().position(|&unit| unit == 0).unwrap_or(found.cFileName.len());
  Ok(OsString::from_wide(&found.cFileName[..len]))
}

/// No call gives the name of one entry here, so the listing of its directory is read: 1.8 ms
/// for 2,000 entries of a folder that a Mac shares. The engine asks only in a directory that
/// takes two spellings of a name for one (a folder of another system, a FAT or SMB disk, a
/// directory with the casefold flag), and there only for a name that it is not sure of.
///
/// The entry is the one with the very name, or else one that differs in the case of ASCII
/// letters, or else one with the same inode number (another Unicode form). If the listing has
/// none of these, the directory has no such entry, whatever `lstat` said: on a folder that a
/// Mac shares, Linux still finds a name that was renamed away through another spelling
/// (measured: the kernel keeps what it knew about each spelling).
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

/// Everywhere else a name has one spelling: the one of the path.
#[cfg(not(any(unix, windows)))]
fn name_on_disk(path: &Path) -> io::Result<OsString> {
  fs::symlink_metadata(path)?;
  path.file_name().map(OsStr::to_owned).ok_or_else(|| io::ErrorKind::InvalidInput.into())
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

  /// On APFS the listing tells of a regular file what `lstat` tells, also while the file is
  /// open and written. Of a directory and of a link it tells the kind.
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
