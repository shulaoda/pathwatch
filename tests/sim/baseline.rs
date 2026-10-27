//! The baseline of `watch`: what exists when `watch` returns is known without an event, and what
//! an older watched path had to report is still reported.
//! A story makes the disk, calls `watch`, changes the disk and compares the events that come
//! out (`C`, `U`, `D` and the path).

use std::{io, time::Duration};

use crate::fixtures::{
  Deliver, Driver, Mode, NONE, Sim, SimFs, caught_up, flush, ignore_node_modules, makes, on_all,
  settled, taken,
};
use pathwatch::engine::Options;

/// A watched directory that was missing appears, and before the flush a file in it becomes a
/// watched path: the directory already had to report that file.
#[test]
fn an_older_watched_directory_appears_during_add() {
  let mut d = Driver::sim(Mode::Inode, Options::default());
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

// ---- what was due before `watch`, and what was not ----
//
// Each of these is a sequence in which a rule of this kind once swallowed an event, or told one
// too many, in this design or in one of the five that were drawn up next to it.

/// A file moves to another name in a watched directory, and the new name becomes a watched path
/// while the `Delete` of the old name still waits. The `Create` is not swallowed, and nothing
/// holds it back.
#[test]
fn add_while_a_delete_waits() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/d").unwrap();
    d.fs().write("/p/d/a.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/d"]), NONE, "{on}");

    d.fs().rename("/p/d/a.js", "/p/d/b.js").unwrap();
    assert_eq!(d.watch(&["/p/d/b.js"]), ["C /p/d/b.js"], "{on}");
    let mut rest = d.settled();
    rest.retain(|event| event != "U /p/d/b.js");
    assert_eq!(rest, ["D /p/d/a.js"], "{on}");
    assert_eq!(d.known(), ["/p/d/b.js"], "{on}");
    d.caught_up();
  });
}

/// A directory above a watched file is found missing, so it is held. It comes back, and before
/// any notification says so, another file in it becomes a watched path. `watch` looks at the disk
/// all the same: its check starts at the held directory, which is back.
#[test]
fn add_below_a_held_directory() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/c/b").unwrap();
    d.fs().mkdir("/q").unwrap();
    d.fs().write("/c/b/f.js", "1").unwrap();
    assert_eq!(d.watch(&["/c/b/f.js"]), NONE, "{on}");

    d.fs().rename("/c", "/q/c").unwrap();
    assert_eq!(d.window(), NONE, "{on}");
    assert_eq!(d.pending_deletes(), 1, "{on}");
    d.fs().rename("/q/c", "/c").unwrap();
    d.fs().write("/c/b/g.js", "1").unwrap();
    assert_eq!(d.watch(&["/c/b/g.js"]), NONE, "{on}");
    assert_eq!(d.known(), ["/c/b/f.js", "/c/b/g.js"], "{on}");
    assert_eq!(d.pending_deletes(), 0, "{on}");
    // The notification of the write comes after the `watch`.
    assert!(d.settled().iter().all(|event| event == "U /c/b/g.js"), "{on}");
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
  event: &'static str,
  known: &'static [&'static str],
}

