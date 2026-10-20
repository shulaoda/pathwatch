//! Links: on the way to a watched path, at a watched path, and inside a watched directory.
//! A story makes the disk, calls `watch`, changes the disk and compares the events that come
//! out (`C`, `U`, `D` and the path). `on_all` tells one story on every backend.

use std::{collections::BTreeSet, io, path::PathBuf, time::Duration};

use crate::fixtures::{
  Deliver, Driver, Mode, NONE, SimBackend, caught_up, check_fingerprints, flush, on_all, settled,
  sorted, taken, window,
};
use pathwatch::engine::Options;

fn driver(mode: Mode) -> Driver<SimBackend> {
  Driver::sim(mode, Options::default())
}

/// The registered path goes through links (`/tmp` and `$TMPDIR` on macOS), and the OS
/// reports real paths. The events carry the registered spelling, with no help from the poll.
#[test]
fn linked_prefixes_keep_the_registered_spelling() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/private/tmp/proj").unwrap();
    d.fs().mkdir_all("/private/var/folders/x/T").unwrap();
    d.fs().symlink("private/tmp", "/tmp").unwrap();
    d.fs().symlink("private/var", "/var").unwrap();
    d.fs().write("/tmp/proj/a.js", "1").unwrap();
    assert_eq!(d.watch(&["/tmp/proj/a.js", "/var/folders/x/T/work"]), NONE, "{on}");

    d.fs().write("/private/tmp/proj/a.js", "22").unwrap();
    d.fs().mkdir("/var/folders/x/T/work").unwrap();
    d.fs().write("/private/var/folders/x/T/work/out.js", "1").unwrap();
    assert_eq!(d.window(), ["U /tmp/proj/a.js", "C /var/folders/x/T/work/out.js"], "{on}");
    assert_eq!(d.settled(), NONE, "{on}");
    d.caught_up();
  });
}

/// A hint with a name that is not plain queues a scan of the names of its directory. A link
/// watched inside the quiet time of that hint still gets its first look: the scan of the names
/// skips what the listing names, so it would not follow the link.
#[test]
fn a_link_watched_inside_the_quiet_time_of_a_hint_with_a_name_that_is_not_plain() {
  for mode in Mode::ALL {
    let mut d = driver(mode);
    d.fs.mkdir_all("/p/dir").unwrap();
    d.fs.mkdir_all("/shared/real").unwrap();
    d.fs.write("/shared/real/c.js", "1").unwrap();
    // A link inside a watched directory is a file like the others, until a watched path goes
    // through it.
    d.fs.symlink("/shared/real", "/p/dir/link").unwrap();
    d.watch(&["/p/dir"]);

    d.fs.write("/p/dir/café.js", "1").unwrap();
    d.deliver(usize::MAX);
    d.watch(&["/p/dir/link/c.js"]);
    let events = settled(&mut d);
    assert!(events.contains(&"C /p/dir/café.js".to_owned()), "{mode:?}: {events:?}");

    d.fs.write("/shared/real/c.js", "22").unwrap();
    assert_eq!(settled(&mut d), ["U /p/dir/link/c.js"], "{mode:?}");
    caught_up(&mut d);
  }
}

/// Windows makes a junction or a directory link in two steps, a plain directory first, and the
/// second step gives no record that the backend asks for. A look between the steps sees a
/// directory; it is looked at once more at the next flush, whatever moment of the call the
/// second step falls into: inside a watched directory, at a watched path, and on the way to one.
#[test]
fn a_link_made_in_two_steps() {
  let options = Options { two_step_links: true, ..Options::default() };
  let places: [(&str, &str, &[&str]); 3] = [
    ("inside", "/p/w/j", &["/p/w"]),
    ("at", "/p/b/cur", &["/p/b/cur"]),
    ("on the way", "/p/b/mid", &["/p/b/mid/a.txt"]),
  ];
  for mode in Mode::ALL {
    for (place, link, watched) in places {
      let prepare = || {
        let d = Driver::sim(mode, options.clone());
        d.fs.mkdir_all("/t/sub").unwrap();
        d.fs.write("/t/a.txt", "1").unwrap();
        d.fs.write("/t/sub/b.txt", "1").unwrap();
        d.fs.mkdir_all("/p/w").unwrap();
        d.fs.mkdir_all("/p/b").unwrap();
        // The first step: a plain directory.
        d.fs.mkdir(link).unwrap();
        d.fs.take_raw(usize::MAX);
        d
      };
      let second_step = move |fs: &crate::fixtures::SimFs| {
        fs.remove_all(link).unwrap();
        fs.symlink("/t", link).unwrap();
      };
      let mut d = prepare();
      let before = d.fs.calls().total();
      d.watch(watched);
      let calls = d.fs.calls().total() - before;
      caught_up(&mut d);

      for moment in 0..=calls {
        let mut d = prepare();
        d.fs.at_call(moment, second_step);
        d.watch(watched);
        if d.fs.traps_run() == 0 {
          d.fs.clear_traps();
          second_step(&d.fs);
        }
        // The OS says nothing of the second step.
        d.fs.take_raw(usize::MAX);
        d.settle();
        caught_up(&mut d);
        let _ = (place, &mut d);
      }
    }
  }
}

