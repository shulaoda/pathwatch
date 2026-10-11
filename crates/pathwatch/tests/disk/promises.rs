//! The promises of the README, and which test proves which (`c::` is `contract`, `p::` is
//! `projects`):
//!
//! 1. A watched path is a file, a directory or nothing: `c::s1`, `c::s9`, `c::s4`,
//!    `a_file_is_anything_that_is_not_a_directory`.
//! 2. Events are the difference to the disk, one for a path in a batch:
//!    `p::random_changes_end_in_the_state_of_the_disk`; `Watch::receive` checks every batch.
//! 3. `Update`: `touch_is_an_update_and_chmod_is_not`, `c::s2`,
//!    `c::s3_deleted_and_created_again_inside_the_delete_delay`.
//! 4. What exists at `watch` time: `what_exists_at_watch_time_is_not_reported`.
//! 5. Missing, file, directory: `c::s4` to `c::s8`, `a_name_on_the_way_changes_its_kind`,
//!    `a_missing_chain_that_appears_at_once_is_reported_promptly`.
//! 6. A directory goes or comes: `c::s10`, `c::s11`, `c::s12`.
//! 7. `unwatch`: `unwatch_of_a_file_and_of_a_directory`,
//!    `unwatch_keeps_what_another_watched_path_covers`.
//! 8. Paths: `a_relative_path_with_dot_dot`, `one_bad_path_among_good_ones`.
//! 9. Calls inside the handler: `watch_and_unwatch_inside_the_handler`.
//! 10. The ignore filter: `c::s14`, `an_ignored_directory_is_not_read`,
//!     `an_ignored_directory_comes_and_goes`, `a_watched_path_that_the_filter_ignores`,
//!     `a_watched_path_that_is_ignored_for_both_kinds_gets_nothing`,
//!     `a_watched_file_in_an_ignored_directory_gets_nothing`,
//!     `an_ignored_watched_directory_is_reported_when_a_file_takes_its_place`.
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
  env, fs,
  path::{Component, Path, PathBuf},
  sync::{Arc, Mutex},
  time::{Duration, Instant, SystemTime},
};

use pathwatch::{
  Config, EntryKind, ErrorKind, Event,
  EventKind::{Create, Delete, Update},
  Watcher,
};

use crate::{each, each_backend, each_backend_inside, each_backend_with};

