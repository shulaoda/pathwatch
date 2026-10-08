//! A watched path in another spelling than the disk has, on a disk that ignores the case of
//! letters (`SimFs::set_ignore_case`). The OS names an entry that is there as the disk spells
//! it, and one that is gone as the caller spelled it. So a hint for a name that the tree does
//! not have makes the directory check the names that can be it. The events (`C`, `U`, `D` and
//! the path) carry the registered spelling.

#![cfg(unix)]

use std::sync::Arc;

use pathwatch::engine::{Ignored, Options};
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
  // A name of ideographs is only itself, like a name of ASCII letters.
  d.fs.write("/p/src/\u{4e2d}\u{6587}.js", "1").unwrap();
  assert_eq!(d.deliver(usize::MAX), 52);
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

  // A name with an accent: the rules of the disk for such letters are not known, so every
  // registered name with such a letter is checked, and no other.
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

/// A watched path is the path that its caller gave, and the OS says what is at it. So on every
/// disk it is looked at when a name next to it changes that differs only in the case of its
/// letters. On a disk that tells the two names apart, that is one look that finds nothing new.
/// A name that is no spelling of a watched path costs nothing.
#[test]
fn a_strict_disk_costs_one_look() {
  let mut d = Driver::sim(Mode::Inode, Options::default());
  d.fs.mkdir_all("/p/src").unwrap();
  d.fs.write("/p/src/app.js", "1").unwrap();
  d.watch(&["/p/src/App.js"]);
  assert_eq!(pathwatch_sim::known(&d), NONE);

  let before = d.fs.calls();
  d.fs.write("/p/src/other.js", "1").unwrap();
  assert_eq!(d.deliver(usize::MAX), 1);
  // The hint left no work: nothing is due but the poll.
  assert_eq!(d.engine.deadline(), Some(d.now + std::time::Duration::from_secs(1)));
  assert_eq!(d.fs.calls(), before);

  d.fs.write("/p/src/app.js", "22").unwrap();
  assert_eq!(window(&mut d), NONE);
  assert_eq!(d.fs.calls().lstat - before.lstat, 1);
  // `App.js` is another file than `app.js` on this disk.
  d.fs.write("/p/src/App.js", "1").unwrap();
  assert_eq!(window(&mut d), ["C /p/src/App.js"]);
  d.fs.write("/p/src/app.js", "333").unwrap();
  assert_eq!(window(&mut d), NONE);
  caught_up(&mut d);
}

/// Two watches are two questions. The watch of the directory reports the file as the disk
/// spells it, and the watched path reports it in the spelling of its caller.
#[test]
fn a_watched_path_in_another_spelling_inside_a_watched_directory() {
  on_all(&loose(), |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/p/R").unwrap();
    d.fs().write("/p/R/app.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R", "/p/R/App.js"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/R/App.js", "/p/R/app.js"], "{on}");

    d.fs().write("/p/R/app.js", "22").unwrap();
    assert_eq!(d.window(), ["U /p/R/App.js", "U /p/R/app.js"], "{on}");
    d.fs().remove("/p/R/app.js").unwrap();
    assert_eq!(d.settled(), ["D /p/R/App.js", "D /p/R/app.js"], "{on}");
    d.fs().write("/p/R/app.js", "333").unwrap();
    assert_eq!(d.settled(), ["C /p/R/App.js", "C /p/R/app.js"], "{on}");

    // The entry goes and comes back as `App.js`, and no hint names `app.js`: the directory
    // has no entry of that name any more.
    d.fs().remove("/p/R/App.js").unwrap();
    d.fs().write("/p/R/App.js", "4444").unwrap();
    assert_eq!(d.settled(), ["U /p/R/App.js", "D /p/R/app.js"], "{on}");
    d.caught_up();
  });
}

