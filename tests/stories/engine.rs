//! What the engine has to get right below the promises: a batch of paths, a directory that
//! becomes a watched path above others, files and links on the way to a watched path, and
//! directories that are renamed or replaced. Each came from a case that once went wrong.

use std::time::Duration;

use pathwatch::{
  EntryKind,
  EventKind::{Create, Delete, Update},
};

use crate::{Settings, everywhere, everywhere_with, skipped};

/// A watched file and its directory, in one call or in two, in both orders: the file has one
/// home, so a change gives one event, and the other files of the directory are the baseline.
#[test]
fn a_watched_file_and_its_directory_are_one_home() {
  everywhere(|w| {
    for (base, paths) in [("one", ["one/d", "one/d/f.js"]), ("two", ["two/d/f.js", "two/d"])] {
      w.write(&format!("{base}/d/f.js"), "1");
      w.write(&format!("{base}/d/g.js"), "1");
      w.watch(&paths);
      w.expect(&[]);
      w.write(&format!("{base}/d/f.js"), "22");
      w.write(&format!("{base}/d/g.js"), "22");
      w.expect(&[(Update, &format!("{base}/d/f.js")), (Update, &format!("{base}/d/g.js"))]);
    }
    for (base, first, second) in
      [("three", "three/d/f.js", "three/d"), ("four", "four/d", "four/d/f.js")]
    {
      w.write(&format!("{base}/d/f.js"), "1");
      w.write(&format!("{base}/d/g.js"), "1");
      w.watch(&[first]);
      w.watch(&[second]);
      w.expect(&[]);
      w.write(&format!("{base}/d/f.js"), "22");
      w.write(&format!("{base}/d/g.js"), "22");
      w.expect(&[(Update, &format!("{base}/d/f.js")), (Update, &format!("{base}/d/g.js"))]);
    }
    // Two levels apart: the directory in between is a full part of the watched directory.
    for (base, paths) in
      [("five", ["five/r", "five/r/lib/a.js"]), ("six", ["six/r/lib/a.js", "six/r"])]
    {
      w.write(&format!("{base}/r/top.js"), "1");
      w.write(&format!("{base}/r/lib/a.js"), "1");
      w.write(&format!("{base}/r/lib/other.js"), "1");
      w.watch(&paths);
      w.expect(&[]);
      w.write(&format!("{base}/r/lib/b.js"), "1");
      w.write(&format!("{base}/r/lib/other.js"), "22");
      w.expect(&[
        (Create, &format!("{base}/r/lib/b.js")),
        (Update, &format!("{base}/r/lib/other.js")),
      ]);
    }
  });
}

/// A directory above watched paths becomes a watched path itself. Until then the directories
/// on the way only lead to the watched paths and report nothing of their own. From then on
/// everything below is a part of it, with what is there as the baseline, down to the last level.
#[test]
fn a_directory_above_watched_paths_becomes_a_watched_path() {
  everywhere(|w| {
    w.write("r/top.js", "1");
    w.write("r/lib/a.js", "1");
    w.write("r/lib/old.js", "1");
    w.write("r/lib/sub/deep.js", "1");
    w.watch(&["r/lib/a.js", "r/lib/sub/missing.js"]);
    w.write("r/top.js", "22");
    w.write("r/lib/old.js", "22");
    w.expect(&[]);

    w.watch(&["r"]);
    w.expect(&[]);
    w.write("r/top.js", "333");
    w.write("r/lib/b.js", "1");
    w.write("r/lib/old.js", "333");
    w.write("r/lib/sub/c.js", "1");
    w.write("r/lib/sub/new/d.js", "1");
    w.expect(&[
      (Update, "r/top.js"),
      (Create, "r/lib/b.js"),
      (Update, "r/lib/old.js"),
      (Create, "r/lib/sub/c.js"),
      (Create, "r/lib/sub/new/d.js"),
    ]);
  });
}

/// A directory on the way to a watched file is missing inside a watched directory. When it
/// appears it is a full part of that directory, with all of its files.
#[test]
fn a_missing_directory_on_the_way_appears_inside_a_watched_directory() {
  everywhere(|w| {
    w.mkdir("r");
    w.watch(&["r", "r/gen/x.js"]);
    w.mkdir("r/gen");
    w.write("r/gen/x.js", "1");
    w.write("r/gen/y.js", "1");
    w.expect(&[(Create, "r/gen/x.js"), (Create, "r/gen/y.js")]);
  });
}

