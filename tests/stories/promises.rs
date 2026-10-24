//! The promises of `references/behavior.md`, and which test proves which (`c::` is
//! `contract`, `p::` is `projects`):
//!
//! 1. A watched path is a file, a directory or nothing: `c::s1`, `c::s9`, `c::s4`,
//!    `a_file_is_anything_that_is_not_a_directory`.
//! 2. Events are the difference to the disk, one for a path in a batch:
//!    `p::random_changes_end_in_the_state_of_the_disk`; `Disk::receive` checks every batch.
//!    On Windows also `a_name_that_windows_reads_as_another_is_reported`.
//! 3. `Update`: `touch_is_an_update_and_chmod_is_not`, `c::s2`,
//!    `c::s3_deleted_and_created_again_inside_the_delete_delay`.
//! 4. What exists at `watch` time: `what_exists_at_watch_time_is_not_reported`.
//! 5. Missing, file, directory: `c::s4` to `c::s8`, `a_name_on_the_way_changes_its_kind`,
//!    `a_missing_chain_that_appears_at_once_is_reported_promptly`.
//! 6. A directory goes or comes: `c::s10`, `c::s11`, `c::s12`.
//! 7. `unwatch`: `unwatch_of_a_file_and_of_a_directory`,
//!    `unwatch_keeps_what_another_watched_path_covers`.
//! 8. Paths: `a_relative_path_with_dot_dot`, `one_bad_path_among_good_ones`.
//! 9. Calls inside the handler: `watch_and_unwatch_inside_the_handler`,
//!    `close_inside_the_handler`.
//! 10. The ignore filter: `c::s14`, `an_ignored_directory_is_not_read`,
//!     `an_ignored_directory_comes_and_goes`,
//!     `a_watched_path_that_the_filter_ignores_counts_as_missing`,
//!     `a_watched_path_below_an_ignored_directory_gets_nothing`.
//! 11. Nothing beside a watched path: `nothing_is_reported_beside_a_watched_path`.
//! 12. Symbolic links: `p::two_spellings_of_one_directory`,
//!     `p::watched_paths_below_a_symlinked_prefix`, `a_watched_link_to_a_file_is_followed`,
//!     `a_link_inside_a_watched_directory_is_a_file`.
//! 13. Spelling: `p::watched_paths_in_another_spelling`, `a_watched_file_in_another_unicode_form`,
//!     `names_below_a_watched_directory_are_spelled_as_on_the_disk`,
//!     `a_file_is_renamed_to_another_letter_case`,
//!     `a_file_goes_through_another_spelling_of_its_name`,
//!     `a_directory_is_renamed_to_another_letter_case`,
//!     `every_watched_spelling_of_a_file_gets_its_events`,
//!     `watched_paths_through_another_spelling_inside_a_watched_directory`,
//!     `a_directory_above_a_watched_path_is_renamed_to_another_letter_case`.

use std::{
  fs,
  path::{Path, PathBuf},
  sync::{Arc, Mutex, mpsc::RecvTimeoutError},
  time::{Duration, Instant, SystemTime},
};

use pathwatch::{
  EntryKind, ErrorKind, Event,
  EventKind::{Create, Delete, Update},
  Watcher,
};

use crate::{
  Inside, Settings, World,
  disk::{PATIENCE, relative_scratch},
  each, everywhere, everywhere_with, on_disk, on_disk_inside, on_disk_with, skipped,
};

#[cfg(unix)]
#[test]
fn a_file_is_anything_that_is_not_a_directory() {
  use std::os::unix::{fs::symlink, net::UnixListener};

  on_disk(|w| {
    w.mkdir("r");
    // macOS 26 says nothing when a socket is made: there the sockets exist before the watch,
    // and only their removal is heard.
    let (inner, outer) = (w.path("r/socket"), w.path("socket"));
    let bind = || (UnixListener::bind(&inner).unwrap(), UnixListener::bind(&outer).unwrap());
    let before = cfg!(target_os = "macos").then(bind);
    w.watch(&["r", "socket"]);
    let sockets = before.unwrap_or_else(bind);
    symlink("nowhere", w.path("r/link")).unwrap();
    if cfg!(target_os = "macos") {
      w.expect(&[(Create, "r/link")]);
    } else {
      w.expect(&[(Create, "r/socket"), (Create, "socket"), (Create, "r/link")]);
    }

    drop(sockets);
    w.remove("r/socket");
    w.remove("socket");
    w.remove("r/link");
    w.expect(&[(Delete, "r/socket"), (Delete, "socket"), (Delete, "r/link")]);
  });
}

#[test]
fn touch_is_an_update_and_chmod_is_not() {
  on_disk(|w| {
    w.write("r/a.txt", "1");
    w.write("r/b.txt", "1");
    w.write("f.txt", "1");
    w.watch(&["r", "f.txt"]);

    for rel in ["r/a.txt", "f.txt"] {
      let file = fs::File::options().write(true).open(w.path(rel)).unwrap();
      file.set_modified(SystemTime::now()).unwrap();
    }
    w.expect(&[(Update, "r/a.txt"), (Update, "f.txt")]);

    // The write of the other file lets the test wait for the watcher.
    for rel in ["r/a.txt", "f.txt"] {
      let mut permissions = fs::metadata(w.path(rel)).unwrap().permissions();
      permissions.set_readonly(true);
      fs::set_permissions(w.path(rel), permissions).unwrap();
    }
    w.write("r/b.txt", "22");
    w.expect(&[(Update, "r/b.txt")]);
  });
}