#[cfg(unix)]
#[test]
fn a_file_is_anything_that_is_not_a_directory() {
  use std::os::unix::{fs::symlink, net::UnixListener};

  each_backend(|w| {
    w.mkdir("r");
    // macOS 26 says nothing when a socket is made: there the sockets exist before the watch,
    // and only their removal is heard.
    let bind = || {
      (
        UnixListener::bind(w.path("r/socket")).unwrap(),
        UnixListener::bind(w.path("socket")).unwrap(),
      )
    };
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
  each_backend(|w| {
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
  each_backend(|w| {
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
  each_backend(|w| {
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

  each_backend(|w| {
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
    // With the poll, every chain takes a second.
    assert!(took < Duration::from_secs(5), "{}: {CHAINS} chains took {took:?}", w.backend);
    w.expect(&each(Create, &files));
  });
}

#[test]
fn unwatch_of_a_file_and_of_a_directory() {
  each_backend(|w| {
    w.write("r/a.txt", "1");
    w.write("r/sub/b.txt", "1");
    w.write("f.txt", "1");
    w.write("stays/c.txt", "1");
    w.watch(&["r", "f.txt", "missing.txt", "stays", "stays.txt"]);

    w.unwatch(&["r", "f.txt", "missing.txt"]);
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
  each_backend(|w| {
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
  each_backend(|w| {
    let here = env::current_dir().unwrap();
    let dir = w.path("");
    // On Windows the two can be on two drives: no relative path leads there.
    if here.components().next() != dir.components().next() {
      return;
    }
    // From the current directory up to the root, and down to the temporary directory.
    let is_name = |part: &Component| matches!(part, Component::Normal(_));
    let mut rel: PathBuf =
      here.components().filter(is_name).map(|_| Component::ParentDir).collect();
    rel.extend(dir.components().filter(is_name));
    assert!(rel.is_relative());

    w.watcher.watch(rel.join("a.txt")).unwrap();
    w.watcher.watch(Path::new(".").join(&rel).join("nothing/../sub/./deep/..//b.txt/")).unwrap();
    w.watcher.watch(w.path("x/y/../../c.txt")).unwrap();
    w.write("a.txt", "1");
    w.write("sub/b.txt", "1");
    w.write("c.txt", "1");
    w.expect(&[(Create, "a.txt"), (Create, "sub/b.txt"), (Create, "c.txt")]);

    // The clean path is the watched one: `unwatch` finds it under every spelling.
    w.watcher.unwatch(w.path("sub/../a.txt")).unwrap();
    w.watcher.unwatch(rel.join("sub/b.txt")).unwrap();
    w.write("a.txt", "22");
    w.write("sub/b.txt", "22");
    w.write("c.txt", "22");
    w.expect(&[(Update, "c.txt")]);
  });
}

#[test]
fn one_bad_path_among_good_ones() {
  each_backend(|w| {
    let bad = w.path("nul\0.txt");
    let paths = [w.path("a.txt"), bad.clone(), w.path("r"), PathBuf::new()];
    let error = w.watcher.watch_all(&paths).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidPath);
    assert_eq!(error.path(), Some(bad.as_path()), "the first bad path");

    w.write("a.txt", "1");
    w.write("r/b.txt", "1");
    w.expect(&[(Create, "a.txt"), (Create, "r/b.txt")]);

    let error = w.watcher.unwatch_all(&paths).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidPath);
    assert_eq!(error.path(), Some(bad.as_path()));
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
          .watch_all([event.path.with_file_name("b.txt"), event.path.with_file_name("d")])
          .unwrap();
        watcher.unwatch(&event.path).unwrap();
      }
    }
  }

  each_backend_inside(inside, |w| {
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
  let config = {
    let asked = Arc::clone(&asked);
    Config::default().with_ignored(move |path, kind| {
      asked.lock().unwrap().push(path.to_path_buf());
      kind == EntryKind::Dir && path.ends_with("node_modules")
    })
  };
  each_backend_with(&config, |w| {
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

/// A watched path that the filter ignores counts as missing.
#[test]
fn a_watched_path_that_the_filter_ignores() {
  let config = Config::default().with_ignored(|path, kind| {
    path.ends_with("secret.txt")
      || path.ends_with("hidden")
      || (kind == EntryKind::File && path.extension().is_some_and(|extension| extension == "log"))
  });
  each_backend_with(&config, |w| {
    w.write("hidden/a.txt", "1");
    w.write("r/old.log", "1");
    w.watch(&["secret.txt", "hidden", "r", "r/new.log"]);

    w.write("secret.txt", "1");
    w.write("hidden/a.txt", "22");
    w.write("hidden/b.txt", "1");
    w.write("r/old.log", "22");
    w.write("r/new.log", "1");
    w.write("r/seen.txt", "1");
    w.expect(&[(Create, "r/seen.txt")]);

    w.remove("secret.txt");
    w.remove("hidden");
    w.remove("r/old.log");
    w.remove("r/seen.txt");
    w.expect(&[(Delete, "r/seen.txt")]);
  });
}

/// By a rename and step by step: nothing is reported, and the filter is never asked about a
/// name inside.
#[test]
fn an_ignored_directory_comes_and_goes() {
  let asked = Arc::new(Mutex::new(Vec::new()));
  let config = {
    let asked = Arc::clone(&asked);
    Config::default().with_ignored(move |path, kind| {
      asked.lock().unwrap().push(path.to_path_buf());
      kind == EntryKind::Dir && path.ends_with("node_modules")
    })
  };
  each_backend_with(&config, |w| {
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

/// `watch` does not take it, nothing is reported, and `unwatch` of it does nothing.
#[test]
fn a_watched_path_that_is_ignored_for_both_kinds_gets_nothing() {
  let config = Config::default()
    .with_ignored(|path, _kind| path.components().any(|part| part.as_os_str() == "node_modules"));
  each_backend_with(&config, |w| {
    w.write("p/node_modules/pkg/index.js", "1");
    w.write("p/seen.js", "1");
    w.watch(&[
      "p/node_modules",
      "p/node_modules/pkg/index.js",
      "p/node_modules/new.js",
      "p/seen.js",
    ]);

    w.write("p/node_modules/pkg/index.js", "22");
    w.write("p/node_modules/new.js", "1");
    w.write("p/seen.js", "22");
    w.expect(&[(Update, "p/seen.js")]);

    // A directory where the file was, and a file where the directory was.
    w.remove("p/node_modules/pkg/index.js");
    w.write("p/node_modules/pkg/index.js/deep.js", "1");
    w.write("p/seen.js", "333");
    w.expect(&[(Update, "p/seen.js")]);
    w.remove("p/node_modules");
    w.write("p/node_modules", "a file now");
    w.unwatch(&["p/node_modules", "p/node_modules/new.js"]);
    w.remove("p/seen.js");
    w.expect(&[(Delete, "p/seen.js")]);
  });
}

/// Only the directory that holds the file is asked: a watched file one level further down is
/// reported.
#[test]
fn a_watched_file_in_an_ignored_directory_gets_nothing() {
  let config = Config::default()
    .with_ignored(|path, kind| kind == EntryKind::Dir && path.ends_with("node_modules"));
  each_backend_with(&config, |w| {
    w.write("p/node_modules/top.js", "1");
    w.write("p/node_modules/pkg/index.js", "1");
    w.watch(&["p/node_modules/top.js", "p/node_modules/later.js", "p/node_modules/pkg/index.js"]);

    w.write("p/node_modules/top.js", "22");
    w.write("p/node_modules/later.js", "1");
    w.write("p/node_modules/pkg/index.js", "22");
    w.expect(&[(Update, "p/node_modules/pkg/index.js")]);

    w.remove("p/node_modules/top.js");
    w.remove("p/node_modules/later.js");
    w.write("p/node_modules/top.js", "333");
    w.remove("p/node_modules/pkg/index.js");
    w.expect(&[(Delete, "p/node_modules/pkg/index.js")]);
  });
}

/// While a directory is there the path counts as missing; a file in its place is reported.
#[test]
fn an_ignored_watched_directory_is_reported_when_a_file_takes_its_place() {
  let config =
    Config::default().with_ignored(|path, kind| kind == EntryKind::Dir && path.ends_with("dist"));
  each_backend_with(&config, |w| {
    w.write("p/dist/a.js", "1");
    w.write("p/seen.js", "1");
    w.watch(&["p/dist", "p/seen.js"]);

    w.write("p/dist/a.js", "22");
    w.write("p/dist/b.js", "1");
    w.write("p/seen.js", "22");
    w.expect(&[(Update, "p/seen.js")]);

    w.remove("p/dist");
    w.write("p/dist", "a file now");
    w.expect(&[(Create, "p/dist")]);
    w.write("p/dist", "written");
    w.expect(&[(Update, "p/dist")]);

    w.remove("p/dist");
    w.write("p/dist/a.js", "1");
    w.expect(&[(Delete, "p/dist")]);
  });
}

#[test]
fn nothing_is_reported_beside_a_watched_path() {
  each_backend(|w| {
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
#[cfg(unix)]
#[test]
fn a_watched_link_to_a_file_is_followed() {
  each_backend(|w| {
    w.write("real/f.txt", "1");
    std::os::unix::fs::symlink(w.path("real/f.txt"), w.path("link.txt")).unwrap();
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

#[cfg(unix)]
#[test]
fn a_link_inside_a_watched_directory_is_a_file() {
  use std::os::unix::fs::symlink;

  each_backend(|w| {
    w.write("outside/dir/a.txt", "1");
    w.write("outside/f.txt", "1");
    w.mkdir("r");
    w.watch(&["r"]);

    symlink(w.path("outside/dir"), w.path("r/to_dir")).unwrap();
    symlink(w.path("outside/f.txt"), w.path("r/to_file")).unwrap();
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
/// macOS.
#[cfg(target_os = "macos")]
#[test]
fn a_watched_file_in_another_unicode_form() {
  each_backend(|w| {
    w.write("src/cafe\u{301}.txt", "1");
    // Nothing to test on a disk that tells the two forms apart.
    if !w.path("src/caf\u{e9}.txt").exists() {
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
  each_backend(|w| {
    w.write("proj/Src/Old.TXT", "1");
    // Where the disk takes two spellings for one, the directory is watched in another case.
    let loose = w.path("PROJ").exists();
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
  each_backend(|w| {
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
  each_backend(|w| {
    w.write("r/App.js", "1");
    w.write("r/Other.js", "1");
    // Only a disk that takes two spellings of a name for one finds a file in this way.
    if !w.path("R").exists() {
      return;
    }
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
  each_backend(|w| {
    w.write("r/app.js", "1");
    // Only a disk that takes two spellings of a name for one finds a file in this way.
    if !w.path("R").exists() {
      return;
    }
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
  each_backend(|w| {
    w.write("r/src/a.js", "1");
    w.write("r/src/lib/b.js", "1");
    if !w.path("R").exists() {
      return;
    }
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
  each_backend(|w| {
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
  each_backend(|w| {
    w.write("r/x/a/old.js", "1");
    if !w.path("R").exists() {
      return;
    }
    // Windows refuses to rename a directory above an open directory handle.
    if cfg!(windows) && w.backend == "native" {
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
