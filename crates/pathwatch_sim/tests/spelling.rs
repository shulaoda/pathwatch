//! A watched path in another spelling than the disk has, on a disk that ignores the case of
//! letters (`SimFs::set_ignore_case`). The OS names an entry that is there as the disk spells
//! it, and one that is gone as the caller spelled it. So a hint for a name that the tree does
//! not have makes the directory check the names that can be it. The events (`C`, `U`, `D` and
//! the path) carry the registered spelling.

#![cfg(unix)]

use pathwatch::engine::Options;
use pathwatch_sim::{
  Driver, Mode, NONE, RECURSIVE, caught_up, on_all, on_modes, settled, taken, window,
};

/// The engine is told that the disk takes two spellings of a name for one.
fn loose() -> Options {
  Options { loose_spelling: true, ..Options::default() }
}

/// A single watched file, registered as `App.js` while the disk says `app.js`. Its `Update`,
/// `Delete` and `Create` come, under the registered spelling.
#[test]
fn a_watched_file_in_another_spelling() {
  on_all(&loose(), |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/p/src").unwrap();
    d.fs().write("/p/src/app.js", "1").unwrap();
    d.fs().write("/p/src/other.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/src/App.js"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/src/App.js"], "{on}");

    d.fs().write("/p/src/app.js", "22").unwrap();
    assert_eq!(d.window(), ["U /p/src/App.js"], "{on}");
    d.fs().remove("/p/src/app.js").unwrap();
    assert_eq!(d.settled(), ["D /p/src/App.js"], "{on}");
    // It comes back in a third spelling.
    d.fs().write("/p/src/APP.JS", "1").unwrap();
    assert_eq!(d.window(), ["C /p/src/App.js"], "{on}");
    d.fs().write("/p/src/APP.JS", "22").unwrap();
    assert_eq!(d.window(), ["U /p/src/App.js"], "{on}");
    d.caught_up();
  });
}

/// The other spelling is in a directory name on the way: `Components` is registered, the disk
/// says `components`, and a watch above reports `components/App.js`. The check of a registered
/// directory name goes deep, so the file below it is found.
#[test]
fn a_directory_on_the_way_in_another_spelling() {
  on_all(&loose(), |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/p/src/components/ui").unwrap();
    d.fs().write("/p/src/main.js", "1").unwrap();
    d.fs().write("/p/src/components/ui/App.js", "1").unwrap();
    // `main.js` makes `/p/src` tell its contents: where a watch covers a tree, it is this one.
    let paths = ["/p/src/main.js", "/p/src/Components/UI/App.js"];
    assert_eq!(d.watch(&paths), NONE, "{on}");
    assert_eq!(d.known(), ["/p/src/Components/UI/App.js", "/p/src/main.js"], "{on}");

    d.fs().write("/p/src/components/ui/App.js", "22").unwrap();
    assert_eq!(d.window(), ["U /p/src/Components/UI/App.js"], "{on}");
    d.fs().remove_all("/p/src/components").unwrap();
    assert_eq!(d.settled(), ["D /p/src/Components/UI/App.js"], "{on}");
    d.fs().mkdir_all("/p/src/COMPONENTS/ui").unwrap();
    d.fs().write("/p/src/COMPONENTS/ui/app.js", "1").unwrap();
    assert_eq!(d.settled(), ["C /p/src/Components/UI/App.js"], "{on}");
    d.caught_up();
  });
}

/// The other spelling is two names below the directory of the OS watch: `components` is
/// spelled as on disk, `UI` is not. The hint goes down through the names that match, and the
/// directory in which a name does not match any more checks its registered names.
#[test]
fn the_other_spelling_is_below_a_registered_directory() {
  on_all(&loose(), |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/p/src/components/ui").unwrap();
    d.fs().write("/p/src/main.js", "1").unwrap();
    d.fs().write("/p/src/components/ui/App.js", "1").unwrap();
    // `main.js` makes `/p/src` tell its contents: where a watch covers a tree, it is this one.
    let paths = ["/p/src/main.js", "/p/src/components/UI/App.js"];
    assert_eq!(d.watch(&paths), NONE, "{on}");

    d.fs().write("/p/src/components/ui/App.js", "22").unwrap();
    assert_eq!(d.window(), ["U /p/src/components/UI/App.js"], "{on}");
    d.caught_up();
  });
}