/// A file at a name that only leads to a watched path, inside a watched directory. The file is
/// an entry of the directory while the disk spells it like the watched path. When the disk
/// spells it in another way, it is gone under this name at once: it is there, so no second
/// look can bring it back.
#[test]
fn a_file_at_a_name_on_the_way_changes_its_spelling() {
  on_all(&loose(), |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/p").unwrap();
    d.fs().write("/p/SRC", "1").unwrap();
    assert_eq!(d.watch(&["/p", "/p/SRC/a.js"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/SRC"], "{on}");

    d.fs().rename("/p/SRC", "/p/src").unwrap();
    assert_eq!(d.window(), ["D /p/SRC", "C /p/src"], "{on}");
    d.fs().rename("/p/src", "/p/SRC").unwrap();
    assert_eq!(d.window(), ["C /p/SRC", "D /p/src"], "{on}");
    // The same when a link that leads nowhere takes the place: the link is the entry.
    d.fs().remove("/p/SRC").unwrap();
    d.fs().symlink("/nowhere", "/p/src").unwrap();
    assert_eq!(d.window(), ["D /p/SRC", "C /p/src"], "{on}");
    d.caught_up();
  });
}

/// The same one level up: a watched directory, and below it a watched file and a watched
/// directory that are reached through another spelling of a directory in it. The hint comes
/// for the directory at the top, and the walk goes on through each spelling.
#[test]
fn watched_paths_through_another_spelling_of_a_directory_inside_a_watched_directory() {
  on_all(&loose(), |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/p/src/lib").unwrap();
    d.fs().write("/p/src/a.js", "1").unwrap();
    d.fs().write("/p/src/lib/b.js", "1").unwrap();
    assert_eq!(d.watch(&["/p", "/p/SRC/a.js", "/p/Src/LIB"]), NONE, "{on}");

    d.fs().write("/p/src/a.js", "22").unwrap();
    assert_eq!(d.window(), ["U /p/SRC/a.js", "U /p/src/a.js"], "{on}");
    d.fs().write("/p/src/lib/b.js", "22").unwrap();
    d.fs().write("/p/src/lib/c.js", "1").unwrap();
    let expected =
      ["U /p/Src/LIB/b.js", "C /p/Src/LIB/c.js", "U /p/src/lib/b.js", "C /p/src/lib/c.js"];
    assert_eq!(d.window(), expected, "{on}");
    d.fs().remove("/p/src/a.js").unwrap();
    d.fs().remove("/p/src/lib/b.js").unwrap();
    let expected = ["D /p/SRC/a.js", "D /p/Src/LIB/b.js", "D /p/src/a.js", "D /p/src/lib/b.js"];
    assert_eq!(d.settled(), expected, "{on}");
    d.caught_up();
  });
}

// ---- the rule belongs to the directory ----
//
// Whether two spellings are one name is a matter of the disk under a directory, and one system
// has disks of both kinds: a Linux machine with a folder of a Mac in it. Taking a strict disk
// for a loose one only costs looks, and the other way round gives wrong events. So a directory
// counts as loose until a look proves that it is strict.

/// The first new name of a watched directory is looked at in the other case of its letters
/// too, and then once more as it is. On a disk that tells the two apart, that is the proof: no
/// later look in this directory asks anything. Only a second name in another case of its
/// letters puts the proof in doubt: then the disk is asked for the name.
#[test]
fn a_directory_proves_that_it_is_strict() {
  let mut d = Driver::sim(Mode::Inode, Options::default());
  d.fs.mkdir_all("/p/R").unwrap();
  d.fs.write("/p/R/app.js", "1").unwrap();
  d.watch(&["/p/R"]);
  // The looks, the listings and the questions for a name so far.
  let calls = |d: &Driver<_>| {
    let calls = d.fs.calls();
    (calls.lstat, calls.read_dir, calls.name_on_disk)
  };
  let (looks, listings, _) = calls(&d);

  d.fs.write("/p/R/new.js", "1").unwrap();
  assert_eq!(window(&mut d), ["C /p/R/new.js"]);
  assert_eq!(calls(&d), (looks + 3, listings, 0));

  // Also a name with a letter outside ASCII costs one look now.
  d.fs.write("/p/R/caf\u{e9}.js", "1").unwrap();
  d.fs.write("/p/R/app.js", "22").unwrap();
  assert_eq!(window(&mut d), ["U /p/R/app.js", "C /p/R/caf\u{e9}.js"]);
  assert_eq!(calls(&d), (looks + 5, listings, 0));

  // `APP.js` next to `app.js`: both names are looked at, and each look asks the disk for its
  // name. Both are entries, so the proof stands.
  d.fs.write("/p/R/APP.js", "1").unwrap();
  assert_eq!(window(&mut d), ["C /p/R/APP.js"]);
  assert_eq!(calls(&d), (looks + 7, listings, 2));
  caught_up(&mut d);
}

/// Names that the proof cannot be made with, on a strict disk. `Привет` and `привет` are two
/// files there, with one hash in the tree, and no ASCII letter to try in another case. The
/// look at one of them asks the disk for the name, once, and asks for a listing: that makes
/// the proof with another name of the directory. The proof is about ASCII letters, so such a
/// pair never puts it in doubt.
#[test]
fn names_with_no_ascii_letter_on_a_strict_disk() {
  let mut d = Driver::sim(Mode::Inode, Options::default());
  d.fs.mkdir_all("/p/R").unwrap();
  for name in [
    "app.js",
    "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}",
    "\u{43f}\u{440}\u{438}\u{432}\u{435}\u{442}",
  ] {
    d.fs.write(format!("/p/R/{name}"), "1").unwrap();
  }
  d.watch(&["/p/R"]);
  let calls = |d: &Driver<_>| {
    let calls = d.fs.calls();
    (calls.lstat, calls.read_dir, calls.name_on_disk)
  };
  let (looks, listings, _) = calls(&d);

  // The look at the name, and three for the proof with `app.js`.
  d.fs.write("/p/R/\u{43f}\u{440}\u{438}\u{432}\u{435}\u{442}", "22").unwrap();
  assert_eq!(window(&mut d), ["U /p/R/\u{43f}\u{440}\u{438}\u{432}\u{435}\u{442}"]);
  assert_eq!(calls(&d), (looks + 4, listings + 1, 1));
  d.fs.write("/p/R/\u{43f}\u{440}\u{438}\u{432}\u{435}\u{442}", "333").unwrap();
  assert_eq!(window(&mut d), ["U /p/R/\u{43f}\u{440}\u{438}\u{432}\u{435}\u{442}"]);
  assert_eq!(calls(&d), (looks + 5, listings + 1, 1));
  caught_up(&mut d);
}

/// The first new name of a directory has a letter outside ASCII. The hint asks for a listing
/// of the names, because the directory counts as loose. The look at the name proves that it
/// is strict, and then there is nothing to compare: the many names in it cost nothing.
#[test]
fn the_first_new_name_is_not_plain() {
  let mut d = Driver::sim(Mode::Inode, Options::default());
  d.fs.mkdir_all("/p/R").unwrap();
  for file in 0..50 {
    d.fs.write(format!("/p/R/f{file}.js"), "1").unwrap();
  }
  d.watch(&["/p/R"]);
  let before = d.fs.calls();
  d.fs.write("/p/R/caf\u{e9}.js", "1").unwrap();
  assert_eq!(window(&mut d), ["C /p/R/caf\u{e9}.js"]);
  let after = d.fs.calls();
  assert_eq!((after.lstat - before.lstat, after.read_dir - before.read_dir), (3, 0));
  assert_eq!(after.name_on_disk, 0);
  caught_up(&mut d);
}

/// A directory that was proved strict is replaced by one from a loose disk, and the tree
/// cannot tell the two apart (the new one got the inode number of the old one). Each listing
/// of a directory whose nodes the tree knows asks for the rule again.
#[test]
fn a_strict_directory_is_replaced_by_a_loose_one() {
  on_all(&Options::default(), |d, on| {
    d.fs().set_reuse_inode_numbers(true);
    d.fs().set_mount("mnt");
    d.fs().mkdir_all("/p/R").unwrap();
    d.fs().mkdir_all("/mnt").unwrap();
    d.fs().write("/p/R/app.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R"]), NONE, "{on}");
    d.fs().write("/p/R/new.js", "1").unwrap();
    assert_eq!(d.settled(), ["C /p/R/new.js"], "{on}");

    d.fs().rename("/p/R", "/p/old").unwrap();
    d.fs().remove_all("/p/old").unwrap();
    d.fs().mkdir("/mnt/y").unwrap();
    d.fs().write("/mnt/y/app.js", "22").unwrap();
    d.fs().write("/mnt/y/new.js", "22").unwrap();
    d.fs().rename("/mnt/y", "/p/R").unwrap();
    d.settled();
    d.fs().rename("/p/R/app.js", "/p/R/App.js").unwrap();
    assert_eq!(d.settled(), ["C /p/R/App.js", "D /p/R/app.js"], "{on}");
    d.caught_up();
  });
}

