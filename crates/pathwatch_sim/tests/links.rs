//! Links: on the way to a watched path, at a watched path, and inside a watched directory.
//! A story makes the disk, calls `watch`, changes the disk and compares the events that come
//! out (`C`, `U`, `D` and the path). `on_all` tells one story on every backend.

#![cfg(unix)]

use std::{io, path::Path, sync::Arc, time::Duration};

use pathwatch::engine::Options;
use pathwatch_sim::{
  Deliver, Driver, Mode, NONE, SimBackend, caught_up, flush, on_all, settled, sorted, taken, window,
};

fn driver(mode: Mode) -> Driver<SimBackend> {
  Driver::sim(mode, Options::default())
}

/// The registered path goes through a link (as `/tmp` on macOS), and the OS reports real paths.
#[test]
fn a_linked_prefix() {
  for mode in Mode::ALL {
    let mut d = driver(mode);
    d.fs.mkdir_all("/private/tmp/proj").unwrap();
    d.fs.symlink("private/tmp", "/tmp").unwrap();
    d.fs.write("/tmp/proj/a.js", "1").unwrap();
    d.watch(&["/tmp/proj/a.js", "/tmp/proj/gen"]);

    d.fs.write("/private/tmp/proj/a.js", "12").unwrap();
    d.fs.mkdir("/tmp/proj/gen").unwrap();
    d.fs.write("/tmp/proj/gen/out.js", "1").unwrap();
    flush(&mut d);
    assert_eq!(taken(&mut d), ["U /tmp/proj/a.js", "C /tmp/proj/gen/out.js"], "{mode:?}");
    caught_up(&mut d);
  }
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

/// Nothing watches the target of a link. The poll finds a change behind it.
#[test]
fn a_change_behind_a_link() {
  for mode in Mode::ALL {
    let mut d = driver(mode);
    d.fs.mkdir_all("/p").unwrap();
    d.fs.mkdir_all("/shared/real").unwrap();
    d.fs.write("/shared/config.json", "1").unwrap();
    d.fs.write("/shared/real/c.js", "1").unwrap();
    d.fs.symlink("/shared/config.json", "/p/config.json").unwrap();
    d.fs.symlink("/shared/real", "/p/link").unwrap();
    d.watch(&["/p/config.json", "/p/link/c.js"]);

    // The watched file is a link, and its target is written.
    d.fs.write("/shared/config.json", "12").unwrap();
    assert_eq!(settled(&mut d), ["U /p/config.json"], "{mode:?}");

    // A name on the way is a link, and its target goes away and comes back.
    d.fs.rename("/shared/real", "/shared/gone").unwrap();
    assert_eq!(settled(&mut d), ["D /p/link/c.js"], "{mode:?}");
    d.fs.mkdir("/shared/real").unwrap();
    d.fs.write("/shared/real/c.js", "1").unwrap();
    assert_eq!(settled(&mut d), ["C /p/link/c.js"], "{mode:?}");

    // The link itself is pointed at another file.
    d.fs.write("/shared/other.json", "123").unwrap();
    d.fs.remove("/p/config.json").unwrap();
    d.fs.symlink("/shared/other.json", "/p/config.json").unwrap();
    assert_eq!(settled(&mut d), ["U /p/config.json"], "{mode:?}");
    caught_up(&mut d);
  }
}

/// A link is followed for the check, and the poll finds what changes behind it.
#[test]
fn changes_behind_a_link() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir("/p").unwrap();
    d.fs().mkdir_all("/shared").unwrap();
    d.fs().mkdir_all("/z/real/b").unwrap();
    d.fs().mkdir("/a").unwrap();
    d.fs().write("/shared/config.json", "1").unwrap();
    d.fs().write("/z/real/b/c.js", "1").unwrap();
    d.fs().symlink("/shared/config.json", "/p/config.json").unwrap();
    d.fs().symlink("/z/real", "/a/link").unwrap();
    assert_eq!(d.watch(&["/p/config.json", "/a/link/b/c.js"]), NONE, "{on}");

    // (a) The watched file is a link, and its target is written.
    d.fs().write("/shared/config.json", "22").unwrap();
    assert_eq!(d.settled(), ["U /p/config.json"], "{on}");

    // (b) A name on the way is a link, and its target goes away and comes back as a new
    // directory.
    d.fs().rename("/z/real", "/z/gone").unwrap();
    assert_eq!(d.settled(), ["D /a/link/b/c.js"], "{on}");
    d.fs().mkdir_all("/z/real/b").unwrap();
    d.fs().write("/z/real/b/c.js", "new").unwrap();
    assert_eq!(d.settled(), ["C /a/link/b/c.js"], "{on}");
    d.fs().write("/z/real/b/c.js", "newer").unwrap();
    assert_eq!(d.settled(), ["U /a/link/b/c.js"], "{on}");
    d.caught_up();
  });
}

