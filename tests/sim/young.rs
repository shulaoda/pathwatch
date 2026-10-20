//! The rule for young files: a file whose modification time is too close to the look, so that an
//! equal fingerprint proves nothing, is compared by a hash of its content.
//! A story makes the disk, calls `watch`, changes the disk and compares the events that come
//! out (`C`, `U`, `D` and the path).

use std::{path::Path, time::Duration};

use crate::fixtures::{
  Deliver, Driver, Mode, NONE, Sim, SimBackend, caught_up, known, on_all, settled, show, taken,
  window,
};
use pathwatch::{engine::Options, poll::PollBackend};

fn poll_with_coarse_time(step: Duration, options: Options) -> Driver<PollBackend> {
  let d = Driver::poll(options);
  d.fs.set_time_step(step);
  d.fs.mkdir("/p").unwrap();
  d
}

/// A file that is too big to hash and too young to trust owes one `Update`: a write can have
/// left its time stamp as it is. The poll looks at it in every round, so the `Update` waits
/// for the round that finds the file old. It does not come in each round until then. Such a
/// write happens here, and no look can see it: the one `Update` that the file owes tells it.
#[test]
fn a_big_young_file() {
  let mut d = poll_with_coarse_time(Duration::from_millis(1), Options::default());
  d.fs.write("/p/big", vec![0; 16 * 1024 * 1024 + 1]).unwrap();
  d.watch(&["/p/big"]);
  // Written again inside the same step of the clock: the time stamp and the size stay.
  d.fs.write("/p/big", vec![1; 16 * 1024 * 1024 + 1]).unwrap();
  // The poll at 1 s: young, the same time stamp. The poll at 2 s: old.
  assert!(d.step());
  assert_eq!(taken(&mut d), NONE);
  assert!(d.step());
  assert_eq!(taken(&mut d), ["U /p/big"]);
  assert_eq!(settled(&mut d), NONE);
  assert_eq!(d.fs.calls().hash, 0);
  caught_up(&mut d);
}

/// A write that moves the time stamp of the big young file is an `Update` at once. The file is
/// young again after it, so it owes one more.
#[test]
fn a_big_young_file_with_a_new_time_stamp_is_reported_at_once() {
  let mut d = poll_with_coarse_time(Duration::from_millis(1), Options::default());
  d.fs.write("/p/big", vec![0; 16 * 1024 * 1024 + 1]).unwrap();
  d.watch(&["/p/big"]);
  assert!(d.step());
  assert_eq!(taken(&mut d), NONE);
  // At 1 s.
  d.fs.write("/p/big", vec![1; 16 * 1024 * 1024 + 1]).unwrap();
  assert!(d.step());
  assert_eq!(taken(&mut d), ["U /p/big"]);
  // The poll at 3 s finds it two seconds old.
  assert!(d.step());
  assert_eq!(taken(&mut d), ["U /p/big"]);
  assert_eq!(settled(&mut d), NONE);
  caught_up(&mut d);
}

#[test]
fn only_regular_files_are_opened() {
  let mut d = poll_with_coarse_time(Duration::from_millis(1), Options::default());
  d.fs.mkdir("/p/r").unwrap();
  d.fs.write("/p/r/file", "1").unwrap();
  d.fs.symlink("/p/r/file", "/p/r/link").unwrap();
  d.fs.symlink("/p/r", "/p/r/loop").unwrap();
  d.watch(&["/p/r"]);
  // A link inside a watched directory is a file entry of its own and is not followed.
  assert_eq!(known(&d), ["/p/r/file", "/p/r/link", "/p/r/loop"]);
  assert_eq!(d.fs.calls().hash, 1);
  assert_eq!(show(&d.events), NONE);
  caught_up(&mut d);
}

// ---- how long a file is young ----

/// The clock of the simulated disk starts at a whole second. After this, a time stamp has a
/// part of a second in it, as on every file system that counts in fine steps.
const OFF_THE_SECOND: Duration = Duration::from_millis(5);

/// Scans everything `times` times, 10 ms apart, and says how many of the scans read the
/// content of the file.
fn scans_that_hash<B: Deliver>(d: &mut Driver<B>, times: u64) -> u64 {
  let before = d.fs.calls().hash;
  for _ in 0..times {
    d.engine.overflow(d.now);
    assert!(d.step());
    assert_eq!(taken(d), NONE);
  }
  d.fs.calls().hash - before
}