/// A watched directory in another spelling, below a watch for a tree that reports it as the
/// disk spells it. Its files are reported under the registered spelling of the directory and
/// the disk's spelling of their own names.
#[test]
fn a_watched_directory_in_another_spelling() {
  on_all(&loose(), |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/p/src/deep").unwrap();
    d.fs().write("/p/a.js", "1").unwrap();
    d.fs().write("/p/src/x.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/a.js", "/p/Src"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/Src/x.js", "/p/a.js"], "{on}");

    d.fs().write("/p/src/x.js", "22").unwrap();
    d.fs().write("/p/src/deep/New.js", "1").unwrap();
    assert_eq!(d.window(), ["C /p/Src/deep/New.js", "U /p/Src/x.js"], "{on}");
    d.caught_up();
  });
}

/// A hint for a name that is no registered name in any spelling is dropped: a build that writes
/// next to a watched file costs no look at the disk. Many hints for another spelling of one
/// registered name cost one check of that name.
#[test]
fn only_a_name_that_can_be_the_hinted_one_is_checked() {
  let mut d = Driver::sim(Mode::Inode, loose());
  d.fs.set_ignore_case(true);
  d.fs.mkdir_all("/p/src").unwrap();
  d.fs.write("/p/src/app.js", "1").unwrap();
  d.watch(&["/p/src/App.js", "/p/src/Not-there.js", "/p/src/\u{c9}t\u{e9}.js"]);

  let before = d.fs.calls();
  for file in 0..50 {
    d.fs.write(format!("/p/src/other{file}.js"), "1").unwrap();
  }
  d.fs.write("/p/src/app.jsx", "1").unwrap();
  assert_eq!(d.deliver(usize::MAX), 51);
  // No hint left work: nothing is due but the poll, and the disk was not asked.
  assert_eq!(d.engine.deadline(), Some(d.now + std::time::Duration::from_secs(1)));
  assert_eq!(d.fs.calls(), before);

  for round in 0..50 {
    d.fs.write("/p/src/APP.js", format!("2{round}")).unwrap();
  }
  assert_eq!(d.fs.pending_raw(), 50);
  assert_eq!(window(&mut d), ["U /p/src/App.js"]);
  let after = d.fs.calls();
  // The one registered name. The directory is not listed.
  assert_eq!((after.lstat - before.lstat, after.read_dir - before.read_dir), (1, 0));

  // A name with a letter outside ASCII: the rules of the disk for such letters are not known,
  // so every registered name with such a letter is checked, and no other.
  let before = d.fs.calls();
  d.fs.write("/p/src/caf\u{e9}.js", "1").unwrap();
  assert_eq!(window(&mut d), NONE);
  let after = d.fs.calls();
  assert_eq!((after.lstat - before.lstat, after.read_dir - before.read_dir), (1, 0));
  caught_up(&mut d);
}

/// The other spelling is in a directory name, next to names that have nothing to do with it:
/// `Components` is registered, the disk says `components`. A write below `components` is found
/// by a deep check of `Components` alone, and a write below any other name costs nothing.
#[test]
fn the_other_spelling_of_a_directory_next_to_unrelated_names() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, loose());
    d.fs.set_ignore_case(true);
    d.fs.mkdir_all("/p/src/components").unwrap();
    d.fs.mkdir_all("/p/src/lib").unwrap();
    d.fs.write("/p/src/main.js", "1").unwrap();
    d.fs.write("/p/src/components/App.js", "1").unwrap();
    // `main.js` makes `/p/src` tell its contents: where a watch covers a tree, it is this one.
    d.watch(&["/p/src/main.js", "/p/src/Components/App.js"]);
    assert_eq!(
      pathwatch_sim::known(&d),
      ["/p/src/Components/App.js", "/p/src/main.js"],
      "{mode:?}"
    );

    let before = d.fs.calls();
    d.fs.write("/p/src/lib/x.js", "1").unwrap();
    d.fs.write("/p/src/out.js", "1").unwrap();
    d.fs.write("/p/src/component", "1").unwrap();
    assert!(d.deliver(usize::MAX) >= 2, "{mode:?}");
    // No hint left work: nothing is due but the poll, and the disk was not asked.
    assert_eq!(d.engine.deadline(), Some(d.now + std::time::Duration::from_secs(1)), "{mode:?}");
    assert_eq!(d.fs.calls(), before, "{mode:?}");

    d.fs.write("/p/src/components/App.js", "22").unwrap();
    assert_eq!(window(&mut d), ["U /p/src/Components/App.js"], "{mode:?}");
    d.fs.remove_all("/p/src/components").unwrap();
    assert_eq!(settled(&mut d), ["D /p/src/Components/App.js"], "{mode:?}");
    d.fs.mkdir("/p/src/COMPONENTS").unwrap();
    d.fs.write("/p/src/COMPONENTS/app.js", "1").unwrap();
    assert_eq!(settled(&mut d), ["C /p/src/Components/App.js"], "{mode:?}");
    caught_up(&mut d);
  }
}

