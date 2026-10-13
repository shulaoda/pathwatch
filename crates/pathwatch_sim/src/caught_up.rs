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
  engine::Fingerprint,
  fs::{FileType, Fs, Meta},
};

use crate::SimFs;

/// Compares each stored fingerprint (`Engine::fingerprints`) with a fresh look at the disk.
/// The error names the first file whose fingerprint is not the one on disk. A file that was
/// looked at before its watch was live, and never again, fails here and in no other check.
pub fn check_fingerprints(
  fs: &SimFs,
  watched: &BTreeSet<PathBuf>,
  stored: &[(PathBuf, Fingerprint)],
) -> Result<(), String> {
  let followed = followed(watched);
  for (path, stored) in stored {
    let fail = |disk: &str| {
      Err(format!("{}: the engine stored {stored:?}, and the disk has {disk}", path.display()))
    };
    match fs.quiet(|| look(fs, &followed, path)) {
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
/// from each watched path.
///
/// A watched path stands for itself if a file is there, and for every file below it if a
/// directory is there. The filter is asked about every file and every directory of the walk
/// below a watched directory, and not about a watched path or a name on the way to one.
pub fn reportable(
  fs: &SimFs,
  ignored: &dyn Fn(&Path, EntryKind) -> bool,
  watched: &BTreeSet<PathBuf>,
) -> BTreeSet<PathBuf> {
  let followed = followed(watched);
  let mut walk = Walk { fs, ignored, watched, followed, files: BTreeSet::new() };
  fs.quiet(|| {
    for path in watched {
      walk.visit(path, false);
    }
  });
  walk.files
}

struct Walk<'a> {
  fs: &'a SimFs,
  ignored: &'a dyn Fn(&Path, EntryKind) -> bool,
  watched: &'a BTreeSet<PathBuf>,
  followed: BTreeSet<PathBuf>,
  files: BTreeSet<PathBuf>,
}

impl Walk<'_> {
  /// `in_watched_dir`: a watched directory holds `path`, and the filter ignores no directory
  /// between the two.
  fn visit(&mut self, path: &Path, in_watched_dir: bool) {
    let Ok(meta) = look(self.fs, &self.followed, path) else { return };
    let kind = if meta.kind == FileType::Dir { EntryKind::Dir } else { EntryKind::File };
    let counts = self.watched.contains(path) || (in_watched_dir && !(self.ignored)(path, kind));
    if !counts {
      return;
    }
    if kind == EntryKind::File {
      self.files.insert(path.to_path_buf());
      return;
    }
    let mut names = Vec::new();
    if self.fs.read_dir(path, &mut |name, _| names.push(name.to_owned())).is_ok() {
      for name in names {
        self.visit(&path.join(name), true);
      }
    }
  }
}

/// Every watched path and every path above one. Only these are followed when they are links.
pub fn followed(watched: &BTreeSet<PathBuf>) -> BTreeSet<PathBuf> {
  watched.iter().flat_map(|path| path.ancestors()).map(Path::to_path_buf).collect()
}

/// What the engine finds at `path` when it looks now: a followed path is read through its
/// link, every other path is not.
fn look(fs: &SimFs, followed: &BTreeSet<PathBuf>, path: &Path) -> std::io::Result<Meta> {
  if followed.contains(path) { fs.stat(path) } else { fs.lstat(path) }
}
