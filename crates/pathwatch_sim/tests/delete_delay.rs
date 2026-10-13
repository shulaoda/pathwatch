//! The delete delay: a file that is gone is held for 100 ms before its `Delete` goes out, so a
//! save that removes the file and writes it again is one `Update`.
//! A story makes the disk, calls `watch`, changes the disk and compares the events that come
//! out (`C`, `U`, `D` and the path).

#![cfg(unix)]

use std::{collections::BTreeMap, io, time::Duration};

use pathwatch::{ErrorKind, engine::Options};
use pathwatch_sim::{
  Deliver, Driver, Mode, NONE, Sim, SimBackend, SimFs, caught_up, flush, on_all, settled, show,
  taken, window, window_then_settled,
};

fn inode() -> Driver<SimBackend> {
  Driver::sim(Mode::Inode, Options::default())
}

#[test]
fn a_lonely_delete_goes_out_one_delay_later() {
  let mut d = inode();
  d.fs.mkdir("/p").unwrap();
  d.fs.write("/p/a", "1").unwrap();
  d.watch(&["/p/a"]);
  let start = d.now;

  d.fs.remove("/p/a").unwrap();
  d.deliver(usize::MAX);
  assert_eq!(d.engine.deadline(), Some(start + Duration::from_millis(10)));
  assert!(d.step());
  assert_eq!(taken(&mut d), NONE);
  // The delete has a timer of its own: one delay after the hint, not after the look.
  assert_eq!(d.engine.deadline(), Some(start + Duration::from_millis(100)));
  assert!(d.step());
  assert_eq!(taken(&mut d), ["D /p/a"]);
  caught_up(&mut d);
}

/// The delay counts from the hint of each file, not from the look that found it gone: a file
/// that went earlier gets its `Delete` earlier, also when the look came to it last.
#[test]
fn the_delay_counts_from_the_hint_of_each_file() {
  let mut d = inode();
  d.fs.mkdir("/p").unwrap();
  d.fs.write("/p/a", "1").unwrap();
  d.fs.write("/p/b", "1").unwrap();
  d.watch(&["/p/a", "/p/b"]);
  let start = d.now;

  // `b` goes first. The thread was busy: one flush looks at both, and at `a` first.
  d.fs.remove("/p/b").unwrap();
  d.deliver(usize::MAX);
  d.advance(Duration::from_millis(30));
  d.fs.remove("/p/a").unwrap();
  d.deliver(usize::MAX);
  assert!(d.step());
  assert_eq!(taken(&mut d), NONE);
  assert_eq!(d.engine.deadline(), Some(start + Duration::from_millis(100)));
  assert!(d.step());
  assert_eq!(taken(&mut d), ["D /p/b"]);
  assert_eq!(d.engine.deadline(), Some(start + Duration::from_millis(130)));
  assert!(d.step());
  assert_eq!(taken(&mut d), ["D /p/a"]);
  caught_up(&mut d);
}

/// The first look comes after the delay (the thread was busy, or a flood kept the batch open):
/// the second look comes at once, and the `Delete` with the next batch.
#[test]
fn a_late_first_look_gets_its_second_look_at_once() {
  let mut d = inode();
  d.fs.mkdir("/p").unwrap();
  d.fs.write("/p/a", "1").unwrap();
  d.watch(&["/p/a"]);
  let start = d.now;

  d.fs.remove("/p/a").unwrap();
  d.deliver(usize::MAX);
  d.advance(Duration::from_millis(200));
  d.tick();
  assert_eq!(taken(&mut d), NONE);
  assert_eq!(d.engine.deadline(), Some(start + Duration::from_millis(200)));
  assert!(d.step());
  assert_eq!(taken(&mut d), ["D /p/a"]);
  caught_up(&mut d);
}

#[test]
fn a_lonely_delete_with_the_poll_backend() {
  let mut d = Driver::poll(Options::default());
  d.fs.mkdir("/p").unwrap();
  d.fs.write("/p/a", "1").unwrap();
  d.watch(&["/p/a"]);
  let start = d.now;

  d.fs.remove("/p/a").unwrap();
  assert!(d.step());
  assert_eq!(d.now, start + Duration::from_secs(1));
  assert_eq!(taken(&mut d), NONE);
  assert!(d.step());
  assert_eq!(d.now, start + Duration::from_millis(1100));
  assert_eq!(taken(&mut d), ["D /p/a"]);
  caught_up(&mut d);
}

#[test]
fn zero_turns_the_delay_off() {
  let mut d =
    Driver::sim(Mode::Inode, Options { delete_delay: Duration::ZERO, ..Options::default() });
  d.fs.mkdir_all("/p/r/sub").unwrap();
  d.fs.write("/p/r/a", "1").unwrap();
  d.fs.write("/p/r/sub/b", "1").unwrap();
  d.watch(&["/p/r"]);

  d.fs.remove("/p/r/a").unwrap();
  d.fs.remove_all("/p/r/sub").unwrap();
  flush(&mut d);
  assert_eq!(taken(&mut d), ["D /p/r/a", "D /p/r/sub/b"]);
  assert!(d.engine.pending_deletes().is_empty());
  caught_up(&mut d);
}

#[test]
fn pending_then_back_then_gone() {
  let mut d = inode();
  d.fs.mkdir("/p").unwrap();
  d.fs.write("/p/a", "1").unwrap();
  d.watch(&["/p/a"]);

  d.fs.remove("/p/a").unwrap();
  flush(&mut d);
  d.fs.write("/p/a", "1").unwrap();
  flush(&mut d);
  assert_eq!(taken(&mut d), ["U /p/a"]);
  d.fs.remove("/p/a").unwrap();
  assert_eq!(settled(&mut d), ["D /p/a"]);
  caught_up(&mut d);
}