/// A file where a directory on the way to a watched path should be. It is nothing to report
/// until that path, or a directory above it, is a watched path: then the same file counts, as
/// the baseline. A watched path below a file is found when a directory takes the place of the
/// file, and gone again when a file comes back.
#[test]
fn a_file_on_the_way_to_a_watched_path() {
  everywhere(|w| {
    w.watch(&["a/a/b/c"]);
    w.write("a", "a file");
    w.expect(&[]);
    w.watch(&["a"]);
    w.expect(&[]);
    w.write("a", "22");
    w.expect(&[(Update, "a")]);

    w.write("p/b", "a file");
    w.watch(&["p/b/c/d"]);
    w.expect(&[]);
    w.watch(&["p"]);
    w.expect(&[]);
    w.write("p/b", "22");
    w.expect(&[(Update, "p/b")]);

    // The file is reported like every other file of the watched directory, and the path below
    // it is found when a directory takes its place.
    w.write("q/a", "a file");
    w.watch(&["q"]);
    w.watch(&["q/a/b.js"]);
    w.write("q/a", "22");
    w.expect(&[(Update, "q/a")]);
    w.remove("q/a");
    w.expect(&[(Delete, "q/a")]);
    w.write("q/a/b.js", "1");
    w.expect(&[(Create, "q/a/b.js")]);

    // The same with a watched file: everything below counts once it is a directory, and only
    // the watched path below a file of a watched directory.
    w.write("r/f", "1");
    w.write("s/a", "1");
    w.watch(&["r", "r/f/x", "s/a", "s/a/b"]);
    w.write("r/f", "22");
    w.write("s/a", "22");
    w.expect(&[(Update, "r/f"), (Update, "s/a")]);
    for path in ["r/f", "s/a"] {
      w.remove(path);
      w.mkdir(path);
    }
    for file in ["r/f/x", "r/f/y", "s/a/b", "s/a/c"] {
      w.write(file, "1");
    }
    w.expect(&[
      (Delete, "r/f"),
      (Delete, "s/a"),
      (Create, "r/f/x"),
      (Create, "r/f/y"),
      (Create, "s/a/b"),
      (Create, "s/a/c"),
    ]);
    for path in ["r/f", "s/a"] {
      w.remove(path);
      w.write(path, "1");
    }
    w.expect(&[
      (Create, "r/f"),
      (Create, "s/a"),
      (Delete, "r/f/x"),
      (Delete, "r/f/y"),
      (Delete, "s/a/b"),
      (Delete, "s/a/c"),
    ]);
  });
}

/// `rm -r` and a copy with the same names: one `Update` for a name that is there again,
/// `Delete` for the rest, `Create` for the new ones.
#[test]
fn rm_r_and_a_copy_with_the_same_names() {
  everywhere_with(&Settings::delete_delay(Duration::from_secs(2)), |w| {
    for file in ["x", "y", "z", "sub/s"] {
      w.write(&format!("d/{file}"), "1");
    }
    w.watch(&["d"]);
    w.remove("d");
    for file in ["x", "y", "w", "sub/s"] {
      w.write(&format!("d/{file}"), "copied");
    }
    w.expect(&[
      (Update, "d/x"),
      (Update, "d/y"),
      (Delete, "d/z"),
      (Create, "d/w"),
      (Update, "d/sub/s"),
    ]);
  });
}

/// A link to a directory on the way to a watched file is followed, and what changes behind
/// it is found: the target goes away, comes back as a new directory, and is written.
#[test]
fn changes_behind_a_link_on_the_way() {
  everywhere(|w| {
    w.write("z/real/b/c.js", "1");
    w.mkdir("a");
    if !w.symlink("z/real", "a/link") {
      return;
    }
    // The handle on the directory of the watched file is on the real directory: Windows
    // refuses to rename the directory above it while the handle is open.
    if cfg!(windows) && !w.polls() {
      skipped("Windows refuses to rename a directory above an open handle");
      return;
    }
    w.watch(&["a/link/b/c.js"]);
    w.expect(&[]);

    w.rename("z/real", "z/gone");
    w.expect(&[(Delete, "a/link/b/c.js")]);
    w.write("z/real/b/c.js", "new");
    w.expect(&[(Create, "a/link/b/c.js")]);
    w.write("z/real/b/c.js", "newer");
    w.expect(&[(Update, "a/link/b/c.js")]);
  });
}

/// A watched path that is a link to a file: the file is read by its real path, so a write of
/// the target is one `Update` and nothing comes after it, and the link pointed at another file
/// is an `Update` too.
#[test]
fn a_watched_link_to_a_file_is_read_through_its_target() {
  everywhere(|w| {
    w.write("shared/config.json", "1");
    w.mkdir("p");
    if !w.symlink("shared/config.json", "p/config.json") {
      return;
    }
    w.watch(&["p/config.json"]);
    w.expect(&[]);
    w.write("shared/config.json", "22");
    w.expect(&[(Update, "p/config.json")]);
    // The file is young for two seconds: a look that could not read it would report once more.
    w.pass(Duration::from_millis(2500));
    w.expect(&[]);

    w.write("shared/other.json", "333");
    w.remove("p/config.json");
    w.symlink("shared/other.json", "p/config.json");
    w.expect(&[(Update, "p/config.json")]);
    w.pass(Duration::from_millis(2500));
    w.expect(&[]);
  });
}