#[test]
fn what_exists_at_watch_time_is_not_reported() {
  everywhere(|w| {
    w.write("r/a.txt", "1");
    w.write("r/sub/b.txt", "1");
    w.write("f.txt", "1");
    w.write("late/c.txt", "1");
    w.watch(&["r", "f.txt"]);
    w.expect(&[]);

    w.watch(&["late", "r/a.txt", "r/sub"]);
    w.expect(&[]);

    // A change right after `watch` has returned is reported.
    w.write("more/d.txt", "1");
    w.watch(&["more"]);
    w.write("more/d.txt", "22");
    w.write("more/e.txt", "1");
    w.expect(&[(Update, "more/d.txt"), (Create, "more/e.txt")]);
  });
}

/// `a` and `a/b` are names on the way to the two watched paths.
#[test]
fn a_name_on_the_way_changes_its_kind() {
  everywhere(|w| {
    w.watch(&["a/b/r.txt", "a/b/dir", "seen.txt"]);

    // Missing to file. The other file lets the test wait for the watcher.
    w.write("a", "1");
    w.write("seen.txt", "1");
    w.expect(&[(Create, "seen.txt")]);

    // File to directory.
    w.remove("a");
    w.write("a/b/r.txt", "1");
    w.write("a/b/dir/x.txt", "1");
    w.expect(&[(Create, "a/b/r.txt"), (Create, "a/b/dir/x.txt")]);

    // Directory to file, one name lower.
    w.remove("a/b");
    w.write("a/b", "1");
    w.expect(&[(Delete, "a/b/r.txt"), (Delete, "a/b/dir/x.txt")]);

    // File to missing.
    w.remove("a/b");
    w.write("seen.txt", "22");
    w.expect(&[(Update, "seen.txt")]);

    // Missing to directory.
    w.write("a/b/dir/sub/y.txt", "1");
    w.expect(&[(Create, "a/b/dir/sub/y.txt")]);
  });
}

/// The backend tells it: nobody waits for the poll of the places without an OS watch, which
/// runs once a second.
#[test]
fn a_missing_chain_that_appears_at_once_is_reported_promptly() {
  const CHAINS: usize = 10;

  on_disk(|w| {
    let files: Vec<String> = (0..CHAINS).map(|chain| format!("{chain}/a/b/c/r.txt")).collect();
    let watched: Vec<String> = (0..CHAINS)
      .map(|chain| if chain % 2 == 0 { files[chain].clone() } else { format!("{chain}/a/b") })
      .collect();
    w.watch(&watched.iter().map(String::as_str).collect::<Vec<_>>());

    let start = Instant::now();
    for chain in 0..CHAINS {
      w.write(&files[chain], "1");
      w.wait(&each(Create, &files[..=chain]));
    }
    let took = start.elapsed();
    // The poll of the missing places runs once a second: the chains in turn would take ten. A
    // watcher that only polls rests after a slow round (design.md 5.13): no bound for it.
    let bounded = w.backend == "native";
    assert!(!bounded || took < Duration::from_secs(5), "{CHAINS} chains took {took:?}");
    w.expect(&each(Create, &files));
  });
}

#[test]
fn unwatch_of_a_file_and_of_a_directory() {
  everywhere(|w| {
    w.write("r/a.txt", "1");
    w.write("r/sub/b.txt", "1");
    w.write("f.txt", "1");
    w.write("stays/c.txt", "1");
    w.watch(&["r", "f.txt", "missing.txt", "stays", "stays.txt"]);

    w.unwatch(&["r", "f.txt", "missing.txt"]);
    // A path that is not watched is skipped: never watched, or taken back already.
    w.unwatch(&["never.txt", "f.txt"]);
    w.expect(&[]);

    w.write("r/a.txt", "22");
    w.write("r/new.txt", "1");
    w.remove("r/sub");
    w.write("f.txt", "22");
    w.write("missing.txt", "1");
    w.write("stays/c.txt", "22");
    w.write("stays.txt", "1");
    w.expect(&[(Update, "stays/c.txt"), (Create, "stays.txt")]);
    w.remove("r");
    w.remove("f.txt");
    w.remove("stays.txt");
    w.expect(&[(Delete, "stays.txt")]);

    // Watched again, it is like a first `watch`.
    w.write("r/a.txt", "1");
    w.watch(&["r", "f.txt"]);
    w.write("r/a.txt", "22");
    w.write("f.txt", "1");
    w.expect(&[(Update, "r/a.txt"), (Create, "f.txt")]);
  });
}

#[test]
fn unwatch_keeps_what_another_watched_path_covers() {
  everywhere(|w| {
    w.write("r/a.txt", "1");
    w.write("r/sub/b.txt", "1");
    w.write("r/other/c.txt", "1");
    w.write("q/d.txt", "1");
    w.watch(&["r", "r/a.txt", "r/sub", "q", "q/d.txt"]);

    w.unwatch(&["r"]);
    w.unwatch(&["q/d.txt"]);
    w.expect(&[]);

    w.write("r/a.txt", "22");
    w.write("r/sub/b.txt", "22");
    w.write("r/sub/new.txt", "1");
    w.write("r/other/c.txt", "22");
    w.write("r/new.txt", "1");
    w.write("q/d.txt", "22");
    w.expect(&[
      (Update, "r/a.txt"),
      (Update, "r/sub/b.txt"),
      (Create, "r/sub/new.txt"),
      (Update, "q/d.txt"),
    ]);
  });
}

