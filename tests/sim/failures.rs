//! What goes wrong: notifications are lost, the OS has no room for a watch, the backend dies, a
//! directory cannot be read, the disk does not answer. A story makes the disk, calls `watch`,
//! changes the disk and compares the events that come out (`C`, `U`, `D` and the path).

use std::{
  io,
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
};

use crate::fixtures::{
  Driver, Mode, NONE, RECURSIVE, Sim, SimBackend, caught_up, flush, not_caught_up, on_all,
  on_modes, settled, sorted, taken, window,
};
use pathwatch::{
  ErrorKind,
  backend::Signal,
  engine::{Options, Output},
};

fn driver(mode: Mode) -> Driver<SimBackend> {
  Driver::sim(mode, Options::default())
}

/// A look that gets no answer is tried again deep. An older watched directory below the path,
/// served by the tree watch of a directory between, is unwatched while the disk gives no
/// answer: it stays complete, and the look that gets an answer scans it.
#[test]
fn a_look_that_got_no_answer_is_tried_again_deep() {
  // inotify watches `old` itself: the tree watch of `R` serves it with the other two.
  for mode in [Mode::Prefix, Mode::Handle] {
    let mut d = driver(mode);
    d.fs.mkdir_all("/p/R/sub/old").unwrap();
    d.fs.write("/p/R/top.js", "1").unwrap();
    d.fs.write("/p/R/sub/old/a.js", "1").unwrap();
    d.fs.write("/p/R/sub/old/b.js", "1").unwrap();
    d.watch(&["/p/R/top.js"]);
    d.watch(&["/p/R/sub/old"]);

    d.fs.set_offline(Some(io::ErrorKind::Other));
    d.watch(&["/p"]);
    d.unwatch(&["/p/R/sub/old"]);
    d.fs.write("/p/R/sub/old/a.js", "22").unwrap();
    d.fs.remove("/p/R/sub/old/b.js").unwrap();
    d.fs.write("/p/R/sub/old/new.js", "1").unwrap();
    // A few looks that get no answer.
    for _ in 0..3 {
      d.deliver(usize::MAX);
      d.step();
    }
    assert_eq!(taken(&mut d), NONE, "{mode:?}");
    d.fs.set_offline(None);
    assert_eq!(
      sorted(settled(&mut d)),
      ["C /p/R/sub/old/new.js", "D /p/R/sub/old/b.js", "U /p/R/sub/old/a.js"],
      "{mode:?}"
    );
    caught_up(&mut d);
  }
}

/// Every notification is lost and the queue says so, or the queue of the runtime overflows,
/// which is the coarsest "lost track" there is: the scan of everything finds all of it.
#[test]
fn lost_notifications() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/r/keep/deep").unwrap();
    d.fs().mkdir_all("/p/r/leave").unwrap();
    d.fs().mkdir_all("/p/r/a/b").unwrap();
    d.fs().write("/p/r/keep/deep/a", "1").unwrap();
    d.fs().write("/p/r/leave/b", "1").unwrap();
    d.fs().write("/p/r/a/b/f", "1").unwrap();
    d.fs().write("/p/f.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/r", "/p/f.js"]), NONE, "{on}");

    d.fs().write("/p/r/keep/deep/a", "12").unwrap();
    d.fs().write("/p/r/keep/deep/new", "1").unwrap();
    d.fs().remove_all("/p/r/leave").unwrap();
    d.fs().mkdir_all("/p/r/come/in").unwrap();
    d.fs().write("/p/r/come/in/c", "1").unwrap();
    d.fs().rename("/p/r/a", "/p/r/c").unwrap();
    d.fs().write("/p/f.js", "12").unwrap();
    assert!(!d.fs().take_raw(usize::MAX).is_empty() || on == "Poll", "{on}");
    d.overflow();
    assert_eq!(
      d.window(),
      [
        "U /p/f.js",
        "C /p/r/c/b/f",
        "C /p/r/come/in/c",
        "U /p/r/keep/deep/a",
        "C /p/r/keep/deep/new"
      ],
      "{on}"
    );
    assert_eq!(d.settled(), ["D /p/r/a/b/f", "D /p/r/leave/b"], "{on}");
    // The watches are all there again.
    d.fs().write("/p/r/come/in/d", "1").unwrap();
    d.fs().write("/p/r/keep/deep/a", "123").unwrap();
    assert!(d.fs().pending_raw() >= 2 || on == "Poll", "{on}");
    assert_eq!(d.window(), ["C /p/r/come/in/d", "U /p/r/keep/deep/a"], "{on}");
    d.caught_up();
  });
}

