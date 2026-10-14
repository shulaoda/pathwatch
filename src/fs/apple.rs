//! macOS: a listing with what `lstat` tells of each entry, in one call, and the name of one
//! entry as the disk spells it. A file is taken from the listing on APFS only (`references/design.md`
//! 5.4).

use std::{
  ffi::{CString, OsStr, OsString, c_char, c_void},
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

/// `struct attrlist`.
#[repr(C)]
struct AttrList {
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
const ATTR_BIT_MAP_COUNT: u16 = 5;
const ATTR_CMN_NAME: u32 = 0x1;
const ATTR_CMN_DEVID: u32 = 0x2;
const ATTR_CMN_OBJTYPE: u32 = 0x8;
const ATTR_CMN_MODTIME: u32 = 0x400;
const ATTR_CMN_FLAGS: u32 = 0x4_0000;
const ATTR_CMN_FILEID: u32 = 0x0200_0000;
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
const ATTR_FILE_DATALENGTH: u32 = 0x200;
/// What a look at a regular file reads, in the common attributes.
const FILE: u32 = ATTR_CMN_DEVID | ATTR_CMN_MODTIME | ATTR_CMN_FLAGS | ATTR_CMN_FILEID;
/// The content of the file is in a cloud, not on this disk.
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

/// `volume` is what `apfs_volume` gave for `dir`. An entry on another device is given as a
/// name only: the directory can be another one by now.
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
  // An entry takes less than 1 KiB.
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
      if let Some((name, told)) = read(entry, volume) {
        each(name, told);
      }
    }
  }
}

/// One entry of the answer: its length, the attributes that follow as bits, and then these
/// attributes in the order of their bits. `None`: it has no name.
fn read(entry: &[u8], volume: i32) -> Option<(&OsStr, Entry)> {
  let mut fields = Fields { bytes: entry, at: size_of::<u32>() };
  let [common, _, _, file, _] = [(); 5].map(|()| fields.u32());
  let (common, file) = (common?, file?);
  let has = |attribute: u32| common & attribute != 0;
  let failed = has(ATTR_CMN_ERROR) && fields.u32()? != 0;
  if !has(ATTR_CMN_NAME) {
    return None;
  }
  // The name: where it starts, counted from this field, and its length with the NUL.
  let (reference, offset, len) = (fields.at, fields.i32()?, fields.u32()?);
  let start = reference.checked_add_signed(isize::try_from(offset).ok()?)?;
  let end = start.checked_add((len as usize).checked_sub(1)?)?;
  let name = OsStr::from_bytes(entry.get(start..end)?);
  let device = if has(ATTR_CMN_DEVID) { Some(fields.i32()?) } else { None };
  let kind = if has(ATTR_CMN_OBJTYPE) { Some(fields.u32()?) } else { None };
  let Some(kind) = kind.map(file_type) else { return Some((name, Entry::Name)) };
  let whole = common & FILE == FILE && file & ATTR_FILE_DATALENGTH != 0;
  if failed || kind != FileType::File || !whole || device != Some(volume) {
    return Some((name, Entry::Kind(kind)));
  }
  let (modified, flags, file_id, size) =
    (fields.time(), fields.u32()?, fields.u64()?, fields.u64()?);
  // A time before 1970, and a file in a cloud, are left to `lstat`: not measured.
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

  /// A `struct timespec`; `None` before 1970.
  fn time(&mut self) -> Option<SystemTime> {
    let [seconds, nanos] = [(); 2].map(|()| self.take().map(i64::from_ne_bytes));
    let since = u64::try_from(seconds?).ok().zip(u32::try_from(nanos?).ok());
    since.map(|(seconds, nanos)| SystemTime::UNIX_EPOCH + Duration::new(seconds, nanos))
  }
}

fn file_type(object_type: u32) -> FileType {
  match object_type {
    VREG => FileType::File,
    VDIR => FileType::Dir,
    VLNK => FileType::Symlink,
    _ => FileType::Other,
  }
}

/// The name that the directory holds for the entry at `path`, and the kind of the entry, in
/// one `getattrlist` call. A link at the end is not followed.
pub(crate) fn name_and_kind(path: &Path) -> io::Result<(OsString, FileType)> {
  /// The answer: its length, the name as an offset from `name_offset` and a length with the
  /// NUL, the object type, and then the bytes.
  #[repr(C)]
  struct Answer {
    length: u32,
    name_offset: i32,
    name_length: u32,
    object_type: u32,
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
    common: ATTR_CMN_NAME | ATTR_CMN_OBJTYPE,
    volume: 0,
    dir: 0,
    file: 0,
    fork: 0,
  };
  let mut answer =
    Answer { length: 0, name_offset: 0, name_length: 0, object_type: 0, bytes: [0; NAME_BYTES] };
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
  Ok((OsStr::from_bytes(name).to_owned(), file_type(answer.object_type)))
}

pub(super) fn name_on_disk(path: &Path) -> io::Result<OsString> {
  name_and_kind(path).map(|(name, _)| name)
}
