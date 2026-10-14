//! The fixtures themselves: what the simulated disk and backend do that the other tests rely on,
//! and that the checks of the driver fail when they have to.

use std::{
  io,
  path::{Path, PathBuf},
  time::{Duration, SystemTime},
};

use crate::fixtures::{
  Driver, Mode, NONE, SimBackend, SimFs, asked, at, caught_up, not_caught_up, on_modes, os_watches,
  settled, show,
};
use pathwatch::{
  backend::{Backend, Coverage, DirKey, Hint, Want},
  engine::Options,
  fs::{Fs, Meta},
};

fn lstat(fs: &SimFs, path: &str) -> Meta {
  fs.lstat(Path::new(path)).unwrap()
}

/// The modification time and the change time of the entry at `path`.
fn times(fs: &SimFs, path: &str) -> (SystemTime, SystemTime) {
  let meta = lstat(fs, path);
  (meta.modified, meta.changed)
}

fn names(fs: &SimFs, dir: &str) -> Vec<String> {
  let mut names = Vec::new();
  fs.read_dir(Path::new(dir), &mut |name, _| names.push(name.to_str().unwrap().to_owned()))
    .unwrap();
  names
}

// ---- the change time ----

/// An entry that comes or goes moves both times of its directory. A rename of the entry
/// itself, or other permissions for it, moves only its change time.
#[test]
fn the_change_time_of_a_directory() {
  let fs = SimFs::new();
  let tick = || fs.advance(Duration::from_secs(1));
  fs.mkdir_all("/p/d").unwrap();
  let (made, _) = times(&fs, "/p/d");
  assert_eq!(times(&fs, "/p/d"), (made, made));

  tick();
  fs.write("/p/d/f", "1").unwrap();
  let (filled, changed) = times(&fs, "/p/d");
  assert!(filled > made && changed == filled);
  let parent = times(&fs, "/p");

  tick();
  fs.rename("/p/d", "/p/e").unwrap();
  let (modified, renamed) = times(&fs, "/p/e");
  assert_eq!(modified, filled);
  assert!(renamed > filled);
  // The directory that holds the renamed entry lost one name and got another.
  let (parent_modified, parent_changed) = times(&fs, "/p");
  assert!(parent_modified > parent.0 && parent_changed == parent_modified);

  tick();
  fs.set_denied("/p/e", true).unwrap();
  let (modified, denied) = times(&fs, "/p/e");
  assert_eq!(modified, filled);
  assert!(denied > renamed);
  fs.set_denied("/p/e", false).unwrap();

  tick();
  fs.remove("/p/e/f").unwrap();
  let (emptied, changed) = times(&fs, "/p/e");
  assert!(emptied > filled && changed == emptied);
}

/// A write moves both times of the file and neither time of its directory. A rename of the
/// file moves only its change time.
#[test]
fn the_change_time_of_a_file() {
  let fs = SimFs::new();
  let tick = || fs.advance(Duration::from_secs(1));
  fs.mkdir("/p").unwrap();
  fs.write("/p/f", "1").unwrap();
  let (made, _) = times(&fs, "/p/f");
  let dir = times(&fs, "/p");

  tick();
  fs.write("/p/f", "22").unwrap();
  let (written, changed) = times(&fs, "/p/f");
  assert!(written > made && changed == written);
  assert_eq!(times(&fs, "/p"), dir);

  tick();
  fs.rename("/p/f", "/p/g").unwrap();
  let (modified, renamed) = times(&fs, "/p/g");
  assert_eq!(modified, written);
  assert!(renamed > written);
}

/// The change time is as coarse as the modification time.
#[test]
fn the_change_time_has_the_time_step_of_the_disk() {
  let fs = SimFs::new();
  fs.set_time_step(Duration::from_secs(3600));
  fs.mkdir("/p").unwrap();
  let before = times(&fs, "/p");
  fs.advance(Duration::from_secs(1));
  fs.rename("/p", "/q").unwrap();
  assert_eq!(times(&fs, "/q"), before);
}