/// The OS says for one watch that it lost notifications. Only what is below that watch is
/// scanned, not everything.
#[test]
fn a_loss_report_for_one_watch_scans_below_that_watch() {
  for mode in RECURSIVE {
    let mut d = Driver::sim(mode, Options::default());
    for dir in ["/p/A/sub", "/q/B/sub"] {
      d.fs.mkdir_all(dir).unwrap();
      for file in 0..20 {
        d.fs.write(format!("{dir}/f{file}"), "1").unwrap();
      }
    }
    d.watch(&["/p/A", "/q/B"]);
    assert_eq!(d.fs.os_watches().len(), 2, "{mode:?}");

    d.fs.write("/p/A/sub/f7", "22").unwrap();
    d.fs.write("/p/A/sub/new", "1").unwrap();
    assert!(d.fs.lose_raw(usize::MAX) >= 2, "{mode:?}");
    assert_eq!(d.fs.pending_raw(), 1, "{mode:?}");
    let before = d.fs.calls().read_dir;
    assert_eq!(window(&mut d), ["U /p/A/sub/f7", "C /p/A/sub/new"], "{mode:?}");
    // `/p/A` and `sub` are listed, and nothing below `/q/B`.
    assert_eq!(d.fs.calls().read_dir - before, 2, "{mode:?}");
    caught_up(&mut d);
  }
}

/// "this watch lost track" and "this watch is gone" are no reason to report anything. The
/// directory is watched again and scanned, and only real differences come out.
#[test]
fn a_lost_watch_reports_only_real_changes() {
  on_modes(&Mode::ALL, &Options::default(), |d, on| {
    d.fs().mkdir_all("/p/R/sub").unwrap();
    for file in 0..50 {
      d.fs().write(format!("/p/R/f{file}"), "1").unwrap();
      d.fs().write(format!("/p/R/sub/g{file}"), "1").unwrap();
    }
    assert_eq!(d.watch(&["/p/R"]), NONE, "{on}");

    // The OS drops notifications and says so.
    d.fs().write("/p/R/sub/g7", "22").unwrap();
    d.fs().write("/p/R/sub/new", "1").unwrap();
    assert!(d.fs().lose_raw(usize::MAX) >= 2, "{on}");
    assert_eq!(d.window(), ["U /p/R/sub/g7", "C /p/R/sub/new"], "{on}");

    // Every watch ends while its directory is still there (the drive was gone for a moment).
    d.fs().set_offline(Some(io::ErrorKind::NotFound));
    d.fs().set_offline(None);
    assert!(d.fs().pending_raw() > 0, "{on}");
    assert_eq!(d.window(), NONE, "{on}");
    assert_eq!(d.settled(), NONE, "{on}");
    // The watches were made again.
    d.fs().write("/p/R/sub/g8", "22").unwrap();
    assert!(d.fs().pending_raw() > 0, "{on}");
    assert_eq!(d.window(), ["U /p/R/sub/g8"], "{on}");
    assert!(d.errors().is_empty(), "{on}");
    d.caught_up();
  });
}

