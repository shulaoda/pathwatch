//! `unwatch`: a watched path is taken back. It reports nothing, what only that path covered is
//! forgotten (also a `Delete` that waits), and the OS watches that nothing needs are given back.
//! A story makes the disk, calls `watch`, changes the disk and compares the events that come
//! out (`C`, `U`, `D` and the path).

#![cfg(unix)]

use std::{path::PathBuf, time::Duration};

use pathwatch::{
  backend::Want::{self, Contents, Names},
  engine::Options,
  poll::PollBackend,
};
use pathwatch_sim::{
  Deliver, Driver, Mode, NONE, RECURSIVE, Sim, SimFs, asked, at, caught_up, ignore_node_modules,
  known, on_a_small_stack, on_all, on_modes, os_watches, settled, sorted, taken, window,
};

/// `unwatch` of a directory above a chain deeper than the stack of a thread takes: the names
/// below are uncovered from a list, not by a recursion down the chain.
#[test]
fn unwatch_above_a_chain_deeper_than_the_stack() {
  on_a_small_stack(|| {
    let mut d = Driver::sim(Mode::Handle, Options::default());
    let chain = "/p".to_owned() + &"/d".repeat(1500);
    let leaf = format!("{chain}/leaf");
    d.fs.mkdir_all(&chain).unwrap();
    d.fs.write(&leaf, "1").unwrap();
    d.watch(&["/p", &leaf]);
    d.unwatch(&["/p"]);
    d.fs.write(&leaf, "22").unwrap();
    assert_eq!(settled(&mut d), [format!("U {leaf}")]);
    caught_up(&mut d);
  });
}

/// A watched file is taken back: no event, and nothing is reported for it any more.
#[test]
fn unwatch_a_file() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir("/p").unwrap();
    d.fs().write("/p/a.js", "1").unwrap();
    d.fs().write("/p/b.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/a.js", "/p/b.js"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/a.js", "/p/b.js"], "{on}");

    assert_eq!(d.unwatch(&["/p/a.js"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/b.js"], "{on}");
    d.fs().write("/p/a.js", "22").unwrap();
    d.fs().write("/p/b.js", "22").unwrap();
    assert_eq!(d.settled(), ["U /p/b.js"], "{on}");
    d.fs().remove("/p/a.js").unwrap();
    assert_eq!(d.settled(), NONE, "{on}");
    d.caught_up();
  });
}

/// A watched directory is taken back: every file below it is forgotten, with no event.
#[test]
fn unwatch_a_directory() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/R/sub/deep").unwrap();
    d.fs().write("/p/R/a.js", "1").unwrap();
    d.fs().write("/p/R/sub/deep/b.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R"]), NONE, "{on}");
    assert_eq!(d.known().len(), 2, "{on}");

    assert_eq!(d.unwatch(&["/p/R"]), NONE, "{on}");
    assert_eq!(d.known(), NONE, "{on}");
    d.fs().write("/p/R/a.js", "22").unwrap();
    d.fs().write("/p/R/sub/new.js", "1").unwrap();
    d.fs().remove_all("/p/R/sub/deep").unwrap();
    assert_eq!(d.fs().pending_raw(), 0, "{on}");
    assert_eq!(d.settled(), NONE, "{on}");
    d.caught_up();
  });
}

/// A watched path with nothing at it, below a chain that is not there either.
#[test]
fn unwatch_a_missing_path() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir("/p").unwrap();
    assert_eq!(d.watch(&["/p/gone/deep/x.js", "/p/not-there.js"]), NONE, "{on}");
    assert_eq!(d.unwatch(&["/p/gone/deep/x.js", "/p/not-there.js"]), NONE, "{on}");

    d.fs().mkdir_all("/p/gone/deep").unwrap();
    d.fs().write("/p/gone/deep/x.js", "1").unwrap();
    d.fs().write("/p/not-there.js", "1").unwrap();
    assert_eq!(d.fs().pending_raw(), 0, "{on}");
    assert_eq!(d.settled(), NONE, "{on}");
    d.caught_up();
  });
}

/// `unwatch` takes back a `watch` of the same path and nothing else. A path that was never
/// given to `watch` is skipped, also when it lies inside a watched directory: that directory
/// keeps reporting it. A path that cannot be watched is skipped too.
#[test]
fn unwatch_a_path_that_is_not_watched() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/R/sub").unwrap();
    d.fs().write("/p/R/sub/a.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R"]), NONE, "{on}");
    let calls = d.fs().calls();

    let not_watched = ["/p/R/sub", "/p/R/sub/a.js", "/p", "/q/never", "/", "relative", ""];
    assert_eq!(d.unwatch(&not_watched), NONE, "{on}");
    assert_eq!(d.fs().calls(), calls, "{on}");
    assert!(d.errors().is_empty(), "{on}");
    assert_eq!(d.known(), ["/p/R/sub/a.js"], "{on}");
    d.fs().write("/p/R/sub/a.js", "22").unwrap();
    assert_eq!(d.settled(), ["U /p/R/sub/a.js"], "{on}");

    // Taken back twice: the second time the path is not watched.
    assert_eq!(d.unwatch(&["/p/R"]), NONE, "{on}");
    assert_eq!(d.unwatch(&["/p/R"]), NONE, "{on}");
    assert_eq!(d.known(), NONE, "{on}");
    d.caught_up();
  });
}

