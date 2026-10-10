//! What the engine asks the backend for a directory: `Contents` when it is watched or holds a
//! watched path with a file or nothing at it, `Names` when it only leads to a watched path.
//! A story compares the calls to the backend (`asked`), where an OS watch is (`os_watches`),
//! and the events (`C`, `U`, `D` and the path).

#![cfg(unix)]

use std::time::Duration;

use pathwatch::{
  backend::Want::{Contents, Names},
  engine::Options,
  poll::PollBackend,
};
use pathwatch_sim::{
  Driver, Mode, NONE, RECURSIVE, Sim, SimBackend, SimFs, asked, at, caught_up, known, on_all,
  os_watches, settled, sorted, window, window_then_settled,
};

/// How long the clock may move in a story where no poll runs: less than the poll interval.
const NO_POLL: Duration = Duration::from_secs(1);

type Make = Box<dyn Fn() -> Box<dyn Sim>>;

/// One way to make a driver for each backend, so that a story can be told many times.
fn makes() -> [(&'static str, Make); 4] {
  [
    ("Inode", Box::new(|| Box::new(Driver::sim(Mode::Inode, Options::default())))),
    ("Prefix", Box::new(|| Box::new(Driver::sim(Mode::Prefix, Options::default())))),
    ("Handle", Box::new(|| Box::new(Driver::sim(Mode::Handle, Options::default())))),
    ("Poll", Box::new(|| Box::new(Driver::<PollBackend>::poll(Options::default())))),
  ]
}

/// A watched directory: the watch is on it, and not on the directory above.
#[test]
fn a_watched_directory_asks_nothing_of_the_directory_above() {
  for mode in RECURSIVE {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir_all("/p/R/sub").unwrap();
    d.fs.mkdir("/p/other").unwrap();
    d.fs.write("/p/R/sub/a.js", "1").unwrap();
    d.watch(&["/p/R"]);
    let calls = [at("/", Some(Names)), at("/p", Some(Names)), at("/p/R", Some(Contents))];
    assert_eq!(asked(&d.fs), calls, "{mode:?}");
    assert_eq!(os_watches(&d.fs), [at("/p/R", Contents)], "{mode:?}");

    // The OS says nothing about what happens next to the watched directory.
    d.fs.write("/p/other/x", "1").unwrap();
    d.fs.write("/p/y", "1").unwrap();
    assert_eq!(d.fs.pending_raw(), 0, "{mode:?}");
    d.fs.write("/p/R/sub/a.js", "22").unwrap();
    assert!(d.fs.pending_raw() > 0, "{mode:?}");
    assert_eq!(window(&mut d), ["U /p/R/sub/a.js"], "{mode:?}");
    caught_up(&mut d);
  }

  // One watch for one directory: the directory above tells names, which is enough to know
  // when the watched directory goes or comes.
  let mut d = Driver::sim(Mode::Inode, Options::default());
  d.fs.mkdir_all("/p/R/sub").unwrap();
  d.fs.write("/p/y", "1").unwrap();
  d.watch(&["/p/R"]);
  let watches = [at("/", Names), at("/p", Names), at("/p/R", Contents), at("/p/R/sub", Contents)];
  assert_eq!(os_watches(&d.fs), watches);
  d.fs.write("/p/y", "22").unwrap();
  assert_eq!(d.fs.pending_raw(), 0);
  caught_up(&mut d);
}

/// A watched file, and a watched path with nothing at it: the directory that holds it tells
/// its contents. It is asked so with its first `watch` call, after one look for the kind of
/// the path, and the file itself is looked at once, when the watch is live.
#[test]
fn a_watched_file_makes_its_directory_tell_the_contents() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir_all("/p/src").unwrap();
    d.fs.write("/p/src/a.js", "1").unwrap();
    d.watch(&["/p/src/a.js", "/p/src/not-there.js"]);
    let calls = [at("/", Some(Names)), at("/p", Some(Names)), at("/p/src", Some(Contents))];
    assert_eq!(asked(&d.fs), calls, "{mode:?}");
    // `/`, `/p`, `/p/src`, the two watched paths, and one look for the kind of one of them.
    assert_eq!(d.fs.calls().lstat, 6, "{mode:?}");

    d.fs.write("/p/src/not-there.js", "1").unwrap();
    d.fs.write("/p/src/a.js", "22").unwrap();
    assert!(d.fs.pending_raw() >= 2, "{mode:?}");
    assert_eq!(window(&mut d), ["U /p/src/a.js", "C /p/src/not-there.js"], "{mode:?}");
    caught_up(&mut d);
  }
}

