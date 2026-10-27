//! A directory is renamed, replaced by another one, or away and back.
//! A story makes the disk, calls `watch`, changes the disk and compares the events that come
//! out (`C`, `U`, `D` and the path). `on_all` tells one story on every backend.

use std::{path::Path, time::Duration};

use crate::fixtures::{
  Driver, Mode, NONE, RECURSIVE, Sim, SimRaw, caught_up, flush, on_a_small_stack, on_all, on_modes,
  settled, sorted, taken, window, window_then_settled,
};
use pathwatch::{ErrorKind, engine::Options};

/// The watched directory is renamed away and back while its tree is read, at every moment of
/// the call. The listing that fails finds the directory again from above, and nothing below it
/// is lost: the handle of Windows reports no rename of its own directory, so nothing else
/// would bring the scan back.
#[test]
fn a_directory_away_and_back_while_its_tree_is_read() {
  for mode in Mode::ALL {
    let prepare = || {
      let d = Driver::sim(mode, Options::default());
      d.fs.mkdir_all("/p/r/sub/deep").unwrap();
      d.fs.write("/p/r/a", "1").unwrap();
      d.fs.write("/p/r/sub/b", "1").unwrap();
      d.fs.write("/p/r/sub/deep/c", "1").unwrap();
      d
    };
    let mut d = prepare();
    let before = d.fs.calls().total();
    d.watch(&["/p/r"]);
    let calls = d.fs.calls().total() - before;
    caught_up(&mut d);

    for moment in 0..calls {
      let mut d = prepare();
      d.fs.at_call(moment, |fs| fs.rename("/p/r", "/p/away").unwrap());
      d.fs.at_call(moment + 1, |fs| fs.rename("/p/away", "/p/r").unwrap());
      d.watch(&["/p/r"]);
      if d.fs.traps_run() < 2 {
        d.fs.clear_traps();
        d.fs.rename("/p/away", "/p/r").unwrap();
      }
      let mut events = taken(&mut d);
      events.append(&mut settled(&mut d));
      assert!(
        events.iter().all(|event| !event.starts_with('D')),
        "{mode:?}, call {moment}: {events:?}"
      );
      assert_eq!(d.known(), ["/p/r/a", "/p/r/sub/b", "/p/r/sub/deep/c"], "{mode:?}, call {moment}");
      caught_up(&mut d);
    }
  }
}

/// A tree deeper than the stack of a thread takes is moved away: its nodes vanish from a list,
/// not by a recursion down the tree.
#[test]
fn a_tree_deeper_than_the_stack_is_moved_away() {
  on_a_small_stack(|| {
    let mut d = Driver::sim(Mode::Handle, Options::default());
    let chain = "/p".to_owned() + &"/d".repeat(1500);
    d.fs.mkdir_all(&chain).unwrap();
    d.fs.write(format!("{chain}/leaf"), "1").unwrap();
    d.watch(&["/p"]);
    d.fs.rename("/p/d", "/away").unwrap();
    assert_eq!(settled(&mut d), [format!("D {chain}/leaf")]);
    caught_up(&mut d);
  });
}

/// The sister case: no birth time, and the new directory gets the inode number of the
/// removed one. It is a new directory all the same, and it is watched.
#[test]
fn a_new_directory_with_the_old_inode_number_is_watched() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/R/sub").unwrap();
    d.fs().write("/p/R/sub/a.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R"]), NONE, "{on}");

    d.fs().remove_all("/p/R/sub").unwrap();
    d.fs().mkdir("/p/R/sub").unwrap();
    d.fs().write("/p/R/sub/b.js", "1").unwrap();
    assert_eq!(window_then_settled(d), ["C /p/R/sub/b.js", "D /p/R/sub/a.js"], "{on}");
    d.fs().write("/p/R/sub/c.js", "1").unwrap();
    assert_eq!(d.window(), ["C /p/R/sub/c.js"], "{on}");
    d.caught_up();
  });
}

