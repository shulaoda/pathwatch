//! macOS: a listing with what `lstat` tells of each entry, in one call, and the name of one
//! entry as the disk spells it. A file or a directory is taken from the listing on APFS only
//! (`references/design.md` 5.4).

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

use super::{DirId, Entry, FileType, nanos};

/// `struct attrlist`.
#[repr(C)]
struct AttrList {
  bitmap_count: u16,
  reserved: u16,
  common: u32,
  volume: u32,
  dir: u32,
  file: u32,
  fork: u32,
}
impl AttrList {
  fn new(common: u32, dir: u32, file: u32) -> Self {
    Self { bitmap_count: ATTR_BIT_MAP_COUNT, reserved: 0, common, volume: 0, dir, file, fork: 0 }
  }
}
/// `struct statfs`. Only the device, the flags and the name of the file system are read.
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
const ATTR_CMN_CRTIME: u32 = 0x200;
const ATTR_CMN_MODTIME: u32 = 0x400;
const ATTR_CMN_FLAGS: u32 = 0x4_0000;
const ATTR_CMN_FILEID: u32 = 0x0200_0000;
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
const ATTR_DIR_MOUNTSTATUS: u32 = 0x4;
const ATTR_FILE_DATALENGTH: u32 = 0x200;
/// What a look at a regular file or a directory reads, in the common attributes: the birth time
/// is in the identity of a directory.
const LOOK: u32 =
  ATTR_CMN_DEVID | ATTR_CMN_CRTIME | ATTR_CMN_MODTIME | ATTR_CMN_FLAGS | ATTR_CMN_FILEID;
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

fn stat_fs(dir: &Path) -> Option<StatFs> {
  let c_path = CString::new(dir.as_os_str().as_bytes()).ok()?;
  // SAFETY: all zeros is a valid value of this plain struct of numbers.
  let mut answer: StatFs = unsafe { mem::zeroed() };
  // SAFETY: `c_path` ends with a NUL, and the call writes one whole `struct statfs`.
  let result = unsafe { statfs(c_path.as_ptr(), &raw mut answer) };
  (result == 0).then_some(answer)
}

/// The device number of the disk under `dir`, if it is an APFS disk.
pub fn apfs_volume(dir: &Path) -> Option<i32> {
  stat_fs(dir).filter(|answer| answer.type_name.starts_with(b"apfs\0")).map(|answer| answer.id[0])
}

/// The name of the file system under `dir` (`nfs`, `smbfs`), if it is not on this machine.
pub fn remote_disk(dir: &Path) -> Option<String> {
  const MNT_LOCAL: u32 = 0x1000;
  let answer = stat_fs(dir).filter(|answer| answer.owner_type_flags_subtype[2] & MNT_LOCAL == 0)?;
  let name = answer.type_name.split(|&byte| byte == 0).next().unwrap_or_default();
  Some(String::from_utf8_lossy(name).into_owned())
}

/// `volume` is what `apfs_volume` gave for `dir`. An entry on another device is given as a
/// name only: the directory can be another one by now.
pub fn read_dir(dir: &Path, volume: i32, each: &mut dyn FnMut(&OsStr, Entry)) -> io::Result<()> {
  // Not a pipe, which would wait for its writer.
  let dir = OpenOptions::new().read(true).custom_flags(O_DIRECTORY).open(dir)?;
  let common = ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_ERROR | ATTR_CMN_NAME | ATTR_CMN_OBJTYPE | LOOK;
  let wanted = AttrList::new(common, ATTR_DIR_MOUNTSTATUS, ATTR_FILE_DATALENGTH);
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
  let [common, _, dir, file, _] = [(); 5].map(|()| fields.u32());
  let (common, dir, file) = (common?, dir?, file?);
  let has = |attribute: u32| common & attribute != 0;
  let failed = has(ATTR_CMN_ERROR) && fields.u32()? != 0;
  if !has(ATTR_CMN_NAME) {
    return None;
  }
  let name = OsStr::from_bytes(fields.reference()?);
  let device = if has(ATTR_CMN_DEVID) { Some(fields.i32()?) } else { None };
  let kind = if has(ATTR_CMN_OBJTYPE) { Some(fields.u32()?) } else { None };
  let Some(kind) = kind.map(file_type) else { return Some((name, Entry::Name)) };
  let whole = common & LOOK == LOOK
    && match kind {
      FileType::File => file & ATTR_FILE_DATALENGTH != 0,
      FileType::Dir => dir & ATTR_DIR_MOUNTSTATUS != 0,
      FileType::Symlink | FileType::Other => false,
    };
  if failed || !whole || device != Some(volume) {
    return Some((name, Entry::Kind(kind)));
  }
  let (created, modified, flags, file_id) =
    (fields.time(), fields.time(), fields.u32()?, fields.u64()?);
  // A time before 1970, and a file in a cloud, are left to `lstat`: not measured. So is a
  // directory that a disk is mounted on, as the listing tells of the one below the disk.
  let told = match (kind, created, modified) {
    (FileType::Dir, Some(created), Some(modified)) if fields.u32()? == 0 => {
      let volume = i64::from(volume).cast_unsigned();
      Entry::Dir { id: DirId { volume, inode: file_id, birth: nanos(created) }, modified }
    }
    (FileType::File, _, Some(modified)) if flags & SF_DATALESS == 0 => {
      Entry::File { modified, size: fields.u64()?, file_id }
    }
    _ => Entry::Kind(kind),
  };
  Some((name, told))
}

struct Fields<'a> {
  bytes: &'a [u8],
  at: usize,
}

impl<'a> Fields<'a> {
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

  /// An `attrreference_t`: the bytes it points to, counted from where it starts, without the NUL.
  fn reference(&mut self) -> Option<&'a [u8]> {
    let (from, offset, len) = (self.at, self.i32()?, self.u32()?);
    let start = from.checked_add_signed(isize::try_from(offset).ok()?)?;
    self.bytes.get(start..start.checked_add((len as usize).checked_sub(1)?)?)
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
  const FSOPT_NOFOLLOW: u32 = 0x1;
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
  let wanted = AttrList::new(ATTR_CMN_NAME | ATTR_CMN_OBJTYPE, 0, 0);
  // The length, the name as an `attrreference_t`, the object type, then the name: at most 255
  // UTF-16 units of at most three bytes here, and a NUL.
  let mut answer = [0_u8; 4 * size_of::<u32>() + 255 * 3 + 1];
  // SAFETY: `c_path` ends with a NUL, `wanted` is a whole `attrlist` that the call only reads,
  // and the call writes at most `answer.len()` bytes into `answer`.
  let result = unsafe {
    getattrlist(
      c_path.as_ptr(),
      (&raw const wanted).cast_mut().cast(),
      answer.as_mut_ptr().cast(),
      answer.len(),
      FSOPT_NOFOLLOW,
    )
  };
  if result != 0 {
    return Err(io::Error::last_os_error());
  }
  let mut fields = Fields { bytes: &answer, at: size_of::<u32>() };
  let (name, kind) = fields
    .reference()
    .zip(fields.u32())
    .ok_or_else(|| io::Error::other("getattrlist gave no name"))?;
  Ok((OsStr::from_bytes(name).to_owned(), file_type(kind)))
}

pub(super) fn name_on_disk(path: &Path) -> io::Result<OsString> {
  name_and_kind(path).map(|(name, _)| name)
}
