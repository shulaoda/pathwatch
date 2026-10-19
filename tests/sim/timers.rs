//! When the engine acts: the quiet time before a flush, the poll, and the rest after a slow scan.
//! A story moves the clock by hand (`advance`, `step`) and compares `Engine::deadline` and the
//! events of each batch (`C`, `U`, `D` and the path).

use std::time::Duration;

use crate::fixtures::{
  Deliver, Driver, Mode, NONE, RECURSIVE, Sim, SimBackend, caught_up, ignore_node_modules, settled,
  show, taken, window,
};
use pathwatch::{ErrorKind, engine::Options, poll::PollBackend};

fn driver(mode: Mode) -> Driver<SimBackend> {
  Driver::sim(mode, Options::default())
}

/// With no delay, the flush is due as soon as a hint is there.
#[test]
fn zero_delay() {
  let mut d = Driver::sim(Mode::Inode, Options { debounce: Duration::ZERO, ..Options::default() });
  d.fs.mkdir("/p").unwrap();
  d.watch(&["/p/a"]);
  d.fs.write("/p/a", "1").unwrap();
  d.deliver(usize::MAX);
  assert_eq!(d.engine.deadline(), Some(d.now));
  d.tick();
  assert_eq!(taken(&mut d), ["C /p/a"]);
  caught_up(&mut d);
}

/// The flush comes when no hint came for the quiet time: every hint that leaves work moves it.
/// A hint for a name that no watched path can see leaves none, and moves nothing.
#[test]
fn the_flush_waits_for_the_quiet_time() {
  let mut d = driver(Mode::Inode);
  d.fs.mkdir("/p").unwrap();
  d.fs.write("/p/a", "1").unwrap();
  d.watch(&["/p/a"]);
  let start = d.now;

  // A save in four steps, 2 ms apart, is one batch.
  for size in 2..6 {
    d.fs.write("/p/a", "x".repeat(size)).unwrap();
    assert_eq!(d.deliver(usize::MAX), 1);
    assert_eq!(d.engine.deadline(), Some(d.now + Duration::from_millis(10)));
    d.advance(Duration::from_millis(2));
    d.tick();
    assert_eq!(taken(&mut d), NONE);
  }
  // Another file of the directory is written. The OS tells, and nothing waits for it.
  let due = d.engine.deadline();
  assert_eq!(due, Some(start + Duration::from_millis(6 + 10)));
  d.fs.write("/p/other", "1").unwrap();
  assert_eq!(d.deliver(usize::MAX), 1);
  assert_eq!(d.engine.deadline(), due);

  assert!(d.step());
  assert_eq!(d.now, start + Duration::from_millis(16));
  assert_eq!(taken(&mut d), ["U /p/a"]);
  // inotify watches the directory too: nothing is left for the timer.
  assert_eq!(d.engine.deadline(), None);
  caught_up(&mut d);
}

/// The longest wait is four quiet times, and 500 ms at most: with a quiet time of 200 ms, hints
/// that come every 150 ms never leave 200 ms of quiet, and each batch goes out 500 ms after its
/// first hint.
#[test]
fn the_longest_wait_is_500_ms_at_most() {
  // No poll comes in between: a poll checks what waits, too.
  let (debounce, poll_interval) = (Duration::from_millis(200), Duration::from_secs(60));
  let mut d = Driver::sim(Mode::Inode, Options { debounce, poll_interval, ..Options::default() });
  d.fs.mkdir("/p").unwrap();
  d.fs.write("/p/a", "1").unwrap();
  d.watch(&["/p"]);
  let start = d.now;

  let mut batches = Vec::new();
  for round in 0..12_u64 {
    d.fs.write("/p/a", format!("write {round}")).unwrap();
    d.deliver(usize::MAX);
    if round < 4 {
      let due = (round * 150 + 200).min(500);
      assert_eq!(d.engine.deadline(), Some(start + Duration::from_millis(due)));
    }
    // To the next write, or to the flush if that comes first.
    let next_write = start + Duration::from_millis((round + 1) * 150);
    while d.engine.deadline().is_some_and(|due| due <= next_write) {
      assert!(d.step());
      batches.push(((d.now - start).as_millis(), show(&d.take_events())));
    }
    d.advance(next_write - d.now);
  }
  let update = vec!["U /p/a".to_owned()];
  assert_eq!(batches, [(500, update.clone()), (1100, update.clone()), (1700, update)]);
  caught_up(&mut d);
}