#[test]
fn a_relative_path_with_dot_dot() {
  on_disk(|w| {
    // From the current directory up to the root, and down to the temporary directory.
    let (dir, rel) = relative_scratch();
    w.dir = dir;
    assert!(rel.is_relative());

    w.watcher.watch(&[rel.join("a.txt")]).unwrap();
    w.watcher.watch(&[Path::new(".").join(&rel).join("nothing/../sub/./deep/..//b.txt/")]).unwrap();
    w.watcher.watch(&[w.path("x/y/../../c.txt")]).unwrap();
    w.write("a.txt", "1");
    w.write("sub/b.txt", "1");
    w.write("c.txt", "1");
    w.expect(&[(Create, "a.txt"), (Create, "sub/b.txt"), (Create, "c.txt")]);

    // The clean path is the watched one: `unwatch` finds it under every spelling, and a path
    // watched under two spellings is one watch. The names that `..` takes away need not exist.
    w.watcher.watch(&[w.path("sub/b.txt")]).unwrap();
    w.watcher.unwatch(&[w.path("sub/x/../../a.txt")]).unwrap();
    w.watcher.unwatch(&[rel.join("sub/b.txt")]).unwrap();
    w.write("a.txt", "22");
    w.write("sub/b.txt", "22");
    w.write("c.txt", "22");
    w.expect(&[(Update, "c.txt")]);

    // Above the root there is nothing: the error names the path as it was given.
    let above = w.path("..").join("../".repeat(64)).join("a.txt");
    let error = w.watcher.watch(&[&above]).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidPath);
    assert_eq!(error.path(), Some(above.as_path()));
  });
}

#[test]
fn one_bad_path_among_good_ones() {
  on_disk(|w| {
    let bad = w.path("nul\0.txt");
    let paths = [w.path("a.txt"), PathBuf::new(), bad, w.path("r")];
    let error = w.watcher.watch(&paths).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidPath);
    assert_eq!(error.path(), Some(Path::new("")), "the first bad path");
    assert!(error.to_string().starts_with("the path cannot be watched"), "{error}");
    // The root of the disk alone.
    let root = w.dir.path().ancestors().last().unwrap().to_path_buf();
    let error = w.watcher.watch(&[&root]).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidPath);
    assert_eq!(error.path(), Some(root.as_path()));

    w.write("a.txt", "1");
    w.write("r/b.txt", "1");
    w.expect(&[(Create, "a.txt"), (Create, "r/b.txt")]);

    let error = w.watcher.unwatch(&paths).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidPath);
    assert_eq!(error.path(), Some(Path::new("")));
    w.watch(&["c.txt"]);
    w.write("a.txt", "22");
    w.write("r/b.txt", "22");
    w.write("c.txt", "1");
    w.expect(&[(Create, "c.txt")]);
  });
}

#[test]
fn watch_and_unwatch_inside_the_handler() {
  /// When `a.txt` is created: watches `b.txt` and the directory `d`, and unwatches `a.txt`.
  fn inside(watcher: &Watcher, events: &[Event]) {
    for event in events {
      if event.kind == Create && event.path.ends_with("a.txt") {
        watcher
          .watch(&[event.path.with_file_name("b.txt"), event.path.with_file_name("d")])
          .unwrap();
        watcher.unwatch(&[&event.path]).unwrap();
      }
    }
  }

  on_disk_inside(inside, |w| {
    w.write("d/old.txt", "1");
    w.watch(&["a.txt", "c.txt"]);

    // The calls take effect when the handler has returned. Nothing is reported for them.
    w.write("a.txt", "1");
    w.expect(&[(Create, "a.txt")]);

    w.write("a.txt", "22");
    w.write("b.txt", "1");
    w.write("c.txt", "1");
    w.write("d/old.txt", "22");
    w.write("d/new.txt", "1");
    w.expect(&[(Create, "b.txt"), (Create, "c.txt"), (Update, "d/old.txt"), (Create, "d/new.txt")]);
  });
}

/// The filter is the witness: asked about the ignored directory, never about a name inside it.
#[test]
fn an_ignored_directory_is_not_read() {
  let asked = Arc::new(Mutex::new(Vec::new()));
  let settings = {
    let asked = Arc::clone(&asked);
    Settings::ignored(move |path, kind| {
      asked.lock().unwrap().push(path.to_path_buf());
      kind == EntryKind::Dir && path.ends_with("node_modules")
    })
  };
  everywhere_with(&settings, |w| {
    w.write("r/node_modules/pkg/index.js", "1");
    w.write("r/src/a.js", "1");
    w.watch(&["r"]);

    w.write("r/node_modules/pkg/index.js", "22");
    w.write("r/node_modules/pkg/lib/new.js", "1");
    w.write("r/node_modules/top.js", "1");
    w.write("r/src/b.js", "1");
    w.expect(&[(Create, "r/src/b.js")]);
    w.remove("r/node_modules/pkg");
    w.remove("r/src/a.js");
    w.expect(&[(Delete, "r/src/a.js")]);
  });

  let asked = asked.lock().unwrap();
  let is_inside = |path: &PathBuf| {
    path.parent().is_some_and(|above| above.iter().any(|name| name == "node_modules"))
  };
  let inside: Vec<&PathBuf> = asked.iter().filter(|path| is_inside(path)).collect();
  assert!(inside.is_empty(), "the filter was asked about {inside:?}");
  assert!(asked.iter().any(|path| path.ends_with("r/node_modules")));
  assert!(asked.iter().any(|path| path.ends_with("r/src/b.js")));
}

