//! The baseline of `watch`: what exists when `watch` returns is known without an event, and what
//! an older watched path had to report is still reported.
//! A story makes the disk, calls `watch`, changes the disk and compares the events that come
//! out (`C`, `U`, `D` and the path).

#![cfg(unix)]

use std::{io, path::Path, time::Duration};

use pathwatch::{ErrorKind, engine::Options, poll::PollBackend};
use pathwatch_sim::{
  Deliver, Driver, Mode, NONE, Sim, SimBackend, SimFs, caught_up, flush, ignore_node_modules,
  known, on_all, on_modes, settled, sorted, taken,
};

fn inode() -> Driver<SimBackend> {
  Driver::sim(Mode::Inode, Options::default())
}

/// Runs `test` on the three simulated backends and on the poll backend. The test makes as many
/// drivers as it needs.
fn on_every_backend(test: impl Fn(&mut dyn FnMut() -> Box<dyn Sim>)) {
  for mode in Mode::ALL {
    test(&mut || Box::new(Driver::sim(mode, Options::default())));
  }
  test(&mut || Box::new(Driver::poll(Options::default())));
}

#[test]
fn what_exists_at_watch_is_not_reported() {
  on_every_backend(|make| {
    let mut d = make();
    d.fs().mkdir_all("/p/dir/deep").unwrap();
    d.fs().write("/p/file.js", "1").unwrap();
    d.fs().write("/p/dir/a", "1").unwrap();
    d.fs().write("/p/dir/deep/b", "1").unwrap();
    d.fs().write("/p/unwatched.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/file.js", "/p/dir", "/p/missing.js", "/p/no/such/dir"]), NONE);
    assert_eq!(d.known(), ["/p/dir/a", "/p/dir/deep/b", "/p/file.js"]);
    assert_eq!(d.settled(), NONE);
    d.caught_up();
  });
}

#[test]
fn a_file_and_its_directory_in_one_batch() {
  on_every_backend(|make| {
    for paths in [["/p/d", "/p/d/f.js"], ["/p/d/f.js", "/p/d"]] {
      let mut d = make();
      d.fs().mkdir_all("/p/d").unwrap();
      d.fs().write("/p/d/f.js", "1").unwrap();
      d.fs().write("/p/d/g.js", "1").unwrap();
      assert_eq!(d.watch(&paths), NONE);
      assert_eq!(d.known(), ["/p/d/f.js", "/p/d/g.js"]);
      // The file has one home: one event for a change.
      d.fs().write("/p/d/f.js", "12").unwrap();
      assert_eq!(d.settled(), ["U /p/d/f.js"]);
      d.caught_up();
    }
  });
}

/// A file that a watched directory has to report is reported, also when it becomes a watched
/// path before the flush that would have found it.
#[test]
fn add_does_not_swallow_a_create_that_was_due() {
  let mut d = inode();
  d.fs.mkdir_all("/p/d").unwrap();
  d.watch(&["/p/d"]);

  d.fs.write("/p/d/f.js", "1").unwrap();
  d.watch(&["/p/d/f.js"]);
  assert_eq!(taken(&mut d), ["C /p/d/f.js"]);
  // The notification comes after it. It says "written", which costs one more update.
  assert_eq!(settled(&mut d), ["U /p/d/f.js"]);
  caught_up(&mut d);
}

#[test]
fn add_does_not_swallow_an_update_that_was_due() {
  let mut d = Driver::sim(Mode::Prefix, Options::default());
  d.fs.mkdir_all("/p/src").unwrap();
  d.fs.write("/p/src/a.vue", "1").unwrap();
  d.watch(&["/p/src"]);

  d.fs.write("/p/src/a.vue", "12").unwrap();
  d.watch(&["/p/src/a.vue", "/p/src"]);
  assert_eq!(taken(&mut d), ["U /p/src/a.vue"]);
  assert_eq!(settled(&mut d), NONE);
  caught_up(&mut d);
}

#[test]
fn add_meets_a_pending_delete() {
  let mut d = inode();
  d.fs.mkdir_all("/p/r").unwrap();
  d.fs.write("/p/r/a.js", "1").unwrap();
  d.watch(&["/p/r"]);
  d.fs.remove("/p/r/a.js").unwrap();
  flush(&mut d);
  assert!(d.engine.pending_deletes().contains(Path::new("/p/r/a.js")));

  // A slow "remove, then write" save, and the path becomes a watched path in between.
  d.fs.write("/p/r/a.js", "2").unwrap();
  d.watch(&["/p/r/a.js"]);
  assert_eq!(taken(&mut d), ["U /p/r/a.js"]);
  assert!(d.engine.pending_deletes().is_empty());
  caught_up(&mut d);
}

/// A watched file that appeared is reported when its directory becomes a watched path before
/// the flush. The other files of the directory are new to the handler and are the baseline.
#[test]
fn the_listing_of_add_reports_an_older_watched_path() {
  let mut d = inode();
  d.fs.mkdir_all("/p/r").unwrap();
  d.watch(&["/p/r/a.js"]);

  d.fs.write("/p/r/a.js", "1").unwrap();
  d.fs.write("/p/r/b.js", "1").unwrap();
  d.watch(&["/p/r"]);
  assert_eq!(taken(&mut d), ["C /p/r/a.js"]);
  assert_eq!(known(&d), ["/p/r/a.js", "/p/r/b.js"]);
  caught_up(&mut d);
}

