//! Every way a user writes, renames and links a file, and what the handler gets for it.

use std::sync::mpsc;

use pathwatch::{
  EventKind::{Create, Delete, Update},
  Watcher,
};

use crate::{disk::PATIENCE, everywhere, on_disk, skipped};

/// Every write of a file is an `Update`: in place, appended, cut to nothing, and the same
/// content written again. Not the last one on Windows: NTFS leaves the time of an empty file
/// as it is when it is emptied again (measured), so nothing on the disk tells of that write.
#[test]
fn every_way_to_write_a_file_is_an_update() {
  everywhere(|w| {
    w.write("r/log.txt", "line 1\n");
    w.write("f.txt", "1");
    w.watch(&["r", "f.txt"]);
    w.append("r/log.txt", "line 2\n");
    w.append("f.txt", "1");
    w.expect(&[(Update, "r/log.txt"), (Update, "f.txt")]);
    w.write("r/log.txt", "");
    w.write("f.txt", "");
    w.expect(&[(Update, "r/log.txt"), (Update, "f.txt")]);
    w.write("r/log.txt", "");
    w.write("f.txt", "");
    if cfg!(windows) {
      w.expect(&[]);
    } else {
      w.expect(&[(Update, "r/log.txt"), (Update, "f.txt")]);
    }
  });
}

/// A renamed file is a `Delete` of the old name and a `Create` of the new one: inside a watched
/// directory, out of it, into it, and over another file, which is an `Update` of that one. A
/// file moved over a watched file is an `Update` of the watched path.
#[test]
fn a_file_is_renamed() {
  everywhere(|w| {
    w.write("r/a.txt", "1");
    w.write("r/b.txt", "1");
    w.write("r/c.txt", "1");
    w.write("outside/d.txt", "1");
    w.write("outside/e.txt", "1");
    w.write("f.txt", "1");
    w.write("outside/new-f.txt", "22");
    w.watch(&["r", "f.txt"]);

    w.rename("r/a.txt", "r/a2.txt");
    w.expect(&[(Delete, "r/a.txt"), (Create, "r/a2.txt")]);
    w.rename("r/b.txt", "outside/b.txt");
    w.expect(&[(Delete, "r/b.txt")]);
    w.rename("outside/d.txt", "r/d.txt");
    w.expect(&[(Create, "r/d.txt")]);
    w.rename("outside/e.txt", "r/c.txt");
    w.expect(&[(Update, "r/c.txt")]);
    w.rename("r/d.txt", "r/a2.txt");
    w.expect(&[(Delete, "r/d.txt"), (Update, "r/a2.txt")]);
    w.rename("outside/new-f.txt", "f.txt");
    w.expect(&[(Update, "f.txt")]);
  });
}

/// Directories are not reported, only files: an empty directory that comes, is renamed and
/// goes gives nothing, in a watched directory and as a watched path.
#[test]
fn directories_alone_are_not_reported() {
  everywhere(|w| {
    w.mkdir("r");
    w.mkdir("d");
    w.watch(&["r", "d", "later"]);
    w.mkdir("r/sub");
    w.mkdir("r/sub/deeper");
    w.mkdir("later");
    w.expect(&[]);
    w.rename("r/sub", "r/moved");
    w.rename("later", "later2");
    w.expect(&[]);
    w.remove("r/moved");
    w.remove("d");
    w.expect(&[]);
    w.write("later/x.txt", "1");
    w.expect(&[(Create, "later/x.txt")]);
  });
}

/// A path given to `watch` twice is watched once: one event for a change, and one `unwatch`
/// takes it back.
#[test]
fn watching_a_path_twice_is_one_watch() {
  everywhere(|w| {
    w.write("r/a.txt", "1");
    w.write("f.txt", "1");
    w.watch(&["r", "f.txt"]);
    w.watch(&["r", "f.txt"]);
    w.watch(&["r"]);
    w.write("r/a.txt", "22");
    w.write("f.txt", "22");
    w.expect(&[(Update, "r/a.txt"), (Update, "f.txt")]);
    w.unwatch(&["r", "f.txt"]);
    w.write("r/a.txt", "333");
    w.write("f.txt", "333");
    w.expect(&[]);
  });
}

/// A link that leads nowhere, or to itself, is a file like any other link: it comes and goes,
/// and it is never followed.
#[test]
fn a_link_that_leads_nowhere_or_to_itself_is_a_file() {
  everywhere(|w| {
    w.mkdir("r");
    w.watch(&["r"]);
    if !w.symlink("r/nowhere", "r/dangling") {
      return;
    }
    w.symlink("r/loop", "r/loop");
    w.expect(&[(Create, "r/dangling"), (Create, "r/loop")]);
    w.write("r/nowhere", "1");
    w.expect(&[(Create, "r/nowhere")]);
    w.remove("r/dangling");
    w.remove("r/loop");
    w.expect(&[(Delete, "r/dangling"), (Delete, "r/loop")]);
  });
}

/// A file that becomes a link, and a link that becomes a file, under one name: an `Update`
/// each time, as the entry is a file throughout.
#[test]
fn a_file_becomes_a_link_and_a_link_becomes_a_file() {
  everywhere(|w| {
    w.write("r/a.js", "1");
    w.write("r/other.js", "22");
    w.watch(&["r"]);
    w.remove("r/a.js");
    if !w.symlink("r/other.js", "r/a.js") {
      return;
    }
    w.expect(&[(Update, "r/a.js")]);
    w.remove("r/a.js");
    w.write("r/a.js", "333");
    w.expect(&[(Update, "r/a.js")]);
  });
}

/// Two watchers in one process see the same change, each with its own handler.
#[test]
fn two_watchers_in_one_process_both_see_a_change() {
  on_disk(|w| {
    w.write("r/a.txt", "1");
    w.watch(&["r"]);
    let (sender, receiver) = mpsc::channel();
    let other = Watcher::new(move |result| {
      let _ = sender.send(result);
    })
    .unwrap();
    other.watch(&[w.path("r")]).unwrap();

    w.write("r/a.txt", "22");
    w.expect(&[(Update, "r/a.txt")]);
    let batch = receiver.recv_timeout(PATIENCE).expect("the other handler ran").unwrap();
    assert_eq!(batch.len(), 1, "{batch:?}");
    assert_eq!((batch[0].kind, batch[0].path.as_path()), (Update, w.path("r/a.txt").as_path()));
    other.close();
  });
}

/// A watched file written through another hard link of it is an `Update`: the file is the
/// same, whatever name the writer used. Not with inotify and ReadDirectoryChangesW, which
/// report a write under the name the writer used (a hole of the README).
#[test]
fn a_write_through_another_hard_link_is_an_update() {
  on_disk(|w| {
    if cfg!(any(target_os = "linux", windows)) && w.backend == "native" {
      skipped("inotify and ReadDirectoryChangesW report a write under the name the writer used");
      return;
    }
    w.write("r/a.txt", "1");
    w.write("f.txt", "1");
    w.mkdir("other");
    std::fs::hard_link(w.path("r/a.txt"), w.path("other/same.txt")).unwrap();
    std::fs::hard_link(w.path("f.txt"), w.path("other/same-f.txt")).unwrap();
    w.watch(&["r", "f.txt"]);
    w.write("other/same.txt", "22");
    w.write("other/same-f.txt", "22");
    w.expect(&[(Update, "r/a.txt"), (Update, "f.txt")]);
  });
}