/// By a rename and step by step: nothing is reported, and the filter is never asked about a
/// name inside.
#[test]
fn an_ignored_directory_comes_and_goes() {
  let asked = Arc::new(Mutex::new(Vec::new()));
  let settings = {
    let asked = Arc::clone(&asked);
    Settings::ignored(move |path, kind| {
      asked.lock().unwrap().push(path.to_path_buf());
      kind == EntryKind::Dir && path.ends_with("node_modules")
    })
  };
  everywhere_with(&settings, |w| {
    w.write("away/node_modules/pkg/index.js", "1");
    w.write("away/node_modules/top.js", "1");
    w.write("r/a.js", "1");
    w.watch(&["r"]);

    w.rename("away/node_modules", "r/node_modules");
    w.write("r/b.js", "1");
    w.expect(&[(Create, "r/b.js")]);
    w.write("r/node_modules/top.js", "22");
    w.write("r/node_modules/pkg/new.js", "1");
    w.rename("r/node_modules", "away/node_modules");
    w.write("r/a.js", "22");
    w.expect(&[(Update, "r/a.js")]);

    w.write("r/node_modules/lib/x.js", "1");
    w.write("r/b.js", "22");
    w.expect(&[(Update, "r/b.js")]);
    w.remove("r/node_modules");
    w.remove("r/b.js");
    w.expect(&[(Delete, "r/b.js")]);
  });

  let asked = asked.lock().unwrap();
  let is_inside = |path: &PathBuf| {
    path.parent().is_some_and(|above| above.iter().any(|name| name == "node_modules"))
  };
  let inside: Vec<&PathBuf> = asked.iter().filter(|path| is_inside(path)).collect();
  assert!(inside.is_empty(), "the filter was asked about {inside:?}");
  assert!(asked.iter().any(|path| path.ends_with("r/node_modules")));
}

/// A watched path that the filter ignores for the kind that is there counts as missing: no
/// event while it is so, a `Create` when a kind that the filter takes comes, a `Delete` when
/// it goes. One ignored for both kinds is never anything.
#[test]
fn a_watched_path_that_the_filter_ignores_counts_as_missing() {
  let settings = Settings::ignored(|path, kind| {
    path.ends_with("secret.txt")
      || (kind == EntryKind::Dir && path.ends_with("dist"))
      || (kind == EntryKind::File && path.extension().is_some_and(|extension| extension == "log"))
  });
  everywhere_with(&settings, |w| {
    w.write("p/dist/a.js", "1");
    w.write("r/old.log", "1");
    w.watch(&["secret.txt", "p/dist", "r", "r/new.log"]);

    w.write("secret.txt", "1");
    w.write("p/dist/a.js", "22");
    w.write("p/dist/b.js", "1");
    w.write("r/old.log", "22");
    w.write("r/new.log", "1");
    w.write("r/seen.txt", "1");
    w.expect(&[(Create, "r/seen.txt")]);

    // A file where the ignored directory was, and a directory again.
    w.remove("p/dist");
    w.write("p/dist", "a file now");
    w.expect(&[(Create, "p/dist")]);
    w.write("p/dist", "written");
    w.expect(&[(Update, "p/dist")]);
    w.remove("p/dist");
    w.write("p/dist/a.js", "1");
    w.expect(&[(Delete, "p/dist")]);

    w.remove("secret.txt");
    w.remove("r/old.log");
    w.remove("r/new.log");
    w.remove("r/seen.txt");
    w.expect(&[(Delete, "r/seen.txt")]);
  });
}

/// A watched path below a directory that the filter ignores is never watched, however deep,
/// and whether it exists or not: nothing is reported for it, and `unwatch` of it does
/// nothing. The filter looks at the last name only.
#[test]
fn a_watched_path_below_an_ignored_directory_gets_nothing() {
  everywhere_with(&Settings::ignore_node_modules(), |w| {
    w.write("p/node_modules/top.js", "1");
    w.write("p/node_modules/pkg/index.js", "1");
    w.write("p/seen.js", "1");
    let below = ["p/node_modules/top.js", "p/node_modules/pkg/index.js", "p/node_modules/later.js"];
    w.watch(&["p/node_modules", "p/seen.js"]);
    w.watch(&below);

    w.write("p/node_modules/top.js", "22");
    w.write("p/node_modules/later.js", "1");
    w.write("p/node_modules/pkg/index.js", "22");
    w.write("p/seen.js", "22");
    w.expect(&[(Update, "p/seen.js")]);

    w.unwatch(&below);
    w.remove("p/node_modules");
    w.remove("p/seen.js");
    w.expect(&[(Delete, "p/seen.js")]);
  });
}