/// A watched directory that was missing appears, and before the flush a file in it becomes a
/// watched path: the directory already had to report that file.
#[test]
fn an_older_watched_directory_appears_during_add() {
  let mut d = inode();
  d.fs.mkdir("/p").unwrap();
  d.watch(&["/p/d"]);

  d.fs.mkdir("/p/d").unwrap();
  d.fs.write("/p/d/f.js", "1").unwrap();
  d.fs.write("/p/d/g.js", "1").unwrap();
  d.watch(&["/p/d/f.js"]);
  assert_eq!(taken(&mut d), ["C /p/d/f.js", "C /p/d/g.js"]);
  assert_eq!(settled(&mut d), NONE);
  caught_up(&mut d);
}

/// A directory becomes a watched path while nodes for watched files exist below it: they all
/// become complete, down to the last level.
#[test]
fn a_directory_above_watched_files_becomes_a_watched_path() {
  on_every_backend(|make| {
    let mut d = make();
    d.fs().mkdir_all("/p/r/lib/sub").unwrap();
    d.fs().write("/p/r/lib/a.js", "1").unwrap();
    d.fs().write("/p/r/lib/old.js", "1").unwrap();
    d.fs().write("/p/r/lib/sub/deep.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/r/lib/a.js", "/p/r/lib/sub/missing.js"]), NONE);
    assert_eq!(d.known(), ["/p/r/lib/a.js"]);

    assert_eq!(d.watch(&["/p/r"]), NONE);
    assert_eq!(d.known(), ["/p/r/lib/a.js", "/p/r/lib/old.js", "/p/r/lib/sub/deep.js"]);

    d.fs().write("/p/r/lib/b.js", "1").unwrap();
    d.fs().write("/p/r/lib/old.js", "12").unwrap();
    d.fs().write("/p/r/lib/sub/c.js", "1").unwrap();
    d.fs().mkdir("/p/r/lib/sub/new").unwrap();
    d.fs().write("/p/r/lib/sub/new/d.js", "1").unwrap();
    assert_eq!(
      d.settled(),
      ["C /p/r/lib/b.js", "U /p/r/lib/old.js", "C /p/r/lib/sub/c.js", "C /p/r/lib/sub/new/d.js"]
    );
    d.caught_up();
  });
}

/// A directory on the way to a watched file lies inside a watched directory and is missing:
/// when it appears it is a full part of the watched directory, with all of its files.
#[test]
fn a_missing_ancestor_inside_a_watched_directory() {
  on_every_backend(|make| {
    let mut d = make();
    d.fs().mkdir_all("/p/r").unwrap();
    assert_eq!(d.watch(&["/p/r", "/p/r/gen/x.js"]), NONE);

    d.fs().mkdir("/p/r/gen").unwrap();
    d.fs().write("/p/r/gen/x.js", "1").unwrap();
    d.fs().write("/p/r/gen/y.js", "1").unwrap();
    assert_eq!(d.settled(), ["C /p/r/gen/x.js", "C /p/r/gen/y.js"]);
    d.caught_up();
  });
}

// ---- what was due before `watch`, and what was not ----
//
// Each of these is a sequence in which a rule of this kind once swallowed an event, or told one
// too many, in this design or in one of the five that were drawn up next to it.

/// A watched file below a directory that is missing: nobody could look at it, and when `watch`
/// returned it was known as missing. Then the directory and the file appear, and a path above
/// them is added before the flush: the `Create` of the file is due. The other file of the new
/// directory was no business of the watcher until that `watch`: it is the baseline.
#[test]
fn a_create_below_a_missing_directory_is_due() {
  on_every_backend(|make| {
    for above in ["/p", "/p/gen"] {
      let mut d = make();
      d.fs().mkdir("/p").unwrap();
      assert_eq!(d.watch(&["/p/gen/x.js"]), NONE);

      d.fs().mkdir("/p/gen").unwrap();
      d.fs().write("/p/gen/x.js", "1").unwrap();
      d.fs().write("/p/gen/y.js", "1").unwrap();
      assert_eq!(d.watch(&[above]), ["C /p/gen/x.js"], "{above}");
      assert_eq!(d.known(), ["/p/gen/x.js", "/p/gen/y.js"], "{above}");
      // The notifications come after it. One that says "written" costs an update.
      assert!(d.settled().iter().all(|event| event.starts_with("U /p/gen/")), "{above}");
      d.caught_up();
    }
  });
}

/// A new directory with a file appears in a watched directory, and it becomes a watched path
/// before the flush: the watched directory above already had to report the file.
#[test]
fn a_new_directory_in_a_watched_directory_becomes_a_watched_path() {
  on_every_backend(|make| {
    let mut d = make();
    d.fs().mkdir_all("/p/src").unwrap();
    assert_eq!(d.watch(&["/p/src"]), NONE);

    d.fs().mkdir("/p/src/pages").unwrap();
    d.fs().write("/p/src/pages/x.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/src/pages"]), ["C /p/src/pages/x.js"]);
    assert!(d.settled().iter().all(|event| event == "U /p/src/pages/x.js"));
    d.caught_up();
  });
}

