//! Cases of the design review that fit no other file: a file and its directory as two watched
//! paths, a chain of directories that appears, and a name that is a file and then a directory.
//! A story makes the disk, calls `watch`, changes the disk and compares the events that come
//! out (`C`, `U`, `D` and the path).

use crate::fixtures::{
  Driver, Mode, NONE, SimBackend, caught_up, flush, ignore_node_modules, on_all, settled, sorted,
  taken,
};
use pathwatch::engine::Options;

fn driver(mode: Mode) -> Driver<SimBackend> {
  Driver::sim(mode, Options::default())
}

/// (a): a watched file and its directory as a watched path, in both orders of `watch`.
#[test]
fn case_a_a_file_and_its_directory() {
  on_all(&Options::default(), |d, on| {
    for (base, first, second) in
      [("/1", "/1/p/src/a.js", "/1/p/src"), ("/2", "/2/p/src", "/2/p/src/a.js")]
    {
      d.fs().mkdir_all(format!("{base}/p/src")).unwrap();
      d.fs().write(format!("{base}/p/src/a.js"), "1").unwrap();
      d.fs().write(format!("{base}/p/src/b.js"), "1").unwrap();
      assert_eq!(d.watch(&[first]), NONE, "{on}");
      assert_eq!(d.watch(&[second]), NONE, "{on}");
      // The file has one home: one event for one change.
      d.fs().write(format!("{base}/p/src/a.js"), "22").unwrap();
      d.fs().write(format!("{base}/p/src/b.js"), "22").unwrap();
      assert_eq!(
        d.window(),
        [format!("U {base}/p/src/a.js"), format!("U {base}/p/src/b.js")],
        "{on}"
      );
      assert_eq!(d.settled(), NONE, "{on}");
    }
    d.caught_up();
  });
}

/// `complete` goes down to nodes that already exist, and a directory on the way to a
/// watched path that appears inside a watched directory is a full part of it.
#[test]
fn complete_is_a_rule() {
  on_all(&Options::default(), |d, on| {
    // (a) The directory above a watched file becomes a watched path.
    d.fs().mkdir_all("/p/R/lib/sub").unwrap();
    d.fs().write("/p/R/lib/a.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/R/lib/a.js"]), NONE, "{on}");
    assert_eq!(d.watch(&["/p/R"]), NONE, "{on}");
    d.fs().write("/p/R/lib/b.js", "1").unwrap();
    d.fs().write("/p/R/lib/sub/c.js", "1").unwrap();
    assert_eq!(d.settled(), ["C /p/R/lib/b.js", "C /p/R/lib/sub/c.js"], "{on}");

    // (b) A missing directory on the way to a watched path.
    d.fs().mkdir_all("/q/R").unwrap();
    assert_eq!(d.watch(&["/q/R", "/q/R/gen/x.js"]), NONE, "{on}");
    d.fs().mkdir("/q/R/gen").unwrap();
    d.fs().write("/q/R/gen/x.js", "1").unwrap();
    d.fs().write("/q/R/gen/y.js", "1").unwrap();
    assert_eq!(d.settled(), ["C /q/R/gen/x.js", "C /q/R/gen/y.js"], "{on}");
    d.caught_up();
  });
}

/// The upgrade to "complete" reaches the nodes one level deeper.
#[test]
fn the_upgrade_goes_all_the_way_down() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/src/deep").unwrap();
    d.fs().write("/p/src/a.js", "1").unwrap();
    d.fs().write("/p/src/c.js", "1").unwrap();
    d.fs().write("/p/src/deep/d.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/src/a.js"]), NONE, "{on}");
    assert_eq!(d.watch(&["/p"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/src/a.js", "/p/src/c.js", "/p/src/deep/d.js"], "{on}");

    d.fs().write("/p/src/b.js", "1").unwrap();
    d.fs().write("/p/src/c.js", "22").unwrap();
    d.fs().write("/p/src/deep/d.js", "22").unwrap();
    d.fs().write("/p/src/deep/e.js", "1").unwrap();
    assert_eq!(
      d.window(),
      ["C /p/src/b.js", "U /p/src/c.js", "U /p/src/deep/d.js", "C /p/src/deep/e.js"],
      "{on}"
    );
    d.caught_up();
  });
}

