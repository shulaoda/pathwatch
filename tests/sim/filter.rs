//! The ignore filter and the watched paths: what the filter is asked, what it costs, and what is
//! reported. A story compares the questions (`recording`), the calls and the events (`C`, `U`,
//! `D` and the path). The stories with `node_modules` use a filter that looks at the last name
//! only, which the contract of the filter does not allow: they pin what happens then.

use std::{
  cell::RefCell,
  path::{Path, PathBuf},
  sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
  },
};

use crate::fixtures::{NONE, RECURSIVE, Sim, asked, ignore_node_modules, on_all, on_modes, sorted};
use pathwatch::{EntryKind, engine::Options};

/// What a filter was asked, in the order of the calls.
type Asked = Arc<Mutex<Vec<(PathBuf, EntryKind)>>>;

/// The options with the filter `ignore`, and the list of what it was asked. The driver asks the
/// same filter when everything settles, so a story reads the list before that.
fn recording(
  ignore: impl Fn(&Path, EntryKind) -> bool + Send + Sync + 'static,
) -> (Options, Asked) {
  let asked = Asked::default();
  let list = Arc::clone(&asked);
  let ignored = Arc::new(move |path: &Path, kind: EntryKind| {
    list.lock().unwrap().push((path.to_path_buf(), kind));
    ignore(path, kind)
  });
  (Options { ignored: Some(ignored), ..Options::default() }, asked)
}

/// What the filter was asked since the last call of this.
fn take(asked: &Asked) -> Vec<(PathBuf, EntryKind)> {
  std::mem::take(&mut asked.lock().unwrap())
}

/// The events of the next `count` windows. Nothing is checked against the disk. When every
/// watched path was dropped, nothing is due any more, and there is no window.
fn windows(d: &mut dyn Sim, count: usize) -> Vec<String> {
  (0..count).flat_map(|_| if d.something_is_due() { d.window() } else { Vec::new() }).collect()
}

/// A directory with 1,000 files that the handler knows goes away. That is decided at the
/// directory: one `Delete` for each file, and the filter is not asked.
#[test]
fn a_directory_with_many_known_files_goes() {
  let (options, asked_filter) =
    recording(|path, kind| kind == EntryKind::Dir && path.ends_with("node_modules"));
  on_all(&options, |d, on| {
    d.fs().mkdir_all("/p/r/big").unwrap();
    for file in 0..1000 {
      d.fs().write(format!("/p/r/big/{file}.js"), "1").unwrap();
    }
    assert_eq!(d.watch(&["/p/r"]), NONE, "{on}");
    assert_eq!(d.known().len(), 1000, "{on}");
    take(&asked_filter);

    d.fs().remove_all("/p/r/big").unwrap();
    let events = windows(d, 4);
    assert_eq!(events.len(), 1000, "{on}");
    assert!(events.iter().all(|event| event.starts_with("D /p/r/big/")), "{on}");
    assert_eq!(take(&asked_filter), [], "{on}");
    d.caught_up();
  });
}

/// 10,000 hints from inside an ignored directory, below a watch for a whole tree, in one
/// flush: one question for the filter, and one look at the ignored directory. The watched
/// directory is looked at with it, because the directory above it is only polled. As many
/// hints from a place that no watched path covers, below the watch of a watched file, cost
/// nothing.
#[test]
fn many_hints_from_inside_an_ignored_directory() {
  let (options, asked_filter) =
    recording(|path, kind| kind == EntryKind::Dir && path.ends_with("node_modules"));
  on_modes(&RECURSIVE, &options, |d, on| {
    d.fs().mkdir_all("/p/node_modules/x").unwrap();
    d.fs().mkdir_all("/q/other").unwrap();
    d.fs().write("/q/f.js", "1").unwrap();
    assert_eq!(d.watch(&["/p", "/q/f.js"]), NONE, "{on}");
    take(&asked_filter);
    let before = d.fs().calls();
    for file in 0..10_000 {
      d.fs().write(format!("/p/node_modules/x/{file}.js"), "1").unwrap();
      d.fs().write(format!("/q/other/{file}.js"), "1").unwrap();
    }
    assert!(d.fs().pending_raw() >= 20_000, "{on}");
    assert_eq!(d.window(), NONE, "{on}");
    let after = d.fs().calls();
    let checks = (after.lstat - before.lstat, after.stat - before.stat);
    assert_eq!((checks, after.read_dir - before.read_dir), ((2, 0), 0), "{on}");
    assert_eq!(take(&asked_filter), [(PathBuf::from("/p/node_modules"), EntryKind::Dir)], "{on}");
    d.caught_up();
  });
}

