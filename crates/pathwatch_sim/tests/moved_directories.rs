//! A directory is renamed, replaced by another one, or away and back.
//! A story makes the disk, calls `watch`, changes the disk and compares the events that come
//! out (`C`, `U`, `D` and the path). `on_all` tells one story on every backend.

#![cfg(unix)]

use std::{path::Path, time::Duration};

use pathwatch::{ErrorKind, engine::Options};
use pathwatch_sim::{
  Driver, Mode, NONE, RECURSIVE, SimBackend, caught_up, flush, ignore_node_modules, on_all,
  on_modes, settled, sorted, taken, window, window_then_settled,
};

fn driver(mode: Mode) -> Driver<SimBackend> {
  Driver::sim(mode, Options::default())
}

/// inotify gives a renamed directory its old watch back. The node of the old name must not take
/// that watch away when it goes.
#[test]
fn a_renamed_directory_keeps_its_watch() {
  for (from, to) in [("a", "b"), ("b", "a")] {
    let mut d = driver(Mode::Inode);
    d.fs.mkdir_all(format!("/p/r/{from}/deep")).unwrap();
    d.fs.write(format!("/p/r/{from}/x"), "1").unwrap();
    d.watch(&["/p/r"]);

    d.fs.rename(format!("/p/r/{from}"), format!("/p/r/{to}")).unwrap();
    assert_eq!(settled(&mut d), [format!("C /p/r/{to}/x"), format!("D /p/r/{from}/x")]);
    d.fs.write(format!("/p/r/{to}/new"), "1").unwrap();
    d.fs.write(format!("/p/r/{to}/deep/new"), "1").unwrap();
    assert_eq!(settled(&mut d), [format!("C /p/r/{to}/deep/new"), format!("C /p/r/{to}/new")]);
    caught_up(&mut d);
  }
}

/// `unwatch` of the old name must not take away the watch that the new name got back.
#[test]
fn unwatch_keeps_the_watch_of_a_renamed_directory() {
  on_all(&Options::default(), |d, on| {
    // Both name orders: the marks of one directory have no order.
    for (watched, from, to) in [("/p/r", "a", "b"), ("/p/q", "b", "a")] {
      d.fs().mkdir_all(format!("{watched}/{from}/deep")).unwrap();
      d.fs().write(format!("{watched}/{from}/x"), "1").unwrap();
      assert_eq!(d.watch(&[watched]), NONE, "{on}");

      d.fs().rename(format!("{watched}/{from}"), format!("{watched}/{to}")).unwrap();
      assert_eq!(
        sorted(d.settled()),
        sorted(vec![format!("C {watched}/{to}/x"), format!("D {watched}/{from}/x")]),
        "{on}"
      );
      d.fs().write(format!("{watched}/{to}/new.js"), "1").unwrap();
      d.fs().write(format!("{watched}/{to}/deep/new.js"), "1").unwrap();
      assert_eq!(
        d.settled(),
        [format!("C {watched}/{to}/deep/new.js"), format!("C {watched}/{to}/new.js")],
        "{on}"
      );
    }
    d.caught_up();
  });
}

/// Three ways in which the node of an old name goes after the node of the new name took
/// over the OS watch. The directory under the new name must not be deaf afterwards.
#[test]
fn the_new_name_is_not_deaf() {
  // Case 1: a sub-directory of a watched directory.
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/R/b").unwrap();
    d.fs().write("/R/b/f.js", "1").unwrap();
    assert_eq!(d.watch(&["/R"]), NONE, "{on}");
    d.fs().rename("/R/b", "/R/a").unwrap();
    assert_eq!(sorted(d.settled()), ["C /R/a/f.js", "D /R/b/f.js"], "{on}");
    d.fs().write("/R/a/f.js", "22").unwrap();
    assert_eq!(d.window(), ["U /R/a/f.js"], "{on}");
    d.caught_up();
  });
  // Case 2: an ignored directory with a watched file in it; "nm" sorts before "node_modules".
  on_all(&ignore_node_modules(), |d, on| {
    d.fs().mkdir_all("/p/node_modules/foo").unwrap();
    d.fs().write("/p/node_modules/foo/index.js", "1").unwrap();
    assert_eq!(d.watch(&["/p", "/p/node_modules/foo/index.js"]), NONE, "{on}");
    d.fs().rename("/p/node_modules", "/p/nm").unwrap();
    assert_eq!(
      sorted(d.settled()),
      ["C /p/nm/foo/index.js", "D /p/node_modules/foo/index.js"],
      "{on}"
    );
    d.fs().write("/p/nm/foo/index.js", "22").unwrap();
    d.fs().write("/p/nm/foo/new.js", "1").unwrap();
    d.fs().write("/p/nm/top.js", "1").unwrap();
    assert_eq!(
      d.window(),
      ["U /p/nm/foo/index.js", "C /p/nm/foo/new.js", "C /p/nm/top.js"],
      "{on}"
    );
    d.caught_up();
  });
  // Case 3: two watched files, and the directory of one moves to the place of the other.
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/b").unwrap();
    d.fs().write("/p/b/x.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/a/x.js", "/p/b/x.js"]), NONE, "{on}");
    d.fs().rename("/p/b", "/p/a").unwrap();
    assert_eq!(sorted(d.settled()), ["C /p/a/x.js", "D /p/b/x.js"], "{on}");
    d.fs().write("/p/a/x.js", "22").unwrap();
    assert_eq!(d.window(), ["U /p/a/x.js"], "{on}");
    d.caught_up();
  });
}