/// A notification that comes twice, or long after the fact, changes nothing.
#[test]
fn double_and_late_notifications() {
  for mode in Mode::ALL {
    let mut d = driver(mode);
    d.fs.mkdir_all("/p/r/sub").unwrap();
    d.fs.write("/p/r/sub/a", "1").unwrap();
    d.watch(&["/p/r"]);

    d.fs.write("/p/r/sub/b", "1").unwrap();
    d.fs.remove("/p/r/sub/a").unwrap();
    d.fs.rename("/p/r/sub", "/p/r/moved").unwrap();
    let raws = d.fs.take_raw(usize::MAX);
    for raw in raws.iter().cloned() {
      d.signal(raw);
    }
    assert_eq!(settled(&mut d), ["C /p/r/moved/b", "D /p/r/sub/a"], "{mode:?}");
    for raw in raws.iter().rev().chain(&raws).cloned() {
      d.signal(raw);
    }
    // Nothing comes or goes. A repeated "written" may cost an update of the file it names.
    assert!(settled(&mut d).iter().all(|event| event == "U /p/r/moved/b"), "{mode:?}");
    assert_eq!(d.known.len(), 1, "{mode:?}");
    caught_up(&mut d);
  }
}

/// The OS has no room for a watch: one error, the directory is polled, and the watch is asked
/// for again until it works.
#[test]
fn the_watch_limit() {
  let mut d = driver(Mode::Inode);
  d.fs.mkdir_all("/p/r/a").unwrap();
  d.fs.mkdir_all("/p/r/b").unwrap();
  d.fs.write("/p/r/a/x", "1").unwrap();
  d.fs.write("/p/r/b/y", "1").unwrap();
  // Room for `/`, `/p` and `/p/r`.
  d.fs.set_watch_limit(Some(3));
  d.watch(&["/p/r"]);
  assert_eq!(d.errors.len(), 1);
  assert_eq!(d.errors[0].kind(), ErrorKind::WatchLimit);
  assert_eq!(d.known.len(), 2);

  // No watch, no notification: the poll finds it.
  d.fs.write("/p/r/a/x", "12").unwrap();
  d.fs.write("/p/r/b/new", "1").unwrap();
  assert_eq!(d.fs.pending_raw(), 0);
  assert_eq!(settled(&mut d), ["U /p/r/a/x", "C /p/r/b/new"]);
  assert_eq!(d.errors.len(), 1);

  // The user raises the limit. The next poll gets the watches, and hints come again.
  d.fs.set_watch_limit(None);
  assert_eq!(settled(&mut d), NONE);
  d.fs.write("/p/r/a/x", "123").unwrap();
  assert_eq!(d.fs.pending_raw(), 1);
  flush(&mut d);
  assert_eq!(taken(&mut d), ["U /p/r/a/x"]);
  assert_eq!(d.errors.len(), 1);
  caught_up(&mut d);
}

/// `commit` cannot make live what `watch` promised. The new directory is
/// still scanned (the baseline is there), the old stream keeps working, and the new directory is
/// polled until the backend has room.
#[test]
fn a_failed_commit() {
  let mut d = Driver::sim(Mode::Prefix, Options::default());
  d.fs.mkdir_all("/proj/src").unwrap();
  d.fs.mkdir_all("/other/dir").unwrap();
  d.fs.write("/proj/src/a.js", "1").unwrap();
  d.fs.write("/other/dir/new.js", "1").unwrap();
  d.watch(&["/proj/src/a.js"]);
  d.fs.set_watch_limit(Some(1));
  d.watch(&["/other/dir/new.js"]);
  assert_eq!(Sim::known(&d), ["/other/dir/new.js", "/proj/src/a.js"]);
  assert_eq!(Sim::errors(&d), [ErrorKind::WatchLimit]);

  d.fs.write("/proj/src/a.js", "22").unwrap();
  assert_eq!(d.fs.pending_raw(), 1);
  assert_eq!(Sim::window(&mut d), ["U /proj/src/a.js"]);
  d.fs.write("/other/dir/new.js", "22").unwrap();
  assert_eq!(d.fs.pending_raw(), 0);
  assert_eq!(Sim::settled(&mut d), ["U /other/dir/new.js"]);

  d.fs.set_watch_limit(None);
  assert_eq!(Sim::settled(&mut d), NONE);
  d.fs.write("/other/dir/new.js", "333").unwrap();
  assert_eq!(d.fs.pending_raw(), 1);
  assert_eq!(Sim::window(&mut d), ["U /other/dir/new.js"]);
  assert_eq!(Sim::errors(&d), [ErrorKind::WatchLimit]);
  caught_up(&mut d);
}