#[test]
fn nothing_is_reported_beside_a_watched_path() {
  everywhere(|w| {
    w.write("p/dir/a.txt", "1");
    w.write("p/file.txt", "1");
    w.write("p/beside/b.txt", "1");
    w.write("p/other.txt", "1");
    w.watch(&["p/dir", "p/file.txt", "p/missing/deep.txt"]);

    w.write("p/beside/b.txt", "22");
    w.write("p/beside/new.txt", "1");
    w.write("p/dir2/c.txt", "1");
    w.write("p/other.txt", "22");
    w.write("p/file.txt.bak", "1");
    w.write("p/missing/other.txt", "1");
    w.write("top.txt", "1");
    w.write("p/dir/a.txt", "22");
    w.expect(&[(Update, "p/dir/a.txt")]);

    w.remove("p/beside");
    w.remove("p/dir2");
    w.remove("p/other.txt");
    w.rename("p/file.txt.bak", "p/moved.txt");
    w.write("p/file.txt", "22");
    w.expect(&[(Update, "p/file.txt")]);
  });
}

/// Nothing watches the place a link points to: the native backend finds these changes with its
/// poll.
#[test]
fn a_watched_link_to_a_file_is_followed() {
  everywhere(|w| {
    w.write("real/f.txt", "1");
    if !w.symlink("real/f.txt", "link.txt") {
      return;
    }
    w.watch(&["link.txt"]);

    w.write("real/f.txt", "22");
    w.expect(&[(Update, "link.txt")]);
    // The link points to nothing: no file is at the watched path.
    w.remove("real/f.txt");
    w.expect(&[(Delete, "link.txt")]);
    w.write("real/f.txt", "1");
    w.expect(&[(Create, "link.txt")]);
  });
}

#[test]
fn a_link_inside_a_watched_directory_is_a_file() {
  everywhere(|w| {
    w.write("outside/dir/a.txt", "1");
    w.write("outside/f.txt", "1");
    w.mkdir("r");
    w.watch(&["r"]);

    if !w.symlink("outside/dir", "r/to_dir") {
      return;
    }
    assert!(w.symlink("outside/f.txt", "r/to_file"));
    w.expect(&[(Create, "r/to_dir"), (Create, "r/to_file")]);

    w.write("outside/dir/a.txt", "22");
    w.write("outside/dir/b.txt", "1");
    w.write("outside/f.txt", "22");
    w.write("r/seen.txt", "1");
    w.expect(&[(Create, "r/seen.txt")]);

    w.remove("r/to_dir");
    w.remove("r/to_file");
    w.expect(&[(Delete, "r/to_dir"), (Delete, "r/to_file")]);
  });
}

/// The disk spells it `e` and an accent, the watched path `é`: one name on the usual disk of
/// macOS, and two on every other disk, where there is nothing to test.
#[test]
fn a_watched_file_in_another_unicode_form() {
  on_disk(|w| {
    w.write("src/cafe\u{301}.txt", "1");
    if !w.path("src/caf\u{e9}.txt").exists() {
      skipped("only the usual disk of macOS takes one Unicode form of a name for the other");
      return;
    }
    w.watch(&["src/caf\u{e9}.txt"]);

    w.write("src/cafe\u{301}.txt", "22");
    w.expect(&[(Update, "src/caf\u{e9}.txt")]);
    w.remove("src/cafe\u{301}.txt");
    w.expect(&[(Delete, "src/caf\u{e9}.txt")]);
    w.write("src/cafe\u{301}.txt", "1");
    w.expect(&[(Create, "src/caf\u{e9}.txt")]);
  });
}

#[test]
fn names_below_a_watched_directory_are_spelled_as_on_the_disk() {
  everywhere(|w| {
    w.write("proj/Src/Old.TXT", "1");
    // Where the disk takes two spellings for one, the directory is watched in another case.
    let loose = w.folds_case();
    let root = if loose { "PROJ" } else { "proj" };
    let at = |rel: &str| format!("{root}/{rel}");
    w.watch(&[root]);

    w.write("proj/Src/Old.TXT", "22");
    w.write("proj/Src/New.Js", "1");
    w.write("proj/ReadMe.md", "1");
    w.expect(&[
      (Update, &at("Src/Old.TXT")),
      (Create, &at("Src/New.Js")),
      (Create, &at("ReadMe.md")),
    ]);

    // A writer that spells the names in its own way changes nothing about them.
    if loose {
      w.write("Proj/SRC/old.txt", "333");
      w.expect(&[(Update, &at("Src/Old.TXT"))]);
    }
  });
}

#[test]
fn a_file_is_renamed_to_another_letter_case() {
  everywhere(|w| {
    w.write("r/ReadMe.md", "1");
    w.watch(&["r"]);

    w.rename("r/ReadMe.md", "r/README.md");
    w.expect(&[(Delete, "r/ReadMe.md"), (Create, "r/README.md")]);
    w.remove("r/README.md");
    w.expect(&[(Delete, "r/README.md")]);
  });
}

/// The OS names a file that is gone as the caller spelled it then.
#[test]
fn a_file_goes_through_another_spelling_of_its_name() {
  everywhere_with(&Settings::loose(), |w| {
    w.write("r/App.js", "1");
    w.write("r/Other.js", "1");
    w.watch(&["r"]);

    w.remove("r/APP.JS");
    w.rename("r/other.JS", "away.js");
    w.expect(&[(Delete, "r/App.js"), (Delete, "r/Other.js")]);
    w.write("r/App.js", "22");
    w.expect(&[(Create, "r/App.js")]);
  });
}