/// The time stamp of the file has a part of a second in it, and an OS watch covers the file:
/// it is young for 20 ms, not for 2 s.
#[test]
fn a_file_with_a_fine_time_stamp_is_young_for_20_ms() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.advance(OFF_THE_SECOND);
    d.fs.mkdir("/p").unwrap();
    d.fs.write("/p/a", "1").unwrap();
    d.watch(&["/p/a"]);
    assert_eq!(d.fs.calls().hash, 1, "{mode:?}");
    // The scan at 10 ms: still young, the hash decides and is kept. The scan at 20 ms: old.
    // The stored hash is compared one last time, and no hash is kept.
    assert_eq!(scans_that_hash(&mut d, 2), 2, "{mode:?}");
    // From then on the fingerprint is trusted: the file is not opened.
    assert_eq!(scans_that_hash(&mut d, 10), 0, "{mode:?}");
    caught_up(&mut d);
  }
}

/// The same for a file in a directory below a watch for a whole tree. The backend was not
/// asked for that directory, but the watch above covers the file: 20 ms.
#[test]
fn a_file_below_a_watch_for_a_tree_is_young_for_20_ms() {
  for mode in [Mode::Prefix, Mode::Handle] {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.advance(OFF_THE_SECOND);
    d.fs.mkdir_all("/p/sub").unwrap();
    d.fs.write("/p/sub/a", "1").unwrap();
    d.watch(&["/p"]);
    assert!(d.fs.watch_calls().iter().all(|call| call.dir != Path::new("/p/sub")), "{mode:?}");
    assert_eq!(d.fs.calls().hash, 1, "{mode:?}");
    assert_eq!(scans_that_hash(&mut d, 2), 2, "{mode:?}");
    assert_eq!(scans_that_hash(&mut d, 10), 0, "{mode:?}");
    caught_up(&mut d);
  }
}

/// The time stamp is a whole second, as on a file system that counts in seconds: the file is
/// young for 2 s, also where an OS watch covers it.
#[test]
fn a_file_with_a_whole_second_is_young_for_2_s() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.set_time_step(Duration::from_secs(1));
    d.fs.advance(OFF_THE_SECOND);
    d.fs.mkdir("/p").unwrap();
    d.fs.write("/p/a", "1").unwrap();
    d.watch(&["/p/a"]);
    assert_eq!(d.fs.calls().hash, 1, "{mode:?}");
    assert_eq!(scans_that_hash(&mut d, 20), 20, "{mode:?}");
    d.advance(Duration::from_secs(2));
    // One last compare, then the fingerprint is trusted.
    assert_eq!(scans_that_hash(&mut d, 10), 1, "{mode:?}");
    caught_up(&mut d);
  }
}

/// No OS watch covers the file, so no "written" hint can make up for a wrong guess: it is young
/// for 2 s, whatever its time stamp looks like. Three ways to be there: the poll backend, a
/// watch that the OS had no room for, and a watched path that is a link (nothing watches what
/// is behind it; the file is read by its real path).
#[test]
fn a_file_that_only_the_poll_looks_at_is_young_for_2_s() {
  fn run<B: Deliver>(mut d: Driver<B>, path: &str, on: &str) {
    d.fs.advance(OFF_THE_SECOND);
    d.fs.mkdir_all("/p/real").unwrap();
    d.fs.write("/p/real/a", "1").unwrap();
    d.fs.symlink("/p/real/a", "/p/link").unwrap();
    d.watch(&[path]);
    assert_eq!(d.fs.calls().hash, 1, "{on}");
    assert_eq!(scans_that_hash(&mut d, 20), 20, "{on}");
    d.advance(Duration::from_secs(2));
    assert_eq!(scans_that_hash(&mut d, 10), 1, "{on}");
    caught_up(&mut d);
  }
  run(Driver::poll(Options::default()), "/p/real/a", "the poll backend");
  for mode in Mode::ALL {
    let d = Driver::sim(mode, Options::default());
    d.fs.set_watch_limit(Some(0));
    run(d, "/p/real/a", &format!("{mode:?}, no room for a watch"));
    run(Driver::sim(mode, Options::default()), "/p/link", &format!("{mode:?}, a link"));
  }
}

