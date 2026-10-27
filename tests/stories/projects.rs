//! What editors, package managers and big projects do.

use std::{
  collections::{BTreeMap, BTreeSet},
  fs, thread,
  time::{Duration, Instant},
};

use pathwatch::EventKind::{Create, Delete, Update};

use crate::{
  Disk, Settings, World, disk::files_at, each, everywhere, everywhere_with, on_disk, on_disk_with,
  skipped,
};

/// A delete delay that the changes of a test fit into on a slow machine.
fn slow_delete() -> Settings {
  Settings::delete_delay(Duration::from_secs(2))
}

/// Long enough for the watcher to find the directory gone, and shorter than the delete delay of
/// `slow_delete`: a directory that is away this long is found gone and held, and is back before
/// its second look.
const AWAY: Duration = Duration::from_millis(1200);

/// A fixed sequence of numbers, so that a test picks the same things in every run.
struct Random(u64);

impl Random {
  fn below(&mut self, limit: usize) -> usize {
    self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
    usize::try_from(self.0 >> 33).unwrap() % limit
  }
}

/// One change at a random place. Many fail (the place is missing, or a file is in the way).
/// A write puts `serial` into the file: its own content.
fn random_change(w: &Disk, random: &mut Random, serial: usize) {
  let kind = random.below(10);
  let mut place = |least: usize| {
    let mut path = w.path(["r", "r", "r", "alone", "outside"][random.below(5)]);
    for _name in 0..least + random.below(4 - least) {
      path.push(["a", "b", "x"][random.below(3)]);
    }
    path
  };
  match kind {
    0..4 => {
      let file = place(1);
      let _ = fs::create_dir_all(file.parent().unwrap());
      let _ = fs::write(file, serial.to_string());
    }
    4 => drop(fs::create_dir_all(place(1))),
    5 | 6 => {
      let path = place(0);
      let _ = if path.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) };
    }
    _ => drop(fs::rename(place(0), place(0))),
  }
}

#[test]
fn an_editor_saves_a_file() {
  everywhere(|w| {
    w.write("src/a.txt", "0");
    w.watch(&["src"]);
    w.may_pass("src/a.txt.tmp");

    for round in 0..5 {
      w.write("src/a.txt.tmp", &format!("tmp {round}"));
      w.rename("src/a.txt.tmp", "src/a.txt");
      w.expect(&[(Update, "src/a.txt")]);
    }
  });
}

/// The path is empty between the rename and the write, which the delete delay hides. Polling
/// has none: a round that falls into that moment sees a `Delete` and a `Create` (behavior.md,
/// `with_delete_delay`), so the story skips it.
#[test]
fn an_editor_saves_a_file_with_a_backup() {
  everywhere_with(&Settings::delete_delay(Duration::from_millis(100)), |w| {
    w.write("src/a.txt", "0");
    w.watch(&["src"]);
    w.may_pass("src/a.txt~");

    for round in 0..5 {
      w.rename("src/a.txt", "src/a.txt~");
      w.write("src/a.txt", &format!("backup {round}"));
      w.remove("src/a.txt~");
      w.expect(&[(Update, "src/a.txt")]);
    }
  });
}

#[test]
fn mkdir_p_and_a_file_at_once() {
  // Every round waits for a quiet time, so it is short. With polling the pause grows with what
  // is watched: twice the rounds take four times as long.
  let settings = Settings::debounce(Duration::from_millis(5));
  on_disk_with(&settings, |w| {
    w.mkdir("r");
    w.watch(&["r"]);

    let mut files = Vec::new();
    for round in 0..50 {
      // Both files get their directories and their content with nothing in between.
      let in_dir = format!("r/{round}/a/b/f.txt");
      let alone = format!("alone/{round}/a/b/f.txt");
      w.watch(&[alone.as_str()]);
      w.write(&in_dir, "1");
      w.write(&alone, "1");
      files.extend([in_dir, alone]);
      w.wait(&each(Create, &files));
    }
    w.expect(&each(Create, &files));
  });
}

#[test]
fn a_big_tree_is_moved_in_and_out() {
  everywhere(|w| {
    let mut files = Vec::new();
    for (dir, sub, file) in
      (0..10).flat_map(|d| (0..10).flat_map(move |s| (0..20).map(move |f| (d, s, f))))
    {
      w.write(&format!("outside/tree/{dir}/{sub}/{file}.txt"), "1");
      files.push(format!("r/tree/{dir}/{sub}/{file}.txt"));
    }
    assert_eq!(files.len(), 2000);
    w.mkdir("r");
    w.watch(&["r"]);

    w.rename("outside/tree", "r/tree");
    w.expect(&each(Create, &files));
    w.rename("r/tree", "outside/tree");
    w.expect(&each(Delete, &files));
  });
}