/// The timers. Hints that keep coming move the flush, but they never starve a batch: it goes
/// out four quiet times after its first hint, 500 ms at most, and never before one quiet time.
/// And a watch that failed is asked for again by each poll until it works; then the directory
/// is polled no more.
#[test]
fn no_batch_starves_and_a_failed_watch_is_asked_again() {
  for (quiet, longest) in [(10_u64, 40_u64), (200, 500), (1000, 1000)] {
    let options = Options { debounce: Duration::from_millis(quiet), ..Options::default() };
    let mut d = Driver::sim(Mode::Inode, options);
    d.fs.mkdir_all("/p/logs").unwrap();
    d.fs.write("/p/a.js", "1").unwrap();
    d.fs.write("/p/logs/x.log", "").unwrap();
    d.watch(&["/p"]);
    let start = d.now;
    // A tool appends to a log every 3 ms. No quiet time passes without a hint.
    d.fs.write("/p/a.js", "22").unwrap();
    // The ticks come every 3 ms: the first one at the longest wait or after it.
    let tick = longest.div_ceil(3) * 3;
    let mut batches = Vec::new();
    for round in 1..=usize::try_from(2 * tick / 3 + 1).unwrap() {
      d.fs.write("/p/logs/x.log", "line\n".repeat(round)).unwrap();
      d.deliver(usize::MAX);
      if round == 3 {
        // The flush is due one quiet time after the last hint, not after the first one.
        let due = Duration::from_millis((6 + quiet).min(longest));
        assert_eq!(d.engine.deadline(), Some(start + due));
      }
      d.advance(Duration::from_millis(3));
      d.tick();
      let events = show(&d.take_events());
      if !events.is_empty() {
        batches.push(((d.now - start).as_millis(), events));
      }
    }
    // The first tick after the longest wait, and the first tick that long after the hint that
    // followed it.
    let log = "U /p/logs/x.log".to_owned();
    let first = vec!["U /p/a.js".to_owned(), log.clone()];
    assert_eq!(batches, [(u128::from(tick), first), (u128::from(2 * tick), vec![log])]);
    assert_eq!(Sim::settled(&mut d), ["U /p/logs/x.log"]);
    caught_up(&mut d);
  }

  let mut d = Driver::sim(Mode::Inode, Options::default());
  d.fs.mkdir_all("/p/r/sub").unwrap();
  d.fs.write("/p/r/sub/x", "1").unwrap();
  d.fs.set_watch_limit(Some(3));
  d.watch(&["/p/r"]);
  assert_eq!(Sim::errors(&d), [ErrorKind::WatchLimit]);
  let watches = d.fs.calls().watch;
  let start = d.now;
  Sim::settled(&mut d);
  // One more try with every poll.
  let polls = (d.now - start).as_secs();
  assert!(polls >= 2);
  assert_eq!(d.fs.calls().watch, watches + polls);
  d.fs.set_watch_limit(None);
  Sim::settled(&mut d);
  let (watches, lstats) = (d.fs.calls().watch, d.fs.calls().lstat);
  Sim::settled(&mut d);
  // The watch works: no more tries, and nothing is left for the timer.
  assert_eq!((d.fs.calls().watch, d.fs.calls().lstat), (watches, lstats));
  assert_eq!(d.engine.deadline(), None);
  assert_eq!(Sim::errors(&d), [ErrorKind::WatchLimit]);
  caught_up(&mut d);
}

/// A `watch` call sets the time of the first poll, and no later one moves it: a tool that
/// watches one more path every 400 ms does not keep the poll away.
#[test]
fn a_later_watch_call_does_not_move_the_poll() {
  let mut d = Driver::poll(Options::default());
  d.fs.mkdir("/p").unwrap();
  d.fs.write("/p/a.js", "1").unwrap();
  d.watch(&["/p/a.js"]);
  let first_poll = d.now + Duration::from_secs(1);
  assert_eq!(d.engine.deadline(), Some(first_poll));

  d.fs.write("/p/a.js", "22").unwrap();
  for round in 0..2 {
    d.advance(Duration::from_millis(400));
    d.watch(&[format!("/p/later{round}.js")]);
    assert_eq!(taken(&mut d), NONE);
    assert_eq!(d.engine.deadline(), Some(first_poll));
  }
  assert!(d.step());
  assert_eq!(d.now, first_poll);
  assert_eq!(taken(&mut d), ["U /p/a.js"]);
  caught_up(&mut d);
}