/// A link to a directory inside a watched directory is a file entry, until a watched path
/// goes through it: then the link is followed and is no file any more, and what is behind it
/// is a part of the watched directory.
#[test]
fn a_link_in_a_watched_directory_becomes_a_way_to_a_watched_path() {
  everywhere(|w| {
    w.mkdir("p/d");
    w.write("q/dir/x.js", "1");
    if !w.symlink("q/dir", "p/d/l") {
      return;
    }
    w.watch(&["p/d"]);
    w.expect(&[]);

    w.watch(&["p/d/l/x.js"]);
    w.expect(&[(Delete, "p/d/l")]);
    w.write("q/dir/x.js", "22");
    w.write("q/dir/y.js", "1");
    w.expect(&[(Update, "p/d/l/x.js"), (Create, "p/d/l/y.js")]);
  });
}

/// A link that the filter ignores as a file sits in a watched directory. Then it becomes a
/// watched path: it is followed from now on, with what is behind it as the baseline. The
/// directory above never had to report it: for it the link was an ignored file.
#[test]
fn an_ignored_link_becomes_a_watched_path() {
  let settings = Settings::ignored(|path, kind| kind == EntryKind::File && path.ends_with("link"));
  everywhere_with(&settings, |w| {
    w.mkdir("p/d");
    w.write("q/dir/x.js", "1");
    w.write("q/dir/deep/y.js", "1");
    if !w.symlink("q/dir", "p/d/link") {
      return;
    }
    w.watch(&["p/d"]);
    w.expect(&[]);

    w.watch(&["p/d/link"]);
    w.expect(&[]);
    w.write("q/dir/x.js", "22");
    w.write("q/dir/new.js", "1");
    w.expect(&[(Update, "p/d/link/x.js"), (Create, "p/d/link/new.js")]);
  });
}

/// A directory under its new name is not deaf: a renamed directory in a watched directory,
/// an ignored directory that gets a visible name, and the directory of a watched file that
/// moves to the place of another watched file, in both orders of the names.
#[test]
fn a_renamed_directory_is_not_deaf() {
  everywhere_with(&Settings::ignore_node_modules(), |w| {
    for (watched, from, to) in [("r", "a", "b"), ("q", "b", "a")] {
      w.write(&format!("{watched}/{from}/x"), "1");
      w.mkdir(&format!("{watched}/{from}/deep"));
      w.watch(&[watched]);
      w.rename(&format!("{watched}/{from}"), &format!("{watched}/{to}"));
      w.expect(&[(Create, &format!("{watched}/{to}/x")), (Delete, &format!("{watched}/{from}/x"))]);
      w.write(&format!("{watched}/{to}/new.js"), "1");
      w.write(&format!("{watched}/{to}/deep/new.js"), "1");
      w.expect(&[
        (Create, &format!("{watched}/{to}/new.js")),
        (Create, &format!("{watched}/{to}/deep/new.js")),
      ]);
    }

    w.write("p/node_modules/foo/index.js", "1");
    w.watch(&["p"]);
    w.rename("p/node_modules", "p/nm");
    w.expect(&[(Create, "p/nm/foo/index.js")]);
    w.write("p/nm/foo/index.js", "22");
    w.write("p/nm/foo/new.js", "1");
    w.write("p/nm/top.js", "1");
    w.expect(&[
      (Update, "p/nm/foo/index.js"),
      (Create, "p/nm/foo/new.js"),
      (Create, "p/nm/top.js"),
    ]);

    w.write("s/b/x.js", "1");
    w.watch(&["s/a/x.js", "s/b/x.js"]);
    w.rename("s/b", "s/a");
    w.expect(&[(Create, "s/a/x.js"), (Delete, "s/b/x.js")]);
    w.write("s/a/x.js", "22");
    w.expect(&[(Update, "s/a/x.js")]);
  });
}

/// The directory of a watched file is replaced by another directory that holds a file of the
/// same name: an `Update`. The directory that left is not the watched path any more.
#[test]
fn the_directory_of_a_watched_file_is_replaced() {
  everywhere_with(&Settings::delete_delay(Duration::from_secs(2)), |w| {
    w.write("p/lib/a.js", "1");
    w.write("p/next/a.js", "22");
    w.watch(&["p/lib/a.js"]);
    w.rename("p/lib", "p/old");
    w.rename("p/next", "p/lib");
    w.expect(&[(Update, "p/lib/a.js")]);
    w.write("p/lib/a.js", "333");
    w.expect(&[(Update, "p/lib/a.js")]);
    w.write("p/old/a.js", "4444");
    w.expect(&[]);
  });
}