/// (i): an ignored directory gets no node, no watch and no listing.
#[test]
fn case_i_an_ignored_directory() {
  on_all(&ignore_node_modules(), |d, on| {
    d.fs().mkdir_all("/p/node_modules/foo/lib").unwrap();
    d.fs().write("/p/node_modules/foo/index.js", "1").unwrap();
    d.fs().write("/p/node_modules/foo/lib/other.js", "1").unwrap();
    d.fs().write("/p/a.js", "1").unwrap();
    d.watch(&["/p"]);
    assert_eq!(d.known(), ["/p/a.js"], "{on}");
    // Only `/p` was listed.
    assert_eq!(d.fs().calls().read_dir, 1, "{on}");
    assert!(!asked(d.fs()).iter().any(|(dir, _)| dir.starts_with("/p/node_modules")), "{on}");
    d.fs().write("/p/node_modules/foo/index.js", "22").unwrap();
    assert_eq!(d.settled(), NONE, "{on}");

    // To a visible name: every file is created. Back: every known file is deleted.
    d.fs().rename("/p/node_modules", "/p/vendor").unwrap();
    let files = ["/p/vendor/foo/index.js", "/p/vendor/foo/lib/other.js"];
    assert_eq!(sorted(d.settled()), files.map(|file| format!("C {file}")), "{on}");
    d.fs().rename("/p/vendor", "/p/node_modules").unwrap();
    assert_eq!(sorted(d.settled()), files.map(|file| format!("D {file}")), "{on}");
    d.caught_up();
  });
}

/// The filter is asked about an entry when it is found, and again when its kind changes. Not at
/// every check: for an entry that the tree has, the answer is known. And not about an entry
/// that is not there.
#[test]
fn the_filter_is_asked_once_for_an_entry() {
  for listing_kinds in [true, false] {
    let asked = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&asked);
    let options = Options {
      ignored: Some(Arc::new(move |path: &Path, _kind| {
        count.fetch_add(1, Ordering::Relaxed);
        path.ends_with("skipped.js")
      })),
      ..Options::default()
    };
    // How often the filter was asked since the last call of this.
    let asked = || asked.swap(0, Ordering::Relaxed);
    on_all(&options, |d, on| {
      let on = format!("{on}, listing kinds: {listing_kinds}");
      d.fs().set_listing_kinds(listing_kinds);
      d.fs().mkdir_all("/p/r/sub").unwrap();
      for file in ["/p/r/a.js", "/p/r/b.js", "/p/r/skipped.js", "/p/r/sub/c.js"] {
        d.fs().write(file, "1").unwrap();
      }
      d.fs().write("/p/f.js", "1").unwrap();
      asked();
      // The two directories above the watched paths, each watched path as a file, `/p/r` as
      // a directory when it is found, and `sub` and the four files in `/p/r`.
      assert_eq!(d.watch(&["/p/r", "/p/f.js"]), NONE, "{on}");
      assert_eq!(asked(), 10, "{on}");

      // Everything is scanned again. Only the ignored file has no node, so each listing of
      // its directory asks about it. Nothing else is asked.
      d.overflow();
      assert_eq!(d.window(), NONE, "{on}");
      assert_eq!(asked(), 1, "{on}");
      // With the poll backend, the window below is one poll, which lists the directory.
      let listing = usize::from(on.starts_with("Poll"));

      // Two known files change and a new one comes: one question, for the new one.
      d.fs().write("/p/r/a.js", "22").unwrap();
      d.fs().write("/p/f.js", "22").unwrap();
      d.fs().write("/p/r/sub/new.js", "1").unwrap();
      assert_eq!(d.window(), ["U /p/f.js", "U /p/r/a.js", "C /p/r/sub/new.js"], "{on}");
      assert_eq!(asked(), listing + 1, "{on}");

      // A file becomes a directory: another kind, another question.
      d.fs().remove("/p/r/b.js").unwrap();
      d.fs().mkdir("/p/r/b.js").unwrap();
      assert_eq!(d.window(), ["D /p/r/b.js"], "{on}");
      assert_eq!(asked(), listing + 1, "{on}");
      d.settled();

      // `unwatch` of a path with a missing directory above it asks nothing.
      assert_eq!(d.watch(&["/p/r/gone/d.js"]), NONE, "{on}");
      asked();
      assert_eq!(d.unwatch(&["/p/r/gone/d.js"]), NONE, "{on}");
      assert_eq!(asked(), 0, "{on}");
      d.caught_up();
    });
  }
}