/// A whole chain appears at once. One flush goes down all of it, with as many rounds of
/// "watch, commit, scan" as it needs.
#[test]
fn a_chain_that_appears_needs_many_rounds() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir("/p").unwrap();
    assert_eq!(d.watch(&["/p/a/b/c/x.js"]), NONE, "{on}");
    d.fs().mkdir_all("/p/a/b/c").unwrap();
    d.fs().write("/p/a/b/c/x.js", "1").unwrap();
    assert_eq!(d.window(), ["C /p/a/b/c/x.js"], "{on}");
    d.fs().write("/p/a/b/c/x.js", "22").unwrap();
    assert_eq!(d.window(), ["U /p/a/b/c/x.js"], "{on}");
    assert_eq!(d.settled(), NONE, "{on}");
    d.caught_up();
  });
}

/// (f): the ancestor two levels up is missing, is created, removed, and created again with
/// the whole chain.
#[test]
fn case_f_a_chain_that_comes_and_goes() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir("/p").unwrap();
    assert_eq!(d.watch(&["/p/a/b/c/R.js"]), NONE, "{on}");
    // All of it in one window.
    d.fs().mkdir("/p/a").unwrap();
    d.fs().remove("/p/a").unwrap();
    d.fs().mkdir_all("/p/a/b/c").unwrap();
    d.fs().write("/p/a/b/c/R.js", "1").unwrap();
    assert_eq!(d.window(), ["C /p/a/b/c/R.js"], "{on}");
    assert_eq!(d.settled(), NONE, "{on}");

    // And one change at a time.
    d.fs().remove_all("/p/a").unwrap();
    assert_eq!(d.settled(), ["D /p/a/b/c/R.js"], "{on}");
    d.fs().mkdir_all("/p/a/b").unwrap();
    assert_eq!(d.settled(), NONE, "{on}");
    d.fs().remove_all("/p/a").unwrap();
    assert_eq!(d.settled(), NONE, "{on}");
    d.fs().mkdir_all("/p/a/b/c").unwrap();
    d.fs().write("/p/a/b/c/R.js", "1").unwrap();
    assert_eq!(d.settled(), ["C /p/a/b/c/R.js"], "{on}");
    d.caught_up();
  });
}

/// A new directory is watched before it is listed, so a file is found by the listing or by a
/// notification, and reported once.
#[test]
fn a_file_in_a_new_directory_is_reported_once() {
  for mode in Mode::ALL {
    let mut d = driver(mode);
    d.fs.mkdir_all("/p/r").unwrap();
    d.watch(&["/p/r"]);

    d.fs.mkdir("/p/r/sub").unwrap();
    flush(&mut d);
    // The directory is known and watched now. The file comes after the scan.
    d.fs.write("/p/r/sub/f", "1").unwrap();
    d.fs.mkdir("/p/r/sub/deeper").unwrap();
    d.fs.write("/p/r/sub/deeper/g", "1").unwrap();
    flush(&mut d);
    assert_eq!(taken(&mut d), ["C /p/r/sub/deeper/g", "C /p/r/sub/f"], "{mode:?}");
    assert_eq!(settled(&mut d), NONE, "{mode:?}");
    caught_up(&mut d);
  }
}

/// INV: a name can be a known file and have a node without identity. When the file goes, its
/// `Delete` comes.
#[test]
fn inv_a_file_whose_name_has_a_node() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir("/p").unwrap();
    d.fs().write("/p/a", "a file").unwrap();
    assert_eq!(d.watch(&["/p"]), NONE, "{on}");
    assert_eq!(d.watch(&["/p/a/b.js"]), NONE, "{on}");
    d.fs().write("/p/a", "written").unwrap();
    assert_eq!(d.settled(), ["U /p/a"], "{on}");

    d.fs().remove("/p/a").unwrap();
    assert_eq!(d.settled(), ["D /p/a"], "{on}");
    d.fs().mkdir("/p/a").unwrap();
    d.fs().write("/p/a/b.js", "1").unwrap();
    assert_eq!(d.settled(), ["C /p/a/b.js"], "{on}");
    d.caught_up();
  });
}

