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

/// Every disk is loose, and the write comes under the spelling of the disk, not the one of the
/// watched path: the hint is for the one entry that both names are, so it says "written" for
/// the watched name too. Nothing else could tell of the write: the file is young and not read,
/// and the fingerprint stays as it was.
#[test]
fn a_write_under_the_spelling_of_the_disk_is_a_write_to_the_watched_name() {
  for mode in [Mode::Inode, Mode::Handle] {
    let mut d = Driver::sim(mode, Options { loose_spelling: true, ..hinted() });
    d.fs.set_ignore_case(true);
    d.fs.set_time_step(Duration::from_secs(1));
    d.fs.mkdir("/p").unwrap();
    d.fs.write("/p/a.js", "x=1;").unwrap();
    d.watch(&["/p/A.JS"]);
    d.fs.write("/p/A.JS", "x=2;").unwrap();
    assert_eq!(window(&mut d), ["U /p/A.JS"], "{mode:?}");
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    assert_eq!(d.fs.calls().hash, 0, "{mode:?}");
    caught_up(&mut d);
  }
}

/// A directory below the watch is renamed away, a young file in it is written under the other
/// name, and it is renamed back, all inside one step of the clock. Where the OS watch is on the
/// directory above (Windows), the write is reported under a name the tree does not know, so no
/// hint reaches the file, and its fingerprint stays as it was. The deep scan of the directory
/// that came back counts the young file as written.
#[test]
fn a_directory_away_and_back_counts_its_young_files_as_written() {
  for mode in [Mode::Inode, Mode::Handle] {
    let mut d = Driver::sim(mode, hinted());
    d.fs.set_time_step(Duration::from_secs(1));
    d.fs.mkdir_all("/p/sub").unwrap();
    d.watch(&["/p"]);
    d.fs.write("/p/sub/a.js", "x=1;").unwrap();
    assert_eq!(window(&mut d), ["C /p/sub/a.js"], "{mode:?}");
    d.fs.rename("/p/sub", "/p/other").unwrap();
    d.fs.write("/p/other/a.js", "x=2;").unwrap();
    d.fs.rename("/p/other", "/p/sub").unwrap();
    assert_eq!(window(&mut d), ["U /p/sub/a.js"], "{mode:?}");
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    assert_eq!(d.fs.calls().hash, 0, "{mode:?}");
    caught_up(&mut d);
  }
}

/// The hint of a write goes into a look that finds the file missing (the disk is away for a
/// moment, or the file is), and the look that finds it back sees the fingerprint it had: the
/// second look of a held file counts a young file that was not read as written.
#[test]
fn the_second_look_of_a_held_file_counts_it_as_written() {
  for mode in [Mode::Inode, Mode::Handle] {
    let mut d = Driver::sim(mode, hinted());
    d.fs.set_time_step(Duration::from_secs(1));
    d.fs.mkdir("/p").unwrap();
    d.fs.write("/p/a.js", "x=1;").unwrap();
    d.watch(&["/p/a.js"]);
    d.fs.write("/p/a.js", "x=2;").unwrap();
    d.fs.set_offline(Some(std::io::ErrorKind::NotFound));
    assert_eq!(window(&mut d), NONE, "{mode:?}");
    d.fs.set_offline(None);
    assert_eq!(settled(&mut d), ["U /p/a.js"], "{mode:?}");
    assert_eq!(d.fs.calls().hash, 0, "{mode:?}");
    caught_up(&mut d);
  }
}

/// The same for a file: renamed away, written under the other name inside one step of the
/// clock, and renamed back. The record of its own name is a rename, as every write is a
/// "written" hint, so a young file that was not read counts as written.
#[test]
fn a_file_away_and_back_counts_as_written() {
  for mode in [Mode::Inode, Mode::Handle] {
    let mut d = Driver::sim(mode, hinted());
    d.fs.set_time_step(Duration::from_secs(1));
    d.fs.mkdir("/p").unwrap();
    d.watch(&["/p"]);
    d.fs.write("/p/a.js", "x=1;").unwrap();
    assert_eq!(window(&mut d), ["C /p/a.js"], "{mode:?}");
    d.fs.rename("/p/a.js", "/p/b.js").unwrap();
    d.fs.write("/p/b.js", "x=2;").unwrap();
    d.fs.rename("/p/b.js", "/p/a.js").unwrap();
    assert_eq!(window(&mut d), ["U /p/a.js"], "{mode:?}");
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    assert_eq!(d.fs.calls().hash, 0, "{mode:?}");
    caught_up(&mut d);
  }
}