// ---- a disk that ignores the case of letters ----

fn disk_without_case() -> SimFs {
  let fs = SimFs::new();
  fs.set_ignore_case(true);
  fs.mkdir_all("/Proj/Src").unwrap();
  fs.write("/Proj/Src/App.js", "1").unwrap();
  fs
}

/// Every spelling finds the entry. A listing and a real path spell it as the disk does.
#[test]
fn a_lookup_ignores_the_case_and_a_listing_does_not() {
  let fs = disk_without_case();
  assert_eq!(fs.read("/proj/src/app.js"), Some(b"1".to_vec()));
  assert_eq!(lstat(&fs, "/PROJ/SRC/APP.JS").file_id, lstat(&fs, "/Proj/Src/App.js").file_id);
  assert_eq!(names(&fs, "/proj/SRC"), ["App.js"]);
  assert_eq!(fs.real_path(Path::new("/proj/src/APP.js")).unwrap(), Path::new("/Proj/Src/App.js"));
  assert_eq!(fs.files(), [Path::new("/Proj/Src/App.js")]);

  // Only the case of ASCII letters is ignored, and only that.
  assert_eq!(fs.read("/proj/src/app_js"), None);
  assert_eq!(fs.read("/proj/src/äpp.js"), None);
}

/// Another spelling is the same entry: it is written in place, it cannot be made again, and
/// it is removed.
#[test]
fn another_spelling_is_the_same_entry() {
  let fs = disk_without_case();
  fs.write("/proj/src/APP.JS", "22").unwrap();
  assert_eq!(names(&fs, "/Proj/Src"), ["App.js"]);
  assert_eq!(fs.read("/Proj/Src/App.js"), Some(b"22".to_vec()));

  let exists = |result: io::Result<()>| result.unwrap_err().kind() == io::ErrorKind::AlreadyExists;
  assert!(exists(fs.mkdir("/proj/SRC")));
  assert!(exists(fs.symlink("/x", "/proj/src/app.JS")));

  fs.remove("/PROJ/src/app.js").unwrap();
  assert_eq!(names(&fs, "/Proj/Src"), NONE);
  fs.remove_all("/proj").unwrap();
  assert_eq!(names(&fs, "/"), NONE);
}

/// A rename gives the entry the spelling of the new name, also when that is all that changes.
/// A rename onto another spelling of another entry replaces that entry.
#[test]
fn a_rename_sets_the_spelling() {
  let fs = disk_without_case();
  fs.rename("/proj/src/app.js", "/proj/src/APP.js").unwrap();
  assert_eq!(names(&fs, "/Proj/Src"), ["APP.js"]);
  assert_eq!(fs.read("/Proj/Src/app.js"), Some(b"1".to_vec()));

  fs.write("/Proj/Src/new.js", "new").unwrap();
  fs.rename("/Proj/Src/new.js", "/Proj/Src/app.JS").unwrap();
  assert_eq!(names(&fs, "/Proj/Src"), ["app.JS"]);
  assert_eq!(fs.read("/Proj/Src/APP.js"), Some(b"new".to_vec()));

  fs.rename("/proj", "/PROJ").unwrap();
  assert_eq!(fs.dirs(), [Path::new("/PROJ"), Path::new("/PROJ/Src")]);
}

/// With the case not ignored, two spellings are two entries. This is the default.
#[test]
fn a_disk_that_tells_the_case_apart() {
  let fs = SimFs::new();
  fs.mkdir("/p").unwrap();
  fs.write("/p/a.js", "1").unwrap();
  fs.write("/p/A.js", "2").unwrap();
  assert_eq!(names(&fs, "/p"), ["A.js", "a.js"]);
  assert_eq!(fs.read("/p/a.js"), Some(b"1".to_vec()));
  assert_eq!(fs.read("/P/a.js"), None);
}

