//! One test for each row of the table of `check`: what the tree has at a name, and what a look
//! finds there. A test is named for its row. `flush` runs one look, and `pending` lists the
//! deletes that are held after it.

use std::{path::Path, time::Duration};

use crate::fixtures::{Driver, Mode, NONE, SimBackend, caught_up, settled, taken};
use pathwatch::engine::Options;

/// A driver whose disk has the directory `/p/r`.
fn driver(mode: Mode) -> Driver<SimBackend> {
  let d = Driver::sim(mode, Options::default());
  d.fs.mkdir_all("/p/r").unwrap();
  d
}

/// Delivers what waits and runs the flush that follows.
fn flush(d: &mut Driver<SimBackend>) {
  assert!(d.deliver(usize::MAX) > 0, "nothing was notified");
  assert!(d.step());
}

fn pending(d: &Driver<SimBackend>) -> Vec<&str> {
  let pending = d.engine.pending_deletes();
  ["/p/r/a", "/p/r/sub/a", "/p/r/sub/deep/b", "/p/r/sub/old"]
    .into_iter()
    .filter(|path| pending.contains(Path::new(path)))
    .collect()
}

#[test]
fn missing_and_file() {
  let mut d = driver(Mode::Inode);
  d.fs.write("/p/r/a", "1").unwrap();
  d.watch(&["/p/r"]);

  d.fs.remove("/p/r/a").unwrap();
  flush(&mut d);
  // The delete is pending: the handler still believes in the file.
  assert_eq!(taken(&mut d), NONE);
  assert_eq!(pending(&d), ["/p/r/a"]);
  assert!(d.known.contains(Path::new("/p/r/a")));

  assert!(d.step());
  assert_eq!(taken(&mut d), ["D /p/r/a"]);
  assert_eq!(pending(&d), NONE);
  caught_up(&mut d);
}

#[test]
fn missing_and_directory() {
  let mut d = driver(Mode::Inode);
  d.fs.mkdir_all("/p/r/sub/deep").unwrap();
  d.fs.write("/p/r/sub/a", "1").unwrap();
  d.fs.write("/p/r/sub/deep/b", "1").unwrap();
  d.watch(&["/p/r"]);

  d.fs.remove_all("/p/r/sub").unwrap();
  flush(&mut d);
  assert_eq!(taken(&mut d), NONE);
  assert_eq!(pending(&d), ["/p/r/sub/a", "/p/r/sub/deep/b"]);

  assert!(d.step());
  assert_eq!(taken(&mut d), ["D /p/r/sub/a", "D /p/r/sub/deep/b"]);
  caught_up(&mut d);
}

#[test]
fn missing_and_nothing() {
  let mut d = driver(Mode::Inode);
  d.fs.write("/p/r/a", "1").unwrap();
  d.watch(&["/p/r"]);
  d.fs.remove("/p/r/a").unwrap();
  flush(&mut d);

  // The path is checked once more before its deadline: still nothing to say.
  d.fs.write("/p/r/a", "1").unwrap();
  d.fs.remove("/p/r/a").unwrap();
  flush(&mut d);
  assert_eq!(taken(&mut d), NONE);
  assert_eq!(pending(&d), ["/p/r/a"]);

  // At the deadline it is still missing: the delete goes out.
  assert!(d.step());
  assert_eq!(taken(&mut d), ["D /p/r/a"]);
  // A name that was never known and is gone again is nothing at all.
  d.fs.write("/p/r/never", "1").unwrap();
  d.fs.remove("/p/r/never").unwrap();
  assert_eq!(settled(&mut d), NONE);
  caught_up(&mut d);
}

#[test]
fn file_and_nothing() {
  let mut d = driver(Mode::Inode);
  d.fs.write("/p/r/a", "1").unwrap();
  // What is there at `watch` is the baseline: known, not reported.
  d.watch(&["/p/r"]);
  assert_eq!(taken(&mut d), NONE);
  assert!(d.known.contains(Path::new("/p/r/a")));

  // A new file is a create.
  d.fs.write("/p/r/new", "1").unwrap();
  flush(&mut d);
  assert_eq!(taken(&mut d), ["C /p/r/new"]);

  // A file whose delete is pending is an update.
  d.fs.remove("/p/r/a").unwrap();
  flush(&mut d);
  d.fs.write("/p/r/a", "1").unwrap();
  flush(&mut d);
  assert_eq!(taken(&mut d), ["U /p/r/a"]);
  assert_eq!(pending(&d), NONE);
  assert_eq!(settled(&mut d), NONE);
  caught_up(&mut d);
}

#[test]
fn file_and_file() {
  let mut d = driver(Mode::Inode);
  // With this step, and far from the start of an hour, every write leaves the same time.
  d.fs.set_time_step(Duration::from_secs(3600));
  d.fs.write("/p/r/a", "1").unwrap();
  d.watch(&["/p/r"]);

  // Another fingerprint (the size). The notification comes twice: one update.
  d.fs.write("/p/r/a", "12").unwrap();
  let raws = d.fs.take_raw(usize::MAX);
  for raw in raws.iter().chain(&raws).cloned() {
    d.signal(raw);
  }
  assert!(d.step());
  assert_eq!(taken(&mut d), ["U /p/r/a"]);

  // The same fingerprint, and the backend says "written".
  d.fs.write("/p/r/a", "34").unwrap();
  flush(&mut d);
  assert_eq!(taken(&mut d), ["U /p/r/a"]);

  // The same fingerprint and no word of a write: nothing. The file is replaced by one that
  // looks the same, and of the two notifications only "an entry went" arrives.
  let before = d.fs.calls();
  d.fs.remove("/p/r/a").unwrap();
  let went = d.fs.take_raw(usize::MAX);
  d.fs.write("/p/r/a", "56").unwrap();
  let came = d.fs.take_raw(usize::MAX);
  for raw in went {
    d.signal(raw);
  }
  assert!(d.step());
  assert_eq!(taken(&mut d), NONE);
  assert_eq!(d.fs.calls().lstat, before.lstat + 1);

  // The other one says it: another file is at the name now.
  for raw in came {
    d.signal(raw);
  }
  assert!(d.step());
  assert_eq!(taken(&mut d), ["U /p/r/a"]);
  caught_up(&mut d);
}

