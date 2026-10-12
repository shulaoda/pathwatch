//! The hash of a file on the real disk: it reads up to the limit and no further, and it opens a
//! regular file and nothing else.

use std::{
  fs::{self, File},
  time::{Duration, Instant},
};

use pathwatch::fs::{Fs, StdFs};

#[test]
fn a_file_longer_than_the_limit_has_no_hash() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("f.txt");
  fs::write(&path, [7; 1000]).unwrap();
  assert!(StdFs.hash(&path, 1000).unwrap().is_some());
  assert_eq!(StdFs.hash(&path, 999).unwrap(), None);
  // The hash does not depend on the limit.
  assert_eq!(StdFs.hash(&path, 1000).unwrap(), StdFs.hash(&path, u64::MAX).unwrap());

  // The limit ends the read, not the end of the file: a file that grew since the look is not
  // read whole.
  let big = dir.path().join("big.bin");
  File::create(&big).unwrap().set_len(96 * 1024 * 1024).unwrap();
  let started = Instant::now();
  assert_eq!(StdFs.hash(&big, 16 * 1024 * 1024).unwrap(), None);
  assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_directory_is_no_file_to_hash() {
  let dir = tempfile::tempdir().unwrap();
  assert!(!matches!(StdFs.hash(dir.path(), u64::MAX), Ok(Some(_))));
}

/// A link that takes the place of a file after the look is not followed.
#[test]
fn a_link_is_not_followed() {
  let dir = tempfile::tempdir().unwrap();
  let target = dir.path().join("target.txt");
  fs::write(&target, "1").unwrap();
  assert!(StdFs.hash(&target, u64::MAX).unwrap().is_some());
  let link = dir.path().join("link.txt");
  #[cfg(unix)]
  std::os::unix::fs::symlink(&target, &link).unwrap();
  #[cfg(windows)]
  {
    // A junction stands for another name too.
    let junction = dir.path().join("junction");
    let made = std::process::Command::new("cmd")
      .args(["/C", "mklink", "/J"])
      .arg(&junction)
      .arg(dir.path())
      .output();
    assert!(made.is_ok_and(|output| output.status.success()), "mklink /J failed");
    assert!(!matches!(StdFs.hash(&junction, u64::MAX), Ok(Some(_))), "the junction was followed");
    fs::remove_dir(&junction).unwrap();
    // Making a symbolic link needs a privilege that this user may not have.
    if std::os::windows::fs::symlink_file(&target, &link).is_err() {
      return;
    }
  }
  assert!(!matches!(StdFs.hash(&link, u64::MAX), Ok(Some(_))), "the link was followed");
}

/// A FIFO that takes the place of a file after the look: the open does not wait for a writer.
#[cfg(unix)]
#[test]
fn a_fifo_does_not_block() {
  let dir = tempfile::tempdir().unwrap();
  let fifo = dir.path().join("fifo");
  let made = std::process::Command::new("mkfifo").arg(&fifo).status();
  assert!(made.is_ok_and(|status| status.success()), "mkfifo failed");
  let started = Instant::now();
  assert_eq!(StdFs.hash(&fifo, u64::MAX).unwrap(), None);
  assert!(started.elapsed() < Duration::from_secs(5));
}
