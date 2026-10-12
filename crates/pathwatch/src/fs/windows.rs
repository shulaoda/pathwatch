//! Windows. What the calls of std do not tell, and what they cost too much for.

use std::{
  borrow::Cow,
  error::Error,
  ffi::{CStr, OsStr, OsString, c_void},
  fmt,
  fs::{self, OpenOptions},
  io, mem,
  os::windows::{
    ffi::{OsStrExt, OsStringExt},
    fs::{MetadataExt, OpenOptionsExt},
    io::AsRawHandle,
  },
  path::{Path, PathBuf},
  sync::OnceLock,
  time::{Duration, SystemTime},
};

use windows_sys::Win32::{
  Foundation::{
    ERROR_ACCESS_DENIED, ERROR_BAD_NETPATH, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES,
    ERROR_SHARING_VIOLATION, INVALID_HANDLE_VALUE, MAX_PATH, NTSTATUS, STATUS_DELETE_PENDING,
    STATUS_FILE_IS_A_DIRECTORY, STATUS_NO_SUCH_FILE, STATUS_OBJECT_NAME_NOT_FOUND,
    STATUS_OBJECT_PATH_NOT_FOUND,
  },
  Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_BOTH_DIR_INFO, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FileAttributeTagInfo, FileIdBothDirectoryInfo,
    FileIdBothDirectoryRestartInfo, FindClose, FindExInfoBasic, FindExSearchNameMatch,
    FindFirstFileExW, GetFileInformationByHandle, GetFileInformationByHandleEx,
    GetVolumeInformationByHandleW, WIN32_FIND_DATAW,
  },
  System::{
    IO::DeviceIoControl,
    Ioctl::{FSCTL_READ_FILE_USN_DATA, USN_RECORD_V2, USN_RECORD_V3},
    LibraryLoader::{GetModuleHandleW, GetProcAddress},
  },
};

use super::{DirId, Entry, FileType, Meta, meta};

/// Other programs may still write, delete and rename what is looked at. Not rename another
/// file over it, nor a directory above it: Windows refuses that while any handle is open,
/// whatever the handle shares.
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
/// From 1601 to 1970, in the steps of 100 ns that Windows counts time in.
const TO_1970: u64 = 116_444_736_000_000_000;
/// The bit of a reparse tag that makes the entry a link for std, and so here.
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

/// Windows has no change time: the modification time stands in, as with std.
fn entry(kind: FileType, written: u64, size: u64, dir_id: DirId) -> Meta {
  let modified = time(written);
  Meta { kind, modified, changed: modified, size, file_id: dir_id.inode, dir_id }
}

/// "Access denied" is also what Windows says for an entry that is being deleted and for a
/// directory opened as a file; only the status of the kernel tells these from an entry that
/// may not be read. Call it right after the failed call, on the same thread. A server that
/// cannot be reached says nothing about the path, though std calls that "not found".
pub fn told(error: io::Error) -> io::Error {
  #[link(name = "ntdll")]
  unsafe extern "system" {
    safe fn RtlGetLastNtStatus() -> NTSTATUS;
  }

  match error.raw_os_error().map(i32::cast_unsigned) {
    Some(ERROR_BAD_NETPATH) => return io::Error::new(io::ErrorKind::HostUnreachable, error),
    Some(ERROR_ACCESS_DENIED) => {}
    _ => return error,
  }
  match RtlGetLastNtStatus() {
    STATUS_DELETE_PENDING => io::Error::new(io::ErrorKind::NotFound, DeletePending),
    // The last two come from the listing that std asks for an entry it cannot open.
    STATUS_FILE_IS_A_DIRECTORY
    | STATUS_NO_SUCH_FILE
    | STATUS_OBJECT_NAME_NOT_FOUND
    | STATUS_OBJECT_PATH_NOT_FOUND => io::ErrorKind::NotFound.into(),
    _ => error,
  }
}