/// A tree past the watch limit finds no room for one new directory after the other: the
/// handler is told once.
#[test]
fn the_watch_limit_is_reported_once() {
  let mut d = driver(Mode::Inode);
  d.fs.mkdir_all("/p/r").unwrap();
  d.fs.set_watch_limit(Some(2));
  d.watch(&["/p/r"]);
  for dir in ["a", "b", "c"] {
    d.fs.mkdir_all(format!("/p/r/{dir}")).unwrap();
    d.fs.write(format!("/p/r/{dir}/file"), "1").unwrap();
    Sim::settled(&mut d);
  }
  assert_eq!(Sim::errors(&d), [ErrorKind::WatchLimit]);
  caught_up(&mut d);
}

/// On a disk that other machines change too, the OS reports only what changes here: the handler
/// is told once for the disk, and not where the disk is polled anyway.
#[test]
fn a_disk_that_other_machines_change_is_told_once() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/net/a").unwrap();
    d.fs().mkdir_all("/net/b").unwrap();
    d.fs().set_remote(Some("nfs"));
    d.watch(&["/net/a"]);
    d.watch(&["/net/b"]);
    let told: &[ErrorKind] = if on == "Poll" { &[] } else { &[ErrorKind::RemoteDisk] };
    assert_eq!(d.errors(), told, "{on}");
    d.caught_up();
  });
}

/// A watch for a tree that the backend cannot give. Nothing below it may be left without a
/// watch and without the poll. Where the backend answers for a whole tree, `commit` takes that
/// answer back: the directories below relied on it, so each is asked for by itself, fails too,
/// and all of them are polled until the backend has room. Then the coverage comes back through
/// a later commit.
#[test]
fn below_a_tree_watch_that_failed() {
  on_modes(&Mode::ALL, &Options::default(), |d, on| {
    d.fs().mkdir_all("/p/R/src/deep").unwrap();
    d.fs().write("/p/R/src/a.js", "1").unwrap();
    d.fs().write("/p/R/src/deep/b.js", "1").unwrap();
    d.fs().set_watch_limit(Some(0));
    assert_eq!(d.watch(&["/p/R"]), NONE, "{on}");
    assert_eq!(d.errors(), [ErrorKind::WatchLimit], "{on}");

    d.fs().write("/p/R/src/a.js", "22").unwrap();
    d.fs().write("/p/R/src/deep/b.js", "22").unwrap();
    d.fs().write("/p/R/src/deep/c.js", "1").unwrap();
    assert_eq!(d.fs().pending_raw(), 0, "{on}");
    assert_eq!(
      d.settled(),
      ["U /p/R/src/a.js", "U /p/R/src/deep/b.js", "C /p/R/src/deep/c.js"],
      "{on}"
    );
    assert_eq!(d.errors(), [ErrorKind::WatchLimit], "{on}");

    // With room again, the poll asks again, and the coverage comes back through a later commit.
    d.fs().set_watch_limit(None);
    assert_eq!(d.settled(), NONE, "{on}");
    d.fs().write("/p/R/src/deep/b.js", "333").unwrap();
    assert!(d.fs().pending_raw() > 0, "{on}");
    assert_eq!(d.window(), ["U /p/R/src/deep/b.js"], "{on}");
    assert_eq!(d.errors(), [ErrorKind::WatchLimit], "{on}");
    d.caught_up();
  });
}