/// Where the disk knows a name under one spelling only, the engine is not told otherwise, and
/// a name that is not registered costs nothing: no watched path can see it.
#[test]
fn a_strict_disk_costs_nothing() {
  let mut d = Driver::sim(Mode::Inode, Options::default());
  d.fs.mkdir_all("/p/src").unwrap();
  d.fs.write("/p/src/app.js", "1").unwrap();
  d.watch(&["/p/src/App.js"]);
  assert_eq!(pathwatch_sim::known(&d), NONE);

  let before = d.fs.calls();
  d.fs.write("/p/src/app.js", "22").unwrap();
  d.fs.write("/p/src/other.js", "1").unwrap();
  assert_eq!(d.deliver(usize::MAX), 2);
  // No hint left work: nothing is due but the poll.
  assert_eq!(d.engine.deadline(), Some(d.now + std::time::Duration::from_secs(1)));
  assert_eq!(d.fs.calls(), before);
  // `App.js` is another file than `app.js` on this disk.
  d.fs.write("/p/src/App.js", "1").unwrap();
  assert_eq!(window(&mut d), ["C /p/src/App.js"]);
  caught_up(&mut d);
}

/// What the rule does not cover: the watch of a watched directory reports a registered name
/// below it in the spelling of the disk, and the directory knows that name too. The hint finds
/// the entry of the directory, and the registered spelling is not checked.
///
/// The file is then known under two names, and only the name of the disk gets the events. It
/// is listed in `docs/design.md` as a known limit.
#[test]
fn a_watched_path_in_another_spelling_inside_a_watched_directory() {
  on_modes(&RECURSIVE, &loose(), |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/p/R").unwrap();
    d.fs().write("/p/R/app.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R", "/p/R/App.js"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/R/App.js", "/p/R/app.js"], "{on}");

    d.fs().write("/p/R/app.js", "22").unwrap();
    assert_eq!(d.window(), ["U /p/R/app.js"], "{on}");
    d.not_caught_up("one file under two names: a known limit of a disk that ignores case");
  });
}

// ---- an entry below a watched directory gets another spelling ----
//
// `lstat` finds the entry under the old name too. So the disk is asked how it spells a name
// that was found, when that is not sure: the tree did not have the name, or it has the name in
// two spellings. An entry that the disk spells in another way is gone at once.

/// A rename inside a watched directory that changes only the case of the letters: the file
/// under the old name is gone, and there is a new one. Both events come in one batch.
#[test]
fn a_file_is_renamed_to_another_letter_case() {
  on_all(&loose(), |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/p/R").unwrap();
    d.fs().write("/p/R/app.js", "1").unwrap();
    d.fs().write("/p/R/other.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R"]), NONE, "{on}");

    d.fs().rename("/p/R/app.js", "/p/R/App.js").unwrap();
    assert_eq!(d.window(), ["C /p/R/App.js", "D /p/R/app.js"], "{on}");
    assert_eq!(d.known(), ["/p/R/App.js", "/p/R/other.js"], "{on}");
    assert_eq!(d.settled(), NONE, "{on}");

    // The new name is the one that gets the events from now on.
    d.fs().write("/p/R/App.js", "22").unwrap();
    assert_eq!(d.window(), ["U /p/R/App.js"], "{on}");
    d.fs().remove("/p/R/App.js").unwrap();
    assert_eq!(d.settled(), ["D /p/R/App.js"], "{on}");
    d.caught_up();
  });
}