/// The round trip of S13 inside one window, for a watched file, for the directory itself
/// as a watched path, and for a watched directory above it.
#[test]
fn away_and_back_inside_one_window() {
  on_all(&Options::default(), |d, on| {
    for (number, watched) in ["/proj/lib/a.js", "/proj/lib", "/proj"].iter().enumerate() {
      let base = format!("/{number}");
      let at = |path: &str| format!("{base}{path}");
      d.fs().mkdir_all(at("/proj/lib")).unwrap();
      d.fs().write(at("/proj/lib/a.js"), "1").unwrap();
      assert_eq!(d.watch(&[at(watched).as_str()]), NONE, "{on} {watched}");

      d.fs().rename(at("/proj/lib"), at("/proj/lib2")).unwrap();
      d.fs().write(at("/proj/lib2/a.js"), "edited").unwrap();
      d.fs().write(at("/proj/lib2/new.js"), "1").unwrap();
      d.fs().rename(at("/proj/lib2"), at("/proj/lib")).unwrap();
      let mut expected = vec![format!("U {}", at("/proj/lib/a.js"))];
      if number > 0 {
        expected.push(format!("C {}", at("/proj/lib/new.js")));
      }
      assert_eq!(sorted(d.window()), sorted(expected), "{on} {watched}");
      assert_eq!(d.settled(), NONE, "{on} {watched}");
    }
    d.caught_up();
  });
}

/// `watch` checks a file inside a directory that the tree still holds for another directory.
/// The same flush then finds the directory replaced. The file was never known to the handler,
/// so nothing is said about it.
#[test]
fn the_baseline_inside_a_replaced_directory() {
  let mut d = Driver::sim(Mode::Inode, Options::default());
  d.fs.mkdir_all("/p/d").unwrap();
  d.fs.mkdir_all("/p/next").unwrap();
  d.fs.write("/p/d/f", "1").unwrap();
  d.fs.write("/p/next/f", "22").unwrap();
  d.watch(&["/p/d/other"]);

  d.fs.rename("/p/d", "/p/old").unwrap();
  d.fs.rename("/p/next", "/p/d").unwrap();
  d.fs.take_raw(usize::MAX);
  d.engine.overflow(d.now);
  d.watch(&["/p/d/f"]);
  assert_eq!(taken(&mut d), NONE);
  assert_eq!(d.known.iter().collect::<Vec<_>>(), [Path::new("/p/d/f")]);
  assert_eq!(settled(&mut d), NONE);
  d.fs.write("/p/d/f", "333").unwrap();
  assert_eq!(settled(&mut d), ["U /p/d/f"]);
  caught_up(&mut d);
}

/// A directory above a known file is replaced, and in the new one the path of the file is a
/// directory. The old directory going away does not hold the `Delete` of the file back: it comes
/// in the same batch as the `Create` below its path (design.md 4.3), so the handler never knows a
/// file and a file below it.
#[test]
fn a_file_becomes_a_directory_when_its_parent_is_replaced() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir_all("/a/c").unwrap();
    d.fs.mkdir_all("/new/c/b/a").unwrap();
    d.fs.write("/a/c/b", "a file").unwrap();
    d.fs.write("/new/c/b/a/a", "1").unwrap();
    d.watch(&["/a/c/b", "/a/c/b/a"]);

    d.fs.rename("/a", "/old").unwrap();
    d.fs.rename("/new", "/a").unwrap();
    // The window in which the new file shows up. At most a few polls away.
    let mut events = Vec::new();
    for _ in 0..10 {
      events = window(&mut d);
      if events.iter().any(|event| event == "C /a/c/b/a/a") {
        break;
      }
    }
    assert_eq!(events, ["D /a/c/b", "C /a/c/b/a/a"], "{mode:?}");
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    caught_up(&mut d);
  }
}