/// The backend says it is dead: one error, and from then on the poll finds every change.
#[test]
fn the_backend_dies() {
  for mode in Mode::ALL {
    let mut d = driver(mode);
    d.fs.mkdir_all("/p/r/sub").unwrap();
    d.fs.write("/p/r/sub/a", "1").unwrap();
    d.watch(&["/p/r"]);

    d.fs.write("/p/r/sub/a", "12").unwrap();
    d.fs.take_raw(usize::MAX);
    d.engine.signal(Signal::Failed(io::Error::other("the reader thread stopped")), d.now);
    d.engine.signal(Signal::Failed(io::Error::other("again")), d.now);
    assert_eq!(settled(&mut d), ["U /p/r/sub/a"], "{mode:?}");
    assert_eq!(d.errors.len(), 1, "{mode:?}");
    assert_eq!(d.errors[0].kind(), ErrorKind::Io);

    d.fs.mkdir("/p/r/sub/new").unwrap();
    d.fs.write("/p/r/sub/new/b", "1").unwrap();
    d.fs.take_raw(usize::MAX);
    assert_eq!(settled(&mut d), ["C /p/r/sub/new/b"], "{mode:?}");
    d.watch(&["/p/other.js"]);
    d.fs.write("/p/other.js", "1").unwrap();
    d.fs.take_raw(usize::MAX);
    assert_eq!(settled(&mut d), ["C /p/other.js"], "{mode:?}");
    assert_eq!(d.errors.len(), 1, "{mode:?}");
    caught_up(&mut d);
  }
}

/// A directory that cannot be read counts as empty, is reported once, and is polled.
#[test]
fn permission_denied() {
  for mode in Mode::ALL {
    let mut d = driver(mode);
    d.fs.mkdir_all("/p/r/locked").unwrap();
    d.fs.write("/p/r/locked/a", "1").unwrap();
    d.fs.write("/p/r/b", "1").unwrap();
    d.watch(&["/p/r"]);

    d.fs.set_denied("/p/r/locked", true).unwrap();
    // Nothing tells about a change of permissions. A scan of everything runs into it.
    d.engine.overflow(d.now);
    assert_eq!(settled(&mut d), ["D /p/r/locked/a"], "{mode:?}");
    assert_eq!(d.errors.len(), 1, "{mode:?}");
    assert_eq!(d.errors[0].kind(), ErrorKind::PermissionDenied);
    assert_eq!(settled(&mut d), NONE, "{mode:?}");

    d.fs.set_denied("/p/r/locked", false).unwrap();
    assert_eq!(settled(&mut d), ["C /p/r/locked/a"], "{mode:?}");
    assert_eq!(d.errors.len(), 1, "{mode:?}");
    caught_up(&mut d);
  }
}

/// A top directory is checked by the root, like any directory by its parent. While it is
/// not there, every poll checks it.
#[test]
fn a_top_directory_that_comes_and_goes() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir("/proj").unwrap();
    d.fs().write("/proj/a.js", "1").unwrap();
    // The drive is not there when the path is added.
    d.fs().set_offline(Some(io::ErrorKind::NotFound));
    assert_eq!(d.watch(&["/proj/a.js"]), NONE, "{on}");
    assert_eq!(d.known(), NONE, "{on}");
    assert_eq!(d.settled(), NONE, "{on}");
    d.fs().set_offline(None);
    assert_eq!(d.settled(), ["C /proj/a.js"], "{on}");

    // It goes away and comes back.
    d.fs().set_offline(Some(io::ErrorKind::NotFound));
    assert_eq!(d.settled(), ["D /proj/a.js"], "{on}");
    d.fs().set_offline(None);
    assert_eq!(d.settled(), ["C /proj/a.js"], "{on}");
    d.fs().write("/proj/a.js", "22").unwrap();
    assert_eq!(d.window(), ["U /proj/a.js"], "{on}");
    assert!(d.errors().is_empty(), "{on}");
    d.caught_up();
  });
}