/// A directory is removed and made again at once with its old identity (ext4 gives the inode
/// numbers out again), and the record of its young file comes first: the look at the file finds
/// the same fingerprint, and the new directory has no watch, so the next write in it gives no
/// hint. The end of the old watch comes with the parent's records of the removal, and that hint
/// does not take the loss off the directory: its scan counts the young file as written.
#[test]
fn a_directory_made_again_with_its_identity_counts_its_young_files_as_written() {
  let mut d = Driver::sim(Mode::Inode, hinted());
  d.fs.set_reuse_inode_numbers(true);
  d.fs.set_time_step(Duration::from_secs(1));
  d.fs.mkdir_all("/p/sub").unwrap();
  d.watch(&["/p"]);
  d.fs.write("/p/sub/a.js", "x=1;").unwrap();
  assert_eq!(window(&mut d), ["C /p/sub/a.js"]);
  d.fs.remove_all("/p/sub").unwrap();
  d.fs.mkdir("/p/sub").unwrap();
  d.fs.write("/p/sub/a.js", "x=2;").unwrap();
  // The old watch tells of the removal of the file; the rest is read later.
  assert_eq!(d.deliver(1), 1);
  assert!(d.step());
  assert_eq!(taken(&mut d), ["U /p/sub/a.js"]);
  d.fs.write("/p/sub/a.js", "x=3;").unwrap();
  assert_eq!(window(&mut d), ["U /p/sub/a.js"]);
  assert_eq!(settled(&mut d), NONE);
  assert_eq!(d.fs.calls().hash, 0);
  caught_up(&mut d);
}

/// A young file is written again inside one step of the clock, and before its record is read,
/// `unwatch` gives back the watch of the tree it lies in: the record goes with that watch. The
/// directory above scans the tree deep for the watches it needs now, and counts the young file
/// as written.
#[test]
fn a_tree_watch_given_back_counts_its_young_files_as_written() {
  let mut d = Driver::sim(Mode::Handle, hinted());
  d.fs.set_time_step(Duration::from_secs(1));
  d.fs.mkdir("/p").unwrap();
  d.fs.mkdir("/p/b").unwrap();
  d.watch(&["/p", "/p/b/f.js"]);
  d.fs.write("/p/b/f.js", "x=1;").unwrap();
  assert_eq!(window(&mut d), ["C /p/b/f.js"]);
  d.fs.write("/p/b/f.js", "x=2;").unwrap();
  d.unwatch(&["/p"]);
  assert_eq!(settled(&mut d), ["U /p/b/f.js"]);
  assert_eq!(d.fs.calls().hash, 0);
  caught_up(&mut d);
}

/// A young file is written again inside one step of the clock, the OS loses the record, and its
/// loss report is read only after a look found the file old (here the look of a `watch` call;
/// a reader that is behind, a watcher thread that is busy). A look that reports nothing keeps
/// the doubt of the file, so the loss report still counts it as written (Inode seed 2074718981).
#[test]
fn a_loss_report_read_after_a_look_found_the_file_old_counts_it_as_written() {
  for mode in [Mode::Inode, Mode::Handle] {
    let mut d = Driver::sim(mode, hinted());
    d.fs.set_time_step(Duration::from_secs(1));
    d.fs.mkdir("/p").unwrap();
    d.watch(&["/p"]);
    d.fs.write("/p/a.js", "x=1;").unwrap();
    assert_eq!(window(&mut d), ["C /p/a.js"], "{mode:?}");
    d.fs.write("/p/a.js", "x=2;").unwrap();
    assert!(d.fs.lose_raw(usize::MAX) > 0, "{mode:?}");
    d.advance(Duration::from_secs(3));
    d.watch(&["/p/a.js"]);
    assert_eq!(taken(&mut d), NONE, "{mode:?}");
    assert_eq!(settled(&mut d), ["U /p/a.js"], "{mode:?}");
    caught_up(&mut d);
  }
}

/// A directory is renamed away and back while its file is young, and the file is written under
/// the other name inside one step of the clock; the records are read only after the file got
/// old. A record tells when it came, not when its change was made, so the doubt of the file stays
/// until every record of the time it was young came, and the scan of the directory that was away
/// counts it as written (Handle seed 2074719922).
#[test]
fn records_read_late_of_a_directory_away_and_back_count_its_young_file_as_written() {
  for mode in [Mode::Inode, Mode::Handle] {
    let mut d = Driver::sim(mode, hinted());
    d.fs.set_time_step(Duration::from_secs(1));
    d.fs.mkdir_all("/p/sub").unwrap();
    d.watch(&["/p"]);
    d.fs.write("/p/sub/a.js", "x=1;").unwrap();
    assert_eq!(window(&mut d), ["C /p/sub/a.js"], "{mode:?}");
    d.fs.rename("/p/sub", "/p/other").unwrap();
    d.fs.write("/p/other/a.js", "x=2;").unwrap();
    d.fs.rename("/p/other", "/p/sub").unwrap();
    d.advance(Duration::from_secs(3));
    assert_eq!(settled(&mut d), ["U /p/sub/a.js"], "{mode:?}");
    caught_up(&mut d);
  }
}