/// The filter of a build tool: the output directory and everything below it, for both kinds.
fn ignore_out() -> Options {
  Options {
    ignored: Some(Arc::new(|path: &Path, _kind| path.starts_with("/p/out"))),
    ..Options::default()
  }
}

/// A watched file and a watched directory that the filter ignores for both kinds are no
/// watched paths. No event ever comes for them, and they cost nothing: the engine makes the
/// same calls to the disk and to the backend as when they were never given to `watch`.
#[test]
fn a_path_that_is_ignored_for_both_kinds_costs_nothing() {
  let story = |paths: &'static [&'static str]| {
    let calls = RefCell::new(Vec::new());
    on_all(&ignore_out(), |d, on| {
      d.fs().mkdir_all("/p/out/sub").unwrap();
      d.fs().mkdir("/p/src").unwrap();
      for file in ["/p/out/gen.js", "/p/out/sub/x.js", "/p/src/a.js"] {
        d.fs().write(file, "1").unwrap();
      }
      assert_eq!(d.watch(paths), NONE, "{on}");
      assert_eq!(d.known(), ["/p/src/a.js"], "{on}");
      // No watch at or below the ignored directory.
      let below_out = |(dir, _): &(String, _)| dir.starts_with("/p/out");
      assert!(!asked(d.fs()).iter().any(below_out), "{on}: {:?}", asked(d.fs()));

      // Everything happens to the two paths, and many polls run.
      d.fs().write("/p/out/gen.js", "22").unwrap();
      d.fs().write("/p/out/sub/x.js", "22").unwrap();
      assert_eq!(d.settled(), NONE, "{on}");
      d.fs().remove("/p/out/gen.js").unwrap();
      d.fs().remove_all("/p/out/sub").unwrap();
      assert_eq!(d.settled(), NONE, "{on}");
      d.fs().mkdir("/p/out/gen.js").unwrap();
      d.fs().write("/p/out/sub", "a file now").unwrap();
      assert_eq!(d.settled(), NONE, "{on}");
      // Taking one back does nothing.
      assert_eq!(d.unwatch(&["/p/out/gen.js", "/p/out/sub"]), NONE, "{on}");
      d.fs().write("/p/src/a.js", "22").unwrap();
      assert_eq!(d.settled(), ["U /p/src/a.js"], "{on}");
      d.caught_up();
      calls.borrow_mut().push((on.to_owned(), d.fs().calls()));
    });
    calls.into_inner()
  };
  let with = story(&["/p/out/gen.js", "/p/out/sub", "/p/src/a.js"]);
  let without = story(&["/p/src/a.js"]);
  assert_eq!(with, without);
}