/// A top directory that was not there at the first `watch` is not on disk for the tree, and
/// nothing below it is checked. A later `watch` did not check it, and so returned before the
/// state of its paths was recorded: what was there came as `Create` with the next poll.
#[test]
fn add_checks_a_top_directory_that_was_not_there() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir("/a").unwrap();
    d.fs.set_offline(Some(io::ErrorKind::NotFound));
    d.watch(&["/b/x"]);
    assert!(d.known.is_empty(), "{mode:?}");
    d.fs.set_offline(None);
    d.fs.write("/a/c", "1").unwrap();
    d.watch(&["/a"]);
    assert_eq!(d.known.len(), 1, "{mode:?}");
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    caught_up(&mut d);
  }
}

/// A hint says "check this directory": it was away and is back. The check fails with an
/// error that says nothing. The mark must wait, or the poll later finds the same directory and
/// does not scan it.
#[test]
fn a_mark_that_the_disk_did_not_answer_is_kept() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir_all("/p/R/sub").unwrap();
    d.fs.mkdir("/away").unwrap();
    d.fs.write("/p/R/sub/a", "1").unwrap();
    d.watch(&["/p/R"]);

    d.fs.rename("/p/R/sub", "/away/sub").unwrap();
    d.fs.write("/away/sub/new", "1").unwrap();
    d.fs.rename("/away/sub", "/p/R/sub").unwrap();
    d.fs.set_offline(Some(io::ErrorKind::TimedOut));
    assert_eq!(window(&mut d), NONE, "{mode:?}");
    d.fs.set_offline(None);
    assert_eq!(settled(&mut d), ["C /p/R/sub/new"], "{mode:?}");
    caught_up(&mut d);
  }
}

/// Every watch ended, and the deep scan that makes them again is cut short: from some call
/// on, the disk answers nothing. Whatever the moment, the rest of the scan is made again when
/// the disk is back, so that no directory is left with a watch that is gone. A look that the
/// disk did not answer is told once, as `Io`.
///
/// `dropped`: the queue lost the notifications that said so, and all that is left is "check
/// everything". Then the request to watch again exists only as the deep scan that goes down
/// the tree, and a scan that fails in the middle must not forget it.
#[test]
fn a_deep_scan_that_the_disk_stops_answering() {
  let files = ["/p/r/a", "/p/r/sub/b", "/p/r/sub/deep/c", "/p/f"];
  for (mode, dropped) in Mode::ALL.into_iter().flat_map(|mode| [(mode, false), (mode, true)]) {
    for moment in 0..40 {
      let mut d = Driver::sim(mode, Options::default());
      d.fs.mkdir_all("/p/r/sub/deep").unwrap();
      for file in files {
        d.fs.write(file, "1").unwrap();
      }
      d.watch(&["/p/r", "/p/f"]);

      d.fs.set_offline(Some(io::ErrorKind::NotFound));
      d.fs.set_offline(None);
      if dropped {
        d.fs.take_raw(usize::MAX);
        d.engine.overflow(d.now);
      }
      d.fs.at_call(moment, |fs| fs.set_offline(Some(io::ErrorKind::TimedOut)));
      window(&mut d);
      d.fs.clear_traps();
      d.fs.set_offline(None);
      // Nothing changed. A file whose hash could not be read may count as changed.
      let events = settled(&mut d);
      let on = format!("{mode:?}, dropped: {dropped}, call {moment}");
      assert!(events.iter().all(|event| event.starts_with("U ")), "{on}");

      for file in files {
        d.fs.write(file, "22").unwrap();
      }
      assert_eq!(
        window(&mut d),
        ["U /p/f", "U /p/r/a", "U /p/r/sub/b", "U /p/r/sub/deep/c"],
        "{on}: a watch was not made again"
      );
      // A look that the disk did not answer is told once, as `Io`, and the scans that bring
      // the watches back tell nothing more.
      let errors = Sim::errors(&d);
      assert!(errors.len() <= 1 && errors.iter().all(|error| *error == ErrorKind::Io), "{on}");
      if moment == 0 {
        assert_eq!(errors, [ErrorKind::Io], "{on}");
      }
      caught_up(&mut d);
    }
  }
}