/// A file moves to another name in a watched directory, and the new name becomes a watched path
/// while the `Delete` of the old name still waits. The `Create` is not swallowed, and nothing
/// holds it back.
#[test]
fn add_while_a_delete_waits() {
  on_every_backend(|make| {
    let mut d = make();
    d.fs().mkdir_all("/p/d").unwrap();
    d.fs().write("/p/d/a.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/d"]), NONE);

    d.fs().rename("/p/d/a.js", "/p/d/b.js").unwrap();
    assert_eq!(d.watch(&["/p/d/b.js"]), ["C /p/d/b.js"]);
    let mut rest = d.settled();
    rest.retain(|event| event != "U /p/d/b.js");
    assert_eq!(rest, ["D /p/d/a.js"]);
    assert_eq!(d.known(), ["/p/d/b.js"]);
    d.caught_up();
  });
}

/// A directory and a file in a directory below it, in one batch and in both orders: the
/// directory in between is a full part of the watched directory.
#[test]
fn a_directory_and_a_file_two_levels_below_it_in_one_batch() {
  on_every_backend(|make| {
    for paths in [["/p/R", "/p/R/lib/a.js"], ["/p/R/lib/a.js", "/p/R"]] {
      let mut d = make();
      d.fs().mkdir_all("/p/R/lib").unwrap();
      d.fs().write("/p/R/top.js", "1").unwrap();
      d.fs().write("/p/R/lib/a.js", "1").unwrap();
      d.fs().write("/p/R/lib/other.js", "1").unwrap();
      assert_eq!(d.watch(&paths), NONE);
      assert_eq!(d.known(), ["/p/R/lib/a.js", "/p/R/lib/other.js", "/p/R/top.js"]);

      d.fs().write("/p/R/lib/b.js", "1").unwrap();
      d.fs().write("/p/R/lib/other.js", "22").unwrap();
      assert_eq!(d.settled(), ["C /p/R/lib/b.js", "U /p/R/lib/other.js"]);
      d.caught_up();
    }
  });
}