/// The check at the deadline finds a file that no notification announced.
#[test]
fn the_check_at_the_deadline_finds_the_file() {
  let mut d = inode();
  d.fs.mkdir("/p").unwrap();
  d.fs.write("/p/a", "1").unwrap();
  d.watch(&["/p/a"]);

  d.fs.remove("/p/a").unwrap();
  flush(&mut d);
  d.fs.write("/p/a", "1").unwrap();
  // The notification is lost, and nobody says so.
  assert_eq!(d.fs.take_raw(usize::MAX).len(), 1);
  assert!(d.step());
  assert_eq!(taken(&mut d), ["U /p/a"]);
  caught_up(&mut d);
}

/// A file appears where a directory was: the deletes below it cannot become updates any more
/// and go out in the same batch.
#[test]
fn a_file_sends_the_pending_deletes_below_it() {
  let mut d = inode();
  d.fs.mkdir_all("/p/r/d/e").unwrap();
  d.fs.write("/p/r/d/f", "1").unwrap();
  d.fs.write("/p/r/d/e/g", "1").unwrap();
  d.fs.write("/p/r/dx", "1").unwrap();
  d.watch(&["/p/r"]);

  d.fs.remove_all("/p/r/d").unwrap();
  d.fs.remove("/p/r/dx").unwrap();
  flush(&mut d);
  assert_eq!(d.engine.pending_deletes().len(), 3);

  d.fs.write("/p/r/d", "a file now").unwrap();
  flush(&mut d);
  assert_eq!(taken(&mut d), ["C /p/r/d", "D /p/r/d/e/g", "D /p/r/d/f"]);
  // A name that only starts with the same letters is not below it.
  assert_eq!(settled(&mut d), ["D /p/r/dx"]);
  caught_up(&mut d);
}

/// A directory goes away and comes back inside the delay. A file that is the same as before
/// gets no event, a file that was edited meanwhile gets its `Update`, and a file that did not
/// come back gets its `Delete`.
#[test]
fn a_directory_comes_back_inside_the_delay() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir_all("/p/r/d").unwrap();
    d.fs.mkdir("/q").unwrap();
    d.fs.write("/p/r/d/f", "1").unwrap();
    d.fs.write("/p/r/d/g", "1").unwrap();
    d.fs.write("/p/r/d/h", "1").unwrap();
    d.watch(&["/p/r"]);

    d.fs.rename("/p/r/d", "/q/d").unwrap();
    flush(&mut d);
    assert_eq!(d.engine.pending_deletes().len(), 3, "{mode:?}");
    d.fs.remove("/q/d/g").unwrap();
    d.fs.write("/q/d/h", "22").unwrap();
    d.fs.rename("/q/d", "/p/r/d").unwrap();
    flush(&mut d);
    assert_eq!(taken(&mut d), ["U /p/r/d/h"], "{mode:?}");
    assert_eq!(settled(&mut d), ["D /p/r/d/g"], "{mode:?}");
    caught_up(&mut d);
  }
}

// ---- a directory that goes away is one held node ----

/// `rm -r` of a directory with 1,000 files, and the OS tells about every file (inotify). The
/// directory is one held node: the first flush looks at the directory and at no file, and the
/// batch after the delay has the 1,000 deletes.
#[test]
fn rm_r_of_1000_files_is_one_held_node() {
  let mut d = inode();
  d.fs.mkdir_all("/p/r/big").unwrap();
  for file in 0..1000 {
    d.fs.write(format!("/p/r/big/f{file}"), "1").unwrap();
  }
  d.watch(&["/p/r"]);

  d.fs.remove_all("/p/r/big").unwrap();
  assert!(d.fs.pending_raw() > 1000);
  let before = d.fs.calls();
  flush(&mut d);
  assert_eq!(taken(&mut d), NONE);
  assert_eq!(d.fs.calls().lstat, before.lstat + 1);
  assert_eq!(d.engine.pending_deletes().len(), 1000);

  // The second look: one more `lstat`, and every file goes in one batch.
  assert!(d.step());
  let events = taken(&mut d);
  assert_eq!(events.len(), 1000);
  assert!(events.iter().all(|event| event.starts_with("D /p/r/big/f")));
  assert_eq!(d.fs.calls().lstat, before.lstat + 2);
  assert!(d.known.is_empty());
  caught_up(&mut d);
}

/// A held directory is back when its second look comes, and no notification said so. Nothing
/// is deleted. Everything below it is looked at again: a file that was written while it was
/// away gets its `Update`, a file that is the same gets nothing. And the directory is watched
/// again with everything below it.
#[test]
fn a_directory_that_is_back_with_no_hint_gets_no_delete() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir_all("/p/r/d/deep").unwrap();
    d.fs.mkdir("/q").unwrap();
    d.fs.write("/p/r/d/f", "1").unwrap();
    d.fs.write("/p/r/d/deep/g", "1").unwrap();
    d.watch(&["/p/r"]);

    d.fs.rename("/p/r/d", "/q/d").unwrap();
    flush(&mut d);
    assert_eq!(d.engine.pending_deletes().len(), 2, "{mode:?}");
    d.fs.write("/q/d/deep/g", "22").unwrap();
    d.fs.rename("/q/d", "/p/r/d").unwrap();
    // The notifications are lost, and nobody says so.
    assert!(!d.fs.take_raw(usize::MAX).is_empty(), "{mode:?}");
    assert!(d.step());
    assert_eq!(taken(&mut d), ["U /p/r/d/deep/g"], "{mode:?}");
    assert!(d.engine.pending_deletes().is_empty(), "{mode:?}");

    d.fs.write("/p/r/d/deep/g", "333").unwrap();
    d.fs.write("/p/r/d/new", "1").unwrap();
    assert!(d.fs.pending_raw() >= 2, "{mode:?}: the watches were not made again");
    assert_eq!(window(&mut d), ["U /p/r/d/deep/g", "C /p/r/d/new"], "{mode:?}");
    caught_up(&mut d);
  }
}

