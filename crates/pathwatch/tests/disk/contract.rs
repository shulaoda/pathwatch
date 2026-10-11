//! The behavior contract: one test for each row S1 to S14 of the table in `docs/design.md`.
//! `r` is the watched path.

use std::{
  sync::{Arc, mpsc},
  time::Duration,
};

// The backend for the watch API of this system.
#[cfg(target_os = "macos")]
use pathwatch::fsevents::FsEventsBackend as Native;
#[cfg(any(target_os = "linux", target_os = "android"))]
use pathwatch::inotify::InotifyBackend as Native;
#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos", windows)))]
use pathwatch::poll::PollBackend as Native;
#[cfg(windows)]
use pathwatch::windows::WindowsBackend as Native;
use pathwatch::{
  Config, EntryKind,
  EventKind::{Create, Delete, Update},
  Watcher,
  engine::Options,
  poll::PollBackend,
};

use crate::{POLL_INTERVAL, Watch, backend_name, each_backend, each_backend_with, side_by_side};

#[test]
fn s1_a_write_in_place() {
  each_backend(|w| {
    w.write("r.txt", "1");
    w.watch(&["r.txt"]);

    w.write("r.txt", "22");
    w.expect(&[(Update, "r.txt")]);
    // The same size, and most likely the same time stamp.
    w.write("r.txt", "33");
    w.expect(&[(Update, "r.txt")]);
  });
}

#[test]
fn s2_an_atomic_save() {
  each_backend(|w| {
    w.write("r.txt", "1");
    w.watch(&["r.txt"]);

    for content in ["2", "3", "4"] {
      w.write("r.txt.tmp", content);
      w.rename("r.txt.tmp", "r.txt");
      w.expect(&[(Update, "r.txt")]);
    }
  });
}

#[test]
fn s3_deleted_and_created_again() {
  each_backend(|w| {
    w.write("r.txt", "1");
    w.watch(&["r.txt"]);

    w.remove("r.txt");
    w.expect(&[(Delete, "r.txt")]);
    w.write("r.txt", "1");
    w.expect(&[(Create, "r.txt")]);
  });
}

#[test]
fn s3_deleted_and_created_again_inside_the_delete_delay() {
  // A delay of 100 ms is too short for a slow machine, and no public setting changes it: the
  // watchers are built by hand.
  const DELETE_DELAY: Duration = Duration::from_secs(2);

  side_by_side(|polling| {
    let options = Options {
      debounce: Duration::from_millis(50),
      delete_delay: DELETE_DELAY,
      poll_interval: if polling { POLL_INTERVAL } else { Duration::from_secs(1) },
      loose_spelling: cfg!(any(target_vendor = "apple", windows)),
      ..Options::default()
    };
    let (sender, results) = mpsc::channel();
    let handler = move |result| {
      // The test may be over already.
      let _ = sender.send(result);
    };
    let watcher = if polling {
      Watcher::with_backend(options, handler, PollBackend::new)
    } else {
      Watcher::with_backend(options, handler, Native::new)
    };
    let mut w = Watch {
      watcher: Arc::new(watcher.unwrap()),
      results,
      dir: tempfile::tempdir().unwrap(),
      events: Vec::new(),
      passing: Vec::new(),
      backend: backend_name(polling),
    };

    w.write("r.txt", "1");
    w.watch(&["r.txt"]);

    w.remove("r.txt");
    w.write("r.txt", "1");
    w.expect(&[(Update, "r.txt")]);
    // A `Delete` that still waited would come when the delay is over.
    assert!(!w.receive(DELETE_DELAY), "{}: {:?}", w.backend, w.events);
  });
}

#[test]
fn s4_a_missing_path_becomes_a_file() {
  each_backend(|w| {
    w.watch(&["r.txt"]);

    w.write("r.txt", "1");
    w.expect(&[(Create, "r.txt")]);
    w.write("r.txt", "22");
    w.expect(&[(Update, "r.txt")]);
  });
}

#[test]
fn s5_a_missing_path_becomes_a_directory() {
  each_backend(|w| {
    w.watch(&["r"]);

    w.write("r/f.txt", "1");
    w.expect(&[(Create, "r/f.txt")]);
  });
}

#[test]
fn s6_missing_ancestors() {
  each_backend(|w| {
    w.watch(&["a/b/c/r.txt"]);

    w.write("a/b/c/r.txt", "1");
    w.expect(&[(Create, "a/b/c/r.txt")]);
  });
}