/// The mirror: the files of a directory go away with the directory above it, and in the new
/// one a file is at the path of their directory.
#[test]
fn a_directory_becomes_a_file_when_its_parent_is_replaced() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir_all("/p/D/deep").unwrap();
    d.fs.mkdir("/new").unwrap();
    d.fs.write("/p/D/x", "1").unwrap();
    d.fs.write("/p/D/deep/y", "1").unwrap();
    d.fs.write("/new/D", "a file").unwrap();
    d.watch(&["/p/D"]);

    d.fs.rename("/p", "/old").unwrap();
    d.fs.rename("/new", "/p").unwrap();
    let mut events = Vec::new();
    for _ in 0..10 {
      events = window(&mut d);
      if events.iter().any(|event| event == "C /p/D") {
        break;
      }
    }
    assert_eq!(events, ["C /p/D", "D /p/D/deep/y", "D /p/D/x"], "{mode:?}");
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    caught_up(&mut d);
  }
}

/// A handle follows its directory. While the directory is away, a write in it is still
/// reported, and the path of the watched file is missing: the `Delete` is right. When the
/// directory comes back, the file is created again for the handler.
#[test]
fn a_hint_through_a_watch_that_moved_away() {
  let mut d = Driver::sim(Mode::Handle, Options::default());
  d.fs.mkdir_all("/q/p").unwrap();
  d.fs.write("/q/p/f.js", "1").unwrap();
  d.watch(&["/q/p/f.js"]);

  d.fs.rename("/q/p", "/q/p2").unwrap();
  d.fs.write("/q/p2/f.js", "22").unwrap();
  assert_eq!(d.fs.pending_raw(), 1);
  assert_eq!(window(&mut d), NONE);
  d.advance(Duration::from_millis(200));
  d.tick();
  assert_eq!(taken(&mut d), ["D /q/p/f.js"]);

  // The look that did not find `p` made `/q` tell its contents, so the way back is
  // reported now, and no poll is needed.
  d.fs.rename("/q/p2", "/q/p").unwrap();
  assert!(d.fs.pending_raw() > 0);
  assert_eq!(window(&mut d), ["C /q/p/f.js"]);
  d.fs.write("/q/p/f.js", "333").unwrap();
  assert_eq!(window(&mut d), ["U /q/p/f.js"]);
  caught_up(&mut d);
}

/// The same with another directory at the path while the hint is checked, and the first one
/// back before the poll. The tree must not keep the files of the other directory under the
/// identity of the first.
#[test]
fn a_hint_while_another_directory_is_at_the_path() {
  let mut d = Driver::sim(Mode::Handle, Options::default());
  d.fs.mkdir_all("/q/p").unwrap();
  d.fs.mkdir_all("/q/other").unwrap();
  d.fs.write("/q/p/f.js", "1").unwrap();
  d.fs.write("/q/other/f.js", "other").unwrap();
  d.watch(&["/q/p/f.js"]);

  d.fs.rename("/q/p", "/q/spare").unwrap();
  d.fs.rename("/q/other", "/q/p").unwrap();
  d.fs.write("/q/spare/f.js", "22").unwrap();
  assert_eq!(window(&mut d), ["U /q/p/f.js"]);
  d.fs.rename("/q/p", "/q/other").unwrap();
  d.fs.rename("/q/spare", "/q/p").unwrap();
  assert_eq!(d.fs.pending_raw(), 0);
  // The handler read "other" at the update. The file at the path holds "22" now.
  assert_eq!(settled(&mut d), ["U /q/p/f.js"]);
  caught_up(&mut d);
}