/// Each watched spelling gets its events, and the watched directory reports the name that the
/// disk has.
#[test]
fn every_watched_spelling_of_a_file_gets_its_events() {
  everywhere_with(&Settings::loose(), |w| {
    w.write("r/app.js", "1");
    w.watch(&["r", "r/App.js", "r/APP.JS"]);

    w.write("r/app.js", "22");
    w.expect(&[(Update, "r/app.js"), (Update, "r/App.js"), (Update, "r/APP.JS")]);
    w.remove("r/app.js");
    w.expect(&[(Delete, "r/app.js"), (Delete, "r/App.js"), (Delete, "r/APP.JS")]);
    // The file comes back under a watched spelling: that is its name in the directory now.
    w.write("r/App.js", "333");
    w.expect(&[(Create, "r/App.js"), (Create, "r/APP.JS")]);
  });
}

#[test]
fn watched_paths_through_another_spelling_inside_a_watched_directory() {
  everywhere_with(&Settings::loose(), |w| {
    w.write("r/src/a.js", "1");
    w.write("r/src/lib/b.js", "1");
    w.watch(&["r", "r/SRC/a.js", "r/Src/LIB"]);

    w.write("r/src/a.js", "22");
    w.write("r/src/lib/c.js", "1");
    w.expect(&[
      (Update, "r/src/a.js"),
      (Update, "r/SRC/a.js"),
      (Create, "r/src/lib/c.js"),
      (Create, "r/Src/LIB/c.js"),
    ]);
    w.remove("r/src/a.js");
    w.remove("r/src/lib/b.js");
    w.expect(&[
      (Delete, "r/src/a.js"),
      (Delete, "r/SRC/a.js"),
      (Delete, "r/src/lib/b.js"),
      (Delete, "r/Src/LIB/b.js"),
    ]);
  });
}

#[test]
fn a_directory_is_renamed_to_another_letter_case() {
  everywhere(|w| {
    w.write("r/Src/a.js", "1");
    w.write("r/Src/deep/b.js", "1");
    w.watch(&["r"]);

    w.rename("r/Src", "r/SRC");
    w.expect(&[
      (Delete, "r/Src/a.js"),
      (Delete, "r/Src/deep/b.js"),
      (Create, "r/SRC/a.js"),
      (Create, "r/SRC/deep/b.js"),
    ]);
    w.write("r/SRC/deep/b.js", "22");
    w.write("r/SRC/deep/c.js", "1");
    w.expect(&[(Update, "r/SRC/deep/b.js"), (Create, "r/SRC/deep/c.js")]);
  });
}

/// The watched path still finds its directory, and the events keep the spelling of the caller.
#[test]
fn a_directory_above_a_watched_path_is_renamed_to_another_letter_case() {
  on_disk_with(&Settings::loose(), |w| {
    w.write("r/x/a/old.js", "1");
    // Windows refuses to rename a directory above an open directory handle.
    if cfg!(windows) && w.backend == "native" {
      skipped("Windows refuses to rename a directory above an open handle");
      return;
    }
    w.watch(&["r/x/a"]);

    w.rename("r/x", "r/X");
    w.write("r/X/a/new.js", "1");
    w.expect(&[(Create, "r/x/a/new.js")]);
    w.write("r/X/a/old.js", "22");
    w.write("r/X/a/more.js", "1");
    w.expect(&[(Update, "r/x/a/old.js"), (Create, "r/x/a/more.js")]);
  });
}

/// Windows takes a dot or a space off the end of a name and reads `nul` as a device, unless the
/// path has the `\\?\` form. A directory can still hold such names (git, WSL and an unpacked
/// archive make them), and they are files like the others.
#[cfg(windows)]
#[test]
fn a_name_that_windows_reads_as_another_is_reported() {
  on_disk(|w| {
    w.write("w/foo", "the other one");
    w.watch(&["w"]);
    // Only a path in the `\\?\` form lets std make such a name.
    let verbatim = fs::canonicalize(w.path("w")).unwrap();
    for name in ["foo.", "foo ", "nul", "aux.c"] {
      fs::write(verbatim.join(name), "1").unwrap();
    }
    w.expect(&[(Create, "w/foo."), (Create, "w/foo "), (Create, "w/nul"), (Create, "w/aux.c")]);

    fs::write(verbatim.join("foo."), "22").unwrap();
    w.expect(&[(Update, "w/foo.")]);

    fs::create_dir(verbatim.join("dir.")).unwrap();
    fs::write(verbatim.join("dir.").join("x.txt"), "1").unwrap();
    w.expect(&[(Create, "w/dir./x.txt")]);
    fs::write(verbatim.join("dir.").join("x.txt"), "22").unwrap();
    w.expect(&[(Update, "w/dir./x.txt")]);

    // `foo` stays, and its events are its own.
    fs::remove_file(verbatim.join("foo.")).unwrap();
    fs::remove_file(verbatim.join("nul")).unwrap();
    w.expect(&[(Delete, "w/foo."), (Delete, "w/nul")]);
    w.write("w/foo", "written again");
    w.expect(&[(Update, "w/foo")]);

    fs::remove_dir_all(&verbatim).unwrap();
    w.expect(&[
      (Delete, "w/foo"),
      (Delete, "w/foo "),
      (Delete, "w/aux.c"),
      (Delete, "w/dir./x.txt"),
    ]);
  });
}