/// A second write of the same size inside one step of a fine clock (10 ms, as on exFAT), on a
/// backend that never says "written": the file was young when it was stored, so its hash tells.
#[test]
fn a_second_write_inside_a_fine_time_step() {
  let mut d: Driver<SimBackend> = Driver::sim(Mode::Prefix, Options::default());
  d.fs.set_time_step(Duration::from_millis(10));
  d.fs.advance(OFF_THE_SECOND + Duration::from_millis(10));
  d.fs.mkdir("/p").unwrap();
  d.fs.write("/p/a", "x=1;").unwrap();
  d.watch(&["/p/a"]);
  // 4 ms later, in the same step: the same time, the same size, the same inode.
  d.advance(Duration::from_millis(4));
  d.fs.write("/p/a", "x=2;").unwrap();
  assert_eq!(window(&mut d), ["U /p/a"]);
  // The flush was 10 ms after the write. A write in the step after that has another time.
  d.fs.write("/p/a", "x=3;").unwrap();
  assert_eq!(window(&mut d), ["U /p/a"]);
  assert_eq!(settled(&mut d), NONE);
  caught_up(&mut d);
}

/// A file that is written while a flush runs has a time after the start of that flush. It is
/// young, like every file whose time is in the future: the next write can leave the same time.
#[test]
fn a_file_that_is_written_while_the_flush_runs_is_young() {
  let mut d: Driver<SimBackend> = Driver::sim(Mode::Prefix, Options::default());
  d.fs.set_time_step(Duration::from_millis(10));
  d.fs.mkdir("/p").unwrap();
  d.watch(&["/p"]);

  // The flush scans everything. Its first call takes 30 ms, and then the file is written.
  d.engine.overflow(d.now);
  d.fs.at_call(0, |fs| {
    fs.advance(Duration::from_millis(30));
    fs.write("/p/a", "x=1;").unwrap();
  });
  assert!(d.step());
  assert_eq!(d.fs.traps_run(), 1);
  assert_eq!(taken(&mut d), ["C /p/a"]);

  // 4 ms later, in the same step of the clock: the same time, the same size, the same inode.
  d.advance(Duration::from_millis(4));
  d.fs.write("/p/a", "x=2;").unwrap();
  assert_eq!(window(&mut d), ["U /p/a"]);
  assert_eq!(settled(&mut d), NONE);
  caught_up(&mut d);
}

// ---- cases of the design review ----

/// A second write of the same size inside one tick of the file system clock is found, with
/// and without a "written" hint.
#[test]
fn a_second_write_in_the_same_time_step() {
  on_all(&Options::default(), |d, on| {
    d.fs().set_time_step(Duration::from_secs(2));
    d.fs().mkdir("/p").unwrap();
    d.fs().write("/p/a.js", "x=0;").unwrap();
    assert_eq!(d.watch(&["/p/a.js"]), NONE, "{on}");
    for content in ["x=1;", "x=2;", "x=3;"] {
      d.fs().write("/p/a.js", content).unwrap();
      assert_eq!(d.window(), ["U /p/a.js"], "{on}: {content}");
    }
    assert_eq!(d.settled(), NONE, "{on}");
    d.caught_up();
  });
}

/// The other direction: a "written" notification that comes again and again costs an
/// update each time and nothing else.
#[test]
fn a_repeated_written_hint() {
  for mode in [Mode::Inode, Mode::Handle] {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir("/p").unwrap();
    d.fs.write("/p/a.js", "1").unwrap();
    d.watch(&["/p/a.js"]);
    d.fs.write("/p/a.js", "22").unwrap();
    let raws = d.fs.take_raw(usize::MAX);
    for _ in 0..3 {
      for raw in raws.iter().cloned() {
        d.signal(raw);
      }
      assert_eq!(Sim::window(&mut d), ["U /p/a.js"], "{mode:?}");
    }
    assert_eq!(Sim::settled(&mut d), NONE, "{mode:?}");
    caught_up(&mut d);
  }
}