/// A file is added as a watched path to a directory that was asked for names only. The
/// directory is asked for more before the file is looked at.
#[test]
fn a_directory_is_asked_for_more_when_a_file_in_it_is_watched() {
  let mut d = Driver::sim(Mode::Inode, Options::default());
  d.fs.mkdir_all("/p/R").unwrap();
  d.fs.write("/p/package.json", "1").unwrap();
  d.watch(&["/p/R"]);
  assert_eq!(os_watches(&d.fs)[1], at("/p", Names));
  let lstats = d.fs.calls().lstat;

  d.watch(&["/p/package.json"]);
  assert_eq!(os_watches(&d.fs)[1], at("/p", Contents));
  // One look for the kind, and the scan of `/p` after the commit: `R` and the file.
  assert_eq!(d.fs.calls().lstat, lstats + 3);
  d.fs.write("/p/package.json", "22").unwrap();
  assert_eq!(d.fs.pending_raw(), 1);
  assert_eq!(window(&mut d), ["U /p/package.json"]);

  // One more watched directory in `/p` asks nothing of it.
  d.fs.mkdir("/p/S").unwrap();
  let calls = d.fs.calls();
  d.watch(&["/p/S"]);
  assert_eq!(d.fs.calls().watch, calls.watch + 1);
  caught_up(&mut d);
}

/// The watched directory goes away. From the look that does not find it, the directory above
/// tells its contents, so a file that comes at the path is heard like any watched file.
#[test]
fn the_directory_above_is_asked_for_more_when_the_watched_directory_goes() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/R").unwrap();
    d.fs().write("/p/R/x.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R"]), NONE, "{on}");
    let names_only = os_watches(d.fs()).contains(&at("/p", Names));
    assert_eq!(names_only, on == "Inode", "{on}");

    d.fs().remove_all("/p/R").unwrap();
    // The look that finds nothing at the path. The `Delete` waits.
    assert_eq!(d.window(), NONE, "{on}");
    assert_eq!(d.pending_deletes(), 1, "{on}");
    if on != "Poll" {
      assert!(os_watches(d.fs()).contains(&at("/p", Contents)), "{on}");
    }
    assert_eq!(d.settled(), ["D /p/R/x.js"], "{on}");

    // A file at the path of the watched directory.
    d.fs().write("/p/R", "1").unwrap();
    assert_eq!(d.fs().pending_raw() > 0, on != "Poll", "{on}");
    assert_eq!(d.window(), ["C /p/R"], "{on}");
    d.fs().write("/p/R", "22").unwrap();
    assert_eq!(d.fs().pending_raw() > 0, on != "Poll", "{on}");
    assert_eq!(d.window(), ["U /p/R"], "{on}");
    d.caught_up();
  });
}