/// The rows that the first table did not have.
#[test]
fn the_table_is_closed() {
  on_all(&ignore_node_modules(), |d, on| {
    // 1. An ignored directory appears: it is not entered.
    d.fs().mkdir("/p").unwrap();
    assert_eq!(d.watch(&["/p"]), NONE, "{on}");
    let before = d.fs().calls();
    d.fs().mkdir("/p/node_modules").unwrap();
    d.fs().write("/p/node_modules/x.js", "1").unwrap();
    assert_eq!(d.settled(), NONE, "{on}");
    let after = d.fs().calls();
    assert_eq!(after.watch, before.watch, "{on}");

    // 2. A watched directory with a watched file in it turns into a file: the name is a known
    // file and keeps a node without identity. Both keep working.
    d.fs().mkdir_all("/q/d").unwrap();
    d.fs().write("/q/d/a.js", "1").unwrap();
    assert_eq!(d.watch(&["/q/d", "/q/d/a.js"]), NONE, "{on}");
    d.fs().remove_all("/q/d").unwrap();
    d.fs().write("/q/d", "a file").unwrap();
    assert_eq!(d.window(), ["C /q/d", "D /q/d/a.js"], "{on}");
    d.fs().write("/q/d", "written").unwrap();
    assert_eq!(d.settled(), ["U /q/d"], "{on}");
    d.fs().remove("/q/d").unwrap();
    d.fs().mkdir("/q/d").unwrap();
    d.fs().write("/q/d/a.js", "1").unwrap();
    d.fs().write("/q/d/z.js", "1").unwrap();
    assert_eq!(d.window(), ["D /q/d", "C /q/d/a.js", "C /q/d/z.js"], "{on}");

    // 3. A chain that never existed goes through `check` and `vanish` without a watch: the
    // backend is never asked to forget one.
    d.fs().mkdir("/s").unwrap();
    let before = d.fs().calls();
    assert_eq!(d.watch(&["/s/a/b/c/x.js"]), NONE, "{on}");
    assert_eq!(d.settled(), NONE, "{on}");
    assert_eq!(d.fs().calls().unwatch, before.unwatch, "{on}");

    // 4. A directory that appears at a missing watched path is complete.
    assert_eq!(d.watch(&["/s/content"]), NONE, "{on}");
    d.fs().mkdir_all("/s/content/deep").unwrap();
    d.fs().write("/s/content/f", "1").unwrap();
    d.fs().write("/s/content/deep/g", "1").unwrap();
    assert_eq!(d.settled(), ["C /s/content/deep/g", "C /s/content/f"], "{on}");
    d.caught_up();
  });
}

/// A watched path below a file: `/p/r/f` is a file in a watched directory, `/q/a` is a watched
/// file, and a path below each of them is watched too. So the node of a file has a child. The
/// file is reported like every other file. When a directory takes its place, the watched path
/// below it is found, and when a file comes back, it is gone again.
#[test]
fn a_watched_path_below_a_file() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir_all("/p/r").unwrap();
    d.fs().write("/p/r/f", "1").unwrap();
    d.fs().mkdir("/q").unwrap();
    d.fs().write("/q/a", "1").unwrap();
    assert_eq!(d.watch(&["/p/r", "/p/r/f/x", "/q/a", "/q/a/b"]), NONE, "{on}");
    d.fs().write("/p/r/f", "22").unwrap();
    d.fs().write("/q/a", "22").unwrap();
    assert_eq!(d.settled(), ["U /p/r/f", "U /q/a"], "{on}");

    for path in ["/p/r/f", "/q/a"] {
      d.fs().remove(path).unwrap();
      d.fs().mkdir(path).unwrap();
    }
    for file in ["/p/r/f/x", "/p/r/f/y", "/q/a/b", "/q/a/c"] {
      d.fs().write(file, "1").unwrap();
    }
    assert_eq!(
      sorted(d.settled()),
      ["C /p/r/f/x", "C /p/r/f/y", "C /q/a/b", "C /q/a/c", "D /p/r/f", "D /q/a"],
      "{on}"
    );

    for path in ["/p/r/f", "/q/a"] {
      d.fs().remove_all(path).unwrap();
      d.fs().write(path, "1").unwrap();
    }
    assert_eq!(
      sorted(d.settled()),
      ["C /p/r/f", "C /q/a", "D /p/r/f/x", "D /p/r/f/y", "D /q/a/b", "D /q/a/c"],
      "{on}"
    );
    assert_eq!(d.known(), ["/p/r/f", "/q/a"], "{on}");
    d.caught_up();
  });
}