/// A directory inside an ignored directory becomes a watched path. The watched directory above
/// never looked into the ignored one, so what is found there is the baseline.
#[test]
fn a_watched_directory_inside_an_ignored_directory() {
  on_all(&ignore_node_modules(), |d, on| {
    d.fs().mkdir_all("/p/node_modules/pkg/lib").unwrap();
    d.fs().write("/p/a.js", "1").unwrap();
    d.fs().write("/p/node_modules/pkg/index.js", "1").unwrap();
    d.fs().write("/p/node_modules/pkg/lib/a.js", "1").unwrap();
    assert_eq!(d.watch(&["/p"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/a.js"], "{on}");

    assert_eq!(d.watch(&["/p/node_modules/pkg"]), NONE, "{on}");
    assert_eq!(
      d.known(),
      ["/p/a.js", "/p/node_modules/pkg/index.js", "/p/node_modules/pkg/lib/a.js"],
      "{on}"
    );
    d.fs().write("/p/node_modules/pkg/lib/new.js", "1").unwrap();
    d.fs().write("/p/node_modules/other.js", "1").unwrap();
    assert_eq!(d.settled(), ["C /p/node_modules/pkg/lib/new.js"], "{on}");
    d.caught_up();
  });
}

/// A file sits where a directory on the way to a watched path should be. It is nothing to
/// report, and the tree has "nothing that counts" there. When that path becomes a watched path
/// itself, the same file counts: it is the baseline, not a `Create`.
#[test]
fn a_file_on_the_way_becomes_a_watched_path() {
  on_every_backend(|make| {
    let mut d = make();
    assert_eq!(d.watch(&["/a/a/b/c"]), NONE);
    d.fs().write("/a", "1").unwrap();
    assert_eq!(d.settled(), NONE);

    assert_eq!(d.watch(&["/a"]), NONE);
    assert_eq!(d.known(), ["/a"]);
    d.fs().write("/a", "22").unwrap();
    assert_eq!(d.settled(), ["U /a"]);
    d.caught_up();
  });
}

/// The same when the directory above becomes a watched path: the file that sat on the way to
/// an older watched path is one of its files now, and part of its baseline.
#[test]
fn a_file_on_the_way_inside_a_new_watched_directory() {
  on_every_backend(|make| {
    let mut d = make();
    d.fs().mkdir("/p").unwrap();
    d.fs().write("/p/b", "a file").unwrap();
    assert_eq!(d.watch(&["/p/b/c/d"]), NONE);
    assert_eq!(d.settled(), NONE);

    assert_eq!(d.watch(&["/p"]), NONE);
    assert_eq!(d.known(), ["/p/b"]);
    assert_eq!(d.settled(), NONE);
    d.caught_up();
  });
}

/// A directory becomes a watched path while the disk does not answer, so the look has to be
/// tried again. Before that happens, a file in it becomes a watched path: the directory had to
/// report it since the first `watch`, and it is reported, with everything else in the directory.
#[test]
fn a_look_that_waits_and_a_new_watched_path_below_it() {
  let mut d = Driver::poll(Options::default());
  d.fs.mkdir("/b").unwrap();
  d.fs.write("/b/a", "1").unwrap();
  d.fs.write("/b/z", "1").unwrap();
  d.watch(&["/b/c/x"]);

  d.fs.set_offline(Some(io::ErrorKind::TimedOut));
  d.watch(&["/b"]);
  assert!(d.known.is_empty());
  d.fs.set_offline(None);
  d.watch(&["/b/a"]);
  assert_eq!(taken(&mut d), ["C /b/a", "C /b/z"]);
  assert_eq!(settled(&mut d), NONE);
  caught_up(&mut d);
}

/// A directory above a watched file is found missing, so it is held. It comes back, and before
/// any notification says so, another file in it becomes a watched path. `watch` looks at the disk
/// all the same: its check starts at the held directory, which is back.
#[test]
fn add_below_a_held_directory() {
  on_every_backend(|make| {
    let mut d = make();
    d.fs().mkdir_all("/c/b").unwrap();
    d.fs().mkdir("/q").unwrap();
    d.fs().write("/c/b/f.js", "1").unwrap();
    assert_eq!(d.watch(&["/c/b/f.js"]), NONE);

    d.fs().rename("/c", "/q/c").unwrap();
    assert_eq!(d.window(), NONE);
    assert_eq!(d.pending_deletes(), 1);
    d.fs().rename("/q/c", "/c").unwrap();
    d.fs().write("/c/b/g.js", "1").unwrap();
    assert_eq!(d.watch(&["/c/b/g.js"]), NONE);
    assert_eq!(d.known(), ["/c/b/f.js", "/c/b/g.js"]);
    assert_eq!(d.pending_deletes(), 0);
    // The notification of the write comes after the `watch`.
    assert!(d.settled().iter().all(|event| event == "U /c/b/g.js"));
    d.caught_up();
  });
}

/// A file goes away at any moment of the `watch` that takes it into the baseline. Whatever the
/// moment: the handler never hears of a file it does not know (the driver checks every event
/// against what the handler knows), and when all is over it knows no file.
#[test]
fn a_file_goes_away_while_add_takes_the_baseline() {
  fn run<B: Deliver>(make: impl Fn() -> Driver<B>, on: &str) {
    let prepare = || {
      let d = make();
      d.fs.mkdir_all("/p/r/sub").unwrap();
      d.fs.write("/p/r/a", "1").unwrap();
      d.fs.write("/p/r/sub/b", "1").unwrap();
      d
    };
    let mut d = prepare();
    d.watch(&["/p/r"]);
    let calls = d.fs.calls().total();
    caught_up(&mut d);

    for moment in 0..calls {
      let mut d = prepare();
      d.fs.at_call(moment, |fs| {
        fs.remove("/p/r/a").unwrap();
        fs.remove_all("/p/r/sub").unwrap();
      });
      d.watch(&["/p/r"]);
      assert_eq!(d.fs.traps_run(), 1, "{on}, call {moment}");
      let mut events = taken(&mut d);
      events.append(&mut settled(&mut d));
      // A file that `watch` saw before it went is in the baseline, and then its `Delete` comes.
      assert!(events.iter().all(|event| event.starts_with("D /p/r/")), "{on}: {events:?}");
      assert!(d.known.is_empty(), "{on}, call {moment}");
      caught_up(&mut d);
    }
  }
  for delete_delay in [Duration::ZERO, Duration::from_millis(100)] {
    let options = Options { delete_delay, ..Options::default() };
    for mode in Mode::ALL {
      run(|| Driver::sim(mode, options.clone()), &format!("{mode:?}, delay {delete_delay:?}"));
    }
    run(|| Driver::poll(options.clone()), &format!("Poll, delay {delete_delay:?}"));
  }
}

// ---- a change of the disk in the middle of `watch` ----

/// An older watched path, an `watch` call, and a change of the disk that makes an event due.
struct Due {
  name: &'static str,
  before: fn(&SimFs),
  /// The older watched path.
  first: &'static str,
  /// What goes away after that, so that a delete is pending when the `watch` call comes.
  goes: Option<fn(&SimFs)>,
  /// The path of the `watch` call, and the change that runs in the middle of it.
  second: &'static str,
  change: fn(&SimFs),
  /// The event that is due, and the files the handler knows in the end.
  due: &'static str,
  known: &'static [&'static str],
}

/// Every story is one from above, with its change moved into the call.
const DUE: [Due; 5] = [
  Due {
    name: "a new file in an older watched directory",
    before: |fs| fs.mkdir_all("/p/d").unwrap(),
    first: "/p/d",
    goes: None,
    second: "/p/d/f.js",
    change: |fs| fs.write("/p/d/f.js", "1").unwrap(),
    due: "C /p/d/f.js",
    known: &["/p/d/f.js"],
  },
  Due {
    name: "an older watched file, and its directory becomes a watched path",
    before: |fs| fs.mkdir_all("/p/r").unwrap(),
    first: "/p/r/a.js",
    goes: None,
    second: "/p/r",
    change: |fs| {
      fs.write("/p/r/a.js", "1").unwrap();
      fs.write("/p/r/b.js", "1").unwrap();
    },
    due: "C /p/r/a.js",
    known: &["/p/r/a.js", "/p/r/b.js"],
  },
  Due {
    name: "an older watched file below a directory that is missing",
    before: |fs| fs.mkdir("/p").unwrap(),
    first: "/p/gen/x.js",
    goes: None,
    second: "/p",
    change: |fs| {
      fs.mkdir("/p/gen").unwrap();
      fs.write("/p/gen/x.js", "1").unwrap();
      fs.write("/p/gen/y.js", "1").unwrap();
    },
    due: "C /p/gen/x.js",
    known: &["/p/gen/x.js", "/p/gen/y.js"],
  },
  Due {
    name: "a held file comes back",
    before: |fs| {
      fs.mkdir_all("/p/d").unwrap();
      fs.write("/p/d/f.js", "1").unwrap();
    },
    first: "/p/d",
    goes: Some(|fs| fs.remove("/p/d/f.js").unwrap()),
    second: "/p/d/f.js",
    change: |fs| fs.write("/p/d/f.js", "22").unwrap(),
    due: "U /p/d/f.js",
    known: &["/p/d/f.js"],
  },
  Due {
    name: "a held directory comes back, and a path above it becomes a watched path",
    before: |fs| {
      fs.mkdir_all("/p/gen").unwrap();
      fs.mkdir("/q").unwrap();
      fs.write("/p/gen/x.js", "1").unwrap();
      fs.write("/p/gen/y.js", "1").unwrap();
    },
    first: "/p/gen/x.js",
    goes: Some(|fs| fs.rename("/p/gen", "/q/gen").unwrap()),
    second: "/p",
    change: |fs| {
      fs.write("/q/gen/x.js", "22").unwrap();
      fs.rename("/q/gen", "/p/gen").unwrap();
    },
    due: "U /p/gen/x.js",
    known: &["/p/gen/x.js", "/p/gen/y.js"],
  },
];

fn due_at_every_moment<B: Deliver>(make: impl Fn() -> Driver<B>, story: &Due, on: &str) {
  let prepare = || {
    let mut d = make();
    (story.before)(&d.fs);
    d.watch(&[story.first]);
    if let Some(goes) = story.goes {
      goes(&d.fs);
      flush(&mut d);
      assert_eq!(d.engine.pending_deletes().len(), 1, "{on}, {}", story.name);
    }
    d
  };
  let mut d = prepare();
  let before = d.fs.calls().total();
  d.watch(&[story.second]);
  let calls = d.fs.calls().total() - before;
  caught_up(&mut d);

  // The last moment is after the call.
  for moment in 0..=calls {
    let mut d = prepare();
    d.fs.at_call(moment, story.change);
    d.watch(&[story.second]);
    if d.fs.traps_run() == 0 {
      d.fs.clear_traps();
      (story.change)(&d.fs);
    }
    let mut events = taken(&mut d);
    events.append(&mut settled(&mut d));
    let on = format!("{on}, {}, call {moment}: {events:?}", story.name);
    // A "written" notification that comes after the call may cost one more update.
    let told = events.iter().filter(|event| *event == story.due).count();
    assert!(told == 1 || (told > 1 && story.due.starts_with('U')), "{on}");
    assert!(events.iter().all(|event| !event.starts_with('D')), "{on}");
    assert_eq!(known(&d), story.known, "{on}");
    caught_up(&mut d);
  }
}

/// The disk changes at any moment of an `watch` call: what an older watched path had to report
/// is reported all the same, in that call or after it, and no file is deleted.
///
/// The fuzzer cannot check this. When the disk changes inside a call, it believes what the
/// engine says it left unsaid.
#[test]
fn what_was_due_is_told_whatever_the_moment_of_add() {
  for story in &DUE {
    for mode in Mode::ALL {
      due_at_every_moment(|| Driver::sim(mode, Options::default()), story, &format!("{mode:?}"));
    }
    due_at_every_moment(|| Driver::poll(Options::default()), story, "Poll");
  }
}

#[test]
fn bad_paths_are_told_once_and_the_rest_is_registered() {
  let mut d = inode();
  d.fs.mkdir("/p").unwrap();
  d.fs.write("/p/a.js", "1").unwrap();
  d.watch(&["relative/a.js", "/p/../x", "/", "", "/p/a.js", "/p//a.js/", "/p/./a.js"]);
  assert_eq!(d.errors.len(), 1);
  assert_eq!(d.errors[0].kind(), ErrorKind::InvalidPath);
  assert_eq!(d.errors[0].path(), Some(Path::new("relative/a.js")));

  // The three spellings are one watched path.
  d.fs.write("/p/a.js", "12").unwrap();
  assert_eq!(settled(&mut d), ["U /p/a.js"]);
  let stats = d.fs.calls();
  d.watch(&["/p/a.js"]);
  assert_eq!(d.fs.calls(), stats);
  caught_up(&mut d);
}

/// One batch checks every directory once and every file once. Before a directory that holds
/// watched paths is asked from the backend, one of them is looked at for its kind alone: a
/// directory with a file among its watched paths has to tell more.
#[test]
fn a_batch_costs_one_check_for_a_name() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    let mut paths = Vec::new();
    for dir in 0..20 {
      d.fs.mkdir_all(format!("/p/src/d{dir}")).unwrap();
      for file in 0..10 {
        let path = format!("/p/src/d{dir}/f{file}.js");
        d.fs.write(&path, "1").unwrap();
        paths.push(path);
      }
    }
    d.watch(&paths);
    let calls = d.fs.calls();
    // 200 files, 20 directories, `/p/src`, `/p` and `/`. And one look for the kind in each of
    // the 20 directories that hold watched paths.
    assert_eq!((calls.lstat, calls.stat, calls.read_dir, calls.hash), (243, 0, 0, 200), "{mode:?}");
    assert_eq!(d.known.len(), 200);

    // Half of them again, and some new ones in a known directory: only the new names are
    // checked. Where the backend does not watch `/p/src`, that directory checks `d3` with them.
    let mut again: Vec<String> = paths.iter().step_by(2).cloned().collect();
    for file in 0..5 {
      again.push(format!("/p/src/d3/new{file}.js"));
    }
    d.watch(&again);
    let polled_parent = u64::from(mode != Mode::Inode);
    let calls = d.fs.calls();
    assert_eq!((calls.lstat, calls.stat), (248 + polled_parent, 0), "{mode:?}");
    caught_up(&mut d);
  }
}