/// A watched directory is removed and a file is written at its path. Then the file is written
/// once more at any moment of what the engine does next: before the look that finds the file,
/// between that look and the wider watch on the directory above, or after it. The last
/// content is known in every case: the stored fingerprint is the one on disk.
#[test]
fn a_watched_directory_becomes_a_file_and_is_written_at_any_moment() {
  fn story(d: &mut dyn Sim) {
    d.fs().mkdir_all("/p/R").unwrap();
    d.fs().write("/p/R/x.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R"]), NONE);
    d.fs().remove_all("/p/R").unwrap();
    d.fs().write("/p/R", "1").unwrap();
  }
  for (on, make) in makes() {
    let mut d = make();
    story(&mut *d);
    let before = d.fs().calls().total();
    d.settled();
    let calls = d.fs().calls().total() - before;
    d.caught_up();
    assert!(calls >= 3, "{on}");

    for moment in 0..calls {
      let mut d = make();
      story(&mut *d);
      d.fs().at_call(moment, |fs| fs.write("/p/R", "written again").unwrap());
      let events = d.settled();
      assert_eq!(d.fs().traps_run(), 1, "{on}, call {moment}");
      assert_eq!(d.known(), ["/p/R"], "{on}, call {moment}: {events:?}");
      // The end of the story compares the stored fingerprint with the disk.
      d.caught_up();
    }
  }
}

/// The look for the kind finds a directory at the watched path, so the directory above is
/// asked for names only. Before the scan comes, a file takes the place of the directory. The
/// scan finds the file, the directory above is asked for more, and a later write is heard.
#[test]
fn a_file_takes_the_place_of_the_directory_during_watch() {
  fn prepare(d: &mut dyn Sim) {
    d.fs().mkdir_all("/p/R").unwrap();
  }
  for (on, make) in makes() {
    let mut d = make();
    prepare(&mut *d);
    let before = d.fs().calls().total();
    d.watch(&["/p/R"]);
    let calls = d.fs().calls().total() - before;
    d.caught_up();

    for moment in 0..calls {
      let mut d = make();
      prepare(&mut *d);
      d.fs().at_call(moment, |fs| {
        fs.remove("/p/R").unwrap();
        fs.write("/p/R", "1").unwrap();
      });
      d.watch(&["/p/R"]);
      assert_eq!(d.fs().traps_run(), 1, "{on}, call {moment}");
      d.settled();
      assert_eq!(d.known(), ["/p/R"], "{on}, call {moment}");

      d.fs().write("/p/R", "22").unwrap();
      assert_eq!(d.fs().pending_raw() > 0, on != "Poll", "{on}, call {moment}");
      assert_eq!(d.window(), ["U /p/R"], "{on}, call {moment}");
      d.caught_up();
    }
  }
}

/// A directory comes at a watched path where nothing was. The directory above keeps the watch
/// it has: for one directory the want only goes up.
#[test]
fn a_directory_comes_at_a_watched_path() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir("/p").unwrap();
    d.watch(&["/p/R"]);
    assert!(os_watches(&d.fs).contains(&at("/p", Contents)), "{mode:?}");
    let watches = d.fs.calls().watch;

    d.fs.mkdir("/p/R").unwrap();
    d.fs.write("/p/R/a.js", "1").unwrap();
    assert!(d.fs.pending_raw() > 0, "{mode:?}");
    assert_eq!(settled(&mut d), ["C /p/R/a.js"], "{mode:?}");
    assert!(os_watches(&d.fs).contains(&at("/p", Contents)), "{mode:?}");
    // Where one watch covers a tree, the watch on `/p` covers the new directory too.
    let own_watch = u64::from(mode == Mode::Inode);
    assert_eq!(d.fs.calls().watch, watches + own_watch, "{mode:?}");
    assert_eq!(known(&d), ["/p/R/a.js"], "{mode:?}");
    caught_up(&mut d);
  }
}

/// A watched path that the filter ignores counts as missing, also when a directory is there:
/// the directory above tells its contents, as for every watched path with nothing at it.
#[test]
fn a_watched_directory_that_the_filter_ignores() {
  let mut d = Driver::sim(Mode::Inode, pathwatch_sim::ignore_node_modules());
  d.fs.mkdir_all("/p/node_modules").unwrap();
  d.watch(&["/p/node_modules"]);
  // The look for the kind asks the filter too: `/p` is asked once, for its contents.
  assert_eq!(asked(&d.fs), [at("/", Some(Names)), at("/p", Some(Contents))]);
  assert_eq!(os_watches(&d.fs), [at("/", Names), at("/p", Contents)]);
  // Every later look at `/p` finds its want as it is, and asks nothing.
  let calls = d.fs.calls();
  d.fs.write("/p/node_modules/a.js", "1").unwrap();
  assert_eq!(settled(&mut d), NONE);
  assert_eq!(d.fs.calls().watch, calls.watch);
  caught_up(&mut d);
}