/// A poll round that takes longer than the poll interval is followed by a pause as long as the
/// round. A round that takes no time is followed by the poll interval.
#[test]
fn a_slow_poll_round_is_followed_by_a_pause_as_long() {
  let options = Options { poll_interval: Duration::from_millis(100), ..Options::default() };
  let mut d = Driver::poll(options);
  d.fs.mkdir_all("/p/R").unwrap();
  d.fs.write("/p/R/a", "1").unwrap();
  d.watch(&["/p/R"]);
  let start = d.now;
  assert_eq!(d.engine.deadline(), Some(start + Duration::from_millis(100)));
  assert!(d.step());
  assert_eq!(d.engine.deadline(), Some(start + Duration::from_millis(200)));

  d.fs.at_call(1, |fs| fs.advance(Duration::from_millis(250)));
  assert!(d.step());
  assert_eq!(d.fs.traps_run(), 1);
  // 200 ms, the 250 ms of the round, and a pause of 250 ms: longer than the interval.
  assert_eq!(d.engine.deadline(), Some(start + Duration::from_millis(700)));
  // A change is found by the next round, not sooner.
  d.fs.write("/p/R/a", "22").unwrap();
  assert!(d.step());
  assert_eq!(d.now, start + Duration::from_millis(700));
  assert_eq!(taken(&mut d), ["U /p/R/a"]);
  assert_eq!(d.engine.deadline(), Some(start + Duration::from_millis(800)));
  caught_up(&mut d);
}

/// After a scan of everything that took 30 ms, the next one for lost notifications waits
/// until 300 ms after its start: its 30 ms, and a rest of nine times that. A plain hint does
/// not wait for that.
#[test]
fn a_burst_of_lost_notifications_does_not_scan_without_a_pause() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir_all("/p/r/sub").unwrap();
    d.fs.write("/p/r/sub/a", "1").unwrap();
    d.watch(&["/p/r"]);

    d.fs.at_call(1, |fs| fs.advance(Duration::from_millis(30)));
    d.engine.overflow(d.now);
    assert!(d.step());
    assert_eq!(d.fs.traps_run(), 1, "{mode:?}");
    let scanned = d.now;

    d.fs.write("/p/r/sub/b", "1").unwrap();
    d.fs.write("/p/r/sub/a", "22").unwrap();
    d.fs.take_raw(1);
    d.engine.overflow(d.now);
    d.deliver(usize::MAX);
    assert_eq!(d.engine.deadline(), Some(scanned + Duration::from_millis(10)), "{mode:?}");
    assert!(d.step());
    assert_eq!(taken(&mut d), ["U /p/r/sub/a"], "{mode:?}");
    assert_eq!(d.engine.deadline(), Some(scanned + Duration::from_millis(300)), "{mode:?}");
    assert!(d.step());
    assert_eq!(taken(&mut d), ["C /p/r/sub/b"], "{mode:?}");
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    caught_up(&mut d);
  }
}

/// After scans for lost notifications that took 200 ms, the next ones wait half a second, not
/// nine times that: the rest is against a storm, not against one big directory.
#[test]
fn a_long_scan_after_a_loss_report_rests_half_a_second_at_most() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir_all("/p/r/sub").unwrap();
    d.fs.write("/p/r/sub/a", "1").unwrap();
    d.watch(&["/p/r"]);

    d.fs.at_call(1, |fs| fs.advance(Duration::from_millis(200)));
    d.engine.overflow(d.now);
    assert!(d.step());
    assert_eq!(d.fs.traps_run(), 1, "{mode:?}");
    let scanned = d.now;

    d.fs.write("/p/r/sub/b", "1").unwrap();
    d.fs.write("/p/r/sub/a", "22").unwrap();
    d.fs.take_raw(1);
    d.engine.overflow(d.now);
    d.deliver(usize::MAX);
    assert!(d.step());
    assert_eq!(taken(&mut d), ["U /p/r/sub/a"], "{mode:?}");
    assert_eq!(d.engine.deadline(), Some(scanned + Duration::from_millis(700)), "{mode:?}");
    assert!(d.step());
    assert_eq!(taken(&mut d), ["C /p/r/sub/b"], "{mode:?}");
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    caught_up(&mut d);
  }
}