// ---- cases of the design review ----

/// A bad path is skipped and reported, the rest of the batch is
/// registered, and the cleaned text is the registered path.
#[test]
fn a_bad_path_is_skipped_and_the_rest_is_registered() {
  let mut d = Driver::sim(Mode::Inode, Options::default());
  d.fs.mkdir_all("/p/src").unwrap();
  d.fs.mkdir_all("/p/lib").unwrap();
  d.watch(&["src/a.js", "/p/src/../lib/a.js", "/", "/p/../..", "/p/src/", "/p//src", "/p/./src"]);
  assert_eq!(Sim::errors(&d), [ErrorKind::InvalidPath]);
  d.fs.write("/p/src/a.js", "1").unwrap();
  d.fs.write("/p/lib/a.js", "1").unwrap();
  assert_eq!(Sim::settled(&mut d), ["C /p/lib/a.js", "C /p/src/a.js"]);
  caught_up(&mut d);
}

/// What a batch costs. One check for a name, one watch for a directory, and
/// one look for the kind of a watched path in each directory that holds some.
#[test]
fn the_cost_of_a_batch() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    let mut paths = Vec::new();
    for dir in 0..30 {
      d.fs.mkdir_all(format!("/p/src/d{dir}")).unwrap();
      for file in 0..10 {
        let path = format!("/p/src/d{dir}/f{file}.js");
        d.fs.write(&path, "1").unwrap();
        paths.push(path);
      }
    }
    d.watch(&paths);
    let calls = d.fs.calls();
    // 300 files and 33 directories: 30, `/p/src`, `/p` and `/`. And 30 looks for the kind.
    assert_eq!((calls.lstat, calls.stat, calls.read_dir), (363, 0, 0), "{mode:?}");
    // One watch call for a directory: 30, `/p/src`, `/p` and `/`.
    assert_eq!(calls.watch, 33, "{mode:?}");
    assert_eq!(d.known.len(), 300, "{mode:?}");

    // The same batch again costs nothing at all.
    d.watch(&paths);
    assert_eq!(d.fs.calls(), calls, "{mode:?}");
    caught_up(&mut d);
  }
}