/// The entry is being deleted: its name goes when the last handle on it is closed, unless the
/// holder takes the deletion back. Not found for a look, with this mark on the error.
#[derive(Debug)]
struct DeletePending;

impl fmt::Display for DeletePending {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str("the entry is being deleted")
  }
}

impl Error for DeletePending {}

/// Whether the error says that the entry is being deleted.
pub fn is_delete_pending(error: &io::Error) -> bool {
  error.get_ref().is_some_and(<dyn Error + Send + Sync>::is::<DeletePending>)
}

/// `lstat`, or `stat` with `follow`. std gives no file index and no volume, and only the
/// file index tells a file that was removed and written again from the old one. By name
/// where Windows has the call (3.5 us, measured), else through a handle (10 us).
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

/// A function of `kernel32.dll` that not every Windows has: an import of it would keep the
/// program from starting there.
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

/// `lstat` in one call that opens nothing. `None`: the handle is asked, as for every error
/// but "nothing is there".
fn by_name(path: &Path) -> Option<io::Result<Meta>> {
  let call = by_name_call()?;
  // The call takes the path as it is. A longer one needs the `\\?\` form that std makes, and
  // so does one with a name that Windows would read as another.
  if is_odd(path) {
    return None;
  }
  let mut wide = [0_u16; MAX_PATH as usize];
  for (at, unit) in path.as_os_str().encode_wide().enumerate() {
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
  let path = &*literal(path);
  let link = if follow { 0 } else { FILE_FLAG_OPEN_REPARSE_POINT };
  let opened = OpenOptions::new()
    .access_mode(0)
    .share_mode(SHARE_ALL)
    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | link)
    .open(path);
  let file = match opened.map_err(told) {
    Ok(file) => file,
    Err(error) => {
      // A system file in use cannot be opened. Its listing still tells of it, without an index.
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

/// The place of the last record about the file in the change journal of its disk: it moves
/// with every write, and also with a rename, other attributes, and the close of a writer.
/// `None`: the disk keeps no journal. A read of the content for a hash waits for the virus
/// scanner right after a write (2 ms, 14 ms for one in ten); this call takes 0.04 ms.
pub fn version(path: &Path) -> io::Result<Option<u64>> {
  // A directory cannot be opened in this way: it is no file to compare (see `told`).
  let opened = OpenOptions::new().access_mode(0).share_mode(SHARE_ALL).open(literal(path));
  let file = opened.map_err(told)?;
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
  let at = match major.and_then(|major| major.first_chunk()).map(|major| u16::from_ne_bytes(*major))
  {
    Some(2) => mem::offset_of!(USN_RECORD_V2, Usn),
    Some(3) => mem::offset_of!(USN_RECORD_V3, Usn),
    _ => return Ok(None),
  };
  let usn = bytes.get(at..).and_then(|usn| usn.first_chunk());
  // Zero: the journal has no record about the file (it is off, or younger than the file), so
  // the number would stay the same over a write.
  Ok(usn.map(|usn| u64::from_ne_bytes(*usn)).filter(|&usn| usn != 0))
}

// A record is read from its bytes at these offsets, each checked against the answer.
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

/// Lists `dir` with what `lstat` tells of each regular file, 0.35 us for an entry (measured).
/// `Ok(false)`: the disk is not NTFS, the only one where this was measured to match `lstat`.
/// The listing is behind for a file that a writer has open (`Fs::listing_lags`) and for a
/// hard link written through another name.
pub fn read_dir(dir: &Path, each: &mut dyn FnMut(&OsStr, Entry)) -> io::Result<bool> {
  let dir = OpenOptions::new()
    .access_mode(FILE_LIST_DIRECTORY)
    .share_mode(SHARE_ALL)
    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
    .open(literal(dir))
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
        // The first answer for a directory with no entry at all: the root of an empty disk,
        // which has no `.` and no `..`.
        Some(ERROR_FILE_NOT_FOUND) if class == FileIdBothDirectoryRestartInfo => Ok(true),
        // A disk that says "NTFS" and does not know the class (the drive of a Remote Desktop
        // client), or a handle that is open on a file: std lists, and says what is no directory.
        _ if class == FileIdBothDirectoryRestartInfo => Ok(false),
        _ => Err(told(error)),
      };
    }
    class = FileIdBothDirectoryInfo;
    let mut rest = &records.0[..];
    loop {
      let read = record(rest, u64::from(serial), &mut name);
      let (next, told) = read.ok_or_else(|| io::Error::other("a listing gave no entry"))?;
      let own = OsString::from_wide(&name);
      if own != "." && own != ".." && !has_separator(&name) {
        each(&own, told);
      }
      match next {
        0 => break,
        next => rest = &rest[next..],
      }
    }
  }
}