/// A name on the way to a new watched path is registered in a directory that tells names only.
/// The check that finds nothing at the name asks the directory for its contents, but here that
/// check does not come in the same call: a directory above was held, it is back, and the disk
/// stops answering in the scan that follows. The check waits as a mark above the name.
/// Whatever the moment: when the disk answers again, the directory tells its contents, and a
/// chain that appears at the name is reported with no poll.
#[test]
fn a_name_on_the_way_is_registered_and_the_disk_stops_answering() {
  for moment in 0..12 {
    on_all(&Options::default(), |d, on| {
      let covers_a_tree = on != "Inode" && on != "Poll";
      let on = format!("{on}, call {moment}");
      d.fs().mkdir_all("/a/c/c").unwrap();
      d.fs().write("/a/c/c/x", "1").unwrap();
      assert_eq!(d.watch(&["/a/c/c/x"]), NONE, "{on}");

      // One look finds `/a` gone: it is held, with all that the tree has below it.
      d.fs().rename("/a", "/away").unwrap();
      for _ in 0..10 {
        if d.pending_deletes() == 0 {
          d.window();
        }
      }
      assert_eq!(d.pending_deletes(), 1, "{on}");
      d.fs().rename("/away", "/a").unwrap();

      d.fs().at_call(moment, |fs| fs.set_offline(Some(io::ErrorKind::TimedOut)));
      d.watch(&["/a/c/b/x"]);
      d.fs().clear_traps();
      d.fs().set_offline(None);
      // Nothing changed. A file whose hash could not be read may count as changed.
      assert!(d.settled().iter().all(|event| event.starts_with("U ")), "{on}");

      d.fs().mkdir("/a/c/b").unwrap();
      d.fs().write("/a/c/b/x", "1").unwrap();
      if covers_a_tree {
        assert!(d.fs().pending_raw() > 0, "{on}: `/a/c` does not tell its contents");
      }
      assert_eq!(d.settled(), ["C /a/c/b/x"], "{on}");
      d.caught_up();
    });
  }
}

/// A path that the OS does not take (longer than it allows): the look fails with an invalid
/// name. That is not "not found": nothing can ever be there. The handler is told once, as `Io`,
/// and the path counts as missing.
#[test]
fn a_path_that_the_os_does_not_take() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p").unwrap();
    d.fs().set_offline(Some(io::ErrorKind::InvalidFilename));
    assert_eq!(d.watch(&["/p/f"]), NONE, "{on}");
    assert_eq!(d.errors(), [ErrorKind::Io], "{on}");

    d.fs().set_offline(None);
    d.fs().write("/p/f", "1").unwrap();
    assert_eq!(d.settled(), ["C /p/f"], "{on}");
    d.caught_up();
  });
}

/// `close` does not wait for a flush that reads thousands of files: once the watcher says
/// stop, the flush ends at the next check or scan, with the tree as it is.
#[test]
fn a_stopped_engine_ends_its_flush() {
  let mut d = Driver::poll(Options::default());
  d.fs.mkdir_all("/p/r").unwrap();
  for file in 0..200 {
    d.fs.write(format!("/p/r/f{file}"), "1").unwrap();
  }
  d.watch(&["/p/r"]);
  let stop = Arc::new(AtomicBool::new(false));
  d.engine.stop_with(Arc::clone(&stop));
  d.fs.at_call(8, move |_| stop.store(true, Ordering::Relaxed));

  let before = d.fs.calls();
  let deadline = d.engine.deadline().unwrap();
  d.advance(deadline - d.now);
  d.engine.tick(d.now, &mut Output::default());
  assert_eq!(d.fs.traps_run(), 1);
  assert!(d.fs.calls().lstat - before.lstat < 20);
  not_caught_up(&mut d, "the engine was stopped in the middle of a flush");
}