/// A watched file inside a watched directory is taken back: the directory still covers it, so
/// it stays known and its changes are reported.
#[test]
fn unwatch_a_path_below_a_watched_directory() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/R/sub").unwrap();
    d.fs().write("/p/R/sub/a.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R", "/p/R/sub/a.js", "/p/R/sub", "/p/R/not-there.js"]), NONE, "{on}");

    let below = ["/p/R/sub/a.js", "/p/R/sub", "/p/R/not-there.js"];
    assert_eq!(d.unwatch(&below), NONE, "{on}");
    assert_eq!(d.known(), ["/p/R/sub/a.js"], "{on}");
    d.fs().write("/p/R/sub/a.js", "22").unwrap();
    d.fs().write("/p/R/not-there.js", "1").unwrap();
    assert_eq!(d.settled(), ["C /p/R/not-there.js", "U /p/R/sub/a.js"], "{on}");
    d.caught_up();
  });
}

/// A watched directory with watched files below it is taken back. The watched files stay
/// known, and everything else below the directory is forgotten.
///
/// Where one watch covers a whole tree, the watch of the directory covered the files too. It
/// is given back, and the directory that holds a watched file gets a watch of its own.
#[test]
fn unwatch_a_directory_above_watched_files() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/R/lib/deep").unwrap();
    d.fs().write("/p/R/top.js", "1").unwrap();
    d.fs().write("/p/R/lib/a.js", "1").unwrap();
    d.fs().write("/p/R/lib/other.js", "1").unwrap();
    d.fs().write("/p/R/lib/deep/b.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R"]), NONE, "{on}");
    assert_eq!(d.watch(&["/p/R/lib/a.js", "/p/R/lib/deep"]), NONE, "{on}");
    assert_eq!(d.known().len(), 4, "{on}");

    assert_eq!(d.unwatch(&["/p/R"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/R/lib/a.js", "/p/R/lib/deep/b.js"], "{on}");
    if on == "Prefix" || on == "Handle" {
      assert_eq!(os_watches(d.fs()), [at("/p/R/lib", Contents)], "{on}");
    }

    d.fs().write("/p/R/lib/a.js", "22").unwrap();
    d.fs().write("/p/R/lib/deep/new.js", "1").unwrap();
    assert_eq!(d.fs().pending_raw() > 0, on != "Poll", "{on}");
    assert_eq!(d.window(), ["U /p/R/lib/a.js", "C /p/R/lib/deep/new.js"], "{on}");
    // What the directory covered alone.
    d.fs().write("/p/R/top.js", "22").unwrap();
    d.fs().write("/p/R/lib/other.js", "22").unwrap();
    d.fs().write("/p/R/new.js", "1").unwrap();
    assert_eq!(d.settled(), NONE, "{on}");
    d.caught_up();
  });
}

/// The directory that held a watched file had a watch for its whole tree, and a watched file
/// deeper down relied on it. When the file is taken back, the directory only leads to the
/// other one: its watch is given back, and the other file is heard through a watch of its own.
#[test]
fn unwatch_gives_a_watch_for_a_tree_back() {
  on_modes(&RECURSIVE, &Options::default(), |d, on| {
    d.fs().mkdir_all("/p/sub").unwrap();
    d.fs().write("/p/f.js", "1").unwrap();
    d.fs().write("/p/sub/g.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/f.js", "/p/sub/g.js"]), NONE, "{on}");
    assert_eq!(os_watches(d.fs()), [at("/p", Contents)], "{on}");

    assert_eq!(d.unwatch(&["/p/f.js"]), NONE, "{on}");
    assert_eq!(os_watches(d.fs()), [at("/p/sub", Contents)], "{on}");
    d.fs().write("/p/sub/g.js", "22").unwrap();
    assert!(d.fs().pending_raw() > 0, "{on}");
    assert_eq!(d.window(), ["U /p/sub/g.js"], "{on}");
    d.fs().write("/p/f.js", "22").unwrap();
    assert_eq!(d.fs().pending_raw(), 0, "{on}");
    d.caught_up();
  });
}

