//! macOS: a listing with what `lstat` tells of each entry, in one call, the name of one entry
//! as the disk spells it, and the kind of a disk. A file or a directory is taken from the
//! listing on APFS only (`references/design.md` 5.4).

use std::{
  ffi::{CString, OsStr, OsString, c_char},
  fs::OpenOptions,
  io, mem,
  os::{
    fd::AsRawFd,
    unix::{ffi::OsStrExt, fs::OpenOptionsExt},
  },
  path::Path,
  time::{Duration, SystemTime},
};

use libc::{
  ATTR_BIT_MAP_COUNT, ATTR_CMN_CRTIME, ATTR_CMN_DEVID, ATTR_CMN_FILEID, ATTR_CMN_FLAGS,
  ATTR_CMN_MODTIME, ATTR_CMN_NAME, ATTR_CMN_OBJTYPE, ATTR_CMN_RETURNED_ATTRS, ATTR_DIR_MOUNTSTATUS,
  ATTR_FILE_DATALENGTH, FSOPT_NOFOLLOW, MNT_LOCAL, O_DIRECTORY,
};

use super::{DirId, Entry, FileType, unsigned_nanos};

/// What a look at a regular file or a directory reads, in the common attributes: the birth time
/// is in the identity of a directory.
const LOOK: u32 =
  ATTR_CMN_DEVID | ATTR_CMN_CRTIME | ATTR_CMN_MODTIME | ATTR_CMN_FLAGS | ATTR_CMN_FILEID;
// Not in `libc`.
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
/// The content of the file is in a cloud, not on this disk.
const SF_DATALESS: u32 = 0x4000_0000;
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;

/// The attributes to ask for, in the common group and the groups of a directory and a file.
fn attr_list(common: u32, dir: u32, file: u32) -> libc::attrlist {
  libc::attrlist {
    bitmapcount: ATTR_BIT_MAP_COUNT,
    reserved: 0,
    commonattr: common,
    volattr: 0,
    dirattr: dir,
    fileattr: file,
    forkattr: 0,
  }
}

fn stat_fs(dir: &Path) -> Option<libc::statfs> {
  let c_path = CString::new(dir.as_os_str().as_bytes()).ok()?;
  // SAFETY: all zeros is a valid value of this plain struct of numbers.
  let mut answer: libc::statfs = unsafe { mem::zeroed() };
  // SAFETY: `c_path` ends with a NUL, and the call writes one whole `struct statfs`.
  let result = unsafe { libc::statfs(c_path.as_ptr(), &raw mut answer) };
  (result == 0).then_some(answer)
}

/// The device number of the disk under `dir`, if it is an APFS disk.
fn apfs_volume(dir: &Path) -> Option<i32> {
  let answer = stat_fs(dir)?;
  if !answer.f_fstypename.map(c_char::cast_unsigned).starts_with(b"apfs\0") {
    return None;
  }
  // SAFETY: a `fsid_t` is two numbers, the device first; `libc` keeps them private.
  let [device, _] = unsafe { mem::transmute::<libc::fsid_t, [i32; 2]>(answer.f_fsid) };
  Some(device)
}

/// The name of the file system under `dir` (`nfs`, `smbfs`), if it is not on this machine.
pub fn remote_disk(dir: &Path) -> Option<String> {
  let answer = stat_fs(dir).filter(|answer| answer.f_flags & MNT_LOCAL.cast_unsigned() == 0)?;
  let name = answer.f_fstypename.map(c_char::cast_unsigned);
  let name = name.split(|&byte| byte == 0).next().unwrap_or_default();
  Some(String::from_utf8_lossy(name).into_owned())
}