/// What exists when `watch` returns is the baseline, also when the
/// caller read an older state. Only a change after `watch` is reported.
#[test]
fn a_change_before_add_is_the_baseline() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir("/src").unwrap();
    d.fs().write("/src/a.js", "1").unwrap();
    // The caller read "1"; the user saves before the path is watched.
    d.fs().write("/src/a.js", "22").unwrap();
    assert_eq!(d.watch(&["/src/a.js"]), NONE, "{on}");
    assert_eq!(d.settled(), NONE, "{on}");
    d.fs().write("/src/a.js", "333").unwrap();
    assert_eq!(d.settled(), ["U /src/a.js"], "{on}");
    d.caught_up();
  });
}

/// The baseline of `watch` is only what no older watched path had to report.
#[test]
fn the_baseline_keeps_what_was_due() {
  on_all(&Options::default(), |d, on| {
    // Case 1: a new file in a watched directory becomes a watched path before the flush.
    d.fs().mkdir_all("/p/d").unwrap();
    assert_eq!(d.watch(&["/p/d"]), NONE, "{on}");
    d.fs().write("/p/d/f.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/d/f.js"]), ["C /p/d/f.js"], "{on}");
    // The notification that comes after it may cost one more update, nothing else.
    assert!(d.settled().iter().all(|event| event == "U /p/d/f.js"), "{on}");

    // Case 2: a slow "remove, then write" save, with the `watch` in between.
    d.fs().mkdir_all("/q/R").unwrap();
    d.fs().write("/q/R/a.js", "1").unwrap();
    assert_eq!(d.watch(&["/q/R"]), NONE, "{on}");
    d.fs().remove("/q/R/a.js").unwrap();
    assert_eq!(d.window(), NONE, "{on}");
    assert_eq!(d.pending_deletes(), 1, "{on}");
    d.fs().write("/q/R/a.js", "22").unwrap();
    assert_eq!(d.watch(&["/q/R/a.js"]), ["U /q/R/a.js"], "{on}");
    assert!(d.settled().iter().all(|event| event == "U /q/R/a.js"), "{on}");
    assert_eq!(d.known(), ["/p/d/f.js", "/q/R/a.js"], "{on}");

    // Case 3: a missing watched file appears, and its directory is added before the flush.
    d.fs().mkdir_all("/s/R").unwrap();
    d.fs().write("/s/R/old.js", "1").unwrap();
    assert_eq!(d.watch(&["/s/R/a.js"]), NONE, "{on}");
    d.fs().write("/s/R/a.js", "1").unwrap();
    assert_eq!(d.watch(&["/s/R"]), ["C /s/R/a.js"], "{on}");
    assert_eq!(d.known(), ["/p/d/f.js", "/q/R/a.js", "/s/R/a.js", "/s/R/old.js"], "{on}");
    d.caught_up();
  });
}