/// While a directory is held, nothing below it is checked: a notification that still comes
/// through the watch of a directory in it costs nothing and tells nothing. When the directory
/// is back, everything below it is compared with what was there before.
#[test]
fn nothing_below_a_held_directory_is_checked() {
  let mut d = inode();
  d.fs.mkdir_all("/p/r/d/sub").unwrap();
  d.fs.mkdir("/q").unwrap();
  d.fs.write("/p/r/d/sub/f", "1").unwrap();
  d.fs.write("/p/r/d/sub/g", "1").unwrap();
  d.watch(&["/p/r"]);

  d.fs.rename("/p/r/d", "/q/d").unwrap();
  flush(&mut d);
  // The watch of `sub` went away with it and still reports.
  d.fs.write("/q/d/sub/f", "22").unwrap();
  d.fs.remove("/q/d/sub/g").unwrap();
  assert!(d.fs.pending_raw() > 0);
  let before = d.fs.calls();
  flush(&mut d);
  assert_eq!(taken(&mut d), NONE);
  assert_eq!(d.fs.calls(), before);
  assert_eq!(d.engine.pending_deletes().len(), 2);

  d.fs.rename("/q/d", "/p/r/d").unwrap();
  assert_eq!(window(&mut d), ["U /p/r/d/sub/f"]);
  assert_eq!(settled(&mut d), ["D /p/r/d/sub/g"]);
  caught_up(&mut d);
}

/// A directory is found missing while a directory below it is held already. The one above
/// decides for both, at its own time. If it comes back, what is missing in it is held on its
/// own again: the directory below gets a second look of its own.
#[test]
fn a_held_directory_below_a_held_directory() {
  let millis = Duration::from_millis;
  // When `d` comes back, in milliseconds after the start. `None`: never.
  for back_at in [None, Some(50), Some(120)] {
    let mut d = inode();
    d.fs.mkdir_all("/p/r/d/sub").unwrap();
    d.fs.mkdir("/q").unwrap();
    d.fs.write("/p/r/d/sub/f", "1").unwrap();
    d.fs.write("/p/r/d/g", "1").unwrap();
    d.watch(&["/p/r"]);
    let start = d.now;

    // At 10 ms `sub` is held until 100 ms (its hint came at 0 ms). At 40 ms `d` is held until
    // 130 ms (its hint came at 30 ms).
    d.fs.remove_all("/p/r/d/sub").unwrap();
    flush(&mut d);
    d.advance(millis(20));
    d.fs.rename("/p/r/d", "/q/d").unwrap();
    flush(&mut d);
    assert_eq!(d.now, start + millis(40));
    assert_eq!(d.engine.pending_deletes().len(), 2);

    match back_at {
      None => {
        // The time of `sub` comes first. Nothing happens: `d` decides, at its time.
        assert!(d.step());
        assert_eq!(d.now, start + millis(100));
        assert_eq!(taken(&mut d), NONE);
        assert_eq!(d.engine.pending_deletes().len(), 2);
        assert!(d.step());
        assert_eq!(d.now, start + millis(130));
        assert_eq!(taken(&mut d), ["D /p/r/d/g", "D /p/r/d/sub/f"]);
      }
      Some(50) => {
        // `d` is back before the time of `sub`, without it. Its own file is the same: no
        // event. `sub` keeps its time.
        d.advance(millis(10));
        d.fs.rename("/q/d", "/p/r/d").unwrap();
        flush(&mut d);
        assert_eq!(d.now, start + millis(60));
        assert_eq!(taken(&mut d), NONE);
        assert!(d.step());
        assert_eq!(d.now, start + millis(100));
        assert_eq!(taken(&mut d), ["D /p/r/d/sub/f"]);
      }
      Some(_) => {
        // `d` is back after the time of `sub`, which passed while `d` decided for both. So
        // this is the first look at `sub` that counts: it is held again, for one more delay
        // from that look, since no hint asked for it.
        assert!(d.step());
        assert_eq!(d.now, start + millis(100));
        d.advance(millis(10));
        d.fs.rename("/q/d", "/p/r/d").unwrap();
        flush(&mut d);
        assert_eq!(d.now, start + millis(120));
        assert_eq!(taken(&mut d), NONE);
        assert_eq!(d.engine.pending_deletes().len(), 1);
        // The old time of `d` passes. It means nothing any more.
        assert!(d.step());
        assert_eq!(d.now, start + millis(130));
        assert_eq!(taken(&mut d), NONE);
        assert!(d.step());
        assert_eq!(d.now, start + millis(220));
        assert_eq!(taken(&mut d), ["D /p/r/d/sub/f"]);
      }
    }
    assert_eq!(settled(&mut d), NONE);
    caught_up(&mut d);
  }
}