/// The same directory comes back, with other content.
#[test]
fn a_directory_is_moved_away_changed_and_moved_back() {
  everywhere_with(&slow_delete(), |w| {
    w.write("r/sub/stays.txt", "1");
    w.write("r/sub/goes.txt", "1");
    w.write("r/sub/deep/changes.txt", "1");
    w.mkdir("outside");
    w.watch(&["r"]);
    // The files are old by then: a young one in a directory that was away is reported as
    // written (behavior.md, "Known limitations").
    w.expect(&[]);

    w.rename("r/sub", "outside/sub");
    w.remove("outside/sub/goes.txt");
    w.write("outside/sub/comes.txt", "1");
    w.write("outside/sub/deep/changes.txt", "22");
    w.rename("outside/sub", "r/sub");
    w.expect(&[
      (Delete, "r/sub/goes.txt"),
      (Create, "r/sub/comes.txt"),
      (Update, "r/sub/deep/changes.txt"),
    ]);
  });
}

/// A stream of macOS stands below the renamed directory and gets no record of the renames. The
/// guard of the renamed directory reports the rename at once, and the stream below is reported
/// lost even when the directory is back by then. The delete delay holds the files until the
/// directory is back (architecture.md 4.2).
#[test]
fn a_directory_above_a_watched_path_is_moved_away_changed_and_moved_back() {
  on_disk_with(&slow_delete(), |w| {
    w.write("r/x/a/changes.js", "1");
    w.write("r/x/a/goes.js", "1");
    // Windows refuses to rename a directory above an open directory handle.
    if cfg!(windows) && w.backend == "native" {
      skipped("Windows refuses to rename a directory above an open handle");
      return;
    }
    w.watch(&["r/x/a"]);

    w.rename("r/x", "r/away");
    thread::sleep(AWAY);
    w.write("r/away/a/changes.js", "22");
    w.remove("r/away/a/goes.js");
    w.write("r/away/a/comes.js", "1");
    w.rename("r/away", "r/x");
    w.expect(&[
      (Delete, "r/x/a/goes.js"),
      (Create, "r/x/a/comes.js"),
      (Update, "r/x/a/changes.js"),
    ]);
  });
}

/// The renamed directory is on the real path behind the link, where the watch of macOS stands.
#[test]
fn a_directory_above_the_target_of_a_link_is_moved_away_changed_and_moved_back() {
  on_disk_with(&slow_delete(), |w| {
    w.write("store/x/real/changes.js", "1");
    w.write("store/x/real/goes.js", "1");
    w.mkdir("r");
    // Windows refuses to rename a directory above an open directory handle, which the handle
    // opened through the link is.
    if cfg!(windows) && w.backend == "native" {
      skipped("Windows refuses to rename a directory above an open handle");
      return;
    }
    if !w.symlink("store/x/real", "r/link") {
      return;
    }
    w.watch(&["r/link"]);

    w.rename("store/x", "store/away");
    thread::sleep(AWAY);
    w.write("store/away/real/changes.js", "22");
    w.remove("store/away/real/goes.js");
    w.write("store/away/real/comes.js", "1");
    w.rename("store/away", "store/x");
    w.expect(&[
      (Delete, "r/link/goes.js"),
      (Create, "r/link/comes.js"),
      (Update, "r/link/changes.js"),
    ]);
  });
}

#[test]
#[expect(clippy::print_stderr, reason = "the test tells how long the batch took")]
fn a_big_batch_of_single_files() {
  on_disk(|w| {
    let mut files = Vec::new();
    for (dir, file) in (0..2000).flat_map(|d| (0..10).map(move |f| (d, f))) {
      let path = format!("{dir}/{file}.js");
      w.write(&path, "1");
      files.push(path);
    }
    assert_eq!(files.len(), 20_000);

    let paths: Vec<&str> = files.iter().map(String::as_str).collect();
    let start = Instant::now();
    w.watch(&paths);
    let took = start.elapsed();
    eprintln!("{}: watch of {} watched paths took {took:?}", w.backend, paths.len());
    // Far above the cost: the limit catches work that grows faster than the batch.
    assert!(took < Duration::from_secs(30), "{}: watch took {took:?}", w.backend);

    let mut random = Random(1);
    let mut picked = BTreeSet::new();
    while picked.len() < 200 {
      picked.insert(random.below(files.len()));
    }
    let written: Vec<String> = picked.into_iter().map(|index| files[index].clone()).collect();
    for path in &written {
      w.write(path, "22");
    }
    w.expect(&each(Update, &written));
  });
}

