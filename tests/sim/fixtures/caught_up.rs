//! What must hold when the engine has caught up with the disk: every notification was delivered,
//! no delete is pending, and one whole poll has started after the last change of the disk. The
//! driver and the fuzzer check it each time they let everything settle.
//!
//! The rules are written down here a second time, and read from the disk and the watched paths
//! alone: nothing is taken from the engine.

use std::{
  collections::BTreeSet,
  path::{Path, PathBuf},
};

use pathwatch::{
  EntryKind,
  fs::{FileType, Fs, Meta},
  tree::Fingerprint,
};

use super::SimFs;

/// Compares each stored fingerprint (`Engine::fingerprints`) with a fresh look at the disk, and
/// each link that the engine keeps as a file of its directory (`Engine::link_entries`) with
/// the link itself. The error names the first file whose fingerprint is not the one on disk. A
/// file that was looked at before its watch was live, and never again, fails here and in no
/// other check.
pub fn check_fingerprints(
  fs: &SimFs,
  watched: &BTreeSet<PathBuf>,
  stored: &[(PathBuf, Fingerprint)],
  entries: &[(PathBuf, Fingerprint)],
) -> Result<(), String> {
  let looks = stored.iter().map(|(path, stored)| (path, stored, watched.contains(path)));
  for (path, stored, at_watched_path) in
    looks.chain(entries.iter().map(|(path, stored)| (path, stored, false)))
  {
    let fail = |disk: &str| {
      Err(format!("{}: the engine stored {stored:?}, and the disk has {disk}", path.display()))
    };
    match fs.quiet(|| look(fs, at_watched_path, path)) {
      Ok(meta) if meta.kind == FileType::Dir => return fail("a directory"),
      Ok(meta) if Fingerprint::of(&meta) != *stored => {
        return fail(&format!("{:?}", Fingerprint::of(&meta)));
      }
      Ok(_) => {}
      Err(error) => return fail(&format!("no file ({error})")),
    }
  }
  Ok(())
}

/// The files a handler must know when the engine has caught up, found by a walk of the disk
/// from each watched path that `watch` takes ([`can_be_watched`]).
///
/// A watched path stands for itself if a file is there, and for every file below it if a
/// directory is there. It is read through its links, and nothing below it is: a link there is
/// a file. Each watched path is walked on its own, so a link inside a watched directory that a
/// watched path goes through is a file of the directory, and what is behind it belongs to the
/// path only. The filter is asked about every file and every directory of the walk, the watched
/// path too: one that it ignores for the kind that is there counts as missing.
pub fn reportable(
  fs: &SimFs,
  ignored: &dyn Fn(&Path, EntryKind) -> bool,
  watched: &BTreeSet<PathBuf>,
) -> BTreeSet<PathBuf> {
  let mut walk = Walk { fs, ignored, files: BTreeSet::new() };
  fs.quiet(|| {
    for path in watched.iter().filter(|path| can_be_watched(ignored, path)) {
      walk.visit(path, true);
    }
  });
  walk.files
}

/// Whether `watch` takes `path`: no directory above it is ignored as a directory, and the path
/// itself is not ignored both as a file and as a directory. Such a path can never be reported,
/// so it does not become a watched path at all.
pub fn can_be_watched(ignored: &dyn Fn(&Path, EntryKind) -> bool, path: &Path) -> bool {
  let above_is_ignored = path.ancestors().skip(1).any(|dir| ignored(dir, EntryKind::Dir));
  let both_kinds = ignored(path, EntryKind::File) && ignored(path, EntryKind::Dir);
  !above_is_ignored && !both_kinds
}

struct Walk<'a> {
  fs: &'a SimFs,
  ignored: &'a dyn Fn(&Path, EntryKind) -> bool,
  files: BTreeSet<PathBuf>,
}

impl Walk<'_> {
  fn visit(&mut self, path: &Path, at_watched_path: bool) {
    let Ok(meta) = look(self.fs, at_watched_path, path) else { return };
    let kind = if meta.kind == FileType::Dir { EntryKind::Dir } else { EntryKind::File };
    if (self.ignored)(path, kind) {
      return;
    }
    if kind == EntryKind::File {
      self.files.insert(path.to_path_buf());
      return;
    }
    let mut names = Vec::new();
    if self.fs.read_dir(path, &mut |name, _| names.push(name.to_owned())).is_ok() {
      for name in names {
        self.visit(&path.join(name), false);
      }
    }
  }
}

/// Every watched path and every path above one: the paths that a watched path is read through.
pub fn followed(watched: &BTreeSet<PathBuf>) -> BTreeSet<PathBuf> {
  watched.iter().flat_map(|path| path.ancestors()).map(Path::to_path_buf).collect()
}

/// What the engine finds at `path` when it looks now: a watched path is read through its link,
/// a path below a watched directory is not.
fn look(fs: &SimFs, at_watched_path: bool, path: &Path) -> std::io::Result<Meta> {
  if at_watched_path { fs.stat(path) } else { fs.lstat(path) }
}
