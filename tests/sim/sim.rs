//! The fixtures themselves: what the simulated disk and backend do that the other tests rely on,
//! and that the checks of the driver fail when they have to.

use std::path::{Path, PathBuf};

use crate::fixtures::{
  Driver, Mode, NONE, SimBackend, SimFs, asked, at, caught_up, not_caught_up, on_modes, os_watches,
  settled, show,
};
use pathwatch::{
  backend::{Backend, Coverage, DirKey, Hint, Want},
  engine::Options,
  fs::Fs,
};

// ---- a disk that ignores the case of letters ----

/// Every spelling finds the entry. A listing and a real path spell it as the disk does.
#[test]
fn a_lookup_ignores_the_case_and_a_listing_does_not() {
  let fs = SimFs::new();
  fs.set_ignore_case(true);
  fs.mkdir_all("/Proj/Src").unwrap();
  fs.write("/Proj/Src/App.js", "1").unwrap();
  assert_eq!(fs.read("/proj/src/app.js"), Some(b"1".to_vec()));
  let file_id = |path: &str| fs.lstat(Path::new(path)).unwrap().file_id;
  assert_eq!(file_id("/PROJ/SRC/APP.JS"), file_id("/Proj/Src/App.js"));
  let mut names = Vec::new();
  fs.read_dir(Path::new("/proj/SRC"), &mut |name, _| names.push(name.to_str().unwrap().to_owned()))
    .unwrap();
  assert_eq!(names, ["App.js"]);
  assert_eq!(fs.real_path(Path::new("/proj/src/APP.js")).unwrap(), Path::new("/Proj/Src/App.js"));
  assert_eq!(fs.files(), [Path::new("/Proj/Src/App.js")]);

  // Only the case of ASCII letters is ignored, and only that.
  assert_eq!(fs.read("/proj/src/app_js"), None);
  assert_eq!(fs.read("/proj/src/äpp.js"), None);
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
fn a_write_that_nobody_reports() -> Driver<SimBackend> {
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

#[test]
#[should_panic(expected = "the story has no ending")]
fn a_story_without_an_ending_fails() {
  let mut d = Driver::poll(Options::default());
  d.fs.mkdir("/p").unwrap();
  d.watch(&["/p"]);
}