/// Windows: a junction is made in two steps, a plain directory first, and the second step
/// (the reparse point) gives no record the backend asks for. A look between the steps sees a
/// directory; the watcher looks once more at the next flush, which the poll brings within a
/// second, and reports the link: a file inside a watched directory, and what is behind it at a
/// watched path. Between the steps the story only waits, longer than the quiet time, so that
/// the first look falls between them, and not long, so that no poll does: a flush between the
/// steps would be the one more look, and would see the directory (behavior.md, "Holes").
#[cfg(windows)]
#[test]
fn a_junction_made_in_two_steps() {
  use std::{
    io,
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
  };

  use windows_sys::Win32::{
    Foundation::GENERIC_WRITE,
    Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT},
    System::{IO::DeviceIoControl, Ioctl::FSCTL_SET_REPARSE_POINT},
  };

  /// Longer than the quiet time, so that the first look falls between the steps, and short
  /// next to the second, which the poll brings.
  const BETWEEN_THE_STEPS: Duration = Duration::from_millis(50);

  /// `IO_REPARSE_TAG_MOUNT_POINT`: the second step of `mklink /J` on the directory `link`.
  fn set_mount_point(link: &Path, target: &Path) {
    let dir = fs::OpenOptions::new()
      .access_mode(GENERIC_WRITE)
      .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
      .open(link)
      .unwrap();
    let print: Vec<u16> = target.as_os_str().encode_wide().collect();
    let mut names: Vec<u16> = r"\??\".encode_utf16().chain(print.iter().copied()).collect();
    let substitute = names.len();
    names.push(0);
    names.extend(&print);
    names.push(0);
    // `REPARSE_DATA_BUFFER` in the mount point form: the tag, the length of the data, reserved;
    // the offsets and lengths of the two names in bytes; the names.
    let bytes = |units: usize| u16::try_from(units * 2).unwrap().to_le_bytes();
    let mut buffer: Vec<u8> = Vec::new();
    buffer.extend(0xA000_0003_u32.to_le_bytes());
    buffer.extend(bytes(4 + names.len()));
    buffer.extend(0_u16.to_le_bytes());
    buffer.extend(0_u16.to_le_bytes());
    buffer.extend(bytes(substitute));
    buffer.extend(bytes(substitute + 1));
    buffer.extend(bytes(print.len()));
    buffer.extend(names.iter().flat_map(|unit| unit.to_le_bytes()));
    let mut returned = 0_u32;
    // SAFETY: the handle is open, `buffer` is the whole input, and nothing is written back but
    // the count.
    let set = unsafe {
      let (input, size) = (buffer.as_ptr().cast(), u32::try_from(buffer.len()).unwrap());
      let (no_output, no_overlap) = (std::ptr::null_mut(), std::ptr::null_mut());
      let handle = dir.as_raw_handle();
      DeviceIoControl(
        handle,
        FSCTL_SET_REPARSE_POINT,
        input,
        size,
        no_output,
        0,
        &raw mut returned,
        no_overlap,
      )
    };
    assert!(set != 0, "{}", io::Error::last_os_error());
  }

  on_disk(|w| {
    w.write("t/a.txt", "1");
    w.write("t/sub/b.txt", "1");
    w.mkdir("w");
    w.mkdir("b");
    w.watch(&["w", "b/cur"]);

    // Inside a watched directory: a directory is no event, and the link is a file.
    w.mkdir("w/j");
    std::thread::sleep(BETWEEN_THE_STEPS);
    set_mount_point(&w.path("w/j"), &w.path("t"));
    w.expect(&[(Create, "w/j")]);
    w.write("t/a.txt", "22");
    w.expect(&[]);

    // At a watched path: the link is followed.
    w.mkdir("b/cur");
    std::thread::sleep(BETWEEN_THE_STEPS);
    set_mount_point(&w.path("b/cur"), &w.path("t"));
    w.expect(&[(Create, "b/cur/a.txt"), (Create, "b/cur/sub/b.txt")]);
    w.write("t/a.txt", "333");
    w.expect(&[(Update, "b/cur/a.txt")]);
    fs::remove_dir(w.path("b/cur")).unwrap();
    w.expect(&[(Delete, "b/cur/a.txt"), (Delete, "b/cur/sub/b.txt")]);
  });
}