#[test]
fn s7_a_file_becomes_a_directory() {
  each_backend(|w| {
    w.write("r", "1");
    w.watch(&["r"]);

    w.remove("r");
    w.write("r/x.txt", "1");
    w.write("r/sub/y.txt", "1");
    w.expect(&[(Delete, "r"), (Create, "r/x.txt"), (Create, "r/sub/y.txt")]);
  });
}

#[test]
fn s8_a_directory_becomes_a_file() {
  each_backend(|w| {
    w.write("r/x.txt", "1");
    w.write("r/sub/y.txt", "1");
    w.watch(&["r"]);

    w.remove("r");
    w.write("r", "1");
    w.expect(&[(Delete, "r/x.txt"), (Delete, "r/sub/y.txt"), (Create, "r")]);
  });
}

#[test]
fn s9_new_files_in_a_directory() {
  each_backend(|w| {
    w.mkdir("r");
    w.watch(&["r"]);

    w.write("r/f.txt", "1");
    w.expect(&[(Create, "r/f.txt")]);
    // The directory and the file in it, with nothing in between.
    w.write("r/sub/f.txt", "1");
    w.expect(&[(Create, "r/sub/f.txt")]);
  });
}

#[test]
fn s10_a_tree_is_moved_in() {
  each_backend(|w| {
    w.mkdir("r");
    w.write("outside/tree/a.txt", "1");
    w.write("outside/tree/sub/b.txt", "1");
    w.write("outside/tree/sub/deep/c.txt", "1");
    w.watch(&["r"]);

    w.rename("outside/tree", "r/tree");
    w.expect(&[
      (Create, "r/tree/a.txt"),
      (Create, "r/tree/sub/b.txt"),
      (Create, "r/tree/sub/deep/c.txt"),
    ]);
  });
}

#[test]
fn s11_a_directory_is_moved_out_or_removed() {
  each_backend(|w| {
    w.write("r/one/a.txt", "1");
    w.write("r/one/sub/b.txt", "1");
    w.write("r/two/c.txt", "1");
    w.write("r/two/sub/d.txt", "1");
    w.write("r/stays.txt", "1");
    w.mkdir("outside");
    w.watch(&["r"]);

    w.rename("r/one", "outside/one");
    w.expect(&[(Delete, "r/one/a.txt"), (Delete, "r/one/sub/b.txt")]);
    w.remove("r/two");
    w.expect(&[(Delete, "r/two/c.txt"), (Delete, "r/two/sub/d.txt")]);
  });
}

#[test]
fn s12_the_watched_directory_goes_and_comes_back() {
  each_backend(|w| {
    w.write("r/a.txt", "1");
    w.write("r/sub/b.txt", "1");
    w.watch(&["r"]);

    w.rename("r", "away");
    w.expect(&[(Delete, "r/a.txt"), (Delete, "r/sub/b.txt")]);
    w.rename("away", "r");
    w.expect(&[(Create, "r/a.txt"), (Create, "r/sub/b.txt")]);

    w.remove("r");
    w.expect(&[(Delete, "r/a.txt"), (Delete, "r/sub/b.txt")]);
    w.write("r/c.txt", "1");
    w.expect(&[(Create, "r/c.txt")]);
  });
}

#[test]
fn s13_the_directory_of_a_watched_file_is_renamed() {
  each_backend(|w| {
    w.write("proj/lib/a.js", "1");
    w.watch(&["proj/lib/a.js"]);

    w.rename("proj/lib", "proj/lib2");
    w.expect(&[(Delete, "proj/lib/a.js")]);
    w.rename("proj/lib2", "proj/lib");
    w.expect(&[(Create, "proj/lib/a.js")]);
  });
}

#[test]
fn s14_an_ignored_directory() {
  let config = Config::default()
    .with_ignored(|path, kind| kind == EntryKind::Dir && path.ends_with("node_modules"));
  each_backend_with(&config, |w| {
    w.write("r/node_modules/before.js", "1");
    w.watch(&["r"]);

    // Nothing from inside it. The other file is there so that the test can wait for the watcher.
    w.write("r/node_modules/pkg/index.js", "1");
    w.write("r/node_modules/before.js", "22");
    w.write("r/visible.js", "1");
    w.expect(&[(Create, "r/visible.js")]);

    w.rename("r/node_modules", "r/vendor");
    w.expect(&[(Create, "r/vendor/before.js"), (Create, "r/vendor/pkg/index.js")]);
    w.rename("r/vendor", "r/node_modules");
    w.expect(&[(Delete, "r/vendor/before.js"), (Delete, "r/vendor/pkg/index.js")]);
  });
}