/// Many watched directories in one directory: each costs one look for its kind and one check,
/// whether they come in one batch or one by one. The directory above tells names only.
#[test]
fn many_watched_directories_in_one_directory() {
  let mut d = Driver::sim(Mode::Inode, Options::default());
  let dirs: Vec<String> = (0..50).map(|number| format!("/p/pkg{number}")).collect();
  for dir in &dirs {
    d.fs.mkdir_all(dir).unwrap();
  }
  d.watch(&dirs[..25]);
  // `/` and `/p`, and two looks for each watched directory.
  assert_eq!(d.fs.calls().lstat, 2 + 25 * 2);

  let before = d.fs.calls().lstat;
  for dir in &dirs[25..] {
    d.watch(&[dir]);
  }
  assert_eq!(d.fs.calls().lstat - before, 25 * 2);
  assert_eq!(os_watches(&d.fs)[1], at("/p", Names));
  assert_eq!(os_watches(&d.fs).len(), 2 + 50);
  caught_up(&mut d);
}

/// A watched path that is a link is followed, also by the look for its kind: behind a link to
/// a directory there is a watched directory, and behind a link to a file a watched file.
#[test]
fn a_watched_link() {
  let mut d = Driver::sim(Mode::Inode, Options::default());
  d.fs.mkdir_all("/p").unwrap();
  d.fs.mkdir_all("/q").unwrap();
  d.fs.mkdir("/store").unwrap();
  d.fs.write("/target.js", "1").unwrap();
  d.fs.symlink("/store", "/p/dir").unwrap();
  d.fs.symlink("/target.js", "/q/file.js").unwrap();
  d.watch(&["/p/dir", "/q/file.js"]);
  let calls = [
    at("/", Some(Names)),
    at("/p", Some(Names)),
    at("/q", Some(Contents)),
    at("/p/dir", Some(Contents)),
  ];
  let mut log = asked(&d.fs);
  log.sort();
  assert_eq!(log, sorted(calls.to_vec()));
  caught_up(&mut d);
}

/// For one directory the want only goes up (rule 5 of the backend). The directory was asked
/// for its contents when it held a watched file. Then a directory comes at that watched path,
/// and something makes the engine ask for the watch again: it asks for the contents again.
#[test]
fn the_want_of_a_directory_never_goes_down() {
  let mut d = Driver::sim(Mode::Inode, Options::default());
  d.fs.mkdir_all("/top/p").unwrap();
  d.fs.write("/top/p/R", "1").unwrap();
  d.watch(&["/top/p/R"]);
  d.fs.remove("/top/p/R").unwrap();
  d.fs.mkdir("/top/p/R").unwrap();
  assert_eq!(settled(&mut d), ["D /top/p/R"]);

  // Away and back in one window: `/top/p` is the same directory, and it is asked for again.
  let before = asked(&d.fs).len();
  d.fs.rename("/top/p", "/top/x").unwrap();
  d.fs.rename("/top/x", "/top/p").unwrap();
  assert_eq!(settled(&mut d), NONE);
  let again: Vec<_> =
    asked(&d.fs)[before..].iter().filter(|(dir, _)| dir == "/top/p").cloned().collect();
  assert_eq!(again, [at("/top/p", Some(Contents))]);
  caught_up(&mut d);
}