/// The same for a directory: one `Delete` for each file below the old name, one `Create` for
/// each file below the new one, in one batch.
#[test]
fn a_directory_is_renamed_to_another_letter_case() {
  on_all(&loose(), |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/p/R/Src/deep").unwrap();
    d.fs().write("/p/R/Src/a.js", "1").unwrap();
    d.fs().write("/p/R/Src/deep/b.js", "1").unwrap();
    d.fs().write("/p/R/other.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R"]), NONE, "{on}");

    d.fs().rename("/p/R/Src", "/p/R/SRC").unwrap();
    let events =
      ["C /p/R/SRC/a.js", "C /p/R/SRC/deep/b.js", "D /p/R/Src/a.js", "D /p/R/Src/deep/b.js"];
    assert_eq!(d.window(), events, "{on}");
    assert_eq!(d.settled(), NONE, "{on}");

    // What is below the new name is watched.
    d.fs().write("/p/R/SRC/deep/b.js", "22").unwrap();
    d.fs().write("/p/R/SRC/deep/c.js", "1").unwrap();
    assert_eq!(d.window(), ["U /p/R/SRC/deep/b.js", "C /p/R/SRC/deep/c.js"], "{on}");
    d.caught_up();
  });
}

/// The same for a link below a watched directory. The disk cannot be asked how it spells a
/// link (the real path of a link is the path of its target), so the listing is believed.
#[test]
fn a_link_is_renamed_to_another_letter_case() {
  on_all(&loose(), |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/p/R").unwrap();
    d.fs().write("/p/link.js", "1").unwrap();
    d.fs().symlink("/p/link.js", "/p/R/link.js").unwrap();
    assert_eq!(d.watch(&["/p/R"]), NONE, "{on}");

    d.fs().rename("/p/R/link.js", "/p/R/Link.js").unwrap();
    assert_eq!(d.window(), ["C /p/R/Link.js", "D /p/R/link.js"], "{on}");
    assert_eq!(d.settled(), NONE, "{on}");
    d.caught_up();
  });
}

/// The file is renamed away, and its `Delete` waits. Then it comes back under another letter
/// case. The second look would find it under the old name and take it for the same file.
#[test]
fn a_file_comes_back_in_another_letter_case_while_its_delete_waits() {
  on_all(&loose(), |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/p/R").unwrap();
    d.fs().write("/p/R/app.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R"]), NONE, "{on}");

    d.fs().rename("/p/R/app.js", "/p/away.js").unwrap();
    assert_eq!(d.window(), NONE, "{on}");
    assert_eq!(d.pending_deletes(), 1, "{on}");
    d.fs().rename("/p/away.js", "/p/R/App.js").unwrap();
    assert_eq!(d.window(), ["C /p/R/App.js", "D /p/R/app.js"], "{on}");
    assert_eq!(d.pending_deletes(), 0, "{on}");
    assert_eq!(d.settled(), NONE, "{on}");
    d.caught_up();
  });
}

/// The two notifications of the rename come in two flushes, the new name first. The late one
/// for the old name makes a node again, `lstat` finds the file there, and the scan that
/// follows takes the node away before the handler hears of it.
#[test]
fn the_hint_for_the_old_spelling_comes_late() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, loose());
    d.fs.set_ignore_case(true);
    d.fs.mkdir_all("/p/R").unwrap();
    d.fs.write("/p/R/app.js", "1").unwrap();
    d.watch(&["/p/R"]);

    d.fs.rename("/p/R/app.js", "/p/R/App.js").unwrap();
    let mut events = Vec::new();
    for raw in d.fs.take_raw(usize::MAX).into_iter().rev() {
      d.signal(raw);
      d.step();
      events.append(&mut taken(&mut d));
    }
    assert_eq!(events, ["C /p/R/App.js", "D /p/R/app.js"], "{mode:?}");
    assert_eq!(pathwatch_sim::known(&d), ["/p/R/App.js"], "{mode:?}");
    caught_up(&mut d);
  }
}