/// The one more look is for a directory made in the last moments (the story above makes its
/// directory right before `watch`): an old one is past its second step. The directories of a
/// baseline are old, so the flush after `watch` asks the disk nothing about them.
#[test]
fn an_old_directory_is_not_looked_at_again() {
  let options = Options { two_step_links: true, ..Options::default() };
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, options.clone());
    for dir in 0..20 {
      d.fs.mkdir_all(format!("/p/d{dir:02}/sub")).unwrap();
      d.fs.write(format!("/p/d{dir:02}/sub/a.txt"), "1").unwrap();
    }
    // Older than the coarse youth.
    d.advance(Duration::from_secs(3));
    d.watch(&["/p"]);
    assert_eq!(taken(&mut d), NONE, "{mode:?}");
    let before = d.fs.calls();
    // A change brings the next flush: the new file is looked at, and its directory where the
    // OS names the directory of a change (Prefix, Handle); the 41 directories of the baseline
    // are not (42 more `lstat` calls here before the rule).
    d.fs.write("/p/new.txt", "1").unwrap();
    assert_eq!(window(&mut d), ["C /p/new.txt"], "{mode:?}");
    let looks = d.fs.calls().lstat - before.lstat;
    let expected = if mode == Mode::Inode { 1 } else { 2 };
    assert_eq!(looks, expected, "{mode:?}: lstat calls at the flush after the baseline");
    caught_up(&mut d);
  }
}

/// One directory under two spellings. Each watched path gets its events under its own
/// spelling, with no help from the poll.
#[test]
fn each_watched_path_keeps_its_spelling() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/real").unwrap();
    d.fs().symlink("/p/real", "/p/link").unwrap();
    d.fs().write("/p/real/x.js", "1").unwrap();
    d.fs().write("/p/real/y.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/link/x.js", "/p/real/y.js"]), NONE, "{on}");
    d.fs().write("/p/real/x.js", "22").unwrap();
    d.fs().write("/p/link/y.js", "22").unwrap();
    assert_eq!(d.window(), ["U /p/link/x.js", "U /p/real/y.js"], "{on}");
    d.caught_up();
  });
}

/// A watch for a whole tree reports the real directories below it. A directory that is reached
/// through a link is not one of them and needs a watch of its own.
#[test]
fn a_linked_directory_inside_a_covered_tree() {
  for mode in Mode::ALL {
    let mut d = driver(mode);
    d.fs.mkdir_all("/real/dir/deep").unwrap();
    d.fs.mkdir("/p").unwrap();
    d.fs.symlink("/real/dir", "/p/link").unwrap();
    d.watch(&["/p", "/p/link/x.js"]);

    d.fs.write("/real/dir/x.js", "1").unwrap();
    d.fs.write("/real/dir/deep/y.js", "1").unwrap();
    flush(&mut d);
    assert_eq!(taken(&mut d), ["C /p/link/deep/y.js", "C /p/link/x.js"], "{mode:?}");
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    caught_up(&mut d);
  }
}

/// A link leads into a watched tree, so the stream of the tree reports the directory behind the
/// link too, by its real path. A directory above that one is renamed away and back before a
/// poll, with an edit below it meanwhile. The poll finds the link as it was, and no node on the
/// way to the watched file is the renamed directory. But the stream told about that directory,
/// and the path behind the link stood below it: everything behind the link is scanned again.
#[test]
fn a_directory_above_the_one_behind_a_link_is_away_and_back() {
  let mut d = driver(Mode::Prefix);
  d.fs.mkdir_all("/t/a/real/sub").unwrap();
  d.fs.write("/t/a/real/sub/g.js", "1").unwrap();
  d.fs.symlink("/t/a/real", "/l").unwrap();
  d.watch(&["/t"]);
  d.watch(&["/l/sub/g.js"]);
  assert_eq!(settled(&mut d), NONE);

  d.fs.rename("/t/a", "/t/x").unwrap();
  d.fs.write("/t/x/real/sub/g.js", "22").unwrap();
  d.fs.rename("/t/x", "/t/a").unwrap();
  flush(&mut d);
  assert_eq!(taken(&mut d), ["U /l/sub/g.js", "U /t/a/real/sub/g.js"]);
  caught_up(&mut d);
}