/// The same hole on inotify when the parent has no watch because the limit was reached.
#[test]
fn a_hint_through_a_watch_below_a_failed_one() {
  let mut d = Driver::sim(Mode::Inode, Options::default());
  d.fs.mkdir_all("/q/p").unwrap();
  d.fs.write("/q/p/f.js", "1").unwrap();
  d.fs.set_watch_limit(Some(1));
  // `/` gets the only watch. Then the limit is raised, and `/q/p` gets one; `/q` has none
  // until the next poll.
  d.watch(&["/q/x"]);
  d.fs.set_watch_limit(Some(2));
  d.watch(&["/q/p/f.js"]);
  assert_eq!(d.errors.len(), 1);
  assert_eq!(d.errors[0].kind(), ErrorKind::WatchLimit);

  d.fs.rename("/q/p", "/q/p2").unwrap();
  d.fs.write("/q/p2/f.js", "22").unwrap();
  assert_eq!(d.fs.pending_raw(), 1);
  assert_eq!(window(&mut d), NONE);
  d.advance(Duration::from_millis(200));
  d.tick();
  assert_eq!(taken(&mut d), ["D /q/p/f.js"]);
  d.fs.rename("/q/p2", "/q/p").unwrap();
  assert_eq!(settled(&mut d), ["C /q/p/f.js"]);
  caught_up(&mut d);
}

/// The known hole of a backend that watches by path, closed. A directory above the watched one
/// is renamed away, a file below it is edited, and the directory is back before the next poll.
/// The stream stands at the old path and heard nothing of the edit, and the poll finds the same
/// directory at the path. But the stream ended when the directory above was renamed, and says
/// so, however short the directory was away: everything below it is watched again and scanned.
#[test]
fn a_directory_above_a_tree_watch_is_away_and_back_between_two_polls() {
  on_modes(&RECURSIVE, &Options::default(), |d, on| {
    d.fs().mkdir_all("/q/p/src").unwrap();
    d.fs().write("/q/p/src/f.js", "1").unwrap();
    assert_eq!(d.watch(&["/q/p/src/f.js"]), NONE, "{on}");
    assert_eq!(d.settled(), NONE, "{on}");

    d.fs().rename("/q/p", "/q/x").unwrap();
    d.fs().write("/q/x/src/f.js", "22").unwrap();
    d.fs().rename("/q/x", "/q/p").unwrap();
    if on == "Prefix" {
      assert_eq!(d.fs().pending_raw(), 1, "{on}");
    }
    assert_eq!(d.settled(), ["U /q/p/src/f.js"], "{on}");

    // The same one level higher, and the stream is at the path again.
    d.fs().rename("/q", "/x").unwrap();
    d.fs().write("/x/p/src/f.js", "333").unwrap();
    d.fs().rename("/x", "/q").unwrap();
    assert_eq!(d.settled(), ["U /q/p/src/f.js"], "{on}");
    d.fs().write("/q/p/src/f.js", "4444").unwrap();
    assert!(d.fs().pending_raw() > 0, "{on}");
    assert_eq!(d.window(), ["U /q/p/src/f.js"], "{on}");
    d.caught_up();
  });
}

/// The same right after `watch`, before any poll. The second layout reaches the watched file
/// through a link, and the directory behind the link is the one that is away and back: the
/// stream stands at the real path, so that directory is above it.
#[test]
fn a_directory_is_away_and_back_before_the_first_poll() {
  on_modes(&RECURSIVE, &Options::default(), |d, on| {
    d.fs().mkdir_all("/q/p/src").unwrap();
    d.fs().write("/q/p/src/f.js", "1").unwrap();
    d.fs().mkdir_all("/store/real/src").unwrap();
    d.fs().write("/store/real/src/g.js", "1").unwrap();
    d.fs().symlink("/store/real", "/q/link").unwrap();
    assert_eq!(d.watch(&["/q/p/src/f.js", "/q/link/src/g.js"]), NONE, "{on}");

    d.fs().rename("/q/p", "/q/x").unwrap();
    d.fs().write("/q/x/src/f.js", "22").unwrap();
    d.fs().rename("/q/x", "/q/p").unwrap();
    d.fs().rename("/store/real", "/store/x").unwrap();
    d.fs().write("/store/x/src/g.js", "22").unwrap();
    d.fs().rename("/store/x", "/store/real").unwrap();
    if on == "Prefix" {
      assert_eq!(d.fs().pending_raw(), 2, "{on}");
    }
    assert_eq!(d.settled(), ["U /q/link/src/g.js", "U /q/p/src/f.js"], "{on}");
    d.caught_up();
  });
}