/// A filter that ignores only directories named `dist`. The watched path `/p/dist` is ignored
/// for the kind it has, so it waits like a missing path: nothing in it is read. A file takes
/// its place and is reported. A directory again: the file is gone.
#[test]
fn a_path_that_is_ignored_for_the_kind_it_has_now() {
  let options = Options {
    ignored: Some(Arc::new(|path: &Path, kind| kind == EntryKind::Dir && path.ends_with("dist"))),
    ..Options::default()
  };
  on_all(&options, |d, on| {
    d.fs().mkdir_all("/p/dist").unwrap();
    d.fs().write("/p/dist/a.js", "1").unwrap();
    d.fs().write("/p/dist/b.js", "1").unwrap();
    d.watch(&["/p/dist"]);
    assert_eq!(d.known(), NONE, "{on}");
    d.fs().write("/p/dist/a.js", "22").unwrap();
    d.fs().write("/p/dist/new.js", "1").unwrap();
    d.settled();
    // The directory was never listed, and the backend was never asked to watch it.
    assert_eq!(d.fs().calls().read_dir, 0, "{on}");
    assert!(!asked(d.fs()).iter().any(|(dir, _)| dir == "/p/dist"), "{on}");

    d.fs().remove_all("/p/dist").unwrap();
    d.fs().write("/p/dist", "a file now").unwrap();
    assert_eq!(d.settled(), ["C /p/dist"], "{on}");
    d.fs().write("/p/dist", "written").unwrap();
    assert_eq!(d.settled(), ["U /p/dist"], "{on}");

    d.fs().remove("/p/dist").unwrap();
    d.fs().mkdir("/p/dist").unwrap();
    d.fs().write("/p/dist/a.js", "1").unwrap();
    d.settled();
    assert_eq!(d.fs().calls().read_dir, 0, "{on}");
    d.caught_up();
  });
}

/// A filter that looks at the last name only, and watched paths below a directory named
/// `node_modules`: one level down, two levels down, and one with nothing on disk below the
/// ignored directory. `watch` asks about the directories above each path, from the top down
/// to the ignored one, and takes none of the paths: no look, no watch, nothing due, and
/// nothing is ever reported for them, whatever happens there. `unwatch` of them does nothing.
#[test]
fn a_watched_path_below_an_ignored_directory_is_never_watched() {
  let (options, asked_filter) =
    recording(|path, kind| kind == EntryKind::Dir && path.ends_with("node_modules"));
  on_all(&options, |d, on| {
    d.fs().mkdir_all("/p/node_modules/pkg").unwrap();
    d.fs().write("/p/node_modules/x.js", "1").unwrap();
    d.fs().write("/p/node_modules/pkg/deep.js", "1").unwrap();
    take(&asked_filter);
    let paths =
      ["/p/node_modules/x.js", "/p/node_modules/pkg/deep.js", "/p/node_modules/gone/later.js"];
    d.watch(&paths);
    let way = |dir: &str| (PathBuf::from(dir), EntryKind::Dir);
    let down_to_the_ignored_one = [way("/"), way("/p"), way("/p/node_modules")];
    let for_each_path: Vec<_> = (0..3).flat_map(|_| down_to_the_ignored_one.clone()).collect();
    assert_eq!(take(&asked_filter), for_each_path, "{on}");
    assert_eq!(d.known(), NONE, "{on}");
    assert_eq!(asked(d.fs()), [], "{on}");
    assert!(!d.something_is_due(), "{on}");

    // Everything happens at the paths, and the ignored directory goes away and comes back.
    d.fs().write("/p/node_modules/x.js", "22").unwrap();
    d.fs().mkdir("/p/node_modules/gone").unwrap();
    d.fs().write("/p/node_modules/gone/later.js", "1").unwrap();
    d.fs().remove("/p/node_modules/pkg/deep.js").unwrap();
    assert_eq!(d.settled(), NONE, "{on}");
    d.fs().rename("/p/node_modules", "/p/away").unwrap();
    d.fs().rename("/p/away", "/p/node_modules").unwrap();
    assert_eq!(d.settled(), NONE, "{on}");
    take(&asked_filter);
    d.unwatch(&paths);
    assert_eq!(take(&asked_filter), [], "{on}");

    // A path beside the ignored directory is watched as usual.
    d.fs().write("/p/a.js", "1").unwrap();
    assert_eq!(d.watch(&["/p/a.js"]), NONE, "{on}");
    d.fs().write("/p/a.js", "22").unwrap();
    assert_eq!(d.settled(), ["U /p/a.js"], "{on}");
    d.caught_up();
  });
}