/// The same for the poll, which looks at every directory by itself: while a directory is held,
/// a poll looks at that directory and at nothing below it.
#[test]
fn a_poll_does_not_go_below_a_held_directory() {
  let options = Options { poll_interval: Duration::from_millis(20), ..Options::default() };
  let mut d = Driver::poll(options);
  d.fs.mkdir_all("/p/r/d/sub").unwrap();
  d.fs.mkdir("/q").unwrap();
  d.fs.write("/p/r/d/sub/f", "1").unwrap();
  d.fs.write("/p/r/d/g", "1").unwrap();
  d.watch(&["/p/r"]);

  d.fs.rename("/p/r/d", "/q/d").unwrap();
  assert!(d.step());
  assert_eq!(taken(&mut d), NONE);
  assert_eq!(d.engine.pending_deletes().len(), 2);
  // Three more polls inside the delay. Each one looks at `/`, `/p`, `/p/r` and `d`, and
  // lists `/p/r`, with a look at it when the listing is done.
  for _ in 0..3 {
    let before = d.fs.calls();
    assert!(d.step());
    assert_eq!(taken(&mut d), NONE);
    let after = d.fs.calls();
    assert_eq!((after.lstat - before.lstat, after.read_dir - before.read_dir), (5, 1));
  }
  assert_eq!(settled(&mut d), ["D /p/r/d/g", "D /p/r/d/sub/f"]);
  caught_up(&mut d);
}

/// A file is removed, comes back, and is removed again, all inside one delay. The second
/// `Delete` waits for its own delay, not for the rest of the first one.
#[test]
fn a_file_that_is_held_twice_waits_for_its_second_time() {
  let millis = Duration::from_millis;
  let mut d = inode();
  d.fs.mkdir("/p").unwrap();
  d.fs.write("/p/a", "1").unwrap();
  d.watch(&["/p/a"]);
  let start = d.now;

  // Held at 10 ms, until 100 ms: the delay counts from the hint at 0 ms.
  d.fs.remove("/p/a").unwrap();
  flush(&mut d);
  d.advance(millis(30));
  d.fs.write("/p/a", "22").unwrap();
  flush(&mut d);
  assert_eq!(d.now, start + millis(50));
  assert_eq!(taken(&mut d), ["U /p/a"]);
  // Held again at 80 ms, until 170 ms.
  d.advance(millis(20));
  d.fs.remove("/p/a").unwrap();
  flush(&mut d);
  assert_eq!(d.now, start + millis(80));

  // The first time is over. It means nothing any more, and it costs no look at the disk.
  let before = d.fs.calls();
  assert!(d.step());
  assert_eq!(d.now, start + millis(100));
  assert_eq!(taken(&mut d), NONE);
  assert_eq!(d.fs.calls(), before);
  assert_eq!(d.engine.pending_deletes().len(), 1);
  assert!(d.step());
  assert_eq!(d.now, start + millis(170));
  assert_eq!(taken(&mut d), ["D /p/a"]);
  caught_up(&mut d);
}

/// A tree that is gone for good is not watched any more: when it lives on at another place,
/// nothing from there reaches the engine. With the delete delay and without it.
#[test]
fn a_tree_that_is_gone_is_not_watched_any_more() {
  for mode in Mode::ALL {
    for delete_delay in [Duration::ZERO, Duration::from_millis(100)] {
      let on = format!("{mode:?}, delay {delete_delay:?}");
      let mut d = Driver::sim(mode, Options { delete_delay, ..Options::default() });
      d.fs.mkdir_all("/p/r/d/sub").unwrap();
      d.fs.mkdir("/q").unwrap();
      d.fs.write("/p/r/d/sub/f", "1").unwrap();
      d.watch(&["/p/r"]);

      d.fs.rename("/p/r/d", "/q/d").unwrap();
      assert_eq!(settled(&mut d), ["D /p/r/d/sub/f"], "{on}");
      d.fs.write("/q/d/sub/f", "22").unwrap();
      d.fs.write("/q/d/new", "1").unwrap();
      assert_eq!(d.fs.pending_raw(), 0, "{on}: a watch went away with the tree");
      assert!(d.fs.os_watches().iter().all(|(dir, _)| !dir.starts_with("/q")), "{on}");
      assert_eq!(settled(&mut d), NONE, "{on}");
      caught_up(&mut d);
    }
  }
}

/// A watched file is seen missing and then seen again, in one flush: its mark is checked
/// first, and then everything is scanned. With no delete delay the first look forgets the
/// file. The handler still believes in it, and nothing says that the file at the second look
/// is the same one: that is an `Update`.
///
/// The file comes back at any moment of that flush. Before the first look and after the second
/// one, the events are the plain ones.
#[test]
fn a_file_goes_and_comes_inside_one_flush() {
  fn run<B: Deliver>(make: impl Fn() -> Driver<B>, on: &str) {
    let prepare = || {
      let mut d = make();
      d.fs.mkdir_all("/p/r").unwrap();
      d.fs.write("/p/r/f", "1").unwrap();
      d.watch(&["/p/r"]);
      d.fs.remove("/p/r/f").unwrap();
      d.deliver(usize::MAX);
      d.engine.overflow(d.now);
      d
    };
    let mut d = prepare();
    let before = d.fs.calls().total();
    assert!(d.step());
    let calls = d.fs.calls().total() - before;
    assert_eq!(taken(&mut d), ["D /p/r/f"], "{on}");
    caught_up(&mut d);

    let mut updates = 0;
    for moment in 0..calls {
      let mut d = prepare();
      d.fs.at_call(moment, |fs| fs.write("/p/r/f", "22").unwrap());
      assert!(d.step());
      assert_eq!(d.fs.traps_run(), 1, "{on}, call {moment}");
      let mut events = taken(&mut d);
      events.append(&mut settled(&mut d));
      // A "written" notification of the new file may cost one more update at the end.
      if events.len() > 1 && events.last().is_some_and(|event| event == "U /p/r/f") {
        events.pop();
      }
      if events == ["U /p/r/f"] {
        updates += 1;
      } else {
        assert_eq!(events, ["D /p/r/f", "C /p/r/f"], "{on}: written before call {moment}");
      }
      caught_up(&mut d);
    }
    assert!(updates > 0, "{on}: the file never came back between the two looks");
  }
  let options = Options { delete_delay: Duration::ZERO, ..Options::default() };
  for mode in Mode::ALL {
    run(|| Driver::sim(mode, options.clone()), &format!("{mode:?}"));
  }
  run(|| Driver::poll(options.clone()), "Poll");
}