/// The stream of a directory ended, because the directory was renamed away and back. Before
/// that is told, the directory asks for more: its second spelling leads to a new watched path
/// with nothing at it. It gets a new stream then, and the edit that the old stream did not
/// hear is still found: the end of a stream is told to every key that it had.
#[test]
fn a_directory_gets_a_new_stream_before_the_end_of_its_old_one_is_told() {
  let mut d = Driver::sim(Mode::Prefix, Options { loose_spelling: true, ..Options::default() });
  d.fs.set_ignore_case(true);
  d.fs.mkdir_all("/a/sub").unwrap();
  d.fs.write("/a/sub/f.js", "1").unwrap();
  d.watch(&["/A", "/a/sub/f.js"]);
  assert_eq!(settled(&mut d), NONE);

  d.fs.rename("/a", "/away").unwrap();
  d.fs.write("/away/sub/f.js", "22").unwrap();
  d.fs.rename("/away", "/a").unwrap();
  d.watch(&["/a/new.js"]);
  assert_eq!(taken(&mut d), NONE);
  assert_eq!(settled(&mut d), ["U /A/sub/f.js", "U /a/sub/f.js"]);
  caught_up(&mut d);
}

/// A directory is only polled, and before the poll comes it gets a watch of the backend,
/// because a watched path is added in it. The watch does not tell what happened before it:
/// the directory is scanned once more.
///
/// `/p` is only polled while every registered name in it is a directory: `x` leads to the
/// watched directory `/p/x/d`, which has the only stream.
#[test]
fn a_polled_directory_gets_a_watch() {
  let mut d = Driver::sim(Mode::Prefix, Options::default());
  d.fs.mkdir_all("/p/x/d").unwrap();
  d.fs.mkdir_all("/p/next/d").unwrap();
  d.fs.write("/p/x/d/f.js", "1").unwrap();
  d.fs.write("/p/next/d/f.js", "22").unwrap();
  d.watch(&["/p/x/d"]);
  // No watch covers `/p`. The stream only says that it ended, when it is asked.
  d.fs.rename("/p/x", "/p/old").unwrap();
  d.fs.rename("/p/next", "/p/x").unwrap();
  assert_eq!(d.fs.pending_raw(), 1);

  // `/p` holds a watched path with nothing at it now and gets a stream. It is polled no more.
  d.watch(&["/p/a.js"]);
  assert_eq!(taken(&mut d), ["U /p/x/d/f.js"]);
  d.fs.write("/p/x/d/f.js", "333").unwrap();
  flush(&mut d);
  assert_eq!(taken(&mut d), ["U /p/x/d/f.js"]);
  caught_up(&mut d);
}

/// A volume mounted over a watched directory: the OS says nothing, and the backend tells the
/// mount point by its path. The engine checks the node there and finds another directory.
#[test]
fn a_volume_mounted_over_a_watched_directory() {
  let mut d = Driver::sim(Mode::Inode, Options::default());
  d.fs.mkdir_all("/w/sub").unwrap();
  d.fs.write("/w/sub/old.js", "1").unwrap();
  d.watch(&["/w"]);
  // The volume hides the directory and shows one of its own, with no notification.
  d.fs.rename("/w/sub", "/hidden").unwrap();
  d.fs.mkdir("/w/sub").unwrap();
  d.fs.write("/w/sub/new.js", "1").unwrap();
  d.fs.take_raw(usize::MAX);
  assert_eq!(d.engine.deadline(), None);

  d.signal(SimRaw::mount("/w/sub"));
  assert_eq!(sorted(settled(&mut d)), ["C /w/sub/new.js", "D /w/sub/old.js"]);
  caught_up(&mut d);
}