/// A watch that is only live after `commit`. The baseline holds over all rounds of the
/// `watch`, and what is scanned after the commit is not reported.
#[test]
fn the_baseline_holds_over_the_rounds() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/content/deep/deeper").unwrap();
    d.fs().mkdir_all("/q/other").unwrap();
    d.fs().write("/p/content/a", "1").unwrap();
    d.fs().write("/p/content/deep/deeper/b", "1").unwrap();
    d.fs().write("/q/other/c", "1").unwrap();
    assert_eq!(d.watch(&["/p/content", "/q/other/c", "/q/missing/chain/x"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/content/a", "/p/content/deep/deeper/b", "/q/other/c"], "{on}");
    assert_eq!(d.settled(), NONE, "{on}");
    d.fs().write("/p/content/deep/deeper/b", "22").unwrap();
    d.fs().write("/q/other/c", "22").unwrap();
    assert_eq!(d.window(), ["U /p/content/deep/deeper/b", "U /q/other/c"], "{on}");
    d.caught_up();
  });
}

/// A hint and an `watch` for the same name in the same window. The `Create` that the
/// watched directory owes is not swallowed by the baseline of the new watched path.
#[test]
fn a_hint_and_an_add_for_one_name() {
  on_modes(&Mode::ALL, &Options::default(), |d, on| {
    d.fs().mkdir_all("/p/src").unwrap();
    assert_eq!(d.watch(&["/p/src"]), NONE, "{on}");
    d.fs().write("/p/src/x.js", "1").unwrap();
    // The hint waits as a mark when the `watch` comes.
    let mut events = d.watch(&["/p/src/x.js", "/p/src/missing.js"]);
    events.append(&mut d.settled());
    assert_eq!(events.first().map(String::as_str), Some("C /p/src/x.js"), "{on}");
    assert!(events[1..].iter().all(|event| event == "U /p/src/x.js"), "{on}: {events:?}");
    d.caught_up();
  });
}

fn prepare_c2(d: &mut dyn Sim) {
  d.fs().mkdir_all("/proj/src").unwrap();
  d.fs().mkdir_all("/proj/other").unwrap();
  d.fs().mkdir("/fresh").unwrap();
  d.fs().write("/proj/src/a.js", "1").unwrap();
  d.fs().write("/proj/other/new.js", "1").unwrap();
  assert_eq!(d.watch(&["/proj/src"]), NONE);
}

/// How many calls to the disk and to the backend one `watch` of `path` makes in this story.
fn calls_of_add(make: &dyn Fn() -> Box<dyn Sim>, path: &str) -> u64 {
  let mut d = make();
  prepare_c2(&mut *d);
  let before = d.fs().calls().total();
  d.watch(&[path]);
  let calls = d.fs().calls().total() - before;
  d.caught_up();
  calls
}

/// No gap. A write to a file that is already watched, at any moment while a later `watch`
/// asks for its watches, commits and scans, is reported.
#[test]
fn a_later_add_opens_no_gap() {
  type Make = Box<dyn Fn() -> Box<dyn Sim>>;
  let makes: [(&str, Make); 4] = [
    ("Inode", Box::new(|| Box::new(Driver::sim(Mode::Inode, Options::default())))),
    ("Prefix", Box::new(|| Box::new(Driver::sim(Mode::Prefix, Options::default())))),
    ("Handle", Box::new(|| Box::new(Driver::sim(Mode::Handle, Options::default())))),
    ("Poll", Box::new(|| Box::new(Driver::<PollBackend>::poll(Options::default())))),
  ];
  for (on, make) in &makes {
    let calls = calls_of_add(make.as_ref(), "/proj/other/new.js");
    assert!(calls >= 3, "{on}");
    for moment in 0..calls {
      let mut d = make();
      prepare_c2(&mut *d);
      d.fs().at_call(moment, |fs| fs.write("/proj/src/a.js", "changed").unwrap());
      let mut events = d.watch(&["/proj/other/new.js"]);
      assert_eq!(d.fs().traps_run(), 1, "{on}, call {moment}");
      events.append(&mut d.settled());
      assert!(!events.is_empty(), "{on}: the write before call {moment} of add was lost");
      assert!(events.iter().all(|event| event == "U /proj/src/a.js"), "{on}: {events:?}");
      d.caught_up();
    }

    // A watched file that appears at any moment of its own `watch`, in a directory which had no
    // watch before, is part of the baseline or gets its `Create`: the directory is watched,
    // and the watch is live, before it is read.
    let calls = calls_of_add(make.as_ref(), "/fresh/late.js");
    assert!(calls >= 2, "{on}");
    for moment in 0..calls {
      let mut d = make();
      prepare_c2(&mut *d);
      d.fs().at_call(moment, |fs| fs.write("/fresh/late.js", "1").unwrap());
      d.watch(&["/fresh/late.js"]);
      assert_eq!(d.fs().traps_run(), 1, "{on}, call {moment}");
      d.settled();
      assert_eq!(
        d.known(),
        ["/fresh/late.js", "/proj/src/a.js"],
        "{on}: the file made before call {moment} of add was lost"
      );
      d.caught_up();
    }
  }
}