/// The second look gets no answer from the disk. The `Delete` stays held until the disk
/// answers: a file is never reported as gone because of an error that says nothing.
#[test]
fn the_second_look_waits_for_the_disk() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir("/p").unwrap();
    d.fs.write("/p/a", "1").unwrap();
    d.fs.write("/p/gone", "1").unwrap();
    d.watch(&["/p"]);

    d.fs.remove("/p/a").unwrap();
    d.fs.remove("/p/gone").unwrap();
    flush(&mut d);
    // One of the two is back, and the notification is lost.
    d.fs.write("/p/a", "22").unwrap();
    d.fs.take_raw(usize::MAX);
    d.fs.set_offline(Some(io::ErrorKind::TimedOut));
    assert!(d.step());
    assert_eq!(taken(&mut d), NONE, "{mode:?}");
    assert_eq!(d.engine.pending_deletes().len(), 2, "{mode:?}");
    assert_eq!(d.errors.len(), 1, "{mode:?}");
    assert_eq!(d.errors[0].kind(), ErrorKind::Io);

    d.fs.set_offline(None);
    assert_eq!(settled(&mut d), ["U /p/a", "D /p/gone"], "{mode:?}");
    caught_up(&mut d);
  }
}

/// A held file is back when its second look comes, and it goes away again at any moment of
/// that look, also between the `lstat` and the read of its content. No notification of all
/// that arrives. The engine saw a file and then no file in one look, so it looks again by
/// itself: the `Delete` comes, and the node does not stay held for ever.
#[test]
fn a_file_is_back_and_gone_again_during_its_second_look() {
  fn run<B: Deliver>(make: impl Fn() -> Driver<B>, on: &str) {
    // The file is held, and back with other content. Nobody says so.
    let prepare = || {
      let mut d = make();
      d.fs.mkdir("/p").unwrap();
      d.fs.write("/p/a", "1").unwrap();
      d.watch(&["/p/a"]);
      d.fs.remove("/p/a").unwrap();
      flush(&mut d);
      assert_eq!(d.engine.pending_deletes().len(), 1, "{on}");
      d.fs.write("/p/a", "22").unwrap();
      d.fs.take_raw(usize::MAX);
      d
    };
    let mut d = prepare();
    let before = d.fs.calls().total();
    assert!(d.step());
    let calls = d.fs.calls().total() - before;
    assert_eq!(taken(&mut d), ["U /p/a"], "{on}");
    caught_up(&mut d);

    for moment in 0..calls {
      let mut d = prepare();
      d.fs.at_call(moment, |fs| fs.remove("/p/a").unwrap());
      assert!(d.step());
      assert_eq!(d.fs.traps_run(), 1, "{on}, call {moment}");
      d.fs.take_raw(usize::MAX);
      let mut events = taken(&mut d);
      events.append(&mut settled(&mut d));
      // The look may have seen the new content before the file went.
      events.retain(|event| event != "U /p/a");
      assert_eq!(events, ["D /p/a"], "{on}: removed before call {moment} of the second look");
      caught_up(&mut d);
    }
  }
  for mode in Mode::ALL {
    run(|| Driver::sim(mode, Options::default()), &format!("{mode:?}"));
  }
  run(|| Driver::poll(Options::default()), "Poll");
}

// ---- what a hold keeps, and what it does not change ----

/// A file that the handler does not know is never held: when it is gone, it is forgotten. So
/// for such a file the delete delay changes nothing.
///
/// A new file has a notification, and notifications were lost too: the flush checks the file
/// first and scans everything after that, so it can look at the file twice. The file goes away
/// at any moment of that flush. With the delay and without it, the events are the same, and
/// nothing waits for a second look.
#[test]
fn the_delay_changes_nothing_for_a_file_the_handler_does_not_know() {
  fn run<B: Deliver>(make: impl Fn(Duration) -> Driver<B>, on: &str) {
    let prepare = |delay| {
      let mut d = make(delay);
      d.fs.mkdir_all("/p/r").unwrap();
      d.watch(&["/p/r"]);
      d.fs.write("/p/r/f", "1").unwrap();
      d.deliver(usize::MAX);
      d.engine.overflow(d.now);
      d
    };
    let mut d = prepare(Duration::from_millis(100));
    let before = d.fs.calls().total();
    assert!(d.step());
    let calls = d.fs.calls().total() - before;
    assert_eq!(taken(&mut d), ["C /p/r/f"], "{on}");
    caught_up(&mut d);

    let mut silent = 0;
    for moment in 0..calls {
      let events = [Duration::ZERO, Duration::from_millis(100)].map(|delay| {
        let mut d = prepare(delay);
        d.fs.at_call(moment, |fs| fs.remove("/p/r/f").unwrap());
        assert!(d.step());
        assert_eq!(d.fs.traps_run(), 1, "{on}, call {moment}");
        let pending = d.engine.pending_deletes();
        assert!(pending.is_empty(), "{on}, call {moment}, delay {delay:?}: {pending:?}");
        let mut events = taken(&mut d);
        events.append(&mut settled(&mut d));
        // The notification of the write may cost one update before the `Delete`.
        events.retain(|event| event != "U /p/r/f");
        caught_up(&mut d);
        events
      });
      assert_eq!(events[0], events[1], "{on}: removed before call {moment}");
      silent += u64::from(events[1].is_empty());
    }
    // Early in the flush the file is never told. Late in it, it is told and then deleted.
    // With polling there is no "late": the one look at the file is the end of the flush.
    assert!(silent > 0 && (silent < calls || on == "Poll"), "{on}: {silent} of {calls}");
  }
  for mode in Mode::ALL {
    let make = |delete_delay| Driver::sim(mode, Options { delete_delay, ..Options::default() });
    run(make, &format!("{mode:?}"));
  }
  run(|delete_delay| Driver::poll(Options { delete_delay, ..Options::default() }), "Poll");
}

