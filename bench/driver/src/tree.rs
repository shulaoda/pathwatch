//! Making the trees of files, and changing them.

use std::{
  fs, io,
  path::{Path, PathBuf},
  sync::atomic::{AtomicUsize, Ordering},
};

/// A tree of `groups` directories, each with `dirs` directories, each with `files` files:
/// `<root>/group-003/directory-017/module-04.js`. Every name is longer than 8.3, so Windows
/// gives each entry a short name too: a library that asks for short names pays for them.
#[derive(Clone, Copy)]
pub struct Shape {
  pub groups: usize,
  pub dirs: usize,
  pub files: usize,
}

impl Shape {
  /// How many files.
  #[must_use]
  pub fn count(&self) -> usize {
    self.groups * self.dirs * self.files
  }

  /// How many directories hold files.
  #[must_use]
  pub fn dir_count(&self) -> usize {
    self.groups * self.dirs
  }

  #[must_use]
  pub fn group(root: &Path, group: usize) -> PathBuf {
    root.join(format!("group-{group:03}"))
  }

  /// The paths of all files, in a fixed order. Nothing is made.
  #[must_use]
  pub fn paths(&self, root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(self.count());
    for group in 0..self.groups {
      for dir in 0..self.dirs {
        let dir = Self::group(root, group).join(format!("directory-{dir:03}"));
        paths.extend((0..self.files).map(|file| dir.join(format!("module-{file:02}.js"))));
      }
    }
    paths
  }

  /// Makes the directories and the files, and returns the paths of the files.
  pub fn make(&self, root: &Path) -> io::Result<Vec<PathBuf>> {
    let paths = self.paths(root);
    write_all(&paths)?;
    Ok(paths)
  }
}

/// Writes every file, and makes the directories that are missing.
pub fn write_all(paths: &[PathBuf]) -> io::Result<()> {
  let mut last_dir = None;
  for path in paths {
    let dir = path.parent();
    if dir != last_dir {
      fs::create_dir_all(dir.expect("a file has a directory"))?;
      last_dir = dir;
    }
    write(path)?;
  }
  Ok(())
}

/// Every write of the driver has another content with another length, so a watcher that
/// compares sizes or times always sees a change.
static WRITES: AtomicUsize = AtomicUsize::new(0);

/// Writes the file in place (open, cut to zero, write, close), the way most tools save.
pub fn write(path: &Path) -> io::Result<()> {
  let count = WRITES.fetch_add(1, Ordering::Relaxed);
  let tail = "x".repeat(count % 61);
  fs::write(path, format!("export const value = {count};\n// {tail}\n"))
}

/// `count` of the paths, picked at equal steps over the whole list.
#[must_use]
pub fn spread(paths: &[PathBuf], count: usize) -> Vec<PathBuf> {
  let count = count.min(paths.len());
  (0..count).map(|index| paths[index * paths.len() / count].clone()).collect()
}

/// Every file at or below `dir`.
#[must_use]
pub fn files_below(dir: &Path) -> Vec<PathBuf> {
  let mut files = Vec::new();
  let mut dirs = vec![dir.to_path_buf()];
  while let Some(dir) = dirs.pop() {
    let Ok(entries) = fs::read_dir(&dir) else { continue };
    for entry in entries.flatten() {
      match entry.file_type() {
        Ok(kind) if kind.is_dir() => dirs.push(entry.path()),
        Ok(_) => files.push(entry.path()),
        Err(_) => {}
      }
    }
  }
  files
}