/// Lists `dir` with what `lstat` tells of each entry. `Ok(false)`: std lists it instead, as the
/// disk is not APFS. An entry on another device is given as a name only: the directory can be
/// another one by now.
pub fn read_dir(dir: &Path, each: &mut dyn FnMut(&OsStr, Entry)) -> io::Result<bool> {
  let Some(volume) = apfs_volume(dir) else { return Ok(false) };
  // Not a pipe, which would wait for its writer.
  let dir = OpenOptions::new().read(true).custom_flags(O_DIRECTORY).open(dir)?;
  let common = ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_ERROR | ATTR_CMN_NAME | ATTR_CMN_OBJTYPE | LOOK;
  let wanted = attr_list(common, ATTR_DIR_MOUNTSTATUS, ATTR_FILE_DATALENGTH);
  // An entry takes less than 1 KiB.
  let mut buffer = [0_u8; 16 * 1024];
  loop {
    // SAFETY: `dir` is open, `wanted` is a whole `attrlist` that the call only reads, and the
    // call writes at most `buffer.len()` bytes into `buffer`.
    let count = unsafe {
      libc::getattrlistbulk(
        dir.as_raw_fd(),
        (&raw const wanted).cast_mut().cast(),
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
      return Ok(true);
    }
    let mut rest = &buffer[..];
    for _ in 0..count {
      let entry = Fields { bytes: rest, at: 0 }.u32().and_then(|len| rest.get(..len as usize));
      let entry = entry.ok_or_else(|| io::Error::other("getattrlistbulk gave no entry"))?;
      rest = &rest[entry.len()..];
      if let Some((name, told)) = record(entry, volume) {
        each(name, told);
      }
    }
  }
}

/// One entry of the answer: its length, the attributes that follow as bits, and then these
/// attributes in the order of their bits. `None`: it has no name.
fn record(entry: &[u8], volume: i32) -> Option<(&OsStr, Entry)> {
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
      Entry::Dir { id: DirId { volume, inode: file_id, birth: unsigned_nanos(created) }, modified }
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
  let c_path = CString::new(path.as_os_str().as_bytes())?;
  let wanted = attr_list(ATTR_CMN_NAME | ATTR_CMN_OBJTYPE, 0, 0);
  // The length, the name as an `attrreference_t`, the object type, then the name: at most 255
  // UTF-16 units of at most three bytes here, and a NUL.
  let mut answer = [0_u8; 4 * size_of::<u32>() + 255 * 3 + 1];
  // SAFETY: `c_path` ends with a NUL, `wanted` is a whole `attrlist` that the call only reads,
  // and the call writes at most `answer.len()` bytes into `answer`.
  let result = unsafe {
    libc::getattrlist(
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

#[cfg(test)]
mod tests {
  use std::{
    fs,
    io::{self, Write},
    os::unix::fs::symlink,
    process::Command,
  };

  use crate::fs::{Entry, FileType, Fs, StdFs};

  /// On APFS the listing tells of a file and of a directory what `lstat` tells, also while the
  /// file is open and written.
  #[test]
  fn the_listing_tells_what_lstat_tells() {
    let top = tempfile::tempdir().unwrap();
    fs::create_dir(top.path().join("dir")).unwrap();
    symlink("file", top.path().join("link")).unwrap();
    let mut file = fs::File::create(top.path().join("file")).unwrap();
    file.write_all(b"written").unwrap();

    let mut seen = Vec::new();
    StdFs.read_dir(top.path(), &mut |name, entry| seen.push((name.to_owned(), entry))).unwrap();
    seen.sort_by(|a, b| a.0.cmp(&b.0));
    let [
      (_, Entry::Dir { id, modified: dir_modified }),
      (_, Entry::File { modified, size, file_id }),
      (_, Entry::Kind(FileType::Symlink)),
    ] = seen[..]
    else {
      panic!("{seen:?}");
    };
    let meta = StdFs.lstat(&top.path().join("file")).unwrap();
    assert_eq!((modified, size, file_id), (meta.modified, meta.size, meta.file_id));
    let dir = StdFs.lstat(&top.path().join("dir")).unwrap();
    assert_eq!((id, dir_modified), (dir.dir_id, dir.modified));

    // A pipe is no directory: the listing fails and does not wait for a writer.
    let pipe = top.path().join("pipe");
    assert!(Command::new("mkfifo").arg(&pipe).status().unwrap().success());
    let error = StdFs.read_dir(&pipe, &mut |_, _| {}).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotADirectory);
  }
}
