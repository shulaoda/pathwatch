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

  /// A number that the disk keeps for the regular file at `path` and changes with every
  /// change of it: a write, and also a rename or other attributes. `None`: the disk has no
  /// such number, or the file cannot be asked.
  ///
  /// Where an OS watch tells about writes, the engine takes it in place of the hash: the same
  /// number at the next look says that nothing was written, and the file is not opened.
  fn version(&self, _path: &Path) -> Option<u64> {
    None
  }

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

  /// Whether a listing can be behind `lstat` for a file that its writer still has open
  /// (NTFS: the directory gets the new size and time when the file is closed). Then the
  /// engine takes a file from a listing only where an OS watch reports the write, and asks
  /// about each file itself in a directory that it polls.
  fn listing_lags(&self) -> bool {
    false
  }
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
  /// The inode on Unix, the file index on Windows.
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
  /// A directory that is no link, with the identity that an `lstat` would tell: the engine
  /// asks none then.
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

/// Windows. What the calls of std do not tell, and what they cost too much for.
#[cfg(windows)]
mod windows {
  use std::{
    ffi::{CStr, OsStr, OsString, c_void},
    fs::{self, OpenOptions},
    io, mem,
    os::windows::{
      ffi::{OsStrExt, OsStringExt},
      fs::{MetadataExt, OpenOptionsExt},
      io::AsRawHandle,
    },
    path::Path,
    sync::OnceLock,
    time::{Duration, SystemTime},
  };

  use windows_sys::Win32::{
    Foundation::{
      ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, ERROR_SHARING_VIOLATION,
      INVALID_HANDLE_VALUE, MAX_PATH, NTSTATUS, STATUS_DELETE_PENDING, STATUS_FILE_IS_A_DIRECTORY,
      STATUS_NO_SUCH_FILE, STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND,
    },
    Storage::FileSystem::{
      BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
      FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
      FILE_ID_BOTH_DIR_INFO, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ,
      FILE_SHARE_WRITE, FileAttributeTagInfo, FileIdBothDirectoryInfo,
      FileIdBothDirectoryRestartInfo, FindClose, FindFirstFileW, GetFileInformationByHandle,
      GetFileInformationByHandleEx, GetVolumeInformationByHandleW, WIN32_FIND_DATAW,
    },
    System::{
      IO::DeviceIoControl,
      Ioctl::{FSCTL_READ_FILE_USN_DATA, USN_RECORD_V2, USN_RECORD_V3},
      LibraryLoader::{GetModuleHandleW, GetProcAddress},
    },
  };

  use super::{DirId, Entry, FileType, Meta, meta};

  /// Other programs may still rename, delete and write what is looked at.
  const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
  /// From 1601 to 1970, in the steps of 100 ns that Windows counts time in.
  const TO_1970: u64 = 116_444_736_000_000_000;
  /// A reparse point with this bit in its tag stands for another name: a junction, a link.
  /// The rule of std for "is a symbolic link".
  const NAME_SURROGATE: u32 = 0x2000_0000;

  /// A time before 1970 is no time a watcher has to tell from another one.
  fn time(since_1601: u64) -> SystemTime {
    let steps = since_1601.saturating_sub(TO_1970);
    SystemTime::UNIX_EPOCH + Duration::from_nanos(steps.saturating_mul(100))
  }