/// Two watches lose notifications, and the scans for both run in one flush that takes 30 ms.
/// The next loss report waits until 300 ms after the start of that flush: its 30 ms, and a rest
/// of nine times that. The rest is for all loss reports together, not for each watch. A plain
/// hint does not wait.
#[test]
fn the_scans_after_loss_reports_are_paced_together() {
  for mode in RECURSIVE {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir_all("/p/A").unwrap();
    d.fs.mkdir_all("/q/B").unwrap();
    d.watch(&["/p/A", "/q/B"]);

    d.fs.write("/p/A/x", "1").unwrap();
    d.fs.write("/q/B/y", "1").unwrap();
    assert!(d.fs.lose_raw(usize::MAX) >= 2, "{mode:?}");
    assert_eq!(d.fs.pending_raw(), 2, "{mode:?}");
    d.fs.at_call(1, |fs| fs.advance(Duration::from_millis(30)));
    assert_eq!(window(&mut d), ["C /p/A/x", "C /q/B/y"], "{mode:?}");
    assert_eq!(d.fs.traps_run(), 1, "{mode:?}");
    let scanned = d.now;

    // One watch loses again. The other one tells a change as usual.
    d.fs.write("/p/A/x2", "1").unwrap();
    assert!(d.fs.lose_raw(usize::MAX) >= 1, "{mode:?}");
    assert_eq!(d.fs.pending_raw(), 1, "{mode:?}");
    d.fs.write("/q/B/y2", "1").unwrap();
    assert!(d.deliver(usize::MAX) >= 2, "{mode:?}");
    assert_eq!(d.engine.deadline(), Some(scanned + Duration::from_millis(10)), "{mode:?}");
    assert!(d.step());
    assert_eq!(taken(&mut d), ["C /q/B/y2"], "{mode:?}");
    assert_eq!(d.engine.deadline(), Some(scanned + Duration::from_millis(300)), "{mode:?}");
    assert!(d.step());
    assert_eq!(taken(&mut d), ["C /p/A/x2"], "{mode:?}");
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    caught_up(&mut d);
  }
}

/// A loss report for a directory and a plain hint for a file in the same directory, in one
/// flush. The loss scan is still a loss scan: a rest follows it. And while that rest runs, the
/// next loss report waits, also when a plain hint for the same directory comes with it: the
/// hint is checked at once, and the scan of everything else waits.
///
/// The parent of the directory is only polled, so with every mark in the directory the engine
/// looks whether the directory is still the same one (`check_moved`). That look joined the
/// loss report as a plain mark and took it out of the pacing: in a burst, where loss reports
/// and plain hints come together, no loss scan was ever paced.
#[test]
fn a_plain_hint_does_not_end_the_wait_of_a_loss_report() {
  for mode in RECURSIVE {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir_all("/p/A").unwrap();
    d.watch(&["/p/A"]);

    // One notification is lost, the next one comes through.
    d.fs.write("/p/A/x", "1").unwrap();
    assert!(d.fs.lose_raw(usize::MAX) >= 1, "{mode:?}");
    d.fs.write("/p/A/y", "1").unwrap();
    d.fs.at_call(1, |fs| fs.advance(Duration::from_millis(30)));
    assert_eq!(window(&mut d), ["C /p/A/x", "C /p/A/y"], "{mode:?}");
    assert_eq!(d.fs.traps_run(), 1, "{mode:?}");
    let scanned = d.now;

    // The same again, inside the rest: 30 ms of scan, and nine times that.
    d.fs.write("/p/A/x2", "1").unwrap();
    assert!(d.fs.lose_raw(usize::MAX) >= 1, "{mode:?}");
    d.fs.write("/p/A/y2", "1").unwrap();
    assert!(d.deliver(usize::MAX) >= 2, "{mode:?}");
    assert_eq!(d.engine.deadline(), Some(scanned + Duration::from_millis(10)), "{mode:?}");
    assert!(d.step());
    assert_eq!(taken(&mut d), ["C /p/A/y2"], "{mode:?}");
    assert_eq!(d.engine.deadline(), Some(scanned + Duration::from_millis(300)), "{mode:?}");
    assert!(d.step());
    assert_eq!(taken(&mut d), ["C /p/A/x2"], "{mode:?}");
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    caught_up(&mut d);
  }
}