/// A file is moved away and back inside the delay. It is the same file: nothing is reported.
/// Only inotify cannot know that. It says "written" for every file that is moved in, and that
/// costs an update.
#[test]
fn the_same_file_is_back_inside_the_delay() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir("/p").unwrap();
    d.fs().mkdir("/q").unwrap();
    d.fs().write("/p/a", "1").unwrap();
    assert_eq!(d.watch(&["/p/a"]), NONE, "{on}");

    d.fs().rename("/p/a", "/q/a").unwrap();
    assert_eq!(d.window(), NONE, "{on}");
    assert_eq!(d.pending_deletes(), 1, "{on}");
    d.fs().rename("/q/a", "/p/a").unwrap();
    let expected: &[&str] = if on == "Inode" { &["U /p/a"] } else { &[] };
    assert_eq!(d.settled(), expected, "{on}");
    assert_eq!(d.known(), ["/p/a"], "{on}");
    d.caught_up();
  });
}

/// A held file keeps what the tree had for it, also the hash of a young file. The file is
/// removed and written again in the same step of the clock, with the same size, and the disk
/// gives the new file the inode number of the old one: the fingerprint is the same, and only
/// the hash tells that it is another file.
#[test]
fn a_young_file_that_is_back_is_compared_by_its_hash() {
  on_all(&Options::default(), |d, on| {
    d.fs().set_time_step(Duration::from_secs(1));
    d.fs().mkdir("/p").unwrap();
    d.fs().write("/p/a", "x=1;").unwrap();
    assert_eq!(d.watch(&["/p/a"]), NONE, "{on}");

    d.fs().remove("/p/a").unwrap();
    assert_eq!(d.window(), NONE, "{on}");
    assert_eq!(d.pending_deletes(), 1, "{on}");
    d.fs().write("/p/a", "x=2;").unwrap();
    assert_eq!(d.window(), ["U /p/a"], "{on}");
    assert_eq!(d.settled(), NONE, "{on}");
    d.caught_up();
  });
}

/// A file and a directory are held. Then the directory above them goes away, and the flush
/// that finds out is the one of their second look. That look is not made: the directory above
/// decides for everything below it now, and their own holds end. Then the directory above
/// comes back without the two, and from some call on the disk answers nothing. Whatever the
/// moment: the tree stays in order, and when the disk answers again, the two `Delete`s come.
#[test]
fn a_directory_is_back_and_the_disk_stops_answering() {
  fn run<B: Deliver>(make: impl Fn() -> Driver<B>, on: &str) {
    let prepare = || {
      let mut d = make();
      d.fs.mkdir_all("/p/r/d/sub").unwrap();
      d.fs.mkdir("/q").unwrap();
      d.fs.write("/p/r/d/f", "1").unwrap();
      d.fs.write("/p/r/d/keep", "1").unwrap();
      d.fs.write("/p/r/d/sub/g", "1").unwrap();
      d.watch(&["/p/r"]);
      d.fs.remove("/p/r/d/f").unwrap();
      d.fs.remove_all("/p/r/d/sub").unwrap();
      flush(&mut d);
      assert_eq!(d.engine.pending_deletes().len(), 2, "{on}");

      // `d` goes away one millisecond before the second look of the two.
      let second_look = d.engine.deadline().unwrap();
      let wait = second_look.saturating_duration_since(d.now);
      d.advance(wait.saturating_sub(Duration::from_millis(1)));
      d.fs.rename("/p/r/d", "/q/d").unwrap();
      d.deliver(usize::MAX);
      assert!(d.step(), "{on}");
      assert_eq!(d.now, second_look, "{on}");
      assert_eq!(taken(&mut d), NONE, "{on}");
      assert_eq!(d.engine.pending_deletes().len(), 3, "{on}");

      d.fs.rename("/q/d", "/p/r/d").unwrap();
      d.deliver(usize::MAX);
      d
    };
    let mut d = prepare();
    let before = d.fs.calls().total();
    assert!(d.step());
    let calls = d.fs.calls().total() - before;
    let mut events = taken(&mut d);
    events.append(&mut settled(&mut d));
    events.sort();
    assert_eq!(events, ["D /p/r/d/f", "D /p/r/d/sub/g"], "{on}");
    caught_up(&mut d);

    for moment in 0..calls {
      let mut d = prepare();
      d.fs.at_call(moment, |fs| fs.set_offline(Some(io::ErrorKind::TimedOut)));
      assert!(d.step());
      assert_eq!(d.fs.traps_run(), 1, "{on}, call {moment}");
      d.fs.set_offline(None);
      let mut events = taken(&mut d);
      events.append(&mut settled(&mut d));
      // A young file whose hash could not be read may count as changed.
      events.retain(|event| !event.starts_with("U "));
      events.sort();
      assert_eq!(events, ["D /p/r/d/f", "D /p/r/d/sub/g"], "{on}: no answer from call {moment}");
      caught_up(&mut d);
    }
  }
  for mode in Mode::ALL {
    run(|| Driver::sim(mode, Options::default()), &format!("{mode:?}"));
  }
  // With polling, the poll and the second look fall into one flush.
  let options = Options { poll_interval: Duration::from_millis(100), ..Options::default() };
  run(|| Driver::poll(options.clone()), "Poll");
}