/// A notification names an entry that is there as the disk spells it, whatever spelling the
/// writer used, and an entry that is gone as the caller spelled it. An engine that knows about
/// such a disk finds the file both times.
#[test]
fn the_spelling_of_a_notification() {
  let loose = Options { loose_spelling: true, ..Options::default() };
  on_modes(&Mode::ALL, &loose, |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/Proj/Src").unwrap();
    d.fs().write("/Proj/Src/App.js", "1").unwrap();
    assert_eq!(d.watch(&["/Proj/Src"]), NONE, "{on}");

    d.fs().write("/proj/src/APP.JS", "22").unwrap();
    assert_eq!(d.settled(), ["U /Proj/Src/App.js"], "{on}");
    d.fs().remove("/PROJ/SRC/app.js").unwrap();
    assert_eq!(d.settled(), ["D /Proj/Src/App.js"], "{on}");
    d.caught_up();
  });
}

// ---- the log of the backend ----

/// Every `watch` and `unwatch` call is in the log, and the disk says where an OS watch is.
#[test]
fn the_watch_calls_of_the_inode_backend() {
  let mut d = Driver::sim(Mode::Inode, Options::default());
  d.fs.mkdir_all("/p/r").unwrap();
  d.fs.write("/p/r/a.js", "1").unwrap();
  d.watch(&["/p/r/a.js"]);
  // From the top down: each directory is asked for when its parent has found it.
  let calls =
    [at("/", Some(Want::Names)), at("/p", Some(Want::Names)), at("/p/r", Some(Want::Contents))];
  assert_eq!(asked(&d.fs), calls);
  let watches = [at("/", Want::Names), at("/p", Want::Names), at("/p/r", Want::Contents)];
  assert_eq!(os_watches(&d.fs), watches);
  // The key of a call is the one that `unwatch` gets later.
  let key = d.fs.watch_calls()[2].key;

  // An inode watch goes with its directory to the new name.
  d.fs.rename("/p/r", "/p/moved").unwrap();
  assert_eq!(os_watches(&d.fs)[2], at("/p/moved", Want::Contents));
  assert_eq!(settled(&mut d), ["D /p/r/a.js"]);
  // The look that did not find `r` took its watch away. Then it asked `/p` for its contents:
  // a name on the way to a watched path has nothing at it now.
  let mut log = d.fs.watch_calls();
  let (last, before) = (log.pop().unwrap(), log.pop().unwrap());
  assert_eq!((before.key, before.dir, before.want), (key, PathBuf::from("/p/r"), None));
  assert_eq!((last.dir, last.want), (PathBuf::from("/p"), Some(Want::Contents)));
  assert_eq!(os_watches(&d.fs), [at("/", Want::Names), at("/p", Want::Contents)]);
  assert_eq!(d.fs.calls().unwatch, 1);
  caught_up(&mut d);
}

/// A stream is asked for by `watch` and is there after `commit`. It stays at its path.
#[test]
fn the_watch_calls_of_the_prefix_backend() {
  let mut d = Driver::sim(Mode::Prefix, Options::default());
  d.fs.mkdir_all("/p/r").unwrap();
  d.fs.symlink("/p", "/link").unwrap();
  d.fs.write("/p/r/a.js", "1").unwrap();
  d.watch(&["/link/r/a.js"]);
  let asked_dirs: Vec<String> = asked(&d.fs).into_iter().map(|(dir, _)| dir).collect();
  assert_eq!(asked_dirs, ["/", "/link", "/link/r"]);
  // The stream is at the real path of the directory that holds the watched file.
  assert_eq!(os_watches(&d.fs), [at("/p/r", Want::Contents)]);
  caught_up(&mut d);
}