/// A link inside a watched directory is a file entry. `rm` and `ln -s` in the same tick of
/// the file system clock give the new link the inode number, the time and (with a target of
/// the same length) the size of the old one: the stored fingerprint is still the one on disk.
/// A link is never opened, so no hash can tell. The target tells.
#[test]
fn a_link_entry_pointed_elsewhere_in_the_same_time_step() {
  fn repoint<B: Deliver>(d: &Driver<B>, on: &str) {
    let stored = d.engine.fingerprints();
    d.fs.remove("/p/r/l").unwrap();
    d.fs.symlink("/x/b", "/p/r/l").unwrap();
    let watched = BTreeSet::from([PathBuf::from("/p/r")]);
    assert_eq!(check_fingerprints(&d.fs, &watched, &stored), Ok(()), "{on}: the fingerprint tells");
  }
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.set_time_step(Duration::from_secs(1));
    d.fs.mkdir_all("/p/r").unwrap();
    d.fs.symlink("/x/a", "/p/r/l").unwrap();
    d.watch(&["/p/r"]);
    repoint(&d, &format!("{mode:?}"));
    assert_eq!(window(&mut d), ["U /p/r/l"], "{mode:?}");
    caught_up(&mut d);
  }
  let mut d = Driver::poll(Options::default());
  d.fs.set_time_step(Duration::from_secs(2));
  d.fs.mkdir_all("/p/r").unwrap();
  d.fs.symlink("/x/a", "/p/r/l").unwrap();
  d.watch(&["/p/r"]);
  repoint(&d, "Poll");
  assert_eq!(window(&mut d), ["U /p/r/l"]);
  caught_up(&mut d);
}

/// Two watched paths are links to two directories, and the links trade their targets. One
/// flush finds both: each directory is watched again under the key of the other path. No
/// `unwatch` comes in between, so the backend has to hand each OS watch over from one key to
/// the other without a gap.
#[test]
fn two_linked_paths_trade_their_directories() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/x/sub").unwrap();
    d.fs().mkdir_all("/y/sub").unwrap();
    d.fs().mkdir("/p").unwrap();
    d.fs().write("/x/sub/in-x.js", "1").unwrap();
    d.fs().write("/y/sub/in-y.js", "1").unwrap();
    d.fs().symlink("/x", "/p/a").unwrap();
    d.fs().symlink("/y", "/p/b").unwrap();
    assert_eq!(d.watch(&["/p/a", "/p/b"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/a/sub/in-x.js", "/p/b/sub/in-y.js"], "{on}");

    for (link, target) in [("/p/a", "/y"), ("/p/b", "/x")] {
      d.fs().remove(link).unwrap();
      d.fs().symlink(target, link).unwrap();
    }
    assert_eq!(
      sorted(d.settled()),
      ["C /p/a/sub/in-y.js", "C /p/b/sub/in-x.js", "D /p/a/sub/in-x.js", "D /p/b/sub/in-y.js"],
      "{on}"
    );
    // Both directories report under their new names, with no help from the poll.
    d.fs().write("/x/sub/new.js", "1").unwrap();
    d.fs().write("/y/sub/in-y.js", "22").unwrap();
    assert!(d.fs().pending_raw() >= 2 || on == "Poll", "{on}: a watch is gone");
    assert_eq!(d.window(), ["U /p/a/sub/in-y.js", "C /p/b/sub/new.js"], "{on}");
    d.caught_up();
  });
}

/// A directory on the way to a watched path is moved, and a link to its new place is put where
/// it was. It is the same directory, so its identity says "nothing happened". But a watch by
/// path (FSEvents) reports real paths, and the real path is another one now: the old stream is
/// deaf for good.
#[test]
fn a_directory_is_replaced_by_a_link_to_itself() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir_all("/a/c/b").unwrap();
    d.fs.write("/a/c/b/f", "1").unwrap();
    d.watch(&["/a/c/b"]);

    d.fs.rename("/a", "/store").unwrap();
    d.fs.symlink("/store", "/a").unwrap();
    d.fs.write("/a/c/b/g", "1").unwrap();
    assert_eq!(settled(&mut d), ["C /a/c/b/g"], "{mode:?}");
    d.fs.write("/a/c/b/g", "22").unwrap();
    assert!(d.fs.pending_raw() > 0, "{mode:?}: no watch at the new real path");
    assert_eq!(window(&mut d), ["U /a/c/b/g"], "{mode:?}");
    caught_up(&mut d);
  }
}