// ---- cases of the design review ----

/// A lonely pending `Delete` goes out one delete delay after the hint, in a batch of its own.
#[test]
fn the_deadline_of_a_pending_delete() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir("/p").unwrap();
    d.fs.write("/p/x", "1").unwrap();
    d.fs.write("/p/y", "1").unwrap();
    d.watch(&["/p"]);
    let start = d.now;
    d.fs.remove("/p/x").unwrap();
    assert_eq!(Sim::window(&mut d), NONE, "{mode:?}");
    assert_eq!(d.now, start + Duration::from_millis(10), "{mode:?}");
    assert_eq!(d.engine.deadline(), Some(start + Duration::from_millis(100)), "{mode:?}");
    assert!(d.step());
    assert_eq!(show(&d.take_events()), ["D /p/x"], "{mode:?}");
    caught_up(&mut d);
  }
}

/// A pending `Delete` waits no longer when the kind of something at or above its path
/// changes: it goes into the same batch, in front of the `Create`s below it.
#[test]
fn a_pending_delete_and_a_change_of_kind() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/W/d").unwrap();
    d.fs().write("/p/R", "file").unwrap();
    d.fs().write("/p/W/d/f", "1").unwrap();
    d.fs().write("/p/W/keep", "1").unwrap();
    assert_eq!(d.watch(&["/p/R", "/p/W"]), NONE, "{on}");

    // A file is deleted, then a directory with a file comes at its path.
    d.fs().remove("/p/R").unwrap();
    assert_eq!(d.window(), NONE, "{on}");
    assert_eq!(d.pending_deletes(), 1, "{on}");
    d.fs().mkdir("/p/R").unwrap();
    d.fs().write("/p/R/x", "1").unwrap();
    assert_eq!(d.window(), ["D /p/R", "C /p/R/x"], "{on}");

    // The mirror: the deletes that `vanish` made are pending too, and a file at the path
    // of the directory sends them out with its `Create`.
    d.fs().remove_all("/p/W/d").unwrap();
    assert_eq!(d.window(), NONE, "{on}");
    assert_eq!(d.pending_deletes(), 1, "{on}");
    d.fs().write("/p/W/d", "a file now").unwrap();
    assert_eq!(d.window(), ["C /p/W/d", "D /p/W/d/f"], "{on}");

    // A file that came and went inside one flush is nothing: no delete becomes pending.
    d.fs().write("/p/W/short", "1").unwrap();
    d.fs().remove("/p/W/short").unwrap();
    assert_eq!(d.window(), NONE, "{on}");
    assert_eq!(d.pending_deletes(), 0, "{on}");
    assert_eq!(d.settled(), NONE, "{on}");
    d.caught_up();
  });
}

/// Hints keep coming, and a file is removed. The quiet time does not stretch the delete delay:
/// the look at the longest wait finds nothing, the delay since the hint is over by then, and
/// the `Delete` goes out with the next batch. The hints wait for their quiet time meanwhile:
/// the log gets its next `Update` at the longest wait, after the last of them.
#[test]
fn the_delete_delay_holds_while_hints_keep_coming() {
  // No poll comes in between.
  let options = Options { poll_interval: Duration::from_secs(60), ..Options::default() };
  let mut d = Driver::sim(Mode::Inode, options);
  d.fs.mkdir_all("/p/logs").unwrap();
  d.fs.write("/p/a.js", "1").unwrap();
  d.fs.write("/p/logs/x.log", "").unwrap();
  d.watch(&["/p"]);
  let start = d.now;
  d.fs.remove("/p/a.js").unwrap();
  // A tool appends to a log every 3 ms. No 10 ms pass without a hint.
  let mut batches = Vec::new();
  for round in 1..=250 {
    d.fs.write("/p/logs/x.log", "line\n".repeat(round)).unwrap();
    d.deliver(usize::MAX);
    d.advance(Duration::from_millis(3));
    d.tick();
    let events = show(&d.take_events());
    if !events.is_empty() {
      batches.push(((d.now - start).as_millis(), events));
    }
  }
  // The look at 501 ms finds nothing at `/p/a.js`, 100 ms after its hint are long over: the
  // second look is due at once, and the next tick makes it, whatever the hints do.
  let log = vec!["U /p/logs/x.log".to_owned()];
  let delete = vec!["D /p/a.js".to_owned()];
  assert_eq!(batches, [(501, log.clone()), (504, delete)]);
  assert_eq!(settled(&mut d), log);
  caught_up(&mut d);
}