/// A directory that the filter ignores leads to a watched path, inside a watched directory. A
/// file takes its place, and before the flush the path becomes a watched path itself. The
/// watched directory above already had to report that file: its `Create` is due.
#[test]
fn a_directory_on_the_way_turns_into_a_file_and_becomes_a_watched_path() {
  on_all(&ignore_node_modules(), |d, on| {
    // The watched file is one level below the ignored directory: the directory that holds a
    // watched file is its watch target, and an ignored target would hide the file.
    d.fs().mkdir_all("/p/R/node_modules/pkg").unwrap();
    d.fs().write("/p/R/node_modules/pkg/x.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R", "/p/R/node_modules/pkg/x.js"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/R/node_modules/pkg/x.js"], "{on}");
    assert_eq!(d.settled(), NONE, "{on}");

    d.fs().remove_all("/p/R/node_modules").unwrap();
    d.fs().write("/p/R/node_modules", "a file now").unwrap();
    let mut events = d.watch(&["/p/R/node_modules"]);
    events.append(&mut d.settled());
    events.retain(|event| event != "U /p/R/node_modules");
    assert_eq!(sorted(events), ["C /p/R/node_modules", "D /p/R/node_modules/pkg/x.js"], "{on}");
    d.caught_up();
  });
}

/// A directory becomes a watched path while the disk does not answer. The check is made again
/// later, and that can be inside another `watch`. What it finds then is not the baseline of that
/// other `watch`: the files are reported.
#[test]
fn a_scan_that_waited_runs_inside_a_later_add() {
  let mut d = Driver::poll(Options::default());
  d.fs.mkdir("/b").unwrap();
  d.fs.write("/b/a", "1").unwrap();
  d.watch(&["/b/c/x"]);

  d.fs.set_offline(Some(io::ErrorKind::TimedOut));
  d.watch(&["/b"]);
  assert!(d.known.is_empty());
  d.fs.set_offline(None);
  d.watch(&["/z"]);
  assert_eq!(taken(&mut d), ["C /b/a"]);
  assert_eq!(settled(&mut d), NONE);
  caught_up(&mut d);
}

/// The same, and the later `watch` names a file below the directory that waits. The check of
/// that file comes before the scan from above: its directory is still partial in the tree. But
/// the older watched path covered the file, so it is no baseline of the later `watch`.
#[test]
fn a_scan_that_waited_runs_inside_a_watch_of_a_file_below() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir("/b").unwrap();
    d.fs().mkdir("/b/c").unwrap();
    d.fs().write("/b/c/a", "1").unwrap();
    assert_eq!(d.watch(&["/b/c/x"]), NONE, "{on}");

    d.fs().set_offline(Some(io::ErrorKind::TimedOut));
    assert_eq!(d.watch(&["/b"]), NONE, "{on}");
    d.fs().set_offline(None);
    assert_eq!(d.watch(&["/b/c/a"]), ["C /b/c/a"], "{on}");
    assert_eq!(d.settled(), NONE, "{on}");
    d.caught_up();
  });
}

/// The tree can be behind the disk when `watch` is called: a notification is still on its way.
/// A name on the way that the directory above listed under another spelling at its last look
/// (`/b/C` for an entry `c`) is looked at again before the new path below it: by now a
/// directory spelled `C` can be there, which that directory covers, and whose files are new.
#[test]
fn a_name_in_another_spelling_is_looked_at_again_before_a_new_path_below_it() {
  on_all(&Options { loose_spelling: true, ..Options::default() }, |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/b/c").unwrap();
    d.fs().write("/b/c/f", "1").unwrap();
    assert_eq!(d.watch(&["/b", "/b/C/x"]), NONE, "{on}");
    assert_eq!(d.known(), ["/b/c/f"], "{on}");

    d.fs().remove_all("/b/c").unwrap();
    d.fs().mkdir("/b/C").unwrap();
    d.fs().write("/b/C/a", "1").unwrap();
    assert_eq!(d.watch(&["/b/C/a"]), ["C /b/C/a"], "{on}");
    assert_eq!(d.settled(), ["D /b/c/f"], "{on}");
    d.caught_up();
  });
}

/// A name that the running `watch` registers is spelled by its caller, whatever the tree knew
/// at it: here a file entry of the watched directory, which was renamed to another case and
/// replaced by a link to a directory meanwhile, unheard. The disk is asked how it spells the
/// name: the directory does not list it so, and what is behind the link is not an entry of it.
/// The entry under the name the disk has is looked at in the same flush.
#[test]
fn a_known_entry_that_becomes_a_registered_name_is_asked_for_its_spelling() {
  on_all(&Options { loose_spelling: true, ..Options::default() }, |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/d").unwrap();
    d.fs().mkdir_all("/q/dir").unwrap();
    d.fs().write("/q/dir/g", "1").unwrap();
    d.fs().write("/d/c", "1").unwrap();
    assert_eq!(d.watch(&["/d"]), NONE, "{on}");

    d.fs().remove("/d/c").unwrap();
    d.fs().symlink("/q/dir", "/d/C").unwrap();
    assert_eq!(d.watch(&["/d/c/x"]), ["C /d/C", "D /d/c"], "{on}");
    assert_eq!(d.known(), ["/d/C"], "{on}");
    // The hint for the new link comes late: the `Update` it can bring is one too many, not wrong.
    assert!(d.settled().iter().all(|event| event == "U /d/C"), "{on}");
    d.caught_up();
  });
}