/// A young file is written again inside one step of the clock, the OS loses the record, and the
/// disk gives no answer at some moment of the flush that the loss report asks for: a look at an
/// entry, or the listing of the directory of the file. The look that is tried again knows that
/// hints may be lost, as the one that got no answer did, and counts the file as written. Before,
/// a listing with no answer left a plain deep mark, and the scan after it took the file's
/// fingerprint for the truth (Inode seed 103232).
#[test]
fn a_look_with_no_answer_in_the_scans_of_a_loss_report_keeps_the_doubt() {
  for mode in [Mode::Inode, Mode::Handle] {
    let prepare = || {
      let mut d = Driver::sim(mode, hinted());
      d.fs.set_time_step(Duration::from_secs(1));
      d.fs.mkdir_all("/p/sub").unwrap();
      d.watch(&["/p"]);
      d.fs.write("/p/sub/a.js", "x=1;").unwrap();
      assert_eq!(window(&mut d), ["C /p/sub/a.js"], "{mode:?}");
      d.fs.write("/p/sub/a.js", "x=2;").unwrap();
      assert!(d.fs.lose_raw(usize::MAX) > 0, "{mode:?}");
      d
    };
    let mut d = prepare();
    let before = d.fs.calls().total();
    assert_eq!(window(&mut d), ["U /p/sub/a.js"], "{mode:?}");
    let calls = d.fs.calls().total() - before;
    caught_up(&mut d);

    for moment in 0..calls {
      let mut d = prepare();
      d.fs.at_call(moment, |fs| fs.set_offline(Some(std::io::ErrorKind::TimedOut)));
      let mut events = window(&mut d);
      d.fs.set_offline(None);
      events.append(&mut settled(&mut d));
      assert_eq!(events, ["U /p/sub/a.js"], "{mode:?}, call {moment}");
      caught_up(&mut d);
    }
  }
}

/// Once every record of the time a young file was young came (`Hint::Delivered`), its doubt
/// ends: a loss report long after counts nothing as written, and neither does its directory
/// renamed away and back. Before, the doubt stayed until a look reported the file, which gave an
/// `Update` of every untouched file that was young at its last look.
#[test]
fn a_file_whose_records_all_came_is_not_counted_as_written_later() {
  for mode in [Mode::Inode, Mode::Handle] {
    let mut d = Driver::sim(mode, hinted());
    d.fs.set_time_step(Duration::from_secs(1));
    d.fs.mkdir_all("/p/sub").unwrap();
    d.watch(&["/p"]);
    d.fs.write("/p/sub/a.js", "x=1;").unwrap();
    assert_eq!(window(&mut d), ["C /p/sub/a.js"], "{mode:?}");
    d.advance(Duration::from_secs(3));
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    d.engine.overflow(d.now);
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    d.fs.rename("/p/sub", "/p/other").unwrap();
    d.fs.rename("/p/other", "/p/sub").unwrap();
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    assert_eq!(d.fs.calls().hash, 0, "{mode:?}");
    caught_up(&mut d);
  }
}

/// A burst of young files gives its room back once their records all came: the table of what is
/// kept to compare, and the queue of the files that wait for a delivery, do not stay as big as
/// the burst for the life of the watcher.
#[test]
fn a_burst_of_young_files_gives_its_room_back() {
  const FILES: usize = 10_000;
  for mode in [Mode::Inode, Mode::Handle] {
    let mut d = Driver::sim(mode, hinted());
    d.fs.mkdir("/p").unwrap();
    d.watch(&["/p"]);
    for file in 0..FILES {
      d.fs.write(format!("/p/{file}.js"), "x=1;").unwrap();
    }
    assert_eq!(window(&mut d).len(), FILES, "{mode:?}");
    let room = d.engine.young_room();
    assert!(room >= FILES, "{mode:?}: room for {room} with {FILES} young files");
    d.advance(Duration::from_secs(3));
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    let room = d.engine.young_room();
    assert!(room < FILES / 10, "{mode:?}: room for {room} with no young file");
    caught_up(&mut d);
  }
}

/// A file that is written again before each look, so that every look finds it young (a log, a
/// database), keeps one entry in the queue of the files that wait for a delivery. Before, each
/// look added one that the next delivery passed on to the new time of the file, as many as there
/// were: the queue grew by one entry a flush for as long as the writes went on.
#[test]
fn a_file_that_is_young_at_every_look_keeps_one_place_in_the_queue() {
  const WRITES: usize = 2_000;
  for mode in [Mode::Inode, Mode::Handle] {
    let mut d = Driver::sim(mode, hinted());
    d.fs.mkdir("/p").unwrap();
    d.watch(&["/p"]);
    d.fs.write("/p/log", "0").unwrap();
    assert_eq!(window(&mut d), ["C /p/log"], "{mode:?}");
    for write in 1..=WRITES {
      d.fs.write("/p/log", write.to_string()).unwrap();
      assert_eq!(window(&mut d), ["U /p/log"], "{mode:?}, write {write}");
    }
    let room = d.engine.young_room();
    assert!(room < 100, "{mode:?}: room for {room} with one young file");
    d.advance(Duration::from_secs(3));
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    caught_up(&mut d);
  }
}