#[test]
fn node_modules_is_ignored() {
  everywhere_with(&Settings::ignore_node_modules(), |w| {
    w.write("proj/src/a.js", "1");
    w.write("proj/node_modules/dep/index.js", "1");
    w.watch(&["proj"]);

    for (package, file) in (0..50).flat_map(|p| (0..10).map(move |f| (p, f))) {
      w.write(&format!("proj/node_modules/pkg{package}/lib/{file}.js"), "1");
    }
    w.write("proj/node_modules/dep/other.js", "1");
    w.write("proj/node_modules/dep/index.js", "22");
    w.write("proj/src/a.js", "22");
    w.expect(&[(Update, "proj/src/a.js")]);

    w.remove("proj/node_modules");
    w.write("proj/src/b.js", "1");
    w.expect(&[(Create, "proj/src/b.js")]);
  });
}

/// Every change is reported once for each spelling, whichever one the writer used.
#[test]
fn two_spellings_of_one_directory() {
  everywhere(|w| {
    w.write("real/a.txt", "1");
    if !w.symlink("real", "link") {
      return;
    }
    w.watch(&["real", "link"]);

    w.write("real/b.txt", "1");
    w.expect(&[(Create, "real/b.txt"), (Create, "link/b.txt")]);
    w.write("link/a.txt", "22");
    w.expect(&[(Update, "real/a.txt"), (Update, "link/a.txt")]);
    w.remove("real/a.txt");
    w.expect(&[(Delete, "real/a.txt"), (Delete, "link/a.txt")]);
  });
}

/// On a disk that takes two spellings of a name for one, the events carry the spelling that
/// was given to `watch`.
#[test]
fn watched_paths_in_another_spelling() {
  everywhere_with(&Settings::loose(), |w| {
    w.write("src/app.js", "1");
    w.write("src/components/ui/button.js", "1");
    w.watch(&["src/App.js", "src/Components/UI/Button.js"]);

    w.write("src/app.js", "22");
    w.expect(&[(Update, "src/App.js")]);
    w.write("src/components/ui/button.js", "22");
    w.expect(&[(Update, "src/Components/UI/Button.js")]);
    w.remove("src/app.js");
    w.expect(&[(Delete, "src/App.js")]);
    w.write("src/APP.JS", "1");
    w.expect(&[(Create, "src/App.js")]);
  });
}

/// Other programs use the real path. Every temporary directory of macOS is like this: `/var`
/// is a link.
#[test]
fn watched_paths_below_a_symlinked_prefix() {
  everywhere(|w| {
    w.write("store/proj/src/a.js", "1");
    w.write("store/proj/config.json", "1");
    if !w.symlink("store", "prefix") {
      return;
    }
    w.watch(&["prefix/proj/src", "prefix/proj/config.json", "prefix/proj/later/x.js"]);

    w.write("store/proj/src/a.js", "22");
    w.write("store/proj/src/sub/b.js", "1");
    w.write("store/proj/config.json", "22");
    w.write("store/proj/later/x.js", "1");
    w.expect(&[
      (Update, "prefix/proj/src/a.js"),
      (Create, "prefix/proj/src/sub/b.js"),
      (Update, "prefix/proj/config.json"),
      (Create, "prefix/proj/later/x.js"),
    ]);

    // The link is replaced: the next look at its name, from a hint or the poll, finds its target.
    w.write("other/proj/src/c.js", "1");
    w.remove("prefix");
    assert!(w.symlink("other", "prefix"));
    w.expect(&[
      (Delete, "prefix/proj/src/a.js"),
      (Delete, "prefix/proj/src/sub/b.js"),
      (Delete, "prefix/proj/config.json"),
      (Delete, "prefix/proj/later/x.js"),
      (Create, "prefix/proj/src/c.js"),
    ]);
  });
}

/// Whatever the events are, each fits what the handler knows, and in the end it knows exactly
/// the files on the disk, with their content.
#[test]
fn random_changes_end_in_the_state_of_the_disk() {
  const CHANGES: usize = 150;

  on_disk(|w| {
    w.mkdir("r");
    w.mkdir("outside");
    let watched = ["r", "alone/a/x"];
    w.watch(&watched);

    let mut random = Random(2);
    let mut known = BTreeMap::new();
    for round in 0..5 {
      for change in 0..CHANGES {
        random_change(w, &mut random, round * CHANGES + change);
      }
      let mut on_disk = BTreeMap::new();
      for path in watched {
        files_at(&w.path(path), &mut on_disk);
      }
      w.expect_known(&mut known, &on_disk);
    }
  });
}