/// The record at the start of `bytes`: where the next one starts (zero: none) and what it
/// tells, with its name in `name`. `None`: it does not fit into the bytes.
fn record(bytes: &[u8], volume: u64, name: &mut Vec<u16>) -> Option<(usize, Entry)> {
  let u32_at = |at: usize| Some(u32::from_ne_bytes(*bytes.get(at..)?.first_chunk()?));
  let u64_at = |at: usize| Some(u64::from_ne_bytes(*bytes.get(at..)?.first_chunk()?));
  let next = usize::try_from(u32_at(NEXT)?).ok()?;
  let name_bytes = bytes.get(NAME..)?.get(..usize::try_from(u32_at(NAME_BYTES)?).ok()?)?;
  if next != 0 && (next < NAME + name_bytes.len() || next >= bytes.len()) {
    return None;
  }
  name.clear();
  name.extend(name_bytes.as_chunks().0.iter().map(|&unit| u16::from_ne_bytes(unit)));
  let (attributes, index) = (u32_at(ATTRIBUTES)?, u64_at(INDEX)?);
  let plain = attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0;
  let told = match kind(attributes, u32_at(TAG)?) {
    // A reparse point that is no link (a file in a cloud, a mounted disk) is left to `lstat`.
    FileType::File if plain => {
      Entry::File { modified: time(u64_at(WRITTEN)?), size: u64_at(SIZE)?, file_id: index }
    }
    // The identity never changes, so the listing is never behind with it.
    FileType::Dir if plain => {
      Entry::Dir { id: DirId { volume, inode: index, birth: u64_at(CREATED)? } }
    }
    kind => Entry::Kind(kind),
  };
  Some((next, told))
}

/// Whether Windows reads a name of `path` as another one, unless the path has the `\\?\` form:
/// it takes a dot or a space off the end of a name, and reads the name of a device (`NUL`,
/// `COM1`, on an older Windows also `aux.c`) as the device. No watched path has a name of the
/// first kind (`path::normalize`), but a directory can list both.
fn is_odd(path: &Path) -> bool {
  let is_separator = |byte: &u8| matches!(byte, b'\\' | b'/');
  let mut names = path.as_os_str().as_encoded_bytes().split(is_separator);
  names.any(|name| {
    let cut_off = matches!(name.last(), Some(b'.' | b' ')) && name != b"." && name != b"..";
    cut_off || is_device(name)
  })
}

/// Whether some Windows reads `name` as a device: what is before its first dot or colon,
/// without the spaces at its end, is the name of one.
fn is_device(name: &[u8]) -> bool {
  let end = name.iter().position(|byte| matches!(byte, b'.' | b':')).unwrap_or(name.len());
  let stem = name[..end].trim_ascii_end();
  let is = |device: &[u8]| stem.eq_ignore_ascii_case(device);
  let is_port =
    |port: &[u8]| port.eq_ignore_ascii_case(b"COM") || port.eq_ignore_ascii_case(b"LPT");
  match stem {
    [_, _, _] => is(b"CON") || is(b"PRN") || is(b"AUX") || is(b"NUL"),
    // `COM0` to `COM9` and `LPT0` to `LPT9`.
    [port @ .., digit] if port.len() == 3 && digit.is_ascii_digit() => is_port(port),
    // The same with a digit that is written high: `¹`, `²`, `³`.
    [port @ .., 0xC2, 0xB2 | 0xB3 | 0xB9] if port.len() == 3 => is_port(port),
    _ => is(b"CONIN$") || is(b"CONOUT$"),
  }
}