/// `unwatch` is the one way down for a want, and the want can go up again after it. The
/// directory gives its watch for the tree back when its watched file is taken back. When the
/// watched directory next to that file becomes a file later, the directory is asked for its
/// contents once more.
#[test]
fn the_want_goes_down_with_unwatch_and_up_again() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir_all("/p/R").unwrap();
    d.fs.write("/p/f.js", "1").unwrap();
    d.fs.write("/p/R/x.js", "1").unwrap();
    d.watch(&["/p/f.js", "/p/R"]);
    d.unwatch(&["/p/f.js"]);
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    if mode == Mode::Inode {
      // A watch for one directory stays as it is: it tells more than is needed now.
      let watches = [at("/", Names), at("/p", Contents), at("/p/R", Contents)];
      assert_eq!(os_watches(&d.fs), watches, "{mode:?}");
    } else {
      assert_eq!(os_watches(&d.fs), [at("/p/R", Contents)], "{mode:?}");
    }

    d.fs.remove_all("/p/R").unwrap();
    d.fs.write("/p/R", "1").unwrap();
    assert_eq!(settled(&mut d), ["C /p/R", "D /p/R/x.js"], "{mode:?}");
    assert!(os_watches(&d.fs).contains(&at("/p", Contents)), "{mode:?}");
    d.fs.write("/p/R", "22").unwrap();
    assert!(d.fs.pending_raw() > 0, "{mode:?}");
    assert_eq!(window(&mut d), ["U /p/R"], "{mode:?}");
    caught_up(&mut d);
  }
}

/// A watched path that the filter ignores, with another watched path below it. For events it
/// counts as missing. But a directory on the way to a watched path is never filtered, so the
/// tree has a directory there, and the directory above needs to tell names only. The look for
/// the kind says the same as the real look: the watch for the tree is put where the watched
/// file is, not above the ignored directory with all that is in it.
///
/// The look for the kind asked the filter in every case and said "nothing is there". The
/// directory above was asked for its contents, and for one directory the want only goes up.
#[test]
fn an_ignored_watched_directory_on_the_way_to_a_watched_path() {
  for mode in RECURSIVE {
    let mut d = Driver::sim(mode, pathwatch_sim::ignore_node_modules());
    d.fs.mkdir_all("/p/node_modules/pkg").unwrap();
    d.fs.write("/p/node_modules/pkg/x.js", "1").unwrap();
    d.fs.write("/p/node_modules/other.js", "1").unwrap();
    d.watch(&["/p/node_modules", "/p/node_modules/pkg/x.js"]);
    assert_eq!(known(&d), ["/p/node_modules/pkg/x.js"], "{mode:?}");
    let contents: Vec<_> =
      asked(&d.fs).into_iter().filter(|(_, want)| *want == Some(Contents)).collect();
    assert_eq!(contents, [at("/p/node_modules/pkg", Some(Contents))], "{mode:?}");
    assert_eq!(os_watches(&d.fs), [at("/p/node_modules/pkg", Contents)], "{mode:?}");

    // The OS says nothing about the rest of the ignored directory.
    d.fs.write("/p/node_modules/other.js", "22").unwrap();
    assert_eq!(d.fs.pending_raw(), 0, "{mode:?}");
    d.fs.write("/p/node_modules/pkg/x.js", "22").unwrap();
    assert_eq!(window(&mut d), ["U /p/node_modules/pkg/x.js"], "{mode:?}");
    caught_up(&mut d);
  }
}