#[test]
fn file_and_directory() {
  let mut d = driver(Mode::Inode);
  d.fs.mkdir("/p/r/sub").unwrap();
  d.fs.write("/p/r/sub/a", "1").unwrap();
  d.watch(&["/p/r"]);

  d.fs.remove_all("/p/r/sub").unwrap();
  d.fs.write("/p/r/sub", "now a file").unwrap();
  flush(&mut d);
  // A change of kind: the delete does not wait.
  assert_eq!(taken(&mut d), ["C /p/r/sub", "D /p/r/sub/a"]);
  assert_eq!(pending(&d), NONE);
  caught_up(&mut d);
}

#[test]
fn directory_and_file() {
  let mut d = driver(Mode::Inode);
  d.fs.write("/p/r/a", "1").unwrap();
  d.watch(&["/p/r"]);

  d.fs.remove("/p/r/a").unwrap();
  d.fs.mkdir("/p/r/a").unwrap();
  d.fs.write("/p/r/a/x", "1").unwrap();
  flush(&mut d);
  assert_eq!(taken(&mut d), ["D /p/r/a", "C /p/r/a/x"]);
  assert_eq!(pending(&d), NONE);
  caught_up(&mut d);
}

#[test]
fn directory_and_nothing() {
  let mut d = driver(Mode::Inode);
  d.watch(&["/p/r"]);

  // A new directory: one create for each file in it.
  d.fs.mkdir("/p/r/new").unwrap();
  d.fs.write("/p/r/new/x", "1").unwrap();
  flush(&mut d);
  assert_eq!(taken(&mut d), ["C /p/r/new/x"]);
  caught_up(&mut d);
}

#[test]
fn directory_and_another_directory() {
  let mut d = driver(Mode::Inode);
  d.fs.set_reuse_inode_numbers(false);
  d.fs.mkdir("/p/r/sub").unwrap();
  d.fs.write("/p/r/sub/a", "1").unwrap();
  d.fs.write("/p/r/sub/old", "1").unwrap();
  d.watch(&["/p/r"]);

  d.fs.remove_all("/p/r/sub").unwrap();
  d.fs.mkdir("/p/r/sub").unwrap();
  d.fs.write("/p/r/sub/a", "1").unwrap();
  d.fs.write("/p/r/sub/new", "1").unwrap();
  flush(&mut d);
  // A file that is in both is an update, a new one a create, and the one that is gone waits.
  assert_eq!(taken(&mut d), ["U /p/r/sub/a", "C /p/r/sub/new"]);
  assert_eq!(pending(&d), ["/p/r/sub/old"]);
  assert_eq!(settled(&mut d), ["D /p/r/sub/old"]);

  // The new directory is watched.
  d.fs.write("/p/r/sub/more", "1").unwrap();
  assert_eq!(settled(&mut d), ["C /p/r/sub/more"]);
  caught_up(&mut d);
}

/// A listing shows a directory that is still the same one: it is not scanned.
#[test]
fn same_directory_found_by_a_listing() {
  let mut d = driver(Mode::Handle);
  d.fs.mkdir_all("/p/r/sub/deep").unwrap();
  d.watch(&["/p/r"]);
  assert_eq!(settled(&mut d), NONE);

  // Every poll scans the root, `/` and `/p`, which no handle covers. The root checks `/`, `/`
  // checks `p`, and `/p` checks `r`: each finds the same directory and stops. `/p/r` is the
  // watched directory, so its handle covers everything below.
  let before = d.fs.calls();
  d.advance(Duration::from_secs(1));
  d.tick();
  let after = d.fs.calls();
  assert_eq!(after.read_dir, before.read_dir);
  assert_eq!(after.lstat, before.lstat + 3);
  caught_up(&mut d);
}

/// "This directory was modified" is no reason to scan it.
#[test]
fn same_directory_with_a_written_mark() {
  let mut d = driver(Mode::Handle);
  d.fs.mkdir("/p/r/sub").unwrap();
  d.watch(&["/p/r"]);

  let before = d.fs.calls();
  d.fs.write("/p/r/sub/new", "1").unwrap();
  // The new file, and the noise for the directory it is in.
  assert_eq!(d.fs.pending_raw(), 2);
  flush(&mut d);
  assert_eq!(taken(&mut d), ["C /p/r/sub/new"]);
  let after = d.fs.calls();
  assert_eq!(after.read_dir, before.read_dir);
  // The two marks, and `/p`: its parent `/` is only polled, so it is checked with them.
  assert_eq!(after.lstat, before.lstat + 3);
  caught_up(&mut d);
}