/// (d): an editor save (write a temporary file, rename the old one to a backup, rename the new
/// one in, delete the backup) is one `Update`, for a watched file and for a watched directory,
/// in one window and split over four.
#[test]
fn case_d_editor_save() {
  on_all(&Options::default(), |d, on| {
    for (watched, split) in [("/1/p/a.js", false), ("/2/p", false), ("/3/p/a.js", true)] {
      let dir = &watched[..4];
      d.fs().mkdir_all(dir).unwrap();
      d.fs().write(format!("{dir}/a.js"), "old").unwrap();
      assert_eq!(d.watch(&[watched]), NONE, "{on} {watched}");

      let mut events = Vec::new();
      let changes: [&dyn Fn(&SimFs); 4] = [
        &|fs| fs.write(format!("{dir}/a.js.tmp"), "new content").unwrap(),
        &|fs| fs.rename(format!("{dir}/a.js"), format!("{dir}/a.js~")).unwrap(),
        &|fs| fs.rename(format!("{dir}/a.js.tmp"), format!("{dir}/a.js")).unwrap(),
        &|fs| fs.remove(format!("{dir}/a.js~")).unwrap(),
      ];
      for change in changes {
        change(d.fs());
        if split && d.fs().pending_raw() > 0 {
          events.append(&mut d.window());
        }
      }
      events.append(&mut d.settled());
      assert_eq!(events, [format!("U {dir}/a.js")], "{on} {watched}");
    }
    d.caught_up();
  });
}

/// (c): a `git checkout` that touches many files, with a flush in the middle of it. Whatever
/// the split, the last event of every path tells its state, and nothing else gets an event.
#[test]
fn case_c_git_checkout() {
  for mode in Mode::ALL {
    for cut in [0, 3, 7, 12, usize::MAX] {
      let mut d = Driver::sim(mode, Options::default());
      for dir in ["src", "src/old", "lib"] {
        d.fs.mkdir_all(format!("/repo/{dir}")).unwrap();
        for file in ["a", "b", "c"] {
          d.fs.write(format!("/repo/{dir}/{file}.js"), "1").unwrap();
        }
      }
      d.watch(&["/repo"]);

      // Each changed file is replaced by "unlink, create", as git does.
      for file in ["src/a.js", "lib/a.js", "lib/b.js"] {
        d.fs.remove(format!("/repo/{file}")).unwrap();
        d.fs.write(format!("/repo/{file}"), "22").unwrap();
      }
      d.deliver(cut);
      d.advance(Duration::from_millis(10));
      d.tick();
      d.fs.remove("/repo/src/c.js").unwrap();
      d.fs.remove_all("/repo/src/old").unwrap();
      d.fs.mkdir("/repo/new").unwrap();
      d.fs.write("/repo/new/x.js", "1").unwrap();
      d.fs.write("/repo/lib/d.js", "1").unwrap();
      let events = Sim::settled(&mut d);

      let last: BTreeMap<String, char> =
        events.iter().map(|event| (event[2..].to_owned(), event.chars().next().unwrap())).collect();
      let expected = [
        ("/repo/lib/a.js", 'U'),
        ("/repo/lib/b.js", 'U'),
        ("/repo/lib/d.js", 'C'),
        ("/repo/new/x.js", 'C'),
        ("/repo/src/a.js", 'U'),
        ("/repo/src/c.js", 'D'),
        ("/repo/src/old/a.js", 'D'),
        ("/repo/src/old/b.js", 'D'),
        ("/repo/src/old/c.js", 'D'),
      ];
      let expected: BTreeMap<String, char> =
        expected.iter().map(|(path, kind)| ((*path).to_owned(), *kind)).collect();
      assert_eq!(last, expected, "{mode:?}, cut at {cut}");
      // A replaced file is never reported as deleted: the delete delay joins the pair.
      assert!(!events.iter().any(|event| event == "D /repo/src/a.js"), "{mode:?}, cut at {cut}");
      caught_up(&mut d);
    }
  }
}

/// (e): `rm -r` and a copy with the same names: one `Update` for a name that is still there,
/// `Delete` for the rest, `Create` for the new ones.
#[test]
fn case_e_remove_and_copy_again() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/D/sub").unwrap();
    for file in ["x", "y", "z", "sub/s"] {
      d.fs().write(format!("/p/D/{file}"), "1").unwrap();
    }
    assert_eq!(d.watch(&["/p/D"]), NONE, "{on}");

    d.fs().remove_all("/p/D").unwrap();
    d.fs().mkdir_all("/p/D/sub").unwrap();
    for file in ["x", "y", "w", "sub/s"] {
      d.fs().write(format!("/p/D/{file}"), "copied").unwrap();
    }
    assert_eq!(
      window_then_settled(d),
      ["C /p/D/w", "D /p/D/z", "U /p/D/sub/s", "U /p/D/x", "U /p/D/y"],
      "{on}"
    );
    d.caught_up();
  });
}

/// The delay of a `Delete` that no hint asked for counts from the look that found nothing, not
/// from the start of the flush: a flush can take long before it reaches the file.
#[test]
fn the_delay_counts_from_the_look() {
  let mut d = Driver::poll(Options::default());
  d.fs.mkdir_all("/p/r").unwrap();
  d.fs.write("/p/r/a", "1").unwrap();
  d.watch(&["/p/r"]);

  d.fs.remove("/p/r/a").unwrap();
  // The disk takes 300 ms before the first look of the poll.
  d.fs.at_call(0, |fs| fs.advance(Duration::from_millis(300)));
  assert!(d.step());
  assert_eq!(taken(&mut d), NONE);
  assert_eq!(d.engine.pending_deletes().len(), 1);
  // The second look comes 100 ms after the first, which was 300 ms into the flush.
  assert_eq!(d.engine.deadline(), Some(d.now + Duration::from_millis(400)));
  assert_eq!(settled(&mut d), ["D /p/r/a"]);
  caught_up(&mut d);
}