/// `rm -r` and `mkdir` between two flushes: the new directory gets the inode number of the old
/// one, so it looks like the same directory. Its watch is gone all the same.
#[test]
fn a_new_directory_with_the_old_inode_number() {
  for mode in Mode::ALL {
    let mut d = driver(mode);
    d.fs.mkdir_all("/p/r/sub").unwrap();
    d.fs.write("/p/r/sub/a", "1").unwrap();
    d.watch(&["/p/r"]);

    d.fs.remove_all("/p/r/sub").unwrap();
    d.fs.mkdir("/p/r/sub").unwrap();
    d.fs.write("/p/r/sub/b", "1").unwrap();
    assert_eq!(settled(&mut d), ["C /p/r/sub/b", "D /p/r/sub/a"], "{mode:?}");
    d.fs.write("/p/r/sub/c", "1").unwrap();
    assert_eq!(settled(&mut d), ["C /p/r/sub/c"], "{mode:?}");
    caught_up(&mut d);
  }
}

/// A directory that left and came back with the same identity is scanned.
#[test]
fn a_directory_that_was_away_is_scanned() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/R/sub").unwrap();
    d.fs().mkdir("/tmp").unwrap();
    d.fs().write("/p/R/sub/a.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R"]), NONE, "{on}");

    // One window: out of the watched tree, changed there, back.
    d.fs().rename("/p/R/sub", "/tmp/sub").unwrap();
    d.fs().remove("/tmp/sub/a.js").unwrap();
    d.fs().write("/tmp/sub/new.js", "1").unwrap();
    d.fs().rename("/tmp/sub", "/p/R/sub").unwrap();
    assert_eq!(window_then_settled(d), ["C /p/R/sub/new.js", "D /p/R/sub/a.js"], "{on}");

    // The same for a watched file whose directory makes the trip.
    d.fs().mkdir("/q").unwrap();
    d.fs().mkdir("/q/lib").unwrap();
    d.fs().write("/q/lib/a.js", "1").unwrap();
    assert_eq!(d.watch(&["/q/lib/a.js"]), NONE, "{on}");
    d.fs().rename("/q/lib", "/tmp/lib").unwrap();
    d.fs().write("/tmp/lib/a.js", "edited").unwrap();
    d.fs().rename("/tmp/lib", "/q/lib").unwrap();
    assert_eq!(d.window(), ["U /q/lib/a.js"], "{on}");
    assert_eq!(d.settled(), NONE, "{on}");
    d.caught_up();
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

/// Spread over three windows: the same trip is a `Delete` and a `Create`.
#[test]
fn away_and_back_in_three_windows() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/proj/lib").unwrap();
    d.fs().write("/proj/lib/a.js", "1").unwrap();
    assert_eq!(d.watch(&["/proj/lib/a.js"]), NONE, "{on}");
    d.fs().rename("/proj/lib", "/proj/lib2").unwrap();
    assert_eq!(d.settled(), ["D /proj/lib/a.js"], "{on}");
    d.fs().write("/proj/lib2/a.js", "edited").unwrap();
    assert_eq!(d.settled(), NONE, "{on}");
    d.fs().rename("/proj/lib2", "/proj/lib").unwrap();
    assert_eq!(d.settled(), ["C /proj/lib/a.js"], "{on}");
    d.caught_up();
  });
}

/// The parent of a watched file is replaced by another directory that holds the same file.
#[test]
fn the_parent_of_a_watched_file_is_replaced() {
  for mode in Mode::ALL {
    let mut d = driver(mode);
    d.fs.mkdir_all("/p/lib").unwrap();
    d.fs.mkdir_all("/p/next").unwrap();
    d.fs.write("/p/lib/a.js", "1").unwrap();
    d.fs.write("/p/next/a.js", "22").unwrap();
    d.watch(&["/p/lib/a.js"]);

    d.fs.rename("/p/lib", "/p/old").unwrap();
    d.fs.rename("/p/next", "/p/lib").unwrap();
    assert_eq!(settled(&mut d), ["U /p/lib/a.js"], "{mode:?}");
    d.fs.write("/p/lib/a.js", "333").unwrap();
    assert_eq!(settled(&mut d), ["U /p/lib/a.js"], "{mode:?}");
    // The directory that left is not the watched path any more.
    d.fs.write("/p/old/a.js", "4444").unwrap();
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    caught_up(&mut d);
  }
}

/// `watch` checks a file inside a directory that the tree still holds for another directory.
/// The same flush then finds the directory replaced. The file was never known to the handler,
/// so nothing is said about it.
#[test]
fn the_baseline_inside_a_replaced_directory() {
  let mut d = driver(Mode::Inode);
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
/// directory. The `Delete` of the file came from the old directory going away, which is held
/// back; the `Create` below its path came at once. For 100 ms the handler knew a file and a
/// file below it.
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
  let mut d = driver(Mode::Prefix);
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
  let mut d = driver(Mode::Prefix);
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