/// Where one watch covers a whole tree: single files are watched, three directories deep, and
/// then the tree above them. The watch of the tree takes over: each directory below gives its
/// own back, so it does not matter which of them was watched first. When the tree is taken
/// back, each directory asks for its own again, and what is below is scanned deep once.
#[test]
fn a_tree_above_watched_files_takes_their_watches_over() {
  for mode in RECURSIVE {
    let mut d = Driver::sim(mode, Options::default());
    let mut files = Vec::new();
    let dirs = ["/p/t/a/src/x", "/p/t/a/src/y", "/p/t/b/src/x", "/p/t/b/src/y"];
    for dir in dirs {
      d.fs.mkdir_all(dir).unwrap();
      for file in 0..5 {
        files.push(format!("{dir}/file{file}.js"));
        d.fs.write(files.last().unwrap(), "1").unwrap();
      }
    }
    let of_the_files: Vec<_> = dirs.map(|dir| at(dir, Contents)).into();
    d.watch(&files);
    assert_eq!(os_watches(&d.fs), of_the_files, "{mode:?}");
    d.watch(&["/p/t"]);
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    assert_eq!(os_watches(&d.fs), [at("/p/t", Contents)], "{mode:?}");
    d.fs.write("/p/t/a/src/x/file1.js", "22").unwrap();
    assert_eq!(settled(&mut d), ["U /p/t/a/src/x/file1.js"], "{mode:?}");

    let before = d.fs.calls();
    d.unwatch(&["/p/t"]);
    assert_eq!(taken(&mut d), NONE, "{mode:?}");
    let after = d.fs.calls();
    // One look for each of the 8 directories and the 20 files below the tree.
    assert_eq!(after.lstat - before.lstat, 28, "{mode:?}");
    // The tree gives its watch back, and the backend is asked once for each of the 9
    // directories: the top one then, and the others when the deep scan comes to them.
    assert_eq!(after.unwatch - before.unwatch, 1, "{mode:?}");
    assert_eq!(after.watch - before.watch, 9, "{mode:?}");
    assert_eq!(os_watches(&d.fs), of_the_files, "{mode:?}");

    d.fs.write("/p/t/b/src/y/file3.js", "22").unwrap();
    d.fs.write("/p/t/b/src/y/new.js", "1").unwrap();
    assert_eq!(settled(&mut d), ["U /p/t/b/src/y/file3.js"], "{mode:?}");
    caught_up(&mut d);
  }
}

/// A notification is on its way through the watch of a watched directory. Then the directory
/// above gets a watch for its tree, and the watched directory is taken back: its own watch is
/// given back, and the watch above covers it from now on. The notification is gone with the
/// watch that made it, and the watch above started after the change. So what stays below the
/// directory is looked at when the watch is given back.
#[test]
fn unwatch_gives_a_watch_back_that_has_a_notification_on_its_way() {
  on_modes(&RECURSIVE, &Options::default(), |d, on| {
    d.fs().mkdir_all("/p/d/sub").unwrap();
    d.fs().write("/p/d/sub/a.js", "1").unwrap();
    d.fs().write("/p/x.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/d", "/p/d/sub"]), NONE, "{on}");
    assert_eq!(os_watches(d.fs()), [at("/p/d", Contents)], "{on}");

    d.fs().write("/p/d/sub/a.js", "22").unwrap();
    assert!(d.fs().pending_raw() > 0, "{on}");
    // A watched file in `/p`: that directory tells its contents now, for its whole tree.
    assert_eq!(d.watch(&["/p/x.js"]), NONE, "{on}");
    assert_eq!(os_watches(d.fs()), [at("/p", Contents), at("/p/d", Contents)], "{on}");

    let mut events = d.unwatch(&["/p/d"]);
    assert_eq!(os_watches(d.fs()), [at("/p", Contents)], "{on}");
    events.append(&mut d.settled());
    assert_eq!(events, ["U /p/d/sub/a.js"], "{on}");
    d.caught_up();
  });
}

/// A file is written between `unwatch` and the moment the new watches are live, at any call
/// of the engine in between. The watched file that stays is not looked at too early: its last
/// content is known.
#[test]
fn unwatch_opens_no_gap_for_what_stays() {
  fn prepare(d: &mut Driver<pathwatch_sim::SimBackend>) {
    d.fs.mkdir_all("/p/R/lib").unwrap();
    d.fs.write("/p/R/lib/a.js", "1").unwrap();
    d.watch(&["/p/R", "/p/R/lib/a.js"]);
  }
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    prepare(&mut d);
    let before = d.fs.calls().total();
    d.unwatch(&["/p/R"]);
    let calls = d.fs.calls().total() - before;
    caught_up(&mut d);

    for moment in 0..calls {
      let mut d = Driver::sim(mode, Options::default());
      prepare(&mut d);
      d.fs.at_call(moment, |fs| fs.write("/p/R/lib/a.js", "written").unwrap());
      d.unwatch(&["/p/R"]);
      assert_eq!(d.fs.traps_run(), 1, "{mode:?}, call {moment}");
      let mut events = taken(&mut d);
      events.append(&mut settled(&mut d));
      assert!(!events.is_empty(), "{mode:?}: the write before call {moment} was lost");
      assert!(events.iter().all(|event| event == "U /p/R/lib/a.js"), "{mode:?}: {events:?}");
      assert_eq!(known(&d), ["/p/R/lib/a.js"], "{mode:?}, call {moment}");
      // The end of the story compares the stored fingerprint with the disk.
      caught_up(&mut d);
    }
  }
}