/// A chain of directories above a watched path is missing. The lowest directory that exists
/// holds a name on the way with nothing at it: a directory can come there with all that is
/// below it at once. So it tells its contents, and where a watch covers a tree it is put
/// there. The file is reported with no poll.
///
/// The disk is not asked for the kind of a name on the way: the directory is asked for names
/// first, and for more after the check that finds nothing.
#[test]
fn a_missing_chain_above_a_watched_path() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir("/p").unwrap();
    d.watch(&["/p/gen/out/x.json"]);
    let calls = [at("/", Some(Names)), at("/p", Some(Names)), at("/p", Some(Contents))];
    assert_eq!(asked(&d.fs), calls, "{mode:?}");
    if mode == Mode::Inode {
      assert_eq!(os_watches(&d.fs), [at("/", Names), at("/p", Contents)], "{mode:?}");
    } else {
      assert_eq!(os_watches(&d.fs), [at("/p", Contents)], "{mode:?}");
    }
    // `/`, `/p`, and `gen` twice: the second look comes when the wider watch is live.
    assert_eq!(d.fs.calls().lstat, 4, "{mode:?}");

    let start = d.now;
    d.fs.mkdir_all("/p/gen/out").unwrap();
    d.fs.write("/p/gen/out/x.json", "1").unwrap();
    assert!(d.fs.pending_raw() > 0, "{mode:?}");
    assert_eq!(window(&mut d), ["C /p/gen/out/x.json"], "{mode:?}");
    d.fs.write("/p/gen/out/x.json", "22").unwrap();
    assert_eq!(window(&mut d), ["U /p/gen/out/x.json"], "{mode:?}");
    assert!(d.now - start < NO_POLL, "{mode:?}");
    if mode != Mode::Inode {
      // The watch stays where it is: for one directory the want only goes up.
      assert_eq!(os_watches(&d.fs), [at("/p", Contents)], "{mode:?}");
    }
    caught_up(&mut d);
  }
}

/// A file is at a name on the way to a watched path. For the watched path that is the same as
/// nothing: the directory that holds the file tells its contents, and the chain that takes
/// the place of the file is found with no poll.
#[test]
fn a_file_on_the_way_to_a_watched_path() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir("/p").unwrap();
    d.fs.write("/p/gen", "a file").unwrap();
    d.watch(&["/p/gen/out/x.json"]);
    assert!(os_watches(&d.fs).contains(&at("/p", Contents)), "{mode:?}");
    assert_eq!(known(&d), NONE, "{mode:?}");

    let start = d.now;
    d.fs.write("/p/gen", "written").unwrap();
    assert_eq!(window(&mut d), NONE, "{mode:?}");
    d.fs.remove("/p/gen").unwrap();
    d.fs.mkdir_all("/p/gen/out").unwrap();
    d.fs.write("/p/gen/out/x.json", "1").unwrap();
    assert_eq!(window(&mut d), ["C /p/gen/out/x.json"], "{mode:?}");
    assert!(d.now - start < NO_POLL, "{mode:?}");

    // And back: a file takes the place of the chain.
    d.fs.remove_all("/p/gen").unwrap();
    d.fs.write("/p/gen", "a file again").unwrap();
    assert_eq!(settled(&mut d), ["D /p/gen/out/x.json"], "{mode:?}");
    caught_up(&mut d);
  }
}

/// The chain is there when the path is watched, so no directory above it tells more than
/// names. Then the chain goes away (`rm -r`) and comes back (`mkdir -p` and a write at once).
/// The look that does not find a name on the way asks the directory above for more, so the
/// way back is reported with no poll.
#[test]
fn a_chain_above_a_watched_path_goes_and_comes_back() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir_all("/p/gen/out").unwrap();
    d.fs.write("/p/gen/out/x.json", "1").unwrap();
    d.watch(&["/p/gen/out/x.json"]);
    if mode != Mode::Inode {
      assert_eq!(os_watches(&d.fs), [at("/p/gen/out", Contents)], "{mode:?}");
    }

    d.fs.remove_all("/p/gen").unwrap();
    assert_eq!(settled(&mut d), ["D /p/gen/out/x.json"], "{mode:?}");
    assert!(os_watches(&d.fs).contains(&at("/p", Contents)), "{mode:?}");

    for round in 0..3 {
      let start = d.now;
      d.fs.mkdir_all("/p/gen/out").unwrap();
      d.fs.write("/p/gen/out/x.json", "1").unwrap();
      assert!(d.fs.pending_raw() > 0, "{mode:?}, round {round}");
      assert_eq!(window(&mut d), ["C /p/gen/out/x.json"], "{mode:?}, round {round}");
      assert!(d.now - start < NO_POLL, "{mode:?}, round {round}");
      d.fs.remove_all("/p/gen").unwrap();
      assert!(d.fs.pending_raw() > 0, "{mode:?}, round {round}");
      assert_eq!(settled(&mut d), ["D /p/gen/out/x.json"], "{mode:?}, round {round}");
    }
    caught_up(&mut d);
  }
}