/// The watched path is a link that nobody touches, and a directory on the way to its target
/// is replaced by a link to itself. Same directory, same link, another real path.
#[test]
fn the_real_path_behind_a_link_changes() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.mkdir_all("/b/b/a").unwrap();
    d.fs.write("/b/b/a/c", "1").unwrap();
    d.fs.symlink("/b/b", "/c").unwrap();
    d.watch(&["/c"]);
    assert_eq!(d.known.len(), 1, "{mode:?}");

    d.fs.rename("/b", "/store").unwrap();
    d.fs.symlink("/store", "/b").unwrap();
    d.fs.remove("/c/a/c").unwrap();
    assert_eq!(settled(&mut d), ["D /c/a/c"], "{mode:?}");
    d.fs.write("/c/a/d", "1").unwrap();
    assert!(d.fs.pending_raw() > 0, "{mode:?}: no watch at the new real path");
    assert_eq!(window(&mut d), ["C /c/a/d"], "{mode:?}");
    caught_up(&mut d);
  }
}

/// The same change, and from some call on the disk answers nothing in the flush that finds it.
/// Whatever the moment: when the disk answers again, the directory is watched at its new real
/// path.
#[test]
fn the_real_path_behind_a_link_changes_and_the_disk_stops_answering() {
  for mode in Mode::ALL {
    for moment in 0..30 {
      let on = format!("{mode:?}, call {moment}");
      let mut d = Driver::sim(mode, Options::default());
      d.fs.mkdir_all("/b/b/a").unwrap();
      d.fs.write("/b/b/a/c", "1").unwrap();
      d.fs.symlink("/b/b", "/c").unwrap();
      d.watch(&["/c"]);
      assert_eq!(d.known.len(), 1, "{on}");

      d.fs.rename("/b", "/store").unwrap();
      d.fs.symlink("/store", "/b").unwrap();
      d.fs.at_call(moment, |fs| fs.set_offline(Some(io::ErrorKind::TimedOut)));
      // One second later: the flush, and the poll that checks the link.
      d.advance(Duration::from_secs(1));
      d.deliver(usize::MAX);
      d.tick();
      d.fs.clear_traps();
      d.fs.set_offline(None);
      // Nothing changed. A file whose hash could not be read may count as changed.
      assert!(settled(&mut d).iter().all(|event| event.starts_with("U ")), "{on}");

      d.fs.write("/c/a/d", "1").unwrap();
      assert!(d.fs.pending_raw() > 0, "{on}: no watch at the new real path");
      assert_eq!(window(&mut d), ["C /c/a/d"], "{on}");
      caught_up(&mut d);
    }
  }
}

/// A link took the place of a directory inside a watched directory, and before the engine hears
/// of it, `watch` registers that name. Only a registered name is followed, so the call has to
/// look at the name itself: what is behind the link is its baseline. (Fuzz seed 97001874.)
#[test]
fn a_directory_became_a_link_before_its_name_is_registered() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/a/c").unwrap();
    d.fs().mkdir("/t").unwrap();
    d.fs().write("/p/a/c/old.js", "1").unwrap();
    d.fs().write("/t/new.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/a"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/a/c/old.js"], "{on}");

    // No hint of this reaches the engine before the next `watch`.
    d.fs().rename("/p/a/c", "/p/away").unwrap();
    d.fs().symlink("/t", "/p/a/c").unwrap();
    assert_eq!(d.watch(&["/p/a/c/gen/x.js"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/a/c/new.js", "/p/a/c/old.js"], "{on}");
    // The `Delete` waited for the delete delay.
    assert_eq!(d.settled(), ["D /p/a/c/old.js"], "{on}");
    d.caught_up();
  });
}

/// Between the look at a directory and its listing, the directory is moved away and a link to
/// another directory is put in its place. The listing went through the link, so it is not
/// taken: the same directory has to be at the path when the listing is done. The next look
/// finds the link, a file inside the watched directory, and not the files behind it.
#[test]
fn a_directory_becomes_a_link_while_it_is_listed() {
  let mut d = Driver::poll(Options::default());
  d.fs.mkdir_all("/p/r/d").unwrap();
  d.fs.mkdir_all("/q/other").unwrap();
  d.fs.write("/p/r/d/f", "1").unwrap();
  d.fs.write("/q/other/g", "1").unwrap();
  d.watch(&["/p/r"]);
  // The tenth call of the poll is the listing of `d`.
  d.fs.at_call(9, |fs| {
    fs.rename("/p/r/d", "/q/away").unwrap();
    fs.symlink("/q/other", "/p/r/d").unwrap();
  });
  assert!(d.step());
  assert_eq!(d.fs.traps_run(), 1);
  assert_eq!(taken(&mut d), NONE);
  assert_eq!(settled(&mut d), ["C /p/r/d", "D /p/r/d/f"]);
  caught_up(&mut d);
}