/// `path` as Windows reads these very names: in the `\\?\` form when a name in it is odd
/// (`is_odd`). Every call that takes a path of the engine gets it this way.
pub(crate) fn literal(path: &Path) -> Cow<'_, Path> {
  if !is_odd(path) {
    return Cow::Borrowed(path);
  }
  Cow::Owned(PathBuf::from(OsString::from_wide(&verbatim(path, true))))
}

/// The units of `path`, in the `\\?\` form when it is long, as std does for its own calls, or
/// when `always`.
fn verbatim(path: &Path, always: bool) -> Vec<u16> {
  const SEPARATOR: u16 = b'\\' as u16;
  let units = path.as_os_str().encode_wide();
  let units: Vec<u16> =
    units.map(|unit| if unit == u16::from(b'/') { SEPARATOR } else { unit }).collect();
  let is_verbatim = units.starts_with(&[SEPARATOR, SEPARATOR, u16::from(b'?'), SEPARATOR]);
  let (prefix, rest) = match units.as_slice() {
    units if is_verbatim || (!always && units.len() < MAX_PATH as usize - 12) => ("", units),
    // `\\server\share` becomes `\\?\UNC\server\share`.
    [SEPARATOR, rest @ ..] if rest.first() == Some(&SEPARATOR) => (r"\\?\UNC", rest),
    units => (r"\\?\", units),
  };
  prefix.encode_utf16().chain(rest.iter().copied()).collect()
}

/// `path` with a NUL at its end, in the form that names this very entry.
fn wide_path(path: &Path) -> Vec<u16> {
  let mut units = verbatim(path, is_odd(path));
  units.push(0);
  units
}

/// Opens the regular file at `path` for reading. A link or a junction (a reparse point that
/// stands for another name) is not followed: `None`. Any other reparse point (a file of a cloud
/// drive, a compressed file) is the file itself, and is read as such.
pub fn open_regular(path: &Path) -> io::Result<Option<fs::File>> {
  let path = &*literal(path);
  let opened = OpenOptions::new()
    .read(true)
    .share_mode(SHARE_ALL)
    .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
    .open(path);
  let file = opened.map_err(told)?;
  let meta = file.metadata()?;
  if meta.is_dir() || meta.file_type().is_symlink() {
    return Ok(None);
  }
  if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
    return Ok(Some(file));
  }
  drop(file);
  let file = OpenOptions::new().read(true).share_mode(SHARE_ALL).open(path).map_err(told)?;
  Ok(file.metadata()?.is_file().then_some(file))
}

/// A name with a separator in it, which a disk that another system filled can hold: no path
/// names such an entry, so it is left out of a listing.
pub fn has_separator(name: &[u16]) -> bool {
  name.iter().any(|&unit| unit == u16::from(b'\\') || unit == u16::from(b'/'))
}