/// Windows: a deletion that waits for the last handle to close (`FileDispositionInfo`, not the
/// way of today's `DeleteFile`) hides the file from a look while the directory still lists
/// it. Taken back, the file is there again and the OS says nothing: the watcher looks again.
#[cfg(windows)]
#[test]
fn a_pending_deletion_that_is_taken_back() {
  use std::{
    io::{self, Write},
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
  };

  use windows_sys::Win32::{
    Foundation::{GENERIC_READ, GENERIC_WRITE},
    Storage::FileSystem::{
      DELETE, FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
    },
  };

  on_disk(|w| {
    w.write("d/held.txt", "1");
    w.watch(&["d"]);
    let mut holder = fs::OpenOptions::new()
      .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
      .open(w.path("d/held.txt"))
      .unwrap();
    let dispose = |holder: &fs::File, delete: bool| {
      let info = FILE_DISPOSITION_INFO { DeleteFile: delete };
      let size = u32::try_from(size_of::<FILE_DISPOSITION_INFO>()).unwrap();
      // SAFETY: the handle is open, and `info` is one whole struct of the class.
      let set = unsafe {
        let (class, info) = (FileDispositionInfo, (&raw const info).cast());
        SetFileInformationByHandle(holder.as_raw_handle(), class, info, size)
      };
      assert!(set != 0, "{}", io::Error::last_os_error());
    };
    // NTFS reports a write through an open handle when the file is closed or its cache is
    // flushed: the flush is what makes the watcher look.
    holder.write_all(b"22").unwrap();
    holder.sync_all().unwrap();
    w.expect(&[(Update, "d/held.txt")]);
    dispose(&holder, true);
    holder.write_all(b"333").unwrap();
    holder.sync_all().unwrap();
    w.expect(&[(Delete, "d/held.txt")]);

    // Nothing is reported for this: the watcher looks again by itself.
    dispose(&holder, false);
    w.expect(&[(Create, "d/held.txt")]);
    drop(holder);
    w.expect(&[]);
    w.remove("d/held.txt");
    w.expect(&[(Delete, "d/held.txt")]);
  });
}

/// The drive letter in the other case is another spelling of the path, and another top of
/// the tree. A path watched that way shares the handle of the watched directory above, and
/// the directory still gets the events of the other files in the same directory.
#[cfg(windows)]
#[test]
fn a_watched_path_with_the_drive_letter_in_the_other_case() {
  on_disk(|w| {
    w.write("p/src/a.js", "1");
    w.write("p/src/b.js", "1");
    w.watch(&["p"]);
    let text = w.path("p/src/a.js").to_str().unwrap().to_owned();
    assert_eq!(&text[1..2], ":", "{text}");
    let drive = &text[..1];
    let other = if drive == drive.to_ascii_uppercase() {
      drive.to_ascii_lowercase()
    } else {
      drive.to_ascii_uppercase()
    };
    w.watcher.watch(&[format!("{other}{}", &text[1..])]).unwrap();

    w.write("p/src/b.js", "22");
    w.write("p/src/c.js", "1");
    w.expect(&[(Update, "p/src/b.js"), (Create, "p/src/c.js")]);
    w.remove("p/src/b.js");
    w.expect(&[(Delete, "p/src/b.js")]);
  });
}

/// `close` inside the handler, also with the backend of the OS: its threads end, and the
/// `close` of the test, which waits for the thread of the watcher, comes back.
#[test]
fn close_inside_the_handler() {
  let inside: Inside = |watcher, _events| watcher.close();
  on_disk_inside(inside, |w| {
    w.mkdir("r");
    w.watch(&["r"]);
    w.write("r/a.txt", "1");
    // The batch in which the handler closed the watcher still comes, and it is the last one:
    // the thread ends, and its end of the channel with it.
    let batch = w.results.recv_timeout(PATIENCE).expect("the handler ran").unwrap();
    assert_eq!(batch, [Event { path: w.path("r/a.txt"), kind: Create }], "{}", w.backend);
    w.write("r/b.txt", "1");
    let end = w.results.recv_timeout(PATIENCE);
    assert!(matches!(end, Err(RecvTimeoutError::Disconnected)), "{}: {end:?}", w.backend);

    let closed = w.watcher.watch(&[w.path("r/x")]).unwrap_err();
    assert_eq!(closed.kind(), ErrorKind::Closed, "{}", w.backend);
    w.watcher.close();
  });
}

/// Windows: a directory that another program holds with no sharing, for a moment, is no
/// error: nothing is wrong with the disk, and the next look finds what is in it.
#[cfg(windows)]
#[test]
fn a_directory_held_by_another_program_is_no_error() {
  use std::os::windows::fs::OpenOptionsExt;

  use windows_sys::Win32::{
    Foundation::GENERIC_READ, Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS,
  };

  on_disk(|w| {
    w.mkdir("r/held");
    w.write("r/other.txt", "1");
    let holder = fs::OpenOptions::new()
      .access_mode(GENERIC_READ)
      .share_mode(0)
      .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
      .open(w.path("r/held"))
      .unwrap();
    w.watch(&["r"]);
    w.write("r/new.txt", "1");
    w.expect(&[(Create, "r/new.txt")]);
    drop(holder);
    w.write("r/held/inside.txt", "1");
    w.expect(&[(Create, "r/held/inside.txt")]);
  });
}

/// Windows: a write by a writer that keeps the file open is reported under the poll, although
/// the listing of the directory lags behind until the writer closes the file. The file is old,
/// so no hash of its content hides the lag. The backend of the OS reports the write when the
/// writer flushes.
#[cfg(windows)]
#[test]
fn a_write_by_a_writer_that_keeps_the_file_open_is_reported() {
  use std::io::Write;

  on_disk(|w| {
    w.write("r/log.txt", "1");
    let an_hour_ago = SystemTime::now() - Duration::from_secs(3600);
    fs::File::options()
      .write(true)
      .open(w.path("r/log.txt"))
      .unwrap()
      .set_modified(an_hour_ago)
      .unwrap();
    w.watch(&["r"]);
    let mut holder = fs::OpenOptions::new().append(true).open(w.path("r/log.txt")).unwrap();
    holder.write_all(b"22").unwrap();
    if w.backend == "native" {
      holder.sync_all().unwrap();
    }
    w.expect(&[(Update, "r/log.txt")]);
    drop(holder);
  });
}