/// A path is taken back and watched again: what is there then is the baseline once more, and
/// it is not reported.
#[test]
fn unwatch_then_watch_again() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/R/sub").unwrap();
    d.fs().write("/p/R/sub/a.js", "1").unwrap();
    d.fs().write("/p/f.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R", "/p/f.js"]), NONE, "{on}");
    assert_eq!(d.unwatch(&["/p/R", "/p/f.js"]), NONE, "{on}");
    assert_eq!(d.known(), NONE, "{on}");

    // The disk changes while nothing is watched.
    d.fs().write("/p/R/sub/a.js", "22").unwrap();
    d.fs().write("/p/R/new.js", "1").unwrap();
    d.fs().write("/p/f.js", "22").unwrap();
    assert_eq!(d.watch(&["/p/R", "/p/f.js"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/R/new.js", "/p/R/sub/a.js", "/p/f.js"], "{on}");
    assert_eq!(d.settled(), NONE, "{on}");

    d.fs().write("/p/R/sub/a.js", "333").unwrap();
    d.fs().write("/p/f.js", "333").unwrap();
    assert_eq!(d.window(), ["U /p/R/sub/a.js", "U /p/f.js"], "{on}");
    d.caught_up();
  });
}

/// A `Delete` waits for its second look, and the path that covers the file is taken back. The
/// `Delete` is dropped with it.
#[test]
fn unwatch_while_a_delete_is_held() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/R/sub").unwrap();
    d.fs().write("/p/R/a.js", "1").unwrap();
    d.fs().write("/p/R/sub/b.js", "1").unwrap();
    d.fs().write("/p/f.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R", "/p/f.js"]), NONE, "{on}");

    // A file, a directory with a file, and a watched file: three deletes wait.
    d.fs().remove("/p/R/a.js").unwrap();
    d.fs().remove_all("/p/R/sub").unwrap();
    d.fs().remove("/p/f.js").unwrap();
    assert_eq!(d.window(), NONE, "{on}");
    assert_eq!(d.pending_deletes(), 3, "{on}");

    assert_eq!(d.unwatch(&["/p/R", "/p/f.js"]), NONE, "{on}");
    assert_eq!(d.pending_deletes(), 0, "{on}");
    assert_eq!(d.known(), NONE, "{on}");
    assert_eq!(d.settled(), NONE, "{on}");
    d.caught_up();
  });
}

/// The watched directory itself is gone and its `Delete`s wait. It is taken back, but a
/// watched file below it stays: only the `Delete` of that file comes.
#[test]
fn unwatch_a_held_directory_above_a_watched_file() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/R/lib").unwrap();
    d.fs().write("/p/R/lib/a.js", "1").unwrap();
    d.fs().write("/p/R/lib/other.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R", "/p/R/lib/a.js"]), NONE, "{on}");

    d.fs().remove_all("/p/R").unwrap();
    assert_eq!(d.window(), NONE, "{on}");
    assert_eq!(d.pending_deletes(), 2, "{on}");
    assert_eq!(d.unwatch(&["/p/R"]), NONE, "{on}");
    assert_eq!(d.pending_deletes(), 1, "{on}");
    assert_eq!(d.known(), ["/p/R/lib/a.js"], "{on}");
    assert_eq!(d.settled(), ["D /p/R/lib/a.js"], "{on}");

    // The watched file still has its watch when it comes back.
    d.fs().mkdir_all("/p/R/lib").unwrap();
    d.fs().write("/p/R/lib/a.js", "1").unwrap();
    d.fs().write("/p/R/lib/other.js", "1").unwrap();
    assert_eq!(d.settled(), ["C /p/R/lib/a.js"], "{on}");
    d.caught_up();
  });
}

/// The last watched path of a directory is taken back: the engine tells the backend to forget
/// the directory, and every directory above it that led nowhere else.
#[test]
fn unwatch_frees_the_os_watches() {
  let mut d = Driver::sim(Mode::Inode, Options::default());
  d.fs.mkdir_all("/p/src/deep").unwrap();
  d.fs.mkdir("/q").unwrap();
  d.fs.write("/p/src/a.js", "1").unwrap();
  d.fs.write("/p/src/b.js", "1").unwrap();
  d.fs.write("/q/c.js", "1").unwrap();
  d.watch(&["/p/src/a.js", "/p/src/b.js", "/q/c.js"]);
  let all = [at("/", Names), at("/p", Names), at("/p/src", Contents), at("/q", Contents)];
  assert_eq!(os_watches(&d.fs), all);
  let calls = asked(&d.fs).len();

  // One of two: the directory is still needed.
  d.unwatch(&["/p/src/a.js"]);
  assert_eq!(os_watches(&d.fs), all);
  assert_eq!(asked(&d.fs).len(), calls);

  // The last one: `/p/src` and `/p` go, from the bottom up. `/` still leads to `/q/c.js`.
  d.unwatch(&["/p/src/b.js"]);
  assert_eq!(asked(&d.fs)[calls..], [at("/p/src", None), at("/p", None)]);
  assert_eq!(os_watches(&d.fs), [at("/", Names), at("/q", Contents)]);

  // A whole watched tree: one `unwatch` call for each directory of it.
  d.watch(&["/p/src"]);
  assert_eq!(os_watches(&d.fs).len(), 5);
  d.unwatch(&["/p/src", "/q/c.js"]);
  assert_eq!(os_watches(&d.fs), []);
  assert_eq!(d.fs.calls().unwatch, 2 + 5);
  d.fs.write("/q/c.js", "22").unwrap();
  assert_eq!(d.fs.pending_raw(), 0);
  assert_eq!(settled(&mut d), NONE);
  caught_up(&mut d);
}