/// Only the scans that a loss report asked for wait for the end of a rest. A deep check that
/// a hint asked for by name does not: a tree that is moved in, and a directory that is renamed,
/// are read at once.
#[test]
fn a_tree_that_is_moved_in_does_not_wait_for_the_rest() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir_all("/p/r/sub").unwrap();
    d.fs.mkdir_all("/elsewhere/tree/deep").unwrap();
    d.fs.write("/elsewhere/tree/deep/f", "1").unwrap();
    d.fs.write("/p/r/sub/a", "1").unwrap();
    d.watch(&["/p/r"]);

    d.fs.at_call(1, |fs| fs.advance(Duration::from_millis(30)));
    d.engine.overflow(d.now);
    assert!(d.step());
    assert_eq!(d.fs.traps_run(), 1, "{mode:?}");
    let scanned = d.now;

    // Notifications are lost once more: that scan waits. Then a tree is moved in and a
    // directory is renamed.
    d.engine.overflow(d.now);
    d.fs.rename("/elsewhere/tree", "/p/r/tree").unwrap();
    d.fs.rename("/p/r/sub", "/p/r/sub2").unwrap();
    d.deliver(usize::MAX);
    assert_eq!(d.engine.deadline(), Some(scanned + Duration::from_millis(10)), "{mode:?}");
    assert!(d.step());
    assert_eq!(taken(&mut d), ["C /p/r/sub2/a", "C /p/r/tree/deep/f"], "{mode:?}");
    // The `Delete` under the old name waits for its second look, not for the rest.
    assert_eq!(d.engine.deadline(), Some(scanned + Duration::from_millis(100)), "{mode:?}");
    assert!(d.step());
    assert_eq!(taken(&mut d), ["D /p/r/sub/a"], "{mode:?}");
    assert_eq!(d.engine.deadline(), Some(scanned + Duration::from_millis(300)), "{mode:?}");
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    caught_up(&mut d);
  }
}

/// A loss report waits for the end of a rest, and meanwhile a poll finds that the directory
/// above its place is gone for good. The mark goes with the directory: nothing can be checked
/// in a directory that is gone, and no mark is left where the tree has no directory.
#[test]
fn a_loss_report_waits_and_the_directory_above_goes_away() {
  for mode in RECURSIVE {
    let options = Options { delete_delay: Duration::ZERO, ..Options::default() };
    let mut d = Driver::sim(mode, options);
    d.fs.mkdir_all("/p/d/sub").unwrap();
    d.fs.write("/p/d/sub/a.js", "1").unwrap();
    d.fs.write("/p/x.js", "1").unwrap();
    d.watch(&["/p/d"]);

    // A scan after lost notifications that takes 300 ms: the rest after it is longer than
    // the time to the next poll.
    d.fs.at_call(1, |fs| fs.advance(Duration::from_millis(300)));
    d.engine.overflow(d.now);
    assert!(d.step());
    assert_eq!(d.fs.traps_run(), 1, "{mode:?}");
    // Two watches for a tree, one inside the other: `/p/d` had its own before `/p` got one,
    // and nothing has read `/p/d` again since, so it still has it.
    d.watch(&["/p/x.js"]);
    assert_eq!(d.fs.os_watches().len(), 2, "{mode:?}");
    // Both watches lose a notification, and both loss reports wait.
    d.fs.write("/p/d/sub/a.js", "22").unwrap();
    assert!(d.fs.lose_raw(usize::MAX) >= 2, "{mode:?}");
    assert_eq!(window(&mut d), NONE, "{mode:?}");

    // Nothing tells that `/p` is removed. The poll finds out, inside the rest.
    d.fs.remove_all("/p").unwrap();
    d.fs.take_raw(usize::MAX);
    assert_eq!(window(&mut d), ["D /p/d/sub/a.js", "D /p/x.js"], "{mode:?}");
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    caught_up(&mut d);
  }
}