/// The listing does not name a file, and right after it the file is there again under its own
/// name. `lstat` finds it, and the disk says that this is how it spells the name: the file is
/// not taken for an entry in another spelling.
#[test]
fn a_file_that_comes_back_right_after_the_listing_is_not_another_spelling() {
  let mut d = Driver::poll(loose());
  d.fs.set_ignore_case(true);
  d.fs.mkdir_all("/p/R").unwrap();
  d.fs.write("/p/R/app.js", "1").unwrap();
  d.watch(&["/p/R"]);

  d.fs.remove("/p/R/app.js").unwrap();
  // The poll: `lstat` of `/`, of `/p` and of `/p/R`, then the listing of `/p/R`.
  d.fs.at_call(4, |fs| fs.write("/p/R/app.js", "22").unwrap());
  assert!(d.step());
  assert_eq!(d.fs.traps_run(), 1);
  // The disk was asked how it spells the name.
  assert_eq!(d.fs.calls().name_on_disk, 1);
  assert_eq!(taken(&mut d), ["U /p/R/app.js"]);
  caught_up(&mut d);
}

/// A watched path keeps the spelling of the caller, and `lstat` is asked about it: the rule of
/// the listing is not for a registered name. The watched file is renamed to another letter
/// case, and its events still come under the watched spelling. Where a watched directory holds
/// it too, the directory reports the new name, and the watched path loses nothing.
#[test]
fn a_watched_file_is_renamed_to_another_letter_case() {
  on_all(&loose(), |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/p/src").unwrap();
    d.fs().mkdir_all("/p/R").unwrap();
    d.fs().write("/p/src/app.js", "1").unwrap();
    d.fs().write("/p/R/app.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/src/app.js", "/p/R", "/p/R/app.js"]), NONE, "{on}");

    d.fs().rename("/p/src/app.js", "/p/src/App.js").unwrap();
    d.fs().rename("/p/R/app.js", "/p/R/App.js").unwrap();
    assert_eq!(d.settled(), ["C /p/R/App.js"], "{on}");
    assert_eq!(d.known(), ["/p/R/App.js", "/p/R/app.js", "/p/src/app.js"], "{on}");

    d.fs().write("/p/src/App.js", "22").unwrap();
    assert_eq!(d.window(), ["U /p/src/app.js"], "{on}");
    d.fs().remove("/p/src/App.js").unwrap();
    assert_eq!(d.settled(), ["D /p/src/app.js"], "{on}");
    d.caught_up();
  });
}

/// The limit once more, in a directory that only leads to watched paths: two watched paths
/// that are two spellings of one file. The hint names the file as the disk spells it, that
/// name is registered, and so nothing makes the directory check its other registered names.
/// Only the spelling of the disk gets the events. With polling both get them.
#[test]
fn two_watched_paths_that_are_two_spellings_of_one_file() {
  on_modes(&Mode::ALL, &loose(), |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/p/src").unwrap();
    d.fs().write("/p/src/app.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/src/App.js", "/p/src/app.js"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/src/App.js", "/p/src/app.js"], "{on}");

    d.fs().write("/p/src/app.js", "22").unwrap();
    assert_eq!(d.window(), ["U /p/src/app.js"], "{on}");
    d.not_caught_up("one file under two watched names: only the name of the disk is checked");
  });
}

/// A watched directory is renamed, and only the case of its letters changes, on a disk that
/// ignores case. The registered spelling still finds the directory. But a watch by path stands
/// at the old spelling, and the OS reports the new one: the watch is deaf. The rename moves the
/// change time of the directory alone, so the next poll watches it again, also when nothing
/// told about the rename.
#[test]
fn a_watched_directory_is_renamed_to_another_case() {
  for told in [true, false] {
    on_modes(&RECURSIVE, &Options::default(), |d, on| {
      let on = format!("{on}, the rename is told: {told}");
      d.fs().set_ignore_case(true);
      d.fs().mkdir_all("/p/src").unwrap();
      d.fs().write("/p/src/x.js", "1").unwrap();
      assert_eq!(d.watch(&["/p/src"]), NONE, "{on}");
      assert_eq!(d.settled(), NONE, "{on}");

      d.fs().rename("/p/src", "/p/Src").unwrap();
      if !told {
        d.fs().take_raw(usize::MAX);
      }
      d.fs().write("/p/Src/x.js", "22").unwrap();
      assert_eq!(d.settled(), ["U /p/src/x.js"], "{on}");
      d.fs().write("/p/Src/x.js", "333").unwrap();
      assert!(d.fs().pending_raw() > 0, "{on}");
      assert_eq!(d.window(), ["U /p/src/x.js"], "{on}");
      d.caught_up();
    });
  }
}