/// One of two watched files of a directory is taken back. The directory still holds a
/// watched file, so nothing changes for it: the engine reads nothing and asks the backend
/// nothing, also where the watch of the directory covers a whole tree.
#[test]
fn unwatch_one_of_two_files_costs_nothing() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/sub").unwrap();
    for file in ["/p/a.js", "/p/b.js", "/p/sub/c.js"] {
      d.fs().write(file, "1").unwrap();
    }
    assert_eq!(d.watch(&["/p/a.js", "/p/b.js", "/p/sub/c.js"]), NONE, "{on}");
    let (calls, watches) = (d.fs().calls(), os_watches(d.fs()));

    assert_eq!(d.unwatch(&["/p/a.js"]), NONE, "{on}");
    assert_eq!(d.fs().calls(), calls, "{on}");
    assert_eq!(os_watches(d.fs()), watches, "{on}");
    d.fs().write("/p/b.js", "22").unwrap();
    d.fs().write("/p/sub/c.js", "22").unwrap();
    assert_eq!(d.fs().pending_raw() > 0, on != "Poll", "{on}");
    assert_eq!(d.window(), ["U /p/b.js", "U /p/sub/c.js"], "{on}");
    d.caught_up();
  });
}

/// Nothing is watched any more, and the engine has nothing left to look at: a poll reads
/// nothing.
#[test]
fn nothing_is_left_when_every_path_is_taken_back() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/R").unwrap();
    d.fs().write("/p/R/a.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R", "/p/gone/x.js"]), NONE, "{on}");
    assert_eq!(d.unwatch(&["/p/R", "/p/gone/x.js"]), NONE, "{on}");
    assert_eq!(os_watches(d.fs()), [], "{on}");
    let reads = |d: &dyn Sim| {
      let calls = d.fs().calls();
      (calls.lstat, calls.stat, calls.read_dir, calls.hash, calls.watch)
    };
    let before = reads(d);
    assert_eq!(d.settled(), NONE, "{on}");
    assert_eq!(reads(d), before, "{on}");
    d.caught_up();
  });
}

/// A link inside a watched directory is followed only while a watched path goes through it.
/// When that path is taken back, the link is a file entry of the watched directory again. For
/// the handler that is the change of kind that it is: the files that were known behind the
/// link go, and the link comes.
#[test]
fn unwatch_a_path_behind_a_link_inside_a_watched_directory() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/R").unwrap();
    d.fs().mkdir("/store").unwrap();
    d.fs().write("/store/a.js", "1").unwrap();
    d.fs().write("/store/b.js", "1").unwrap();
    d.fs().symlink("/store", "/p/R/link").unwrap();
    // An old link: a young one counts as changed at every look.
    d.advance(Duration::from_secs(3));
    assert_eq!(d.watch(&["/p/R"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/R/link"], "{on}");
    // The link is followed from now on: what is behind it is the baseline.
    assert_eq!(d.watch(&["/p/R/link/a.js"]), ["D /p/R/link"], "{on}");
    assert_eq!(d.known(), ["/p/R/link/a.js", "/p/R/link/b.js"], "{on}");

    let events = d.unwatch(&["/p/R/link/a.js"]);
    assert_eq!(events, ["C /p/R/link", "D /p/R/link/a.js", "D /p/R/link/b.js"], "{on}");
    d.fs().write("/store/a.js", "22").unwrap();
    assert_eq!(d.settled(), NONE, "{on}");
    d.caught_up();
  });
}

/// A watched path that is a link to a file, inside a watched directory. Taken back, it is the
/// link itself that the directory reports: the stored fingerprint is the one of the link.
#[test]
fn unwatch_a_link_to_a_file_inside_a_watched_directory() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/R").unwrap();
    d.fs().write("/target.js", "content").unwrap();
    d.fs().symlink("/target.js", "/p/R/link.js").unwrap();
    // An old link: a young one counts as changed at every look.
    d.advance(Duration::from_secs(3));
    assert_eq!(d.watch(&["/p/R", "/p/R/link.js"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/R/link.js"], "{on}");

    // One `Update` too many is allowed here; a stale fingerprint is not.
    let events = d.unwatch(&["/p/R/link.js"]);
    assert!(events.iter().all(|event| event == "U /p/R/link.js"), "{on}: {events:?}");
    assert_eq!(d.known(), ["/p/R/link.js"], "{on}");
    // The target is not read any more.
    d.fs().write("/target.js", "another content").unwrap();
    assert_eq!(d.settled(), NONE, "{on}");
    d.caught_up();
  });
}