/// One tree with both kinds of disk: `/mnt` takes two spellings of a name for one, and the
/// rest does not. The engine is told nothing about it.
#[test]
fn a_loose_disk_inside_a_strict_one() {
  on_all(&Options::default(), |d, on| {
    d.fs().set_mount("mnt");
    for dir in ["/p/R", "/mnt/R"] {
      d.fs().mkdir_all(dir).unwrap();
      d.fs().write(format!("{dir}/app.js"), "1").unwrap();
      d.fs().write(format!("{dir}/Other.js"), "1").unwrap();
    }
    assert_eq!(d.watch(&["/p/R", "/mnt/R", "/mnt/R/APP.JS"]), NONE, "{on}");
    assert_eq!(d.known().len(), 5, "{on}");

    // On the strict disk the other case is another file, or nothing.
    d.fs().write("/p/R/App.js", "1").unwrap();
    assert!(d.fs().remove("/p/R/OTHER.JS").is_err(), "{on}");
    assert_eq!(d.settled(), ["C /p/R/App.js"], "{on}");

    // On the loose disk it is the same file.
    d.fs().write("/mnt/R/App.js", "22").unwrap();
    d.fs().remove("/mnt/R/OTHER.JS").unwrap();
    assert_eq!(d.settled(), ["U /mnt/R/APP.JS", "U /mnt/R/app.js", "D /mnt/R/Other.js"], "{on}");
    d.fs().rename("/mnt/R/app.js", "/mnt/R/App.js").unwrap();
    assert_eq!(d.settled(), ["C /mnt/R/App.js", "D /mnt/R/app.js"], "{on}");
    d.caught_up();
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

/// A file is renamed to another case of its name, and the filter ignores the new name. The
/// look at the new name finds nothing that counts, and its node goes. The old name was only
/// unsure of its spelling because the tree had the new one too: its look still asks the disk,
/// whichever of the two is looked at first.
#[test]
fn a_file_is_renamed_to_a_spelling_that_the_filter_ignores() {
  let ignored: Ignored = Arc::new(|path, _| path.file_name().is_some_and(|name| name == "c"));
  on_all(&Options { ignored: Some(ignored), ..loose() }, |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/p/R").unwrap();
    d.fs().write("/p/R/A.txt", "1").unwrap();
    d.fs().write("/p/R/C", "1").unwrap();
    assert_eq!(d.watch(&["/p/R"]), NONE, "{on}");
    // The node of `A.txt` is free for the next new name: that one is looked at before `C`.
    d.fs().remove("/p/R/A.txt").unwrap();
    assert_eq!(d.settled(), ["D /p/R/A.txt"], "{on}");

    d.fs().rename("/p/R/C", "/p/R/c").unwrap();
    assert_eq!(d.settled(), ["D /p/R/C"], "{on}");
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

/// Two watched paths that are two spellings of one file: each of them gets its events. The
/// hint names the file as the disk spells it, and the other registered name is looked at too.
#[test]
fn two_watched_paths_that_are_two_spellings_of_one_file() {
  on_all(&loose(), |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/p/src").unwrap();
    d.fs().write("/p/src/app.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/src/App.js", "/p/src/app.js"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/src/App.js", "/p/src/app.js"], "{on}");

    d.fs().write("/p/src/app.js", "22").unwrap();
    assert_eq!(d.window(), ["U /p/src/App.js", "U /p/src/app.js"], "{on}");
    d.fs().remove("/p/src/APP.JS").unwrap();
    assert_eq!(d.settled(), ["D /p/src/App.js", "D /p/src/app.js"], "{on}");
    d.fs().write("/p/src/app.js", "333").unwrap();
    assert_eq!(d.settled(), ["C /p/src/App.js", "C /p/src/app.js"], "{on}");
    d.caught_up();
  });
}

/// The same with a letter outside ASCII in the names. The table of the tree does not find
/// the other name, so it comes from the list of such names.
#[test]
fn two_watched_spellings_with_a_letter_outside_ascii() {
  on_all(&loose(), |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/p/src").unwrap();
    d.fs().write("/p/src/caf\u{e9}.js", "1").unwrap();
    d.fs().write("/p/src/th\u{e9}.js", "1").unwrap();
    let watched = ["/p/src/Caf\u{e9}.js", "/p/src/caf\u{e9}.js", "/p/src/th\u{e9}.js"];
    assert_eq!(d.watch(&watched), NONE, "{on}");

    d.fs().write("/p/src/caf\u{e9}.js", "22").unwrap();
    assert_eq!(d.window(), ["U /p/src/Caf\u{e9}.js", "U /p/src/caf\u{e9}.js"], "{on}");
    d.fs().remove("/p/src/CAF\u{e9}.JS").unwrap();
    assert_eq!(d.settled(), ["D /p/src/Caf\u{e9}.js", "D /p/src/caf\u{e9}.js"], "{on}");
    d.caught_up();
  });
}

/// The rule for a watched path needs no word about the disk. An engine that takes the disk
/// for a strict one (Linux) still finds a watched file in another spelling, on a disk that
/// ignores case (a folder that is shared with macOS or Windows).
#[test]
fn a_watched_path_in_another_spelling_with_an_engine_that_does_not_know_the_disk() {
  on_all(&Options::default(), |d, on| {
    d.fs().set_ignore_case(true);
    d.fs().mkdir_all("/p/src").unwrap();
    d.fs().write("/p/src/app.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/src/App.js"]), NONE, "{on}");

    d.fs().write("/p/src/app.js", "22").unwrap();
    assert_eq!(d.window(), ["U /p/src/App.js"], "{on}");
    d.fs().remove("/p/src/app.js").unwrap();
    assert_eq!(d.settled(), ["D /p/src/App.js"], "{on}");
    d.fs().write("/p/src/app.js", "333").unwrap();
    assert_eq!(d.settled(), ["C /p/src/App.js"], "{on}");
    d.caught_up();
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
/// spellings. Such a name is asked from the disk at every look, and when a hint names one
/// that the tree does not have as an entry, one listing shows which node is gone.
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

    // The entry goes and comes back in another spelling before the engine looks. The hints
    // name the new spelling only, and the listing shows that the old one is gone.
    d.fs().remove("/p/R/CAF\u{e9}.js").unwrap();
    d.fs().write("/p/R/CAF\u{e9}.js", "22").unwrap();
    assert_eq!(d.settled(), ["C /p/R/CAF\u{e9}.js", "D /p/R/caf\u{e9}.js"], "{on}");
    d.caught_up();
  });
}