/// A search for the one path gives the entry as the directory holds it. In a directory that
/// may be passed but not listed the question has no answer, which says nothing against the
/// name, as on Linux.
pub fn name_on_disk(path: &Path) -> io::Result<OsString> {
  let wide = wide_path(path);
  // SAFETY: all zeros is a valid value of this plain struct of numbers.
  let mut found: WIN32_FIND_DATAW = unsafe { mem::zeroed() };
  // SAFETY: `wide` ends with a NUL, and the call writes one whole struct into `found`. The
  // basic answer leaves the short name out, which the disk would have to make.
  let search = unsafe {
    let (basic, by_name, no_filter) = (FindExInfoBasic, FindExSearchNameMatch, std::ptr::null());
    FindFirstFileExW(wide.as_ptr(), basic, (&raw mut found).cast(), by_name, no_filter, 0)
  };
  if search == INVALID_HANDLE_VALUE {
    let error = io::Error::last_os_error();
    return match error.raw_os_error().map(i32::cast_unsigned) {
      // What NTFS answers for an entry that went while the directory was searched.
      Some(ERROR_NO_MORE_FILES) => Err(io::ErrorKind::NotFound.into()),
      Some(ERROR_ACCESS_DENIED) => match told(error) {
        error if error.kind() == io::ErrorKind::NotFound => Err(error),
        _ => {
          path.file_name().map(OsStr::to_owned).ok_or_else(|| io::ErrorKind::InvalidInput.into())
        }
      },
      _ => Err(told(error)),
    };
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

  use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};

  use windows_sys::Win32::{
    Foundation::ERROR_BAD_NETPATH,
    Storage::FileSystem::{
      DELETE, FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
    },
  };

  use super::{
    SHARE_ALL, by_handle, by_name, is_delete_pending, is_odd, literal, name_on_disk, read_dir,
    stat, told, version,
  };
  use crate::fs::{Entry, FileType, Fs, Meta, StdFs};

  fn same(a: &Meta, b: &Meta) -> bool {
    (a.kind, a.modified, a.changed, a.size, a.file_id, a.dir_id)
      == (b.kind, b.modified, b.changed, b.size, b.file_id, b.dir_id)
  }

  fn junction(link: &Path, target: &Path) {
    let made = Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link).arg(target).output();
    assert!(made.is_ok_and(|output| output.status.success()), "mklink /J failed");
  }

  #[test]
  fn the_call_by_name_tells_what_the_handle_tells() {
    let top = tempfile::tempdir().unwrap();
    let (file, dir, link) =
      (top.path().join("file"), top.path().join("dir"), top.path().join("link"));
    fs::write(&file, "written").unwrap();
    fs::create_dir(&dir).unwrap();
    junction(&link, &dir);
    // An older Windows has no call by name.
    if by_name(&file).is_none() {
      return;
    }
    for (path, kind) in [(&file, FileType::File), (&dir, FileType::Dir), (&link, FileType::Symlink)]
    {
      let named = by_name(path).unwrap().unwrap();
      let opened = by_handle(path, false).unwrap();
      assert!(same(&named, &opened), "{}: {named:?} and {opened:?}", path.display());
      assert_eq!(named.kind, kind);
    }
    assert_eq!(stat(&link, true).unwrap().dir_id, stat(&dir, false).unwrap().dir_id);
    for missing in [top.path().join("none"), file.join("below a file")] {
      assert_eq!(stat(&missing, false).unwrap_err().kind(), io::ErrorKind::NotFound);
    }
  }

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
    // Not NTFS: the listing of std is used there.
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
    let dir = top.path().join("dir");
    assert_eq!(id, stat(&dir, false).unwrap().dir_id);
    assert_eq!(id, by_handle(&dir, false).unwrap().dir_id);

    // A handle on a file does not take the class: std lists then, and says what is no directory.
    assert!(!read_dir(&top.path().join("file"), &mut |_, _| {}).unwrap());
    let error = StdFs.read_dir(&top.path().join("file"), &mut |_, _| {}).unwrap_err();
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

  /// Also at the end of a path of more than 260 units.
  #[test]
  fn the_name_on_disk() {
    let top = tempfile::tempdir().unwrap();
    let mut dir = top.path().to_owned();
    for _ in 0..3 {
      dir.push("d".repeat(100));
      assert_eq!(name_on_disk(&dir.join("ReadMe.md")).unwrap_err().kind(), io::ErrorKind::NotFound);
      fs::create_dir(&dir).unwrap();
      fs::write(dir.join("ReadMe.md"), "1").unwrap();
      for spelled in ["ReadMe.md", "README.MD", "readme.md"] {
        assert_eq!(name_on_disk(&dir.join(spelled)).unwrap(), "ReadMe.md", "{}", dir.display());
      }
    }
  }

  /// Takes the right to list a directory away from the user of the test, and gives it back.
  struct Unlisted(std::path::PathBuf);

  impl Unlisted {
    fn new(dir: &Path) -> Self {
      let user = std::env::var("USERNAME").unwrap();
      let denied =
        Command::new("icacls").arg(dir).args(["/deny", &format!("{user}:(RD)")]).output();
      assert!(denied.is_ok_and(|output| output.status.success()), "icacls /deny failed");
      Self(dir.to_owned())
    }
  }

  impl Drop for Unlisted {
    fn drop(&mut self) {
      let user = std::env::var("USERNAME").unwrap();
      let _ = Command::new("icacls").arg(&self.0).args(["/remove:d", &user]).output();
    }
  }

  /// The name of an entry in a directory that may be passed but not listed: the question has
  /// no answer, and the name is taken as given.
  #[test]
  fn the_name_on_disk_in_a_directory_that_may_not_be_listed() {
    let top = tempfile::tempdir().unwrap();
    let dir = top.path().join("closed");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("ReadMe.md"), "1").unwrap();
    let _unlisted = Unlisted::new(&dir);
    assert!(fs::read_dir(&dir).is_err(), "the directory can still be listed");
    assert_eq!(stat(&dir.join("readme.md"), false).unwrap().kind, FileType::File);
    assert_eq!(name_on_disk(&dir.join("readme.md")).unwrap(), "readme.md");
    // The question cannot tell a name that is not there either; the look before it can.
    assert_eq!(name_on_disk(&dir.join("gone.md")).unwrap(), "gone.md");
    assert_eq!(stat(&dir.join("gone.md"), false).unwrap_err().kind(), io::ErrorKind::NotFound);
  }

  /// `SetFileInformationByHandle(FileDispositionInfo)`: the deletion that waits for the last
  /// handle to close, which `DeleteFile` no longer uses on NTFS.
  fn dispose(holder: &fs::File, delete: bool) {
    let info = FILE_DISPOSITION_INFO { DeleteFile: delete };
    let size = u32::try_from(size_of::<FILE_DISPOSITION_INFO>()).unwrap();
    // SAFETY: the handle is open, and `info` is one whole struct of the class.
    let set = unsafe {
      let (class, info) = (FileDispositionInfo, (&raw const info).cast());
      SetFileInformationByHandle(holder.as_raw_handle(), class, info, size)
    };
    assert!(set != 0, "{}", io::Error::last_os_error());
  }

  /// A server that cannot be reached is no answer, and an entry that is being deleted is
  /// missing, with the mark that says so.
  #[test]
  fn what_the_errors_of_windows_mean() {
    let unreachable = told(io::Error::from_raw_os_error(ERROR_BAD_NETPATH.cast_signed()));
    assert_eq!(unreachable.kind(), io::ErrorKind::HostUnreachable, "{unreachable:?}");
    assert_eq!(told(io::Error::from_raw_os_error(2)).kind(), io::ErrorKind::NotFound);

    let top = tempfile::tempdir().unwrap();
    let file = top.path().join("held.txt");
    fs::write(&file, "1").unwrap();
    let holder =
      fs::OpenOptions::new().access_mode(DELETE).share_mode(SHARE_ALL).open(&file).unwrap();
    dispose(&holder, true);
    for follow in [false, true] {
      let error = stat(&file, follow).unwrap_err();
      assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error:?}");
      assert!(is_delete_pending(&error), "{error:?}");
    }
    // The directory still lists the name.
    assert_eq!(name_on_disk(&file).unwrap(), "held.txt");
    // Taken back: the file is there, as if nothing had happened.
    dispose(&holder, false);
    assert_eq!(stat(&file, false).unwrap().size, 1);
    dispose(&holder, true);
    drop(holder);
    let error = stat(&file, false).unwrap_err();
    assert!(error.kind() == io::ErrorKind::NotFound && !is_delete_pending(&error), "{error:?}");
  }

  #[test]
  fn a_name_that_windows_reads_as_another() {
    let odd = [
      r"C:\w\foo.",
      r"C:\w\foo ",
      r"C:\w\dir.\x.txt",
      r"C:\w\...",
      r"C:\w\nul",
      r"C:\w\NUL.txt",
      r"C:\w\aux.c",
      r"C:\w\Com1",
      r"C:\w\lpt9.log",
      "C:\\w\\com\u{b9}",
      r"C:\w\con \x",
      r"C:\w\conin$",
      r"\\server\share\prn",
    ];
    for path in odd {
      assert!(is_odd(Path::new(path)), "{path}");
    }
    let plain = [
      r"C:\w\foo",
      r"C:\w\.git\..x",
      r"C:\w\null",
      r"C:\w\con1",
      r"C:\w\com",
      r"C:\w\com10",
      r"C:\w\xnul",
      r"C:\w\a.nul",
      r"\\server\share\a b\c.d",
    ];
    for path in plain {
      assert!(!is_odd(Path::new(path)), "{path}");
      assert_eq!(literal(Path::new(path)), Path::new(path));
    }
    assert_eq!(literal(Path::new(r"C:\w\foo.")), Path::new(r"\\?\C:\w\foo."));
    assert_eq!(literal(Path::new(r"\\?\C:\w\foo.")), Path::new(r"\\?\C:\w\foo."));
    assert_eq!(literal(Path::new(r"\\server\share\nul")), Path::new(r"\\?\UNC\server\share\nul"));
  }

  /// Every call reads the entry with the odd name, not the one that Windows makes of it.
  #[test]
  fn an_odd_name_is_read_as_it_is() {
    let top = tempfile::tempdir().unwrap();
    // Only a path in `\\?\` form lets std make such a name.
    let verbatim = fs::canonicalize(top.path()).unwrap();
    fs::write(top.path().join("foo"), "the other one").unwrap();
    for name in ["foo.", "foo ", "nul", "aux.c"] {
      fs::write(verbatim.join(name), name).unwrap();
      let path = top.path().join(name);
      let found = stat(&path, false).unwrap();
      assert_eq!((found.kind, found.size), (FileType::File, name.len() as u64), "{name}");
      assert_eq!(name_on_disk(&path).unwrap(), name);
      assert!(version(&path).is_ok(), "{name}");
      assert_eq!(fs::read(literal(&path)).unwrap(), name.as_bytes());
    }
    fs::create_dir(verbatim.join("dir.")).unwrap();
    fs::write(verbatim.join("dir.").join("x.txt"), "1").unwrap();
    let dir = top.path().join("dir.");
    assert_eq!(stat(&dir, false).unwrap().kind, FileType::Dir);
    let mut seen = Vec::new();
    // Not NTFS: the listing of std is used there.
    if read_dir(&dir, &mut |name, _| seen.push(name.to_owned())).unwrap() {
      assert_eq!(seen, ["x.txt"]);
    }
    assert_eq!(stat(&dir.join("x.txt"), false).unwrap().size, 1);
  }

  #[test]
  fn the_version_changes_with_a_write() {
    let top = tempfile::tempdir().unwrap();
    let file = top.path().join("file");
    fs::write(&file, "one").unwrap();
    // A disk without a change journal.
    let Some(first) = version(&file).unwrap() else { return };
    assert_eq!(fs::read(&file).unwrap(), b"one");
    assert_eq!(stat(&file, false).unwrap().size, 3);
    assert_eq!(version(&file).unwrap(), Some(first));

    // The same size, and most often the same time stamp.
    fs::write(&file, "two").unwrap();
    assert_ne!(version(&file).unwrap(), Some(first));

    for path in [top.path().to_owned(), top.path().join("none")] {
      assert_eq!(version(&path).unwrap_err().kind(), io::ErrorKind::NotFound);
    }
  }
}