/// A watched path that is reached through two spellings of one directory gets its events under
/// each spelling.
#[test]
fn two_spellings_of_one_directory() {
  for mode in Mode::ALL {
    let mut d = driver(mode);
    d.fs.mkdir_all("/p/real").unwrap();
    d.fs.symlink("/p/real", "/p/link").unwrap();
    d.fs.write("/p/real/x.js", "1").unwrap();
    d.fs.write("/p/real/y.js", "1").unwrap();
    d.watch(&["/p/link/x.js", "/p/real/y.js", "/p/real/x.js"]);

    d.fs.write("/p/real/x.js", "12").unwrap();
    d.fs.write("/p/link/y.js", "12").unwrap();
    d.deliver(usize::MAX);
    assert!(d.step());
    // No poll was needed: the hints came under every key.
    assert_eq!(taken(&mut d), ["U /p/link/x.js", "U /p/real/x.js", "U /p/real/y.js"], "{mode:?}");
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

/// A file inside a watched directory turns out to be on the way to a new watched path, and it
/// is a link to a directory: it stops being a file.
#[test]
fn a_link_entry_becomes_an_ancestor() {
  let mut d = driver(Mode::Inode);
  d.fs.mkdir_all("/p/d").unwrap();
  d.fs.mkdir_all("/q/dir").unwrap();
  d.fs.write("/q/dir/x.js", "1").unwrap();
  d.fs.symlink("/q/dir", "/p/d/l").unwrap();
  d.watch(&["/p/d"]);
  assert_eq!(d.known.iter().collect::<Vec<_>>(), [Path::new("/p/d/l")]);

  d.watch(&["/p/d/l/x.js"]);
  assert_eq!(taken(&mut d), ["D /p/d/l"]);
  assert_eq!(d.known.iter().collect::<Vec<_>>(), [Path::new("/p/d/l/x.js")]);
  caught_up(&mut d);
}

/// Links in a watched path (see also `changes_behind_a_link`): a link inside a watched
/// directory is a file entry until a watched path goes through it; then it is followed and
/// stops being one.
#[test]
fn a_link_entry_becomes_an_ancestor_and_is_followed() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/d").unwrap();
    d.fs().mkdir_all("/q/dir").unwrap();
    d.fs().write("/q/dir/x.js", "1").unwrap();
    d.fs().symlink("/q/dir", "/p/d/l").unwrap();
    assert_eq!(d.watch(&["/p/d"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/d/l"], "{on}");

    assert_eq!(d.watch(&["/p/d/l/x.js"]), ["D /p/d/l"], "{on}");
    assert_eq!(d.known(), ["/p/d/l/x.js"], "{on}");
    d.fs().write("/q/dir/x.js", "22").unwrap();
    d.fs().write("/q/dir/y.js", "1").unwrap();
    assert_eq!(d.settled(), ["U /p/d/l/x.js", "C /p/d/l/y.js"], "{on}");
    d.caught_up();
  });
}

/// A link to a directory sits in a watched directory, and the filter ignores it as a file. Then
/// the link becomes a watched path: it is followed from now on, and what is behind it is the
/// baseline. The watched directory above never had to report that: for it the link was a file.
#[test]
fn an_ignored_link_becomes_a_watched_path() {
  let options = Options {
    ignored: Some(Arc::new(|path: &Path, kind| {
      kind == pathwatch::EntryKind::File && path.ends_with("link")
    })),
    ..Options::default()
  };
  on_all(&options, |d, on| {
    d.fs().mkdir_all("/p/d").unwrap();
    d.fs().mkdir_all("/q/dir/deep").unwrap();
    d.fs().write("/q/dir/x.js", "1").unwrap();
    d.fs().write("/q/dir/deep/y.js", "1").unwrap();
    d.fs().symlink("/q/dir", "/p/d/link").unwrap();
    assert_eq!(d.watch(&["/p/d"]), NONE, "{on}");
    assert_eq!(d.known(), NONE, "{on}");
    assert_eq!(d.settled(), NONE, "{on}");

    assert_eq!(d.watch(&["/p/d/link"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/d/link/deep/y.js", "/p/d/link/x.js"], "{on}");
    d.fs().write("/q/dir/new.js", "1").unwrap();
    assert_eq!(d.settled(), ["C /p/d/link/new.js"], "{on}");
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
/// the same length) the size of the old one. A link is never opened, so no hash can tell.
#[test]
fn a_link_entry_pointed_elsewhere_in_the_same_time_step() {
  fn repoint<B: Deliver>(d: &Driver<B>) {
    d.fs.remove("/p/r/l").unwrap();
    d.fs.symlink("/x/b", "/p/r/l").unwrap();
  }
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, Options::default());
    d.fs.set_time_step(Duration::from_secs(1));
    d.fs.mkdir_all("/p/r").unwrap();
    d.fs.symlink("/x/a", "/p/r/l").unwrap();
    d.watch(&["/p/r"]);
    repoint(&d);
    assert_eq!(window(&mut d), ["U /p/r/l"], "{mode:?}");
    caught_up(&mut d);
  }
  let mut d = Driver::poll(Options::default());
  d.fs.set_time_step(Duration::from_secs(2));
  d.fs.mkdir_all("/p/r").unwrap();
  d.fs.symlink("/x/a", "/p/r/l").unwrap();
  d.watch(&["/p/r"]);
  repoint(&d);
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