/// A watcher that watches nothing has nothing due: no poll wakes its thread. That is so before
/// the first `watch` and after `unwatch` of every watched path. The next `watch` brings the
/// poll back.
#[test]
fn nothing_is_due_when_nothing_is_watched() {
  fn story<B: Deliver>(mut d: Driver<B>, on: &str) {
    let second = Duration::from_secs(1);
    assert_eq!(d.engine.deadline(), None, "{on}");
    // A path that `watch` does not take is no watched path.
    d.watch(&[""]);
    assert_eq!(d.engine.deadline(), None, "{on}");

    d.fs.mkdir_all("/p/lib/pkg").unwrap();
    d.fs.write("/p/a.js", "1").unwrap();
    d.fs.write("/p/lib/pkg/x.js", "1").unwrap();
    d.watch(&["/p/a.js", "/p/lib/pkg/x.js"]);
    // inotify watches the directories above too: nothing is left for the timer.
    let polls = on != "Inode";
    let poll = polls.then_some(d.now + second);
    assert_eq!(d.engine.deadline(), poll, "{on}");
    // One watched path is left: the poll stays, also when nothing is at the path any more.
    d.unwatch(&["/p/a.js"]);
    assert_eq!(d.engine.deadline(), poll, "{on}");
    d.fs.remove_all("/p/lib/pkg").unwrap();
    assert_eq!(settled(&mut d), ["D /p/lib/pkg/x.js"], "{on}");
    assert_eq!(d.engine.deadline().is_some(), polls, "{on}");
    d.unwatch(&["/p/lib/pkg/x.js"]);
    assert_eq!(d.engine.deadline(), None, "{on}");

    d.watch(&["/p/a.js"]);
    assert_eq!(d.engine.deadline(), polls.then_some(d.now + second), "{on}");
    // A watched path that is a link is checked by the poll on every system.
    d.fs.symlink("a.js", "/p/l.js").unwrap();
    d.watch(&["/p/l.js"]);
    assert_eq!(d.engine.deadline(), Some(d.now + second), "{on}");
    d.unwatch(&["/p/l.js"]);
    assert_eq!(d.engine.deadline().is_some(), polls, "{on}");
    d.unwatch(&["/p/a.js"]);
    assert_eq!(d.engine.deadline(), None, "{on}");
    caught_up(&mut d);
  }
  for mode in Mode::ALL {
    story(Driver::sim(mode, ignore_node_modules()), &format!("{mode:?}"));
  }
  story(Driver::<PollBackend>::poll(ignore_node_modules()), "Poll");
}

/// The quiet time can be longer than the poll interval. A poll that runs meanwhile checks the
/// directories that no OS watch tells about; the changes that hints told of wait for the quiet
/// time.
#[test]
fn a_quiet_time_longer_than_the_poll_interval() {
  let options = Options { debounce: Duration::from_secs(3), ..Options::default() };
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, options.clone());
    d.fs.mkdir_all("/p/r").unwrap();
    d.fs.mkdir_all("/p/other").unwrap();
    d.fs.write("/p/r/a", "1").unwrap();
    d.watch(&["/p/r"]);
    d.fs.write("/p/r/a", "22").unwrap();
    d.deliver(usize::MAX);
    let hint_at = d.now;

    // Two polls, a second apart, report nothing of it. With inotify no poll runs.
    let flush_at = hint_at + Duration::from_secs(3);
    let polls = if mode == Mode::Inode { 0 } else { 2 };
    for _ in 0..polls {
      assert!(d.step());
      assert!(d.now < flush_at, "{mode:?}");
      assert_eq!(taken(&mut d), NONE, "{mode:?}");
    }
    // The flush comes three seconds after the hint.
    assert!(d.step());
    assert_eq!(d.now, flush_at, "{mode:?}");
    assert_eq!(taken(&mut d), ["U /p/r/a"], "{mode:?}");
    caught_up(&mut d);
  }
}