// ---- cases of the design review ----

/// The want of a node is raised before the scan, and nothing is asked below a watch that
/// covers a tree.
#[test]
fn the_want_is_raised_before_the_scan() {
  let mut d = Driver::sim(Mode::Inode, Options::default());
  d.fs.mkdir_all("/p/src").unwrap();
  d.fs.write("/p/src/a.js", "1").unwrap();
  d.fs.write("/p/package.json", "1").unwrap();
  d.watch(&["/p/src/a.js"]);
  // `/p` only leads to a watched path: a write in it is not reported by the OS.
  d.fs.write("/p/package.json", "22").unwrap();
  assert_eq!(d.fs.pending_raw(), 0);
  d.watch(&["/p/package.json"]);
  d.fs.write("/p/package.json", "333").unwrap();
  assert_eq!(d.fs.pending_raw(), 1);
  assert_eq!(Sim::window(&mut d), ["U /p/package.json"]);
  caught_up(&mut d);

  for mode in RECURSIVE {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir_all("/p/R/one/two/three").unwrap();
    d.fs.write("/p/R/one/two/three/f", "1").unwrap();
    d.watch(&["/p/R"]);
    // `/` and `/p` only lead to the watched directory, `/p/R` is it. Nothing is asked for the
    // four directories below `/p/R`.
    assert_eq!(d.fs.calls().watch, 3, "{mode:?}");
    d.fs.write("/p/R/one/two/three/f", "22").unwrap();
    d.fs.mkdir("/p/R/one/new").unwrap();
    d.fs.write("/p/R/one/new/g", "1").unwrap();
    assert_eq!(Sim::window(&mut d), ["C /p/R/one/new/g", "U /p/R/one/two/three/f"], "{mode:?}");
    assert_eq!(d.fs.calls().watch, 3, "{mode:?}");
    caught_up(&mut d);
  }
}

/// A directory that only leads to a watched path is watched for names: inotify does not report
/// a write in it. When a directory above it becomes a watched path, it is a full part of that
/// directory, and its watch must report writes from then on.
#[test]
fn a_directory_on_the_way_becomes_a_part_of_a_watched_directory() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/R/lib/sub").unwrap();
    d.fs().write("/p/R/top.js", "1").unwrap();
    d.fs().write("/p/R/lib/old.js", "1").unwrap();
    d.fs().write("/p/R/lib/sub/x.js", "1").unwrap();
    // `/p/R` and `/p/R/lib` only lead to the watched file.
    assert_eq!(d.watch(&["/p/R/lib/sub/x.js"]), NONE, "{on}");
    d.fs().write("/p/R/top.js", "22").unwrap();
    d.fs().write("/p/R/lib/old.js", "22").unwrap();
    assert_eq!(d.settled(), NONE, "{on}");

    assert_eq!(d.watch(&["/p/R"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/R/lib/old.js", "/p/R/lib/sub/x.js", "/p/R/top.js"], "{on}");
    d.fs().write("/p/R/top.js", "333").unwrap();
    d.fs().write("/p/R/lib/old.js", "333").unwrap();
    assert_eq!(d.window(), ["U /p/R/lib/old.js", "U /p/R/top.js"], "{on}");
    d.caught_up();
  });
}

/// How many `lstat` calls the next poll makes.
fn lstats_of_the_next_poll(d: &mut Driver<SimBackend>) -> u64 {
  let before = d.fs.calls().lstat;
  assert!(d.step(), "no poll is due");
  d.fs.calls().lstat - before
}