/// Two keys of the prefix backend change places in one commit: one leaves a stream, and the
/// other one joins that same stream. The stream must not stop in between: a watch is live
/// when `commit` returns.
#[test]
fn a_stream_that_one_key_leaves_and_another_joins_in_one_commit() {
  let fs = SimFs::new();
  let mut backend = SimBackend::attach(&fs, Mode::Prefix);
  fs.mkdir("/x").unwrap();
  fs.mkdir("/y").unwrap();
  let (one, other) = (DirKey::new(1, 0), DirKey::new(2, 0));
  assert!(matches!(backend.watch(one, Path::new("/x"), Want::Contents), Coverage::WatchedTree));
  backend.commit();
  assert_eq!(os_watches(&fs), [at("/x", Want::Contents)]);

  backend.watch(one, Path::new("/y"), Want::Contents);
  backend.watch(other, Path::new("/x"), Want::Contents);
  backend.commit();
  assert_eq!(os_watches(&fs), [at("/x", Want::Contents), at("/y", Want::Contents)]);

  fs.write("/x/f", "1").unwrap();
  fs.write("/y/g", "1").unwrap();
  let mut hints = Vec::new();
  for raw in fs.take_raw(usize::MAX) {
    backend.translate(raw, &mut |hint| {
      if let Hint::Changed { dir, rel } = hint {
        hints.push((dir, rel.to_path_buf()));
      }
    });
  }
  assert_eq!(hints, [(other, PathBuf::from("f")), (one, PathBuf::from("g"))]);
}

// ---- the checks of the driver ----

/// A watched file is written, and the notification is dropped without a word. The engine
/// cannot know, so what it stored is not what the disk has: the settle says so.
fn a_write_that_nobody_reports() -> Driver<crate::fixtures::SimBackend> {
  let mut d = Driver::sim(Mode::Inode, Options::default());
  d.fs.mkdir("/p").unwrap();
  d.fs.write("/p/a.js", "1").unwrap();
  d.watch(&["/p/a.js"]);
  d.fs.write("/p/a.js", "22").unwrap();
  assert_eq!(d.fs.take_raw(usize::MAX).len(), 1);
  d
}

#[test]
#[should_panic(expected = "not caught up: /p/a.js: the engine stored")]
fn a_settle_fails_on_a_fingerprint_that_is_not_the_one_on_disk() {
  let mut d = a_write_that_nobody_reports();
  d.settle();
}

/// The story of the test above can end where it is, if it says why.
#[test]
fn a_story_can_end_behind_the_disk() {
  let mut d = a_write_that_nobody_reports();
  assert_eq!(show(&d.take_events()), NONE);
  not_caught_up(&mut d, "the notification was dropped and nobody said so");
}

/// A new file in a watched directory, and its notification is dropped without a word. No
/// fingerprint is stored for it, so only the walk of the disk finds it.
#[test]
#[should_panic(expected = "not caught up: the handler does not know the files on disk")]
fn a_settle_fails_on_a_file_that_the_handler_was_not_told_about() {
  let mut d = Driver::sim(Mode::Inode, Options::default());
  d.fs.mkdir_all("/p/r").unwrap();
  d.watch(&["/p/r"]);
  d.fs.write("/p/r/new.js", "1").unwrap();
  assert_eq!(d.fs.take_raw(usize::MAX).len(), 1);
  d.settle();
}

/// With the poll backend the same two stories end well: the next poll finds the change.
#[test]
fn the_poll_catches_up_without_a_notification() {
  let mut d = Driver::poll(Options::default());
  d.fs.mkdir_all("/p/r").unwrap();
  d.fs.write("/p/a.js", "1").unwrap();
  d.watch(&["/p/r", "/p/a.js"]);
  d.fs.write("/p/a.js", "22").unwrap();
  d.fs.write("/p/r/new.js", "1").unwrap();
  assert_eq!(settled(&mut d), ["U /p/a.js", "C /p/r/new.js"]);
  caught_up(&mut d);
}

#[test]
#[should_panic(expected = "the story has no ending")]
fn a_story_without_an_ending_fails() {
  let mut d = Driver::poll(Options::default());
  d.fs.mkdir("/p").unwrap();
  d.watch(&["/p"]);
}