/// Every story is one from above, with its change moved into the call.
const DUE: [Due; 7] = [
  Due {
    name: "a new file in an older watched directory",
    before: |fs| fs.mkdir_all("/p/d").unwrap(),
    first: "/p/d",
    goes: None,
    second: "/p/d/f.js",
    change: |fs| fs.write("/p/d/f.js", "1").unwrap(),
    event: "C /p/d/f.js",
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
    event: "C /p/r/a.js",
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
    event: "C /p/gen/x.js",
    known: &["/p/gen/x.js", "/p/gen/y.js"],
  },
  Due {
    name: "the missing directory above an older watched file becomes a watched path",
    before: |fs| fs.mkdir("/p").unwrap(),
    first: "/p/gen/x.js",
    goes: None,
    second: "/p/gen",
    change: |fs| {
      fs.mkdir("/p/gen").unwrap();
      fs.write("/p/gen/x.js", "1").unwrap();
      fs.write("/p/gen/y.js", "1").unwrap();
    },
    event: "C /p/gen/x.js",
    known: &["/p/gen/x.js", "/p/gen/y.js"],
  },
  Due {
    name: "a new directory in an older watched directory becomes a watched path",
    before: |fs| fs.mkdir_all("/p/src").unwrap(),
    first: "/p/src",
    goes: None,
    second: "/p/src/pages",
    change: |fs| {
      fs.mkdir("/p/src/pages").unwrap();
      fs.write("/p/src/pages/x.js", "1").unwrap();
    },
    event: "C /p/src/pages/x.js",
    known: &["/p/src/pages/x.js"],
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
    event: "U /p/d/f.js",
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
    event: "U /p/gen/x.js",
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
    let told = events.iter().filter(|event| *event == story.event).count();
    assert!(told == 1 || (told > 1 && story.event.starts_with('U')), "{on}");
    assert!(events.iter().all(|event| !event.starts_with('D')), "{on}");
    assert_eq!(d.known(), story.known, "{on}");
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

/// Three spellings of one path in a batch are one watched path: a change is told once, and a
/// `watch` of the path again costs nothing.
#[test]
fn three_spellings_of_a_path_are_one_watched_path() {
  let mut d = Driver::sim(Mode::Inode, Options::default());
  d.fs.mkdir("/p").unwrap();
  d.fs.write("/p/a.js", "1").unwrap();
  d.watch(&["/p/a.js", "/p//a.js/", "/p/./a.js"]);

  d.fs.write("/p/a.js", "12").unwrap();
  assert_eq!(settled(&mut d), ["U /p/a.js"]);
  let stats = d.fs.calls();
  d.watch(&["/p/a.js"]);
  assert_eq!(d.fs.calls(), stats);
  caught_up(&mut d);
}

// ---- cases of the design review ----

/// What a batch costs. One check for a name, one watch for a directory, and one look for the
/// kind of a watched path in each directory that holds some. A name that is watched already
/// costs nothing.
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
    // Every file is young, and read once for its hash.
    assert_eq!((calls.lstat, calls.stat, calls.read_dir, calls.hash), (363, 0, 0, 300), "{mode:?}");
    // One watch call for a directory: 30, `/p/src`, `/p` and `/`.
    assert_eq!(calls.watch, 33, "{mode:?}");
    assert_eq!(d.known.len(), 300, "{mode:?}");

    // The same batch again costs nothing at all.
    d.watch(&paths);
    assert_eq!(d.fs.calls(), calls, "{mode:?}");

    // Half of them again, and five new names in a known directory: only the new names are
    // looked at. Where the backend does not watch `/p/src`, that directory checks `d3` with them.
    let mut again: Vec<String> = paths.iter().step_by(2).cloned().collect();
    for file in 0..5 {
      again.push(format!("/p/src/d3/new{file}.js"));
    }
    d.watch(&again);
    let polled_parent = u64::from(mode != Mode::Inode);
    let calls = d.fs.calls();
    assert_eq!((calls.lstat, calls.stat), (368 + polled_parent, 0), "{mode:?}");
    caught_up(&mut d);
  }
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
    assert!(d.settled().iter().all(|event| event == "U /s/R/a.js"), "{on}");

    // Case 4: as case 1, and the hint for the new file is a mark in the engine already.
    d.fs().mkdir_all("/t/src").unwrap();
    assert_eq!(d.watch(&["/t/src"]), NONE, "{on}");
    d.fs().write("/t/src/x.js", "1").unwrap();
    assert!(d.something_is_due(), "{on}");
    assert_eq!(d.watch(&["/t/src/x.js", "/t/src/missing.js"]), ["C /t/src/x.js"], "{on}");
    assert!(d.settled().iter().all(|event| event == "U /t/src/x.js"), "{on}");

    // Case 5: an update that was due in a watched directory, and the file and the directory
    // are watched again before the flush.
    d.fs().mkdir_all("/u/src").unwrap();
    d.fs().write("/u/src/a.vue", "1").unwrap();
    assert_eq!(d.watch(&["/u/src"]), NONE, "{on}");
    d.fs().write("/u/src/a.vue", "12").unwrap();
    assert_eq!(d.watch(&["/u/src/a.vue", "/u/src"]), ["U /u/src/a.vue"], "{on}");
    assert!(d.settled().iter().all(|event| event == "U /u/src/a.vue"), "{on}");
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
  for (on, make) in makes(&Options::default()) {
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

/// A directory that the filter ignores, inside a watched directory, is replaced by a file, and
/// before the flush the path becomes a watched path itself. The watched directory above
/// already had to report that file: its `Create` is due.
#[test]
fn an_ignored_directory_turns_into_a_file_and_becomes_a_watched_path() {
  on_all(&ignore_node_modules(), |d, on| {
    d.fs().mkdir_all("/p/R/node_modules/pkg").unwrap();
    d.fs().write("/p/R/node_modules/pkg/x.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R"]), NONE, "{on}");
    assert_eq!(d.known(), NONE, "{on}");

    d.fs().remove_all("/p/R/node_modules").unwrap();
    d.fs().write("/p/R/node_modules", "a file now").unwrap();
    let mut events = d.watch(&["/p/R/node_modules"]);
    events.append(&mut d.settled());
    events.retain(|event| event != "U /p/R/node_modules");
    assert_eq!(events, ["C /p/R/node_modules"], "{on}");
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
/// the older watched path covered the file, so it is no baseline of the later `watch`. The same
/// when the file is in the directory that waits itself: it is reported, with everything else in
/// the directory.
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

    d.fs().mkdir("/e").unwrap();
    d.fs().write("/e/a", "1").unwrap();
    d.fs().write("/e/z", "1").unwrap();
    assert_eq!(d.watch(&["/e/c/x"]), NONE, "{on}");

    d.fs().set_offline(Some(io::ErrorKind::TimedOut));
    assert_eq!(d.watch(&["/e"]), NONE, "{on}");
    d.fs().set_offline(None);
    assert_eq!(d.watch(&["/e/a"]), ["C /e/a", "C /e/z"], "{on}");
    // A handle above saw the two files written (NTFS gives a record for the content of a new
    // file), and those records were still on their way when `watch` ran: a write that the
    // backend reports is an `Update`, also for a file the baseline has just seen (design.md 4.4).
    let late = if on == "Handle" { vec!["U /e/a", "U /e/z"] } else { NONE.to_vec() };
    assert_eq!(d.settled(), late, "{on}");
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
    // The handle on `/b` saw the file written, and the record was still on its way when
    // `watch` ran: a write that the backend reports is an `Update`, also after the baseline.
    let late = if on == "Handle" { vec!["U /b/C/a", "D /b/c/f"] } else { vec!["D /b/c/f"] };
    assert_eq!(d.settled(), late, "{on}");
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