/// A young file goes away at any moment of a scan, also between the `lstat` and the read of its
/// content. That is one `Delete` and nothing else.
#[test]
fn a_young_file_goes_away_while_it_is_checked() {
  fn run<B: Deliver>(make: impl Fn() -> Driver<B>, on: &str) {
    // The file is young at `watch`, so its hash is stored. Then everything is scanned.
    let prepare = || {
      let mut d = make();
      d.fs.mkdir_all("/p/r").unwrap();
      d.fs.write("/p/r/a", "1").unwrap();
      d.watch(&["/p/r"]);
      d.engine.overflow(d.now);
      d
    };
    let mut d = prepare();
    let before = d.fs.calls();
    assert!(d.step());
    assert_eq!(d.fs.calls().hash, before.hash + 1, "{on}: the scan reads the young file");
    let calls = d.fs.calls().total() - before.total();
    caught_up(&mut d);

    for moment in 0..calls {
      let mut d = prepare();
      d.fs.at_call(moment, |fs| fs.remove("/p/r/a").unwrap());
      assert!(d.step());
      assert_eq!(d.fs.traps_run(), 1, "{on}, call {moment}");
      let mut events = taken(&mut d);
      events.append(&mut settled(&mut d));
      assert_eq!(events, ["D /p/r/a"], "{on}: removed before call {moment} of the scan");
      caught_up(&mut d);
    }
  }
  for mode in Mode::ALL {
    run(|| Driver::sim(mode, Options::default()), &format!("{mode:?}"));
  }
  run(|| Driver::poll(Options::default()), "Poll");
}

// ---- where every write gives a hint ----

/// The settings of inotify and Windows: every write to a watched file is a "written" hint.
fn hinted() -> Options {
  Options { written_hints: true, ..Options::default() }
}

/// Where every write gives a "written" hint, a young file is not read: the hint tells of a
/// second write of the same size inside one step of the clock.
#[test]
fn a_young_file_is_not_read_where_writes_are_hinted() {
  for mode in [Mode::Inode, Mode::Handle] {
    let mut d = Driver::sim(mode, hinted());
    d.fs.set_time_step(Duration::from_millis(10));
    d.fs.advance(OFF_THE_SECOND + Duration::from_millis(10));
    d.fs.mkdir("/p").unwrap();
    d.fs.write("/p/a.js", "x=1;").unwrap();
    d.watch(&["/p/a.js"]);
    assert_eq!(d.fs.calls().hash, 0, "{mode:?}");
    // 4 ms later, in the same step: the same time, the same size, the same inode.
    d.advance(Duration::from_millis(4));
    d.fs.write("/p/a.js", "x=2;").unwrap();
    assert_eq!(window(&mut d), ["U /p/a.js"], "{mode:?}");
    d.fs.write("/p/a.js", "x=3;").unwrap();
    assert_eq!(window(&mut d), ["U /p/a.js"], "{mode:?}");
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    assert_eq!(d.fs.calls().hash, 0, "{mode:?}");
    caught_up(&mut d);
  }
}

/// A loss report may have taken the hint of such a write: the look it asks for counts a file
/// that is young, or was young at its last look, as written. Once the file is found old, the
/// fingerprint is trusted again. Nothing is read at any of these looks.
#[test]
fn a_loss_report_counts_as_a_write_while_the_file_is_young() {
  for mode in [Mode::Inode, Mode::Handle] {
    let mut d = Driver::sim(mode, hinted());
    d.fs.advance(OFF_THE_SECOND);
    d.fs.mkdir("/p").unwrap();
    d.fs.write("/p/a.js", "x=1;").unwrap();
    d.watch(&["/p/a.js"]);
    // The scan at 10 ms: young. The scan at 20 ms: old, one last time. The scan at 30 ms: no.
    for expected in [vec!["U /p/a.js"], vec!["U /p/a.js"], NONE.to_vec()] {
      d.engine.overflow(d.now);
      assert!(d.step());
      assert_eq!(taken(&mut d), expected, "{mode:?}");
    }
    assert_eq!(d.fs.calls().hash, 0, "{mode:?}");
    caught_up(&mut d);
  }
}

/// No OS watch covers the file: the hint cannot come, and the hash decides as before.
#[test]
fn a_file_that_only_the_poll_looks_at_is_read_where_writes_are_hinted() {
  for mode in [Mode::Inode, Mode::Handle] {
    let d = Driver::sim(mode, hinted());
    d.fs.set_watch_limit(Some(0));
    let mut d = d;
    d.fs.advance(OFF_THE_SECOND);
    d.fs.mkdir("/p").unwrap();
    d.fs.write("/p/a.js", "x=1;").unwrap();
    d.watch(&["/p/a.js"]);
    assert_eq!(d.fs.calls().hash, 1, "{mode:?}");
    caught_up(&mut d);
  }
}