/// By prefix, a directory on the way to a watched path is polled: no stream reports it. Later
/// a stream starts above it. The engine asks again, the directory is looked at once, and from
/// then on the stream tells about it: no poll looks at it any more.
#[test]
fn a_polled_directory_is_asked_again_when_a_stream_above_it_starts() {
  let mut d = Driver::sim(Mode::Prefix, Options::default());
  d.fs.mkdir_all("/p/a/b").unwrap();
  d.fs.write("/p/a/b/f.js", "1").unwrap();
  d.fs.write("/p/x.js", "1").unwrap();
  d.watch(&["/p/a/b/f.js"]);
  assert_eq!(os_watches(&d.fs), [at("/p/a/b", Contents)]);
  // The poll looks at `/`, `p`, `a` and `b`: each from the directory above it.
  assert_eq!(lstats_of_the_next_poll(&mut d), 4);

  // While nothing tells about `/p/a`, another directory comes to the place of `b`. The
  // record of the stream for it is not delivered yet.
  d.fs.rename("/p/a/b", "/p/a/old").unwrap();
  d.fs.mkdir("/p/a/b").unwrap();
  d.fs.write("/p/a/b/f.js", "22").unwrap();

  // `/p` tells its contents now, with a stream. `/p/a` lies below the new stream, so it is
  // asked again, and its look after the commit finds the other directory. `/` lies above the
  // stream and is not asked again.
  d.watch(&["/p/x.js"]);
  let calls = asked(&d.fs);
  let again = [at("/p", Some(Contents)), at("/p/a", Some(Names))];
  assert!(calls.windows(2).any(|calls| calls == again), "{calls:?}");
  assert_eq!(calls.iter().filter(|call| **call == at("/", Some(Names))).count(), 1, "{calls:?}");
  assert_eq!(settled(&mut d), ["U /p/a/b/f.js"]);
  // Only `/` is polled now: it is looked at, and `p` in it.
  assert_eq!(lstats_of_the_next_poll(&mut d), 2);

  // What only the poll of `/p/a` found before, the stream tells now.
  d.fs.rename("/p/a/b", "/p/a/older").unwrap();
  d.fs.mkdir("/p/a/b").unwrap();
  d.fs.write("/p/a/b/f.js", "333").unwrap();
  assert_eq!(window_then_settled(&mut d), ["U /p/a/b/f.js"]);
  caught_up(&mut d);
}

/// A listing that tells all of a file (APFS) is its look: a scan asks no `lstat` for a file,
/// the first one and the one after a loss. What the listing told can be old when the scan
/// comes to the file: the file is written or removed, or a directory takes its place, at any
/// moment of the `watch` call.
#[test]
fn a_listing_that_tells_all_of_a_file_is_its_look() {
  fn prepare(d: &mut dyn Sim, meta: bool) {
    d.fs().set_listing_meta(meta);
    d.fs().mkdir_all("/p/src").unwrap();
    d.fs().write("/p/src/a.js", "1").unwrap();
    d.fs().write("/p/src/b.js", "1").unwrap();
  }
  let changes: [fn(&SimFs); 3] = [
    |fs| fs.write("/p/src/a.js", "22").unwrap(),
    |fs| fs.remove("/p/src/a.js").unwrap(),
    |fs| {
      fs.remove("/p/src/a.js").unwrap();
      fs.mkdir_all("/p/src/a.js/in").unwrap();
    },
  ];
  for (on, make) in makes() {
    let calls = |meta: bool| {
      let mut d = make();
      prepare(&mut *d, meta);
      d.watch(&["/p"]);
      let first = d.fs().calls();
      d.overflow();
      d.settled();
      let again = d.fs().calls().lstat - first.lstat;
      d.caught_up();
      (first, again)
    };
    let ((with_lstat, again_with_lstat), (first, again)) = (calls(false), calls(true));
    assert_eq!((with_lstat.lstat - first.lstat, again_with_lstat - again), (2, 2), "{on}");

    for moment in 0..first.total() {
      for change in changes {
        let mut d = make();
        prepare(&mut *d, true);
        d.fs().at_call(moment, change);
        d.watch(&["/p"]);
        let events = d.settled();
        assert_eq!(d.fs().traps_run(), 1, "{on}, call {moment}: {events:?}");
        d.caught_up();
      }
    }
  }
}