  /// The kind of an entry with these attributes. `tag` is the one of its reparse point.
  fn kind(attributes: u32, tag: u32) -> FileType {
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && tag & NAME_SURROGATE != 0 {
      FileType::Symlink
    } else if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
      FileType::Dir
    } else {
      FileType::File
    }
  }

  /// What `lstat` tells of an entry. There is no change time: the modification time stands in
  /// for it, as with std.
  fn entry(kind: FileType, written: u64, size: u64, dir_id: DirId) -> Meta {
    let modified = time(written);
    Meta { kind, modified, changed: modified, size, file_id: dir_id.inode, dir_id }
  }

  /// What a failed call tells, when Windows says less: "access denied" is also its answer for
  /// an entry that is being deleted (until the one who deletes it closes its handle), and for
  /// a directory that is opened as a file. Both mean that the file the engine asked about is
  /// not there, and only the status of the kernel tells them from an entry that may not be
  /// read.
  ///
  /// Call it right after the call that failed, on the same thread: the status is the one of
  /// the last call that failed there.
  pub fn told(error: io::Error) -> io::Error {
    #[link(name = "ntdll")]
    unsafe extern "system" {
      safe fn RtlGetLastNtStatus() -> NTSTATUS;
    }

    if error.raw_os_error() != Some(ERROR_ACCESS_DENIED.cast_signed()) {
      return error;
    }
    match RtlGetLastNtStatus() {
      // std asks the listing of the directory when the entry cannot be opened, so the status
      // can also be the one of that second call: the entry is gone by then.
      STATUS_DELETE_PENDING
      | STATUS_FILE_IS_A_DIRECTORY
      | STATUS_NO_SUCH_FILE
      | STATUS_OBJECT_NAME_NOT_FOUND
      | STATUS_OBJECT_PATH_NOT_FOUND => io::ErrorKind::NotFound.into(),
      _ => error,
    }
  }

  /// `lstat`, or `stat` with `follow`.
  ///
  /// std gives no file index and no volume, and both are needed: Windows gives a new entry
  /// the creation time of the one that was removed under its name a moment ago, and the clock
  /// of the time stamps moves in steps of up to 16 ms. So a file that is removed and written
  /// again can have every time and the size of the old one, and only the file index tells
  /// that it is another file.
  ///
  /// One call by name where Windows has it (3.5 us for a file, measured). Else the entry is
  /// opened, the way std opens it, and the handle is asked (10 us).
  pub fn stat(path: &Path, follow: bool) -> io::Result<Meta> {
    match by_name(path) {
      // What is behind a link is only found through a handle.
      Some(Ok(found)) if follow && found.kind == FileType::Symlink => {}
      Some(result) => return result,
      None => {}
    }
    by_handle(path, follow)
  }

  /// `FILE_STAT_BASIC_INFORMATION`, the answer of `GetFileInformationByName`.
  #[repr(C)]
  #[derive(Default)]
  struct StatBasic {
    file_id: u64,
    creation_time: u64,
    last_access_time: u64,
    last_write_time: u64,
    change_time: u64,
    allocation_size: u64,
    end_of_file: u64,
    file_attributes: u32,
    reparse_tag: u32,
    number_of_links: u32,
    device_type: u32,
    device_characteristics: u32,
    reserved: u32,
    volume_serial_number: u64,
    file_id_128: [u8; 16],
  }

  /// `GetFileInformationByName(name, class, answer, size)`.
  type ByName = unsafe extern "system" fn(*const u16, i32, *mut c_void, u32) -> i32;
  /// `FileStatBasicByNameInfo`.
  const STAT_BASIC: i32 = 3;

  /// A function of `kernel32.dll` that not every Windows has, by its name. A program that
  /// imports such a function does not start on a Windows without it.
  pub fn kernel32(name: &CStr) -> Option<Unknown> {
    let kernel32: Vec<u16> = "kernel32.dll\0".encode_utf16().collect();
    // SAFETY: both names end with a NUL. `kernel32.dll` is loaded in every process and is
    // never unloaded, so the address stays good.
    unsafe { GetProcAddress(GetModuleHandleW(kernel32.as_ptr()), name.as_ptr().cast()) }
  }

  /// A function whose signature only its name tells.
  pub type Unknown = unsafe extern "system" fn() -> isize;

  /// The call, where this Windows has it (Windows 11 24H2 and later).
  fn by_name_call() -> Option<ByName> {
    static CALL: OnceLock<Option<ByName>> = OnceLock::new();
    *CALL.get_or_init(|| {
      let found = kernel32(c"GetFileInformationByName");
      // SAFETY: the function with this name has this signature.
      found.map(|call| unsafe { mem::transmute::<Unknown, ByName>(call) })
    })
  }

  /// `lstat` in one call that opens nothing. `None`: this call cannot say, and the handle is
  /// asked. Only an entry and "nothing is there" are taken from it: every other error (a disk
  /// that does not know the call, an entry that may not be read) is left to the open.
  fn by_name(path: &Path) -> Option<io::Result<Meta>> {
    let call = by_name_call()?;
    // The call takes the path as it is. A longer one needs the `\\?\` form that std makes.
    let mut wide = [0_u16; MAX_PATH as usize];
    for (at, unit) in path.as_os_str().encode_wide().enumerate() {
      // The last unit stays zero: the end of the text.
      if unit == 0 || at + 1 == wide.len() {
        return None;
      }
      wide[at] = unit;
    }
    let mut info = StatBasic::default();
    let size = u32::try_from(size_of::<StatBasic>()).ok()?;
    // SAFETY: `wide` ends with a NUL, and the call writes at most `size` bytes into `info`.
    if unsafe { call(wide.as_ptr(), STAT_BASIC, (&raw mut info).cast(), size) } == 0 {
      let error = told(io::Error::last_os_error());
      return (error.kind() == io::ErrorKind::NotFound).then_some(Err(error));
    }
    // The low half is the number that a handle gives: both ways must name a volume alike.
    let volume = info.volume_serial_number & u64::from(u32::MAX);
    let dir_id = DirId { volume, inode: info.file_id, birth: info.creation_time };
    let kind = kind(info.file_attributes, info.reparse_tag);
    Some(Ok(entry(kind, info.last_write_time, info.end_of_file, dir_id)))
  }

  fn by_handle(path: &Path, follow: bool) -> io::Result<Meta> {
    let link = if follow { 0 } else { FILE_FLAG_OPEN_REPARSE_POINT };
    let opened = OpenOptions::new()
      // No access to the content: only what the entry is.
      .access_mode(0)
      .share_mode(SHARE_ALL)
      .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | link)
      .open(path);
    let file = match opened.map_err(told) {
      Ok(file) => file,
      Err(error) => {
        // An entry that cannot be opened (a system file that is in use) can still be read
        // from the listing of its directory: std does that. It has no index then.
        let code = error.raw_os_error().map(i32::cast_unsigned);
        if !matches!(code, Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION)) {
          return Err(error);
        }
        let metadata = if follow { fs::metadata(path) } else { fs::symlink_metadata(path) };
        let metadata = metadata.map_err(told)?;
        let created = metadata.creation_time();
        return Ok(meta(&metadata, (created, DirId { volume: 0, inode: 0, birth: created })));
      }
    };
    // SAFETY: all zeros is a valid value of this plain struct of numbers.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { mem::zeroed() };
    // SAFETY: the handle is open, and the call writes one whole struct into `info`.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &raw mut info) } == 0 {
      return Err(io::Error::last_os_error());
    }
    let index = wide(info.nFileIndexHigh, info.nFileIndexLow);
    let birth = wide(info.ftCreationTime.dwHighDateTime, info.ftCreationTime.dwLowDateTime);
    let dir_id = DirId { volume: u64::from(info.dwVolumeSerialNumber), inode: index, birth };
    // Only a reparse point has a tag, and this answer does not hold it.
    let mut tag = FILE_ATTRIBUTE_TAG_INFO { FileAttributes: 0, ReparseTag: 0 };
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
      let size = u32::try_from(size_of::<FILE_ATTRIBUTE_TAG_INFO>()).unwrap_or(0);
      // SAFETY: the handle is open, and the call writes one whole struct into `tag`.
      let asked = unsafe {
        let class = FileAttributeTagInfo;
        GetFileInformationByHandleEx(file.as_raw_handle(), class, (&raw mut tag).cast(), size)
      };
      if asked == 0 {
        return Err(io::Error::last_os_error());
      }
    }
    let written = wide(info.ftLastWriteTime.dwHighDateTime, info.ftLastWriteTime.dwLowDateTime);
    let size = wide(info.nFileSizeHigh, info.nFileSizeLow);
    Ok(entry(kind(info.dwFileAttributes, tag.ReparseTag), written, size, dir_id))
  }

  fn wide(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
  }

  /// The room for one answer of the system. The records in it start at multiples of eight.
  #[repr(C, align(8))]
  struct Aligned<const N: usize>([u8; N]);

  /// A number that changes with each change of the file at `path`: the place of the last
  /// record about the file in the change journal of its disk. `None`: the disk keeps no
  /// journal, and the content has to be read.
  ///
  /// It stands in for a hash of the content, which costs much more here: a program that opens
  /// a file to read it, right after the file was written, waits for the virus scanner (2 ms,
  /// and 14 ms for one in ten, measured). This call opens the file without access to its
  /// content and waits for nothing (0.04 ms). It also changes when the file is renamed or
  /// gets other attributes, and when a writer closes the file.
  pub fn version(path: &Path) -> io::Result<Option<u64>> {
    // A directory cannot be opened in this way: it is no file to compare (see `told`).
    let file = OpenOptions::new().access_mode(0).share_mode(SHARE_ALL).open(path).map_err(told)?;
    // The longest record: its fixed part and a name of 255 units.
    let mut record = Aligned([0; 1024]);
    let size = u32::try_from(record.0.len()).unwrap_or(u32::MAX);
    let mut filled = 0;
    // SAFETY: the handle is open. The call reads no input, writes at most `size` bytes into
    // `record`, and their count into `filled`.
    let read = unsafe {
      let (no_input, wait) = (std::ptr::null(), std::ptr::null_mut());
      let (answer, filled) = ((&raw mut record).cast(), &raw mut filled);
      let handle = file.as_raw_handle();
      DeviceIoControl(handle, FSCTL_READ_FILE_USN_DATA, no_input, 0, answer, size, filled, wait)
    };
    if read == 0 {
      return Ok(None);
    }
    let bytes = record.0.get(..filled as usize).unwrap_or_default();
    let major = bytes.get(mem::offset_of!(USN_RECORD_V2, MajorVersion)..);
    let at =
      match major.and_then(|major| major.first_chunk()).map(|major| u16::from_ne_bytes(*major)) {
        Some(2) => mem::offset_of!(USN_RECORD_V2, Usn),
        Some(3) => mem::offset_of!(USN_RECORD_V3, Usn),
        _ => return Ok(None),
      };
    let usn = bytes.get(at..).and_then(|usn| usn.first_chunk());
    Ok(usn.map(|usn| u64::from_ne_bytes(*usn)))
  }

  // Where the fields of a `FILE_ID_BOTH_DIR_INFO` record are. They are read from the bytes,
  // not through a pointer: every offset and length is checked against the answer.
  const NEXT: usize = mem::offset_of!(FILE_ID_BOTH_DIR_INFO, NextEntryOffset);
  const CREATED: usize = mem::offset_of!(FILE_ID_BOTH_DIR_INFO, CreationTime);
  const WRITTEN: usize = mem::offset_of!(FILE_ID_BOTH_DIR_INFO, LastWriteTime);
  const SIZE: usize = mem::offset_of!(FILE_ID_BOTH_DIR_INFO, EndOfFile);
  const ATTRIBUTES: usize = mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileAttributes);
  const NAME_BYTES: usize = mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileNameLength);
  /// For a reparse point this field holds its tag.
  const TAG: usize = mem::offset_of!(FILE_ID_BOTH_DIR_INFO, EaSize);
  const INDEX: usize = mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileId);
  const NAME: usize = mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);

  /// Lists `dir` with what `lstat` tells of each regular file: one call for some hundred
  /// entries, 0.35 us for each (measured), where an `lstat` for each takes ten times as long
  /// or more. `Ok(false)`: the disk is not NTFS, and nothing was listed. Only there it was
  /// measured that the listing and `lstat` give the same numbers.
  ///
  /// The listing is behind in two cases, both measured: for a file that a writer has open (it
  /// is right again when the writer closes the file), and for a hard link whose file was
  /// written through another name. The engine knows the first (`Fs::listing_lags`).
  pub fn read_dir(dir: &Path, each: &mut dyn FnMut(&OsStr, Entry)) -> io::Result<bool> {
    let dir = OpenOptions::new()
      .access_mode(FILE_LIST_DIRECTORY)
      .share_mode(SHARE_ALL)
      // Without it a directory cannot be opened at all.
      .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
      .open(dir)
      .map_err(told)?;
    let handle = dir.as_raw_handle();
    let mut file_system = [0_u16; 8];
    let mut serial = 0_u32;
    // SAFETY: the handle is open. The call writes at most eight units into `file_system`, one
    // number into `serial`, and nothing through the null pointers.
    let known = unsafe {
      let (no_name, none) = (std::ptr::null_mut(), std::ptr::null_mut());
      let (name, serial) = (file_system.as_mut_ptr(), &raw mut serial);
      GetVolumeInformationByHandleW(handle, no_name, 0, serial, none, none, name, 8)
    };
    if known == 0 || !"NTFS\0".encode_utf16().eq(file_system[..5].iter().copied()) {
      return Ok(false);
    }

    let mut records = Aligned([0; 16 * 1024]);
    let size = u32::try_from(records.0.len()).unwrap_or(u32::MAX);
    let mut name = Vec::new();
    let mut class = FileIdBothDirectoryRestartInfo;
    loop {
      // SAFETY: the handle is open, and the call writes at most `size` bytes into `records`.
      let filled =
        unsafe { GetFileInformationByHandleEx(handle, class, (&raw mut records).cast(), size) };
      if filled == 0 {
        let error = io::Error::last_os_error();
        return match error.raw_os_error().map(i32::cast_unsigned) {
          Some(ERROR_NO_MORE_FILES) => Ok(true),
          // The answer for a handle that is open on a file.
          Some(ERROR_INVALID_PARAMETER) => Err(io::ErrorKind::NotADirectory.into()),
          _ => Err(told(error)),
        };
      }
      class = FileIdBothDirectoryInfo;
      let mut rest = &records.0[..];
      loop {
        let read = record(rest, u64::from(serial), &mut name);
        let (next, told) = read.ok_or_else(|| io::Error::other("a listing gave no entry"))?;
        let own = OsString::from_wide(&name);
        if own != "." && own != ".." {
          each(&own, told);
        }
        match next {
          0 => break,
          next => rest = &rest[next..],
        }
      }
    }
  }

  /// The record at the start of `bytes`: where the next one starts (zero: none), and what it
  /// tells. Its name is put into `name`. `None`: the record does not fit into the bytes.
  /// `volume` is the one of the directory that is listed: an entry that is no reparse point is
  /// on it too.
  fn record(bytes: &[u8], volume: u64, name: &mut Vec<u16>) -> Option<(usize, Entry)> {
    let u32_at = |at: usize| Some(u32::from_ne_bytes(*bytes.get(at..)?.first_chunk()?));
    let u64_at = |at: usize| Some(u64::from_ne_bytes(*bytes.get(at..)?.first_chunk()?));
    let next = usize::try_from(u32_at(NEXT)?).ok()?;
    let name_bytes = bytes.get(NAME..)?.get(..usize::try_from(u32_at(NAME_BYTES)?).ok()?)?;
    // The next record is inside the bytes, and after this one.
    if next != 0 && (next < NAME + name_bytes.len() || next >= bytes.len()) {
      return None;
    }
    name.clear();
    name.extend(name_bytes.as_chunks().0.iter().map(|&unit| u16::from_ne_bytes(unit)));
    let (attributes, index) = (u32_at(ATTRIBUTES)?, u64_at(INDEX)?);
    let plain = attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0;
    let told = match kind(attributes, u32_at(TAG)?) {
      // A reparse point that is no link (a file in a cloud, a mounted disk) is left to
      // `lstat`: not measured.
      FileType::File if plain => {
        Entry::File { modified: time(u64_at(WRITTEN)?), size: u64_at(SIZE)?, file_id: index }
      }
      // The identity as `stat` tells it. It is never behind: it does not change.
      FileType::Dir if plain => {
        Entry::Dir { id: DirId { volume, inode: index, birth: u64_at(CREATED)? } }
      }
      kind => Entry::Kind(kind),
    };
    Some((next, told))
  }

  /// `path` for a call that takes it as it is, with a NUL at its end. A long path only works
  /// in the `\\?\` form, which std makes for its own calls and which takes no `/`.
  fn wide_path(path: &Path) -> Vec<u16> {
    const SEPARATOR: u16 = b'\\' as u16;
    let units = path.as_os_str().encode_wide();
    let units: Vec<u16> =
      units.map(|unit| if unit == u16::from(b'/') { SEPARATOR } else { unit }).collect();
    let verbatim = units.starts_with(&[SEPARATOR, SEPARATOR, u16::from(b'?'), SEPARATOR]);
    let (prefix, rest) = match units.as_slice() {
      units if verbatim || units.len() < MAX_PATH as usize - 12 => ("", units),
      // `\\server\share` becomes `\\?\UNC\server\share`.
      [SEPARATOR, rest @ ..] if rest.first() == Some(&SEPARATOR) => (r"\\?\UNC", rest),
      units => (r"\\?\", units),
    };
    prefix.encode_utf16().chain(rest.iter().copied()).chain([0]).collect()
  }

  /// A search for the one path gives the entry as the directory holds it.
  pub fn name_on_disk(path: &Path) -> io::Result<OsString> {
    let wide = wide_path(path);
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

  #[cfg(test)]
  mod tests {
    use std::{
      fs,
      io::{self, Write},
      path::Path,
      process::Command,
    };

    use super::{by_handle, by_name, name_on_disk, read_dir, stat, version};
    use crate::fs::{Entry, FileType, Meta};

    fn same(a: &Meta, b: &Meta) -> bool {
      (a.kind, a.modified, a.changed, a.size, a.file_id, a.dir_id)
        == (b.kind, b.modified, b.changed, b.size, b.file_id, b.dir_id)
    }

    fn junction(link: &Path, target: &Path) {
      let made = Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link).arg(target).output();
      assert!(made.is_ok_and(|output| output.status.success()), "mklink /J failed");
    }

    /// The call by name and the handle tell the same of a file, a directory and a junction,
    /// and both say "not found" for what is not there.
    #[test]
    fn the_call_by_name_tells_what_the_handle_tells() {
      let top = tempfile::tempdir().unwrap();
      let (file, dir, link) =
        (top.path().join("file"), top.path().join("dir"), top.path().join("link"));
      fs::write(&file, "written").unwrap();
      fs::create_dir(&dir).unwrap();
      junction(&link, &dir);
      // An older Windows has no call by name: there is nothing to compare.
      if by_name(&file).is_none() {
        return;
      }
      for (path, kind) in
        [(&file, FileType::File), (&dir, FileType::Dir), (&link, FileType::Symlink)]
      {
        let named = by_name(path).unwrap().unwrap();
        let opened = by_handle(path, false).unwrap();
        assert!(same(&named, &opened), "{}: {named:?} and {opened:?}", path.display());
        assert_eq!(named.kind, kind);
      }
      // A link is followed through a handle, and the directory behind it is the same one.
      assert_eq!(stat(&link, true).unwrap().dir_id, stat(&dir, false).unwrap().dir_id);
      for missing in [top.path().join("none"), file.join("below a file")] {
        assert_eq!(stat(&missing, false).unwrap_err().kind(), io::ErrorKind::NotFound);
      }
    }

    /// On NTFS the listing tells of a regular file what `lstat` tells, and of a directory and
    /// a junction the kind. It is behind while a writer has the file open.
    #[test]
    fn the_listing_tells_of_a_file_what_lstat_tells() {
      let top = tempfile::tempdir().unwrap();
      fs::create_dir(top.path().join("dir")).unwrap();
      junction(&top.path().join("link"), &top.path().join("dir"));
      fs::write(top.path().join("file"), "written").unwrap();
      let list = || {
        let mut seen = Vec::new();
        let listed = read_dir(top.path(), &mut |name, entry| seen.push((name.to_owned(), entry)));
        seen.sort_by(|a, b| a.0.cmp(&b.0));
        listed.unwrap().then_some(seen)
      };
      // Another file system: the listing of std is used there.
      let Some(seen) = list() else { return };
      let [
        (_, Entry::Dir { id }),
        (_, Entry::File { modified, size, file_id }),
        (_, Entry::Kind(FileType::Symlink)),
      ] = seen[..]
      else {
        panic!("{seen:?}");
      };
      let meta = stat(&top.path().join("file"), false).unwrap();
      assert_eq!((modified, size, file_id), (meta.modified, meta.size, meta.file_id));
      // Both ways of `stat` give the identity that the listing gave.
      let dir = top.path().join("dir");
      assert_eq!(id, stat(&dir, false).unwrap().dir_id);
      assert_eq!(id, by_handle(&dir, false).unwrap().dir_id);

      // A file is no directory.
      let error = read_dir(&top.path().join("file"), &mut |_, _| {}).unwrap_err();
      assert_eq!(error.kind(), io::ErrorKind::NotADirectory, "{error:?}");

      let mut writer = fs::OpenOptions::new().append(true).open(top.path().join("file")).unwrap();
      writer.write_all(b" and more").unwrap();
      let size = || match list().as_deref() {
        Some([_, (_, Entry::File { size, .. }), _]) => *size,
        seen => panic!("{seen:?}"),
      };
      assert_eq!(size(), 7, "the listing knows the new size before the writer closed the file");
      drop(writer);
      assert_eq!(size(), 16);
    }

    /// The name as the directory holds it, also at the end of a path of more than 260 units.
    #[test]
    fn the_name_on_disk() {
      let top = tempfile::tempdir().unwrap();
      let mut dir = top.path().to_owned();
      for _ in 0..3 {
        dir.push("d".repeat(100));
        assert_eq!(
          name_on_disk(&dir.join("ReadMe.md")).unwrap_err().kind(),
          io::ErrorKind::NotFound
        );
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("ReadMe.md"), "1").unwrap();
        for spelled in ["ReadMe.md", "README.MD", "readme.md"] {
          assert_eq!(name_on_disk(&dir.join(spelled)).unwrap(), "ReadMe.md", "{}", dir.display());
        }
      }
    }

    /// The version of a file changes with each write, and with nothing that only reads.
    #[test]
    fn the_version_changes_with_a_write() {
      let top = tempfile::tempdir().unwrap();
      let file = top.path().join("file");
      fs::write(&file, "one").unwrap();
      // A disk without a change journal: the content is hashed there.
      let Some(first) = version(&file).unwrap() else { return };
      assert_eq!(fs::read(&file).unwrap(), b"one");
      assert_eq!(stat(&file, false).unwrap().size, 3);
      assert_eq!(version(&file).unwrap(), Some(first));

      // The same size, and most often the same time stamp.
      fs::write(&file, "two").unwrap();
      assert_ne!(version(&file).unwrap(), Some(first));

      // Neither a directory nor nothing is a file to compare.
      for path in [top.path().to_owned(), top.path().join("none")] {
        assert_eq!(version(&path).unwrap_err().kind(), io::ErrorKind::NotFound);
      }
    }
  }
}

#[cfg(windows)]
pub(crate) use self::windows::{Unknown, kernel32};
#[cfg(windows)]
use self::windows::{name_on_disk, stat, told};

#[cfg(not(windows))]
fn told(error: io::Error) -> io::Error {
  error
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