/// A new entry in a watched directory, on a disk that takes two spellings of a name for one.
/// The disk is asked how it spells the one new name. No listing is made, however many entries
/// the directory has, and a file that the tree knows is looked at as on any disk.
#[test]
fn a_new_entry_costs_one_look_and_one_question() {
  for entries in [3, 300] {
    let mut d = Driver::sim(Mode::Inode, loose());
    d.fs.set_ignore_case(true);
    d.fs.mkdir_all("/p/r/sub").unwrap();
    for entry in 0..entries {
      d.fs.write(format!("/p/r/file{entry}.js"), "1").unwrap();
    }
    d.watch(&["/p/r"]);
    assert_eq!(settled(&mut d), NONE);
    // How often the disk was asked: `lstat`, the spelling of a name, a listing.
    let asked = |d: &Driver<_>| {
      let calls = d.fs.calls();
      [calls.lstat, calls.name_on_disk, calls.read_dir]
    };
    let since = |before: [u64; 3], d: &Driver<_>| {
      let after = asked(d);
      [after[0] - before[0], after[1] - before[1], after[2] - before[2]]
    };

    let before = asked(&d);
    d.fs.write("/p/r/new.js", "1").unwrap();
    assert_eq!(window(&mut d), ["C /p/r/new.js"], "{entries}");
    assert_eq!(since(before, &d), [1, 1, 0], "{entries}");

    let before = asked(&d);
    d.fs.write("/p/r/file2.js", "22").unwrap();
    assert_eq!(window(&mut d), ["U /p/r/file2.js"], "{entries}");
    assert_eq!(since(before, &d), [1, 0, 0], "{entries}");

    // A known entry gets another spelling: a look and a question for each of the two names.
    let before = asked(&d);
    d.fs.rename("/p/r/file1.js", "/p/r/FILE1.js").unwrap();
    assert_eq!(window(&mut d), ["C /p/r/FILE1.js", "D /p/r/file1.js"], "{entries}");
    assert_eq!(since(before, &d), [2, 2, 0], "{entries}");
    caught_up(&mut d);
  }
}

/// The OS names an entry that is gone as the caller spelled it. The file that the caller
/// meant is found by its name without the case of the letters, and no listing is made.
#[test]
fn an_entry_goes_through_another_spelling_of_its_name() {
  on_all(&loose(), |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/p/R").unwrap();
    d.fs().write("/p/R/app.js", "1").unwrap();
    d.fs().write("/p/R/other.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R"]), NONE, "{on}");

    d.fs().remove("/p/R/APP.JS").unwrap();
    assert_eq!(d.settled(), ["D /p/R/app.js"], "{on}");
    d.fs().rename("/p/R/Other.js", "/p/away.js").unwrap();
    assert_eq!(d.settled(), ["D /p/R/other.js"], "{on}");
    d.caught_up();

    // The name is free: a file that comes there is a new one.
    d.fs().write("/p/R/app.js", "22").unwrap();
    assert_eq!(d.settled(), ["C /p/R/app.js"], "{on}");
    d.caught_up();
  });
}

/// A name with a letter outside ASCII: the table of the tree does not find its other
/// spellings. Such a name is asked from the disk at every look, and when a name that is gone
/// names no node, one listing shows which node is gone.
#[test]
fn a_name_with_a_letter_outside_ascii() {
  on_all(&loose(), |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/p/R").unwrap();
    d.fs().write("/p/R/Caf\u{e9}.js", "1").unwrap();
    d.fs().write("/p/R/th\u{e9}.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R"]), NONE, "{on}");

    d.fs().rename("/p/R/Caf\u{e9}.js", "/p/R/caf\u{e9}.js").unwrap();
    assert_eq!(d.window(), ["D /p/R/Caf\u{e9}.js", "C /p/R/caf\u{e9}.js"], "{on}");
    assert_eq!(d.settled(), NONE, "{on}");

    d.fs().remove("/p/R/TH\u{e9}.JS").unwrap();
    assert_eq!(d.settled(), ["D /p/R/th\u{e9}.js"], "{on}");
    d.caught_up();
  });
}