/// A watched file below a directory that the filter ignores, inside a watched directory. While
/// it is watched, the ignored directory has a node for it. Taken back, nothing is left of
/// that: no event, and the hints from inside end at the watched directory again.
#[test]
fn unwatch_a_file_below_an_ignored_directory() {
  on_all(&pathwatch_sim::ignore_node_modules(), |d, on| {
    d.fs().mkdir_all("/p/node_modules/pkg").unwrap();
    d.fs().write("/p/node_modules/pkg/x.js", "1").unwrap();
    d.fs().write("/p/a.js", "1").unwrap();
    assert_eq!(d.watch(&["/p", "/p/node_modules/pkg/x.js"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/a.js", "/p/node_modules/pkg/x.js"], "{on}");

    assert_eq!(d.unwatch(&["/p/node_modules/pkg/x.js"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/a.js"], "{on}");
    d.fs().write("/p/node_modules/pkg/x.js", "22").unwrap();
    d.fs().write("/p/a.js", "22").unwrap();
    assert_eq!(d.settled(), ["U /p/a.js"], "{on}");
    d.caught_up();
  });
}

/// A watched directory that the filter would ignore is watched all the same, and it covers the
/// watched file below it: when that path is taken back, the file stays known through the
/// directory.
#[test]
fn unwatch_below_a_watched_directory_that_the_filter_would_ignore() {
  on_all(&pathwatch_sim::ignore_node_modules(), |d, on| {
    d.fs().mkdir_all("/p/node_modules/pkg").unwrap();
    d.fs().write("/p/node_modules/pkg/x.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/node_modules", "/p/node_modules/pkg/x.js"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/node_modules/pkg/x.js"], "{on}");

    assert_eq!(d.unwatch(&["/p/node_modules/pkg/x.js"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/node_modules/pkg/x.js"], "{on}");
    d.fs().write("/p/node_modules/pkg/x.js", "22").unwrap();
    d.fs().write("/p/node_modules/new.js", "1").unwrap();
    assert_eq!(d.settled(), ["C /p/node_modules/new.js", "U /p/node_modules/pkg/x.js"], "{on}");
    d.caught_up();
  });
}

/// A directory becomes a watched path while the disk does not answer, so its node is not
/// complete yet. A watched file in it is taken back. The directory covers the file all the
/// same: it stays known, and no `Delete` comes for it while the look at the directory waits.
/// When the disk answers, the look that had to wait reports what else it finds.
#[test]
fn unwatch_a_file_in_a_directory_whose_first_look_waits() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/R/sub").unwrap();
    d.fs().write("/p/R/a.js", "1").unwrap();
    d.fs().write("/p/R/sub/b.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R/a.js", "/p/R/sub/b.js"]), NONE, "{on}");

    d.fs().set_offline(Some(std::io::ErrorKind::TimedOut));
    assert_eq!(d.watch(&["/p/R"]), NONE, "{on}");
    assert_eq!(d.unwatch(&["/p/R/a.js", "/p/R/sub/b.js"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/R/a.js", "/p/R/sub/b.js"], "{on}");
    // Many polls pass. Nothing is known about the disk, so nothing is reported.
    d.fs().write("/p/R/new.js", "1").unwrap();
    d.fs().write("/p/R/a.js", "22").unwrap();
    for _ in 0..30 {
      assert_eq!(d.window(), NONE, "{on}");
    }
    assert_eq!(d.known(), ["/p/R/a.js", "/p/R/sub/b.js"], "{on}");

    d.fs().set_offline(None);
    assert_eq!(d.settled(), ["U /p/R/a.js", "C /p/R/new.js"], "{on}");
    let errors = d.errors();
    assert!(!errors.is_empty(), "{on}");
    assert!(errors.iter().all(|kind| *kind == pathwatch::ErrorKind::Io), "{on}: {errors:?}");
    d.caught_up();
  });
}

/// A hint for a forgotten file is still on its way when the path is taken back. It finds
/// nothing to report, also when it comes late.
#[test]
fn a_hint_for_a_path_that_was_taken_back() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir_all("/p/R/sub").unwrap();
    d.fs.write("/p/R/sub/a.js", "1").unwrap();
    d.fs.write("/p/keep.js", "1").unwrap();
    d.watch(&["/p/R", "/p/keep.js"]);

    // One hint is in the engine and waits for its flush, the other is still in the queue.
    d.fs.write("/p/R/sub/a.js", "22").unwrap();
    assert!(d.deliver(usize::MAX) > 0, "{mode:?}");
    d.fs.remove_all("/p/R/sub").unwrap();
    d.unwatch(&["/p/R"]);
    assert_eq!(taken(&mut d), NONE, "{mode:?}");
    assert_eq!(known(&d), ["/p/keep.js"], "{mode:?}");
    d.advance(Duration::from_millis(500));
    assert_eq!(settled(&mut d), NONE, "{mode:?}");

    d.fs.write("/p/keep.js", "22").unwrap();
    assert_eq!(window(&mut d), ["U /p/keep.js"], "{mode:?}");
    caught_up(&mut d);
  }
}

/// A scan is queued when the path is taken back: on a disk that takes two spellings of a name
/// for one, a hint for a name that is not registered made the directory check its registered
/// names. The scan finds the directory changed, or gone, and what is
/// still watched in it is read all the same.
#[test]
fn unwatch_while_a_scan_is_queued() {
  let loose = Options { loose_spelling: true, ..Options::default() };
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, loose.clone());
    d.fs.set_ignore_case(true);
    d.fs.mkdir_all("/p/src/sub").unwrap();
    d.fs.write("/p/src/app.js", "1").unwrap();
    d.fs.write("/p/src/sub/x.js", "1").unwrap();
    d.watch(&["/p/src/App.js", "/p/src/Sub/x.js"]);

    d.fs.write("/p/src/other.js", "1").unwrap();
    d.fs.write("/p/src/app.js", "22").unwrap();
    d.fs.write("/p/src/sub/x.js", "22").unwrap();
    assert!(d.deliver(usize::MAX) > 0, "{mode:?}");
    // One watched path of the directory goes: the directory stays, for the other one.
    d.unwatch(&["/p/src/App.js"]);
    let mut events = taken(&mut d);
    events.append(&mut settled(&mut d));
    assert_eq!(events, ["U /p/src/Sub/x.js"], "{mode:?}");

    // The last one goes: the directory goes with it, and the scan has nothing to read.
    d.fs.write("/p/src/other.js", "22").unwrap();
    d.deliver(usize::MAX);
    let reads = d.fs.calls().lstat;
    d.unwatch(&["/p/src/Sub/x.js"]);
    assert_eq!(d.fs.calls().lstat, reads, "{mode:?}");
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    assert_eq!(os_watches(&d.fs), [], "{mode:?}");
    caught_up(&mut d);
  }
}

// ---- what `unwatch` leaves is what a new watcher has ----

/// A small project: watched directories in each other, an ignored directory, files.
fn project(fs: &SimFs) {
  for dir in ["/p/R/lib/deep", "/p/R/node_modules/pkg", "/p/S/sub", "/q/only"] {
    fs.mkdir_all(dir).unwrap();
  }
  for file in [
    "/p/top.js",
    "/p/R/a.js",
    "/p/R/lib/b.js",
    "/p/R/lib/deep/c.js",
    "/p/R/node_modules/pkg/x.js",
    "/p/S/s.js",
    "/p/S/sub/t.js",
    "/q/only/o.js",
  ] {
    fs.write(file, "1").unwrap();
  }
}

/// The paths that a case watches or takes back: directories, files, an ignored directory and
/// a file below it, a path below a file, and paths with nothing at them.
const PATHS: [&str; 16] = [
  "/p",
  "/p/top.js",
  "/p/R",
  "/p/R/a.js",
  "/p/R/lib",
  "/p/R/lib/b.js",
  "/p/R/lib/deep",
  "/p/R/lib/deep/c.js",
  "/p/R/node_modules",
  "/p/R/node_modules/pkg/x.js",
  "/p/R/a.js/below-a-file",
  "/p/R/gone/deep/missing.js",
  "/p/S",
  "/p/S/sub/t.js",
  "/q/only/o.js",
  "/q/not-there",
];

/// What a watcher is left with when everything has settled.
#[derive(PartialEq, Eq, Debug)]
struct Left {
  /// The files of the handler.
  known: Vec<PathBuf>,
  /// The files of the tree.
  stored: Vec<PathBuf>,
  os_watches: Vec<(String, Want)>,
  /// What one second costs when nothing happens: the `lstat`, `stat`, `read_dir` and `watch`
  /// calls of its polls.
  second: (u64, u64, u64, u64),
}

fn left<B: Deliver>(d: &mut Driver<B>) -> Left {
  caught_up(d);
  let mut stored: Vec<PathBuf> =
    d.engine.fingerprints().into_iter().map(|(path, _)| path).collect();
  stored.sort();
  let (before, end) = (d.fs.calls(), d.now + Duration::from_secs(1));
  while d.engine.deadline().is_some_and(|due| due <= end) {
    assert!(d.step());
  }
  let after = d.fs.calls();
  let second = (
    after.lstat - before.lstat,
    after.stat - before.stat,
    after.read_dir - before.read_dir,
    after.watch - before.watch,
  );
  Left { known: d.known.iter().cloned().collect(), stored, os_watches: os_watches(&d.fs), second }
}

/// One case: a watcher that watched `kept` and `taken` and took `taken` back, against a new
/// watcher that watched `kept` only.
fn same_as_a_new_watcher<B: Deliver>(make: &dyn Fn() -> Driver<B>, kept: &[&str], taken: &[&str]) {
  let case = format!("kept {kept:?}, taken back {taken:?}");
  let mut old = make();
  project(&old.fs);
  old.watch(&[kept, taken].concat());
  old.unwatch(taken);
  assert_eq!(pathwatch_sim::taken(&mut old), NONE, "{case}");
  let mut new = make();
  project(&new.fs);
  new.watch(kept);

  let (mut old, new) = (left(&mut old), left(&mut new));
  // A watch for one directory is not asked for less (rule 5 of the backend): it can tell
  // more than the watch of the new watcher, never less.
  let wider = old.os_watches.iter().zip(&new.os_watches).all(|(old, new)| old.1 >= new.1);
  assert!(wider, "{case}: {:?} against {:?}", old.os_watches, new.os_watches);
  old.os_watches.iter_mut().zip(&new.os_watches).for_each(|(old, new)| old.1 = new.1);
  assert_eq!(old, new, "{case}");
}

/// `unwatch` is the inverse of `watch`, for everything that can be seen from outside: the
/// files the handler knows, the files the tree holds, the place of every OS watch, and what a
/// second of polling costs. 200 cases on every backend: each watches a random part of
/// [`PATHS`] and takes a random part of that back in one call.
#[test]
fn unwatch_leaves_what_a_new_watcher_has() {
  // xorshift64: the same cases on every machine.
  let mut random = 0x2545_F491_4F6C_DD1D_u64;
  let mut next = move || {
    random ^= random << 13;
    random ^= random >> 7;
    random ^= random << 17;
    random
  };
  for _ in 0..200 {
    let (mut kept, mut taken) = (Vec::new(), Vec::new());
    for path in PATHS {
      match next() % 5 {
        0 | 1 => kept.push(path),
        2 | 3 => taken.push(path),
        _ => {}
      }
    }
    for mode in Mode::ALL {
      same_as_a_new_watcher(&|| Driver::sim(mode, ignore_node_modules()), &kept, &taken);
    }
    same_as_a_new_watcher(&|| Driver::<PollBackend>::poll(ignore_node_modules()), &kept, &taken);
  }
}

/// Rounds of `watch` and `unwatch` of a path, each shorter than the poll interval: the list of
/// the directories that the timer looks at does not grow with the rounds, with and without
/// another path that stays watched.
#[test]
fn rounds_of_watch_and_unwatch_leave_no_list_of_the_timer() {
  fn rounds<B: Deliver>(d: &mut Driver<B>, keep: bool) {
    d.fs.mkdir_all("/p/a/b/c").unwrap();
    d.fs.write("/p/a/b/c/f", "1").unwrap();
    d.fs.write("/p/other", "1").unwrap();
    if keep {
      d.watch(&["/p/other"]);
    }
    for _ in 0..2000 {
      d.watch(&["/p/a/b/c/f"]);
      d.unwatch(&["/p/a/b/c/f"]);
    }
    assert!(d.engine.polled_len() < 64, "{} keys on the list", d.engine.polled_len());
    caught_up(d);
  }
  for keep in [false, true] {
    rounds(&mut Driver::sim(Mode::Handle, Options::default()), keep);
    rounds(&mut Driver::sim(Mode::Inode, Options::default()), keep);
    rounds(&mut Driver::<PollBackend>::poll(Options::default()), keep);
  }
}

/// A flush that gave a watch back and queued no scan commits all the same: the backend moves
/// its watches at the commit, not for each key that goes.
#[test]
fn a_watch_that_is_given_back_is_followed_by_a_commit() {
  on_modes(&Mode::ALL, &Options::default(), |d, on| {
    d.fs().mkdir_all("/p/a").unwrap();
    d.fs().mkdir_all("/p/b").unwrap();
    assert_eq!(d.watch(&["/p/a", "/p/b"]), NONE, "{on}");
    let before = d.fs().calls();
    assert_eq!(d.unwatch(&["/p/a"]), NONE, "{on}");
    let after = d.fs().calls();
    assert!(after.unwatch > before.unwatch, "{on}: no watch was given back");
    assert!(after.commit > before.commit, "{on}: no commit followed");
    d.caught_up();
  });
}

/// The directory above lists a name on the way to a watched path under another spelling, and
/// the look that finds that out is still to come when the path is taken back: the disk did not
/// answer. The directory below the name stayed, as an entry through that name. When the look
/// comes, the name is no entry: what was kept through it goes, with a `Delete` for each file
/// the handler knows, and the directory reports the files under its own spelling.
#[test]
fn unwatch_below_a_name_whose_spelling_the_directory_above_has_not_told_yet() {
  on_all(&Options { loose_spelling: true, ..Options::default() }, |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/p/t/x/s").unwrap();
    d.fs().write("/p/t/x/s/f", "1").unwrap();
    assert_eq!(d.watch(&["/p/t/X/S", "/p/t/X/other"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/t/X/S/f"], "{on}");

    d.fs().set_offline(Some(std::io::ErrorKind::TimedOut));
    assert_eq!(d.watch(&["/p/t"]), NONE, "{on}");
    d.fs().set_offline(None);
    let mut events = d.unwatch(&["/p/t/X/S"]);
    events.append(&mut d.settled());
    assert_eq!(sorted(events), ["C /p/t/x/s/f", "D /p/t/X/S/f"], "{on}");
    d.caught_up();
  });
}

/// `unwatch` of a path below a chain of missing names: the directory that stays wants less now,
/// since its other watched path is a directory, also when the directory that held the path went
/// with the chain.
#[test]
fn unwatch_narrows_the_directory_that_stays() {
  let mut d = Driver::sim(Mode::Inode, Options::default());
  d.fs.mkdir_all("/a/b/x/y").unwrap();
  d.watch(&["/a/b/c/d", "/a/b/x/y"]);
  // `c` is missing: `b` tells its contents.
  assert!(os_watches(&d.fs).contains(&at("/a/b", Contents)));
  d.unwatch(&["/a/b/c/d"]);
  assert!(os_watches(&d.fs).contains(&at("/a/b", Names)));
  caught_up(&mut d);
}
