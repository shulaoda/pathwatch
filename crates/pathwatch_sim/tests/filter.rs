//! The ignore filter and the watched paths: what the filter is asked, what it costs, and what is
//! reported. A story compares the questions (`recording`), the calls and the events (`C`, `U`,
//! `D` and the path). The stories with `node_modules` use a filter that looks at the last name
//! only, which the contract of the filter does not allow: they pin what happens then.

#![cfg(unix)]

use std::{
  cell::RefCell,
  path::{Path, PathBuf},
  sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
  },
};

use pathwatch::{
  EntryKind,
  backend::Want::{Contents, Names},
  engine::Options,
  poll::PollBackend,
};
use pathwatch_sim::{
  Driver, Mode, NONE, RECURSIVE, Sim, asked, at, caught_up, flush, ignore_node_modules, on_all,
  on_modes, os_watches, settled, sorted, taken,
};

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

/// The `watch` calls to the backend that asked for the contents of a directory.
fn asked_for_contents(d: &dyn Sim) -> Vec<String> {
  let calls = asked(d.fs()).into_iter().filter(|(_, want)| *want == Some(Contents));
  calls.map(|(dir, _)| dir).collect()
}

/// How often the engine made each call to the disk and to the backend.
fn all_calls(d: &dyn Sim) -> [u64; 9] {
  let c = d.fs().calls();
  [c.lstat, c.stat, c.read_dir, c.hash, c.link_target, c.real_path, c.watch, c.unwatch, c.commit]
}

type Make = Box<dyn Fn() -> Box<dyn Sim>>;

/// One way to make a driver for each backend, so that a story can be told many times.
fn makes(options: &Options) -> Vec<(String, Make)> {
  let sim = |mode: Mode| -> (String, Make) {
    let options = options.clone();
    (format!("{mode:?}"), Box::new(move || Box::new(Driver::sim(mode, options.clone()))))
  };
  let options = options.clone();
  let poll: Make = Box::new(move || Box::new(Driver::<PollBackend>::poll(options.clone())));
  Mode::ALL.into_iter().map(sim).chain([("Poll".to_owned(), poll)]).collect()
}

/// Tells a story once for each call that the engine makes in `act` and in the four windows
/// after it, on every backend, and runs `change` on the disk right before that call. Then
/// everything settles, and `end` checks how the telling ends.
fn at_every_moment(
  options: &Options,
  prepare: fn(&mut dyn Sim),
  act: fn(&mut dyn Sim),
  change: fn(&pathwatch_sim::SimFs),
  end: fn(&mut dyn Sim, &str),
) {
  for (on, make) in makes(options) {
    let mut d = make();
    prepare(&mut *d);
    let before = d.fs().calls().total();
    act(&mut *d);
    windows(&mut *d, 4);
    let calls = d.fs().calls().total() - before;
    d.caught_up();
    assert!(calls >= 2, "{on}");

    for moment in 0..calls {
      let on = format!("{on}, call {moment}");
      let mut d = make();
      prepare(&mut *d);
      d.fs().at_call(moment, change);
      act(&mut *d);
      windows(&mut *d, 4);
      // A change that came early can save a later call: then the story is over.
      if d.fs().traps_run() == 0 {
        d.fs().clear_traps();
        d.caught_up();
        continue;
      }
      d.settled();
      end(&mut *d, &on);
      d.caught_up();
    }
  }
}

/// The filter of a build tool: the output directory and everything below it, for both kinds.
fn ignore_out() -> Options {
  Options {
    ignored: Some(Arc::new(|path: &Path, _kind| path.starts_with("/p/out"))),
    ..Options::default()
  }
}

/// The filter of a build tool that is closed: the directory `/p/out` and everything below it.
/// A file named `/p/out` is not ignored.
fn ignore_out_as_a_directory() -> Options {
  let ignored = |path: &Path, kind| {
    path.starts_with("/p/out") && (kind == EntryKind::Dir || path != Path::new("/p/out"))
  };
  Options { ignored: Some(Arc::new(ignored)), ..Options::default() }
}

/// A watched file and a watched directory that the filter ignores for both kinds are no
/// watched paths. No event ever comes for them, and they cost nothing: the engine makes the
/// same calls to the disk and to the backend as when they were never given to `watch`.
#[test]
fn a_path_that_is_ignored_for_both_kinds_costs_nothing() {
  let story = |paths: &'static [&'static str]| {
    let calls = RefCell::new(Vec::<(String, [u64; 9])>::new());
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
      calls.borrow_mut().push((on.to_owned(), all_calls(d)));
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
    assert_eq!(d.watch(&["/p/dist"]), NONE, "{on}");
    assert_eq!(d.known(), NONE, "{on}");
    d.fs().write("/p/dist/a.js", "22").unwrap();
    d.fs().write("/p/dist/new.js", "1").unwrap();
    assert_eq!(d.settled(), NONE, "{on}");
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
    assert_eq!(d.settled(), ["D /p/dist"], "{on}");
    assert_eq!(d.fs().calls().read_dir, 0, "{on}");
    d.caught_up();
  });
}

/// A filter that looks at the last name only. The directory that holds a watched file is the
/// watch target of that file. The filter ignores it, so it is not asked for its contents, and
/// the file is dropped: nothing is mounted for it any more, also not the `Names` asks that
/// only it needed.
///
/// And the file is not looked at again, whatever happens to it: watching fifty such files
/// costs as many looks, after the first one, as watching one. That is none.
#[test]
fn a_watched_file_whose_directory_is_ignored() {
  let story = |files: usize| {
    let looks = RefCell::new(Vec::<(String, u64)>::new());
    on_all(&ignore_node_modules(), |d, on| {
      d.fs().mkdir_all("/p/node_modules").unwrap();
      let paths: Vec<String> = (0..50).map(|file| format!("/p/node_modules/{file}.js")).collect();
      for path in &paths {
        d.fs().write(path, "1").unwrap();
      }
      let watched: Vec<&str> = paths.iter().take(files).map(String::as_str).collect();
      assert_eq!(d.watch(&watched), NONE, "{on}");
      assert_eq!(d.known(), NONE, "{on}");
      // The directory leads to the files, so it tells names. It is never asked for more, and
      // every watch is given back when the files are dropped.
      if on != "Poll" {
        let calls = [
          at("/", Some(Names)),
          at("/p", Some(Names)),
          at("/p/node_modules", Some(Names)),
          at("/p/node_modules", None),
          at("/p", None),
          at("/", None),
        ];
        assert_eq!(asked(d.fs()), calls, "{on}");
      }
      assert_eq!(os_watches(d.fs()), [], "{on}");

      let before = d.fs().calls().lstat;
      d.fs().write("/p/node_modules/0.js", "22").unwrap();
      assert_eq!(d.settled(), NONE, "{on}");
      d.fs().remove("/p/node_modules/0.js").unwrap();
      assert_eq!(d.settled(), NONE, "{on}");
      d.fs().write("/p/node_modules/0.js", "333").unwrap();
      assert_eq!(d.settled(), NONE, "{on}");
      assert_eq!(asked_for_contents(d), NONE, "{on}");
      assert_eq!(d.fs().calls().lstat, before, "{on}");
      looks.borrow_mut().push((on.to_owned(), d.fs().calls().lstat - before));
      // Taking the paths back does nothing: they are no watched paths any more.
      let calls = all_calls(d);
      assert_eq!(d.unwatch(&watched), NONE, "{on}");
      assert_eq!(all_calls(d), calls, "{on}");
      d.caught_up();
    });
    looks.into_inner()
  };
  assert_eq!(story(50), story(1));
}

/// A filter that looks at the last name only, and a watched file two levels below an ignored
/// directory. The directory that holds the file passes the filter, and that is the only one
/// that is asked: nothing walks up. The file is reported.
#[test]
fn a_watched_file_further_below_an_ignored_directory() {
  let (options, asked_filter) =
    recording(|path, kind| kind == EntryKind::Dir && path.ends_with("node_modules"));
  on_all(&options, |d, on| {
    d.fs().mkdir_all("/p/node_modules/pkg").unwrap();
    d.fs().write("/p/node_modules/pkg/index.js", "1").unwrap();
    take(&asked_filter);
    assert_eq!(d.watch(&["/p/node_modules/pkg/index.js"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/node_modules/pkg/index.js"], "{on}");
    // The path as a file, and its watch target.
    let file = (PathBuf::from("/p/node_modules/pkg/index.js"), EntryKind::File);
    let target = (PathBuf::from("/p/node_modules/pkg"), EntryKind::Dir);
    assert_eq!(take(&asked_filter), [file, target], "{on}");
    assert_eq!(
      asked_for_contents(d),
      if on == "Poll" { vec![] } else { vec!["/p/node_modules/pkg"] }
    );

    d.fs().write("/p/node_modules/pkg/index.js", "22").unwrap();
    assert_eq!(d.settled(), ["U /p/node_modules/pkg/index.js"], "{on}");
    d.caught_up();
  });
}

/// A watched path with a chain of missing directories above it. The lowest directory that
/// exists is its watch target, and the filter ignores that one: the path is dropped, and
/// nothing is mounted for it. No directory is asked for its contents, and nothing is looked at
/// when the chain appears inside the ignored directory.
///
/// Nothing wakes the path up: `unwatch` of it does nothing, and it stays dropped when the
/// ignored directory itself is found anew. `watch` of the path looks again, and now the
/// directory that holds the file is the watch target.
#[test]
fn a_missing_path_below_an_ignored_directory() {
  on_all(&ignore_node_modules(), |d, on| {
    d.fs().mkdir_all("/p/node_modules").unwrap();
    assert_eq!(d.watch(&["/p/node_modules/pkg/index.js"]), NONE, "{on}");
    assert_eq!(asked_for_contents(d), NONE, "{on}");
    assert_eq!(os_watches(d.fs()), [], "{on}");
    // Every call to the disk and to the backend but the commits: a poll ends with one.
    let calls = |d: &dyn Sim| all_calls(d)[..8].to_vec();
    let before = calls(d);

    d.fs().mkdir("/p/node_modules/pkg").unwrap();
    d.fs().write("/p/node_modules/pkg/index.js", "1").unwrap();
    // No hint comes and no poll: nothing is watched.
    assert!(!d.something_is_due(), "{on}");
    // Taking the path back does nothing.
    assert_eq!(d.unwatch(&["/p/node_modules/pkg/index.js"]), NONE, "{on}");
    assert_eq!(calls(d), before, "{on}");

    // The ignored directory goes away and comes back, with all that is in it.
    d.fs().rename("/p/node_modules", "/p/away").unwrap();
    assert_eq!(d.settled(), NONE, "{on}");
    d.fs().rename("/p/away", "/p/node_modules").unwrap();
    assert_eq!(d.settled(), NONE, "{on}");
    assert_eq!(calls(d), before, "{on}");

    // `watch` again looks again: the directory `pkg` is there now.
    assert_eq!(d.watch(&["/p/node_modules/pkg/index.js"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/node_modules/pkg/index.js"], "{on}");
    d.fs().write("/p/node_modules/pkg/index.js", "22").unwrap();
    assert_eq!(d.settled(), ["U /p/node_modules/pkg/index.js"], "{on}");
    d.caught_up();
  });
}

/// A chain of missing directories appears step by step. Each new directory becomes the watch
/// target in turn, and the filter is asked about it when it appears. The second one is
/// ignored: the chain stops there. The path is dropped, and what comes below is not seen,
/// also not when another directory takes the place of the ignored one.
#[test]
fn a_chain_stops_at_an_ignored_directory() {
  let (options, asked_filter) =
    recording(|path, kind| kind == EntryKind::Dir && path.ends_with("node_modules"));
  on_all(&options, |d, on| {
    let dirs = |asked: Vec<(PathBuf, EntryKind)>| -> Vec<String> {
      let dirs = asked.into_iter().filter(|(_, kind)| *kind == EntryKind::Dir);
      dirs.map(|(path, _)| path.display().to_string()).collect()
    };
    d.fs().mkdir("/p").unwrap();
    take(&asked_filter);
    assert_eq!(d.watch(&["/p/a/node_modules/pkg/index.js"]), NONE, "{on}");
    assert_eq!(dirs(take(&asked_filter)), ["/p"], "{on}");

    d.fs().mkdir("/p/a").unwrap();
    assert_eq!(windows(d, 3), NONE, "{on}");
    assert_eq!(dirs(take(&asked_filter)), ["/p/a"], "{on}");

    d.fs().mkdir("/p/a/node_modules").unwrap();
    assert_eq!(windows(d, 3), NONE, "{on}");
    assert_eq!(dirs(take(&asked_filter)), ["/p/a/node_modules"], "{on}");
    // Where one watch covers a tree, the watch on `/p` covers `/p/a` too.
    let contents = match on {
      "Inode" => vec!["/p", "/p/a"],
      "Poll" => vec![],
      _ => vec!["/p"],
    };
    assert_eq!(asked_for_contents(d), contents, "{on}");
    // Dropped: every watch on the way was given back.
    assert_eq!(os_watches(d.fs()), [], "{on}");

    // The rest of the chain appears: nothing is asked, and nothing is reported. No hint comes
    // and no poll.
    d.fs().mkdir("/p/a/node_modules/pkg").unwrap();
    d.fs().write("/p/a/node_modules/pkg/index.js", "1").unwrap();
    assert!(!d.something_is_due(), "{on}");
    assert_eq!(take(&asked_filter), [], "{on}");
    assert_eq!(asked_for_contents(d), contents, "{on}");

    // Taking the path back does nothing, and asks nothing.
    let calls = all_calls(d);
    assert_eq!(d.unwatch(&["/p/a/node_modules/pkg/index.js"]), NONE, "{on}");
    assert_eq!(all_calls(d), calls, "{on}");

    // Another directory takes the place of the ignored one, with the rest of the chain in it:
    // nothing wakes the path up.
    d.fs().rename("/p/a/node_modules", "/p/a/old").unwrap();
    d.fs().mkdir_all("/p/a/node_modules/pkg").unwrap();
    d.fs().write("/p/a/node_modules/pkg/index.js", "22").unwrap();
    assert_eq!(d.settled(), NONE, "{on}");
    assert_eq!(take(&asked_filter), [], "{on}");

    // `watch` again looks again.
    assert_eq!(d.watch(&["/p/a/node_modules/pkg/index.js"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/a/node_modules/pkg/index.js"], "{on}");
    take(&asked_filter);
    d.caught_up();
  });
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

/// An ignored directory with 1,000 files comes into a watched directory and goes again. The
/// filter is asked about the directory, and never about a name inside it.
#[test]
fn an_ignored_directory_with_many_files_comes_and_goes() {
  let (options, asked_filter) =
    recording(|path, kind| kind == EntryKind::Dir && path.ends_with("node_modules"));
  on_all(&options, |d, on| {
    d.fs().mkdir_all("/p/r").unwrap();
    d.fs().mkdir_all("/q/node_modules").unwrap();
    for file in 0..1000 {
      d.fs().write(format!("/q/node_modules/{file}.js"), "1").unwrap();
    }
    assert_eq!(d.watch(&["/p/r"]), NONE, "{on}");
    take(&asked_filter);
    let listings = d.fs().calls().read_dir;

    let the_directory = (PathBuf::from("/p/r/node_modules"), EntryKind::Dir);
    d.fs().rename("/q/node_modules", "/p/r/node_modules").unwrap();
    assert_eq!(windows(d, 3), NONE, "{on}");
    let asked = take(&asked_filter);
    assert!(
      !asked.is_empty() && asked.iter().all(|call| *call == the_directory),
      "{on}: {asked:?}"
    );
    d.fs().write("/p/r/node_modules/0.js", "22").unwrap();
    d.fs().rename("/p/r/node_modules", "/q/node_modules").unwrap();
    assert_eq!(windows(d, 3), NONE, "{on}");
    let asked = take(&asked_filter);
    assert!(asked.iter().all(|call| *call == the_directory), "{on}: {asked:?}");
    // Only the watched directory was listed, never the ignored one.
    let listed = d.fs().calls().read_dir - listings;
    assert!(listed <= 6, "{on}: {listed}");
    d.caught_up();
  });
}

/// 10,000 hints from inside an ignored directory, below a watch for a whole tree, in one
/// flush: one question for the filter, and one look at the ignored directory. The watched
/// directory is looked at with it, because the directory above it is only polled.
#[test]
fn many_hints_from_inside_an_ignored_directory() {
  let (options, asked_filter) =
    recording(|path, kind| kind == EntryKind::Dir && path.ends_with("node_modules"));
  on_modes(&RECURSIVE, &options, |d, on| {
    d.fs().mkdir_all("/p/node_modules/x").unwrap();
    assert_eq!(d.watch(&["/p"]), NONE, "{on}");
    take(&asked_filter);
    let before = d.fs().calls();
    for file in 0..10_000 {
      d.fs().write(format!("/p/node_modules/x/{file}.js"), "1").unwrap();
    }
    assert!(d.fs().pending_raw() >= 10_000, "{on}");
    assert_eq!(d.window(), NONE, "{on}");
    let after = d.fs().calls();
    assert_eq!((after.lstat - before.lstat, after.read_dir - before.read_dir), (2, 0), "{on}");
    assert_eq!(take(&asked_filter), [(PathBuf::from("/p/node_modules"), EntryKind::Dir)], "{on}");
    d.caught_up();
  });
}

/// A file in a watched directory changes 100 times. The filter is asked about it once, when it
/// is found.
#[test]
fn a_file_that_changes_many_times_is_asked_once() {
  let (options, asked_filter) =
    recording(|path, kind| kind == EntryKind::Dir && path.ends_with("node_modules"));
  on_all(&options, |d, on| {
    d.fs().mkdir_all("/p/r").unwrap();
    assert_eq!(d.watch(&["/p/r"]), NONE, "{on}");
    take(&asked_filter);
    d.fs().write("/p/r/a.js", "0").unwrap();
    assert_eq!(d.window(), ["C /p/r/a.js"], "{on}");
    for change in 1..=100 {
      d.fs().write("/p/r/a.js", "1".repeat(change)).unwrap();
      assert_eq!(sorted(d.window()), ["U /p/r/a.js"], "{on}");
    }
    assert_eq!(take(&asked_filter), [(PathBuf::from("/p/r/a.js"), EntryKind::File)], "{on}");
    d.caught_up();
  });
}

/// A closed filter, and a watched path that it ignores as a directory. A file takes the place
/// of the directory at any moment of the `watch` call and of the looks after it: the file is
/// known in the end, and a later write is reported.
#[test]
fn an_ignored_watched_directory_becomes_a_file_at_any_moment() {
  at_every_moment(
    &ignore_out_as_a_directory(),
    |d| {
      d.fs().mkdir_all("/p/out/sub").unwrap();
      d.fs().write("/p/out/gen.js", "1").unwrap();
      d.fs().write("/p/out/sub/x.js", "1").unwrap();
    },
    |d| {
      d.watch(&["/p/out"]);
      d.fs().write("/p/out/gen.js", "22").unwrap_or_default();
    },
    |fs| {
      fs.remove_all("/p/out").unwrap();
      fs.write("/p/out", "a file now").unwrap();
    },
    |d, on| {
      assert_eq!(d.known(), ["/p/out"], "{on}");
      assert_eq!(d.fs().calls().read_dir, 0, "{on}");
      d.fs().write("/p/out", "written").unwrap();
      assert_eq!(d.settled(), ["U /p/out"], "{on}");
    },
  );
}

/// The other way round: a watched file becomes a directory that the closed filter ignores, at
/// any moment. The handler knows no file in the end, the directory is never listed, and a
/// file that comes back at the path is reported.
#[test]
fn a_watched_file_becomes_an_ignored_directory_at_any_moment() {
  at_every_moment(
    &ignore_out_as_a_directory(),
    |d| {
      d.fs().mkdir("/p").unwrap();
      d.fs().write("/p/out", "1").unwrap();
    },
    |d| {
      d.watch(&["/p/out"]);
      d.fs().write("/p/out", "22").unwrap_or_default();
    },
    |fs| {
      fs.remove("/p/out").unwrap();
      fs.mkdir("/p/out").unwrap();
      fs.write("/p/out/gen.js", "1").unwrap();
    },
    |d, on| {
      assert_eq!(d.known(), NONE, "{on}");
      assert_eq!(d.fs().calls().read_dir, 0, "{on}");
      assert!(!asked(d.fs()).iter().any(|(dir, _)| dir == "/p/out"), "{on}");
      d.fs().remove_all("/p/out").unwrap();
      d.fs().write("/p/out", "a file again").unwrap();
      assert_eq!(d.settled(), ["C /p/out"], "{on}");
    },
  );
}

/// A watched file in an ignored directory is written, removed and made again at any moment of
/// the `watch` call and of the looks after it. Nothing is ever reported, and the directory is
/// never asked for its contents.
#[test]
fn a_file_in_an_ignored_directory_changes_at_any_moment() {
  let changes: [fn(&pathwatch_sim::SimFs); 3] = [
    |fs| fs.write("/p/node_modules/x.js", "22").unwrap(),
    |fs| fs.remove("/p/node_modules/x.js").unwrap(),
    |fs| {
      fs.remove("/p/node_modules/x.js").unwrap();
      fs.mkdir("/p/node_modules/x.js").unwrap();
    },
  ];
  for change in changes {
    at_every_moment(
      &ignore_node_modules(),
      |d| {
        d.fs().mkdir_all("/p/node_modules").unwrap();
        d.fs().write("/p/node_modules/x.js", "1").unwrap();
        d.fs().write("/p/a.js", "1").unwrap();
      },
      |d| {
        assert_eq!(d.watch(&["/p/node_modules/x.js", "/p/a.js"]), NONE);
        d.fs().write("/p/a.js", "22").unwrap();
      },
      change,
      |d, on| {
        assert_eq!(d.known(), ["/p/a.js"], "{on}");
        assert!(!asked_for_contents(d).iter().any(|dir| dir == "/p/node_modules"), "{on}");
        // A file, a directory or nothing is at the path now.
        d.fs().remove_all("/p/node_modules/x.js").unwrap_or_default();
        d.fs().write("/p/node_modules/x.js", "333").unwrap();
        assert_eq!(d.settled(), NONE, "{on}");
      },
    );
  }
}

/// The directory that holds a watched file is removed at any moment, with the file. The filter
/// does not ignore that directory, but the one above it: the path is dropped. The handler
/// knows no file in the end, and nothing is reported when the ignored directory is found anew
/// with everything in it. `watch` of the path looks again.
#[test]
fn a_watch_target_below_an_ignored_directory_goes_at_any_moment() {
  at_every_moment(
    &ignore_node_modules(),
    |d| {
      d.fs().mkdir_all("/p/node_modules/pkg").unwrap();
      d.fs().write("/p/node_modules/pkg/x.js", "1").unwrap();
    },
    |d| {
      d.watch(&["/p/node_modules/pkg/x.js"]);
      d.fs().write("/p/node_modules/pkg/x.js", "22").unwrap_or_default();
    },
    |fs| fs.remove_all("/p/node_modules/pkg").unwrap(),
    |d, on| {
      assert_eq!(d.known(), NONE, "{on}");
      assert_eq!(os_watches(d.fs()), [], "{on}");
      d.fs().rename("/p/node_modules", "/p/old").unwrap();
      d.fs().mkdir_all("/p/node_modules/pkg").unwrap();
      d.fs().write("/p/node_modules/pkg/x.js", "333").unwrap();
      assert_eq!(d.settled(), NONE, "{on}");
      assert_eq!(d.watch(&["/p/node_modules/pkg/x.js"]), NONE, "{on}");
      assert_eq!(d.known(), ["/p/node_modules/pkg/x.js"], "{on}");
    },
  );
}

/// A drop tells `Delete` for every file that goes with it, also for one whose `Delete` was
/// waiting. A removal name by name shows the engine the moment when the file is gone and its
/// directory is still there; then the directory goes, and the ignored directory above is the
/// watch target: both watched paths are dropped.
#[test]
fn a_drop_tells_delete_for_every_known_file() {
  on_modes(&Mode::ALL, &ignore_node_modules(), |d, on| {
    let (file, dir) = ("/p/node_modules/pkg/x.js", "/p/node_modules/pkg/lib");
    d.fs().mkdir_all(dir).unwrap();
    d.fs().write(file, "1").unwrap();
    d.fs().write("/p/node_modules/pkg/lib/a.js", "1").unwrap();
    assert_eq!(d.watch(&[file, dir]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/node_modules/pkg/lib/a.js", file], "{on}");

    d.fs().remove(file).unwrap();
    assert_eq!(d.window(), NONE, "{on}");
    assert_eq!(d.pending_deletes(), 1, "{on}");
    d.fs().remove_all("/p/node_modules/pkg").unwrap();
    assert_eq!(d.window(), ["D /p/node_modules/pkg/lib/a.js", &format!("D {file}")], "{on}");
    assert_eq!(d.known(), NONE, "{on}");
    assert_eq!(os_watches(d.fs()), [], "{on}");
    d.caught_up();
  });
}

/// `unwatch` and `watch` again: the new `watch` looks at the place anew.
///
/// The directory `pkg` was missing at the first look, so the ignored directory above it was the
/// watch target, and the path was dropped: it was not looked at when `pkg` came. The second
/// `watch` finds `pkg`: now that one is the watch target, and the file is reported from then
/// on. A file directly in the ignored directory is dropped again by the second `watch`.
#[test]
fn unwatch_and_watch_again_look_at_the_place_anew() {
  let (options, asked_filter) =
    recording(|path, kind| kind == EntryKind::Dir && path.ends_with("node_modules"));
  on_all(&options, |d, on| {
    let (deep, flat) = ("/p/node_modules/pkg/x.js", "/p/node_modules/y.js");
    d.fs().mkdir_all("/p/node_modules").unwrap();
    d.fs().write(flat, "1").unwrap();
    assert_eq!(d.watch(&[deep, flat]), NONE, "{on}");
    d.fs().mkdir("/p/node_modules/pkg").unwrap();
    d.fs().write(deep, "1").unwrap();
    assert_eq!(windows(d, 3), NONE, "{on}");
    assert_eq!(d.known(), NONE, "{on}");

    // Taking the dropped paths back does nothing, and asks the filter nothing.
    take(&asked_filter);
    let calls = all_calls(d);
    assert_eq!(d.unwatch(&[deep, flat]), NONE, "{on}");
    assert_eq!(take(&asked_filter), [], "{on}");
    assert_eq!(all_calls(d), calls, "{on}");
    assert_eq!(asked_for_contents(d), NONE, "{on}");

    assert_eq!(d.watch(&[deep, flat]), NONE, "{on}");
    assert_eq!(d.known(), [deep], "{on}");
    d.fs().write(deep, "22").unwrap();
    d.fs().write(flat, "22").unwrap();
    assert_eq!(d.settled(), [format!("U {deep}")], "{on}");

    // One more watched path next to a dropped one does not wake that one up.
    assert_eq!(d.unwatch(&[deep]), NONE, "{on}");
    d.fs().remove_all("/p/node_modules/pkg").unwrap();
    assert_eq!(d.watch(&[deep]), NONE, "{on}");
    d.fs().mkdir("/p/node_modules/pkg").unwrap();
    d.fs().write(deep, "1").unwrap();
    d.fs().write("/p/node_modules/pkg/z.js", "1").unwrap();
    assert_eq!(windows(d, 3), NONE, "{on}");
    assert_eq!(d.watch(&["/p/node_modules/pkg/z.js"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/node_modules/pkg/z.js"], "{on}");
    // Only `watch` of the dropped path does. The file was not reported before: no event.
    assert_eq!(d.unwatch(&[deep]), NONE, "{on}");
    assert_eq!(d.watch(&[deep]), NONE, "{on}");
    assert_eq!(d.known(), [deep, "/p/node_modules/pkg/z.js"], "{on}");
    d.caught_up();
  });
}

/// The watch target is replaced: the ignored directory is removed, a file takes its place, and
/// a directory comes again. A watched file directly in it is dropped and never reported. A
/// watched file one level further down is reported each time the ignored directory is found
/// anew: its own directory is there at every look.
#[test]
fn an_ignored_watch_target_is_replaced() {
  on_all(&ignore_node_modules(), |d, on| {
    let (deep, flat) = ("/p/node_modules/pkg/x.js", "/p/node_modules/y.js");
    let fill = |d: &mut dyn Sim| {
      d.fs().mkdir_all("/p/node_modules/pkg").unwrap();
      d.fs().write(deep, "1").unwrap();
      d.fs().write(flat, "1").unwrap();
    };
    fill(d);
    assert_eq!(d.watch(&[deep, flat]), NONE, "{on}");
    assert_eq!(d.known(), [deep], "{on}");

    d.fs().remove_all("/p/node_modules").unwrap();
    assert_eq!(d.settled(), [format!("D {deep}")], "{on}");
    d.fs().write("/p/node_modules", "a file now").unwrap();
    assert_eq!(d.settled(), NONE, "{on}");
    d.fs().remove("/p/node_modules").unwrap();
    fill(d);
    assert_eq!(d.settled(), [format!("C {deep}")], "{on}");
    d.fs().write(deep, "22").unwrap();
    d.fs().write(flat, "22").unwrap();
    assert_eq!(d.settled(), [format!("U {deep}")], "{on}");

    // The same in one step: the engine sees a file where the directory was.
    d.fs().remove_all("/p/node_modules").unwrap();
    d.fs().write("/p/node_modules", "a file now").unwrap();
    assert_eq!(d.settled(), [format!("D {deep}")], "{on}");
    // And a directory where the file was.
    d.fs().remove("/p/node_modules").unwrap();
    fill(d);
    assert_eq!(d.settled(), [format!("C {deep}")], "{on}");
    assert!(!asked_for_contents(d).iter().any(|dir| dir == "/p/node_modules"), "{on}");
    d.caught_up();
  });
}

/// A watched file was reported, and the directory that holds it goes away. Now the ignored
/// directory above is the watch target of that name, so the path is dropped: the handler gets
/// the `Delete` of the file, and then the path is forgotten as `unwatch` would do it. The name
/// is not looked at again when the directory comes back, and not when the ignored directory
/// itself is found anew. `watch` looks again.
#[test]
fn a_reported_file_whose_directory_goes_is_not_looked_at_again() {
  on_all(&ignore_node_modules(), |d, on| {
    let deep = "/p/node_modules/pkg/x.js";
    d.fs().mkdir_all("/p/node_modules/pkg").unwrap();
    d.fs().write(deep, "1").unwrap();
    assert_eq!(d.watch(&[deep]), NONE, "{on}");
    assert_eq!(d.known(), [deep], "{on}");
    d.fs().remove_all("/p/node_modules/pkg").unwrap();
    assert_eq!(d.settled(), [format!("D {deep}")], "{on}");
    assert_eq!(d.known(), NONE, "{on}");
    assert_eq!(os_watches(d.fs()), [], "{on}");

    d.fs().mkdir("/p/node_modules/pkg").unwrap();
    d.fs().write(deep, "22").unwrap();
    assert_eq!(d.settled(), NONE, "{on}");
    assert_eq!(asked_for_contents(d).iter().filter(|dir| *dir == "/p/node_modules").count(), 0);

    d.fs().rename("/p/node_modules", "/p/away").unwrap();
    assert_eq!(d.settled(), NONE, "{on}");
    d.fs().rename("/p/away", "/p/node_modules").unwrap();
    assert_eq!(d.settled(), NONE, "{on}");

    let calls = all_calls(d);
    assert_eq!(d.unwatch(&[deep]), NONE, "{on}");
    assert_eq!(all_calls(d), calls, "{on}");
    assert_eq!(d.watch(&[deep]), NONE, "{on}");
    assert_eq!(d.known(), [deep], "{on}");
    d.caught_up();
  });
}

/// A path that is a watched path and also on the way to another one. The filter ignores it as
/// a directory, so nothing in it is reported for it, but it is never filtered as a name on
/// the way: the watched file below it is reported. When that file is taken back, the path
/// waits like a missing one, and a file that takes its place is reported.
#[test]
fn an_ignored_watched_directory_that_leads_to_a_watched_path() {
  on_all(&ignore_node_modules(), |d, on| {
    let (dir, deep, flat) = ("/p/node_modules", "/p/node_modules/pkg/x.js", "/p/node_modules/y.js");
    d.fs().mkdir_all("/p/node_modules/pkg").unwrap();
    for file in [deep, flat, "/p/node_modules/pkg/other.js"] {
      d.fs().write(file, "1").unwrap();
    }
    assert_eq!(d.watch(&[dir, deep, flat]), NONE, "{on}");
    assert_eq!(d.known(), [deep], "{on}");
    for file in [deep, flat, "/p/node_modules/pkg/other.js", "/p/node_modules/new.js"] {
      d.fs().write(file, "22").unwrap();
    }
    assert_eq!(d.settled(), [format!("U {deep}")], "{on}");
    assert_eq!(d.fs().calls().read_dir, 0, "{on}");

    assert_eq!(d.unwatch(&[deep]), NONE, "{on}");
    d.fs().write(deep, "333").unwrap();
    assert_eq!(d.settled(), NONE, "{on}");
    assert_eq!(d.unwatch(&[flat]), NONE, "{on}");
    // Only the watched path is left. A file takes its place.
    d.fs().remove_all(dir).unwrap();
    d.fs().write(dir, "a file now").unwrap();
    assert_eq!(d.settled(), [format!("C {dir}")], "{on}");
    d.fs().write(dir, "written").unwrap();
    assert_eq!(d.window(), [format!("U {dir}")], "{on}");

    // The two files are watched again while a file is in the way, and the directory comes.
    assert_eq!(d.watch(&[deep, flat]), NONE, "{on}");
    d.fs().remove(dir).unwrap();
    d.fs().mkdir_all("/p/node_modules/pkg").unwrap();
    d.fs().write(deep, "1").unwrap();
    d.fs().write(flat, "1").unwrap();
    assert_eq!(sorted(d.settled()), [format!("C {deep}"), format!("D {dir}")], "{on}");
    d.caught_up();
  });
}

/// An ignored directory inside a watched directory is the watch target of a watched file in
/// it: the file is dropped and not reported. The watched directory above tracks every other entry as
/// before, and a watched file further down has a watch target that is not ignored.
#[test]
fn an_ignored_directory_in_a_watched_directory_is_a_watch_target() {
  on_all(&ignore_node_modules(), |d, on| {
    let (deep, flat) = ("/p/r/node_modules/pkg/x.js", "/p/r/node_modules/y.js");
    d.fs().mkdir_all("/p/r/node_modules/pkg").unwrap();
    for file in [deep, flat, "/p/r/a.js"] {
      d.fs().write(file, "1").unwrap();
    }
    assert_eq!(d.watch(&["/p/r", deep, flat]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/r/a.js", deep], "{on}");
    for file in [deep, flat, "/p/r/a.js", "/p/r/node_modules/z.js"] {
      d.fs().write(file, "22").unwrap();
    }
    assert_eq!(sorted(d.settled()), ["U /p/r/a.js".to_owned(), format!("U {deep}")], "{on}");

    // The watched directory is taken back: the two files are as before.
    assert_eq!(d.unwatch(&["/p/r"]), NONE, "{on}");
    for file in [deep, flat, "/p/r/a.js"] {
      d.fs().write(file, "333").unwrap();
    }
    assert_eq!(d.settled(), [format!("U {deep}")], "{on}");
    // And watched again.
    assert_eq!(d.watch(&["/p/r"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/r/a.js", deep], "{on}");
    assert_eq!(d.unwatch(&[deep, flat]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/r/a.js"], "{on}");
    d.fs().write(deep, "4444").unwrap();
    d.fs().write("/p/r/a.js", "4444").unwrap();
    assert_eq!(d.settled(), ["U /p/r/a.js"], "{on}");
    d.caught_up();
  });
}

/// A path that the filter ignores for both kinds leads to a watched path. Giving it to `watch`
/// and to `unwatch` changes nothing: the watched path below it is reported as before.
#[test]
fn a_path_ignored_for_both_kinds_that_leads_to_a_watched_path() {
  let (options, asked_filter) = recording(|path, _kind| path.ends_with("node_modules"));
  on_all(&options, |d, on| {
    let deep = "/p/node_modules/pkg/x.js";
    d.fs().mkdir_all("/p/node_modules/pkg").unwrap();
    d.fs().write(deep, "1").unwrap();
    assert_eq!(d.watch(&[deep]), NONE, "{on}");
    let calls = all_calls(d);
    take(&asked_filter);

    assert_eq!(d.watch(&["/p/node_modules"]), NONE, "{on}");
    // Both kinds are asked, and that is all.
    let both =
      [EntryKind::File, EntryKind::Dir].map(|kind| (PathBuf::from("/p/node_modules"), kind));
    assert_eq!(take(&asked_filter), both, "{on}");
    assert_eq!(d.unwatch(&["/p/node_modules"]), NONE, "{on}");
    assert_eq!(take(&asked_filter), [], "{on}");
    assert_eq!(all_calls(d), calls, "{on}");

    d.fs().write(deep, "22").unwrap();
    assert_eq!(d.settled(), [format!("U {deep}")], "{on}");
    d.caught_up();
  });
}

/// The filter is not asked about an entry that is not there, and nothing is asked below an
/// ignored directory: `unwatch` of a path with a missing directory above it, and of a path
/// below an ignored directory in a watched directory, asks nothing.
#[test]
fn unwatch_asks_the_filter_about_nothing_new() {
  let (options, asked_filter) =
    recording(|path, kind| kind == EntryKind::Dir && path.ends_with("node_modules"));
  on_all(&options, |d, on| {
    // The directory `a` only leads to the watched path, so nothing asked about it before.
    let (missing, deep) = ("/p/r/gone/b.js", "/p/r/node_modules/a/pkg/x.js");
    d.fs().mkdir_all("/p/r/node_modules/a/pkg").unwrap();
    d.fs().write(deep, "1").unwrap();
    assert_eq!(d.watch(&["/p/r", missing, deep]), NONE, "{on}");
    take(&asked_filter);
    assert_eq!(d.unwatch(&[missing]), NONE, "{on}");
    assert_eq!(take(&asked_filter), [], "{on}");
    assert_eq!(d.unwatch(&[deep]), NONE, "{on}");
    assert_eq!(take(&asked_filter), [], "{on}");
    d.caught_up();
  });
}

/// A second watched path is put into a directory that told names only until now, in a later
/// `watch` call. The directory would be its watch target, so the filter is asked first: the
/// ignored directory is not asked for its contents, and the new path is dropped.
#[test]
fn a_later_watched_file_in_an_ignored_directory() {
  let (options, asked_filter) =
    recording(|path, kind| kind == EntryKind::Dir && path.ends_with("node_modules"));
  on_all(&options, |d, on| {
    let (deep, flat) = ("/p/node_modules/pkg/x.js", "/p/node_modules/y.js");
    d.fs().mkdir_all("/p/node_modules/pkg").unwrap();
    d.fs().write(deep, "1").unwrap();
    d.fs().write(flat, "1").unwrap();
    take(&asked_filter);
    assert_eq!(d.watch(&[deep]), NONE, "{on}");
    // The ignored directory only leads to the watched path: it was not asked about.
    let the_directory = (PathBuf::from("/p/node_modules"), EntryKind::Dir);
    assert!(!take(&asked_filter).contains(&the_directory), "{on}");

    assert_eq!(d.watch(&[flat]), NONE, "{on}");
    assert!(take(&asked_filter).contains(&the_directory), "{on}");
    assert!(!asked_for_contents(d).iter().any(|dir| dir == "/p/node_modules"), "{on}");
    d.fs().write(flat, "22").unwrap();
    assert_eq!(d.settled(), NONE, "{on}");
    assert_eq!(d.known(), [deep], "{on}");
    d.caught_up();
  });
}

/// A poll does not look at a watched path in an ignored directory, whether a file, nothing or
/// an ignored directory is at the path: the path was dropped, and every poll makes the same
/// calls to the disk as a poll with no such watched path.
#[test]
fn a_poll_does_not_look_at_a_name_in_an_ignored_directory() {
  let looks_of_five_polls = |paths: &[&str]| {
    let mut d = Driver::<PollBackend>::poll(ignore_node_modules());
    d.fs.mkdir_all("/p/node_modules/pkg").unwrap();
    d.fs.mkdir_all("/p/node_modules/node_modules").unwrap();
    d.fs.write("/p/node_modules/pkg/x.js", "1").unwrap();
    d.fs.write("/p/node_modules/y.js", "1").unwrap();
    d.watch(paths);
    let before = d.fs.calls();
    windows(&mut d, 5);
    let after = d.fs.calls();
    caught_up(&mut d);
    (after.lstat - before.lstat, after.total() - before.total())
  };
  let one = looks_of_five_polls(&["/p/node_modules/pkg/x.js"]);
  let more = looks_of_five_polls(&[
    "/p/node_modules/pkg/x.js",
    "/p/node_modules/y.js",
    "/p/node_modules/nothing.js",
    "/p/node_modules/node_modules",
  ]);
  assert_eq!(more, one);
}

/// A watched directory that the filter ignores, inside another ignored directory, stays in the
/// tree while it leads to a watched path. When that path is taken back, the directory counts
/// as missing, and the ignored directory above it is its watch target: that one is not asked
/// for its contents, and the path is dropped without a look. Nothing is mounted any more.
#[test]
fn unwatch_leaves_an_ignored_directory_in_an_ignored_directory() {
  on_all(&ignore_node_modules(), |d, on| {
    let (dir, deep) = ("/p/node_modules/node_modules", "/p/node_modules/node_modules/pkg/x.js");
    d.fs().mkdir_all("/p/node_modules/node_modules/pkg").unwrap();
    d.fs().write(deep, "1").unwrap();
    assert_eq!(d.watch(&[dir, deep]), NONE, "{on}");
    assert_eq!(d.known(), [deep], "{on}");

    let looks = d.fs().calls().lstat;
    assert_eq!(d.unwatch(&[deep]), NONE, "{on}");
    assert_eq!(d.fs().calls().lstat, looks, "{on}");
    assert!(!asked_for_contents(d).iter().any(|dir| dir == "/p/node_modules"), "{on}");
    assert_eq!(os_watches(d.fs()), [], "{on}");
    // The watched directory that is left counts as missing, and the filter ignores its watch
    // target too: it was dropped. Nothing is watched, so no window comes, and nothing is
    // looked at any more.
    assert!(!d.something_is_due(), "{on}");
    d.caught_up();
    assert_eq!(d.fs().calls().lstat, looks, "{on}");
  });
}

/// Two watched paths in an ignored directory are dropped by `watch`, so no watch is left in
/// that directory. A hint for one of them can still come from a watch for the whole tree
/// above. It waits for its flush, and the path is taken back before the flush comes: that
/// does nothing, and the flush leaves nothing behind.
#[test]
fn unwatch_of_a_name_that_is_not_looked_at_while_a_hint_waits() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, ignore_node_modules());
    d.fs.mkdir_all("/p/node_modules").unwrap();
    // With the watched directory above, a hint comes on every backend.
    d.watch(&["/p", "/p/node_modules/y.js", "/p/node_modules/z.js"]);
    d.fs.write("/p/node_modules/y.js", "1").unwrap();
    assert_eq!(d.deliver(usize::MAX) > 0, RECURSIVE.contains(&mode), "{mode:?}");
    d.unwatch(&["/p/node_modules/y.js"]);
    d.settle();
    assert_eq!(d.take_events(), [], "{mode:?}");
    // The end of the story checks the invariants of the tree.
    caught_up(&mut d);
  }
}

/// Two watched files in an ignored directory, and a watched directory next to them. The two
/// files are dropped. The directory still reports: a directory is at its path, so it needs no
/// watch from the ignored directory above, which keeps the `Names` ask that leads to it.
#[test]
fn two_files_are_dropped_and_the_directory_next_to_them_reports() {
  on_all(&ignore_node_modules(), |d, on| {
    let (a, b, dir) = ("/p/node_modules/a.js", "/p/node_modules/b.js", "/p/node_modules/pkg");
    d.fs().mkdir_all(dir).unwrap();
    for file in [a, b, "/p/node_modules/pkg/x.js"] {
      d.fs().write(file, "1").unwrap();
    }
    assert_eq!(d.watch(&[a, b, dir]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/node_modules/pkg/x.js"], "{on}");
    // The ignored directory was asked for names only, and no watch was given back.
    if on != "Poll" {
      let calls = [
        at("/", Some(Names)),
        at("/p", Some(Names)),
        at("/p/node_modules", Some(Names)),
        at("/p/node_modules/pkg", Some(Contents)),
      ];
      assert_eq!(asked(d.fs()), calls, "{on}");
    }

    for file in [a, b, "/p/node_modules/pkg/x.js", "/p/node_modules/pkg/new.js"] {
      d.fs().write(file, "22").unwrap();
    }
    let events = ["C /p/node_modules/pkg/new.js", "U /p/node_modules/pkg/x.js"];
    assert_eq!(sorted(d.settled()), events, "{on}");
    d.fs().remove(a).unwrap();
    d.fs().remove(b).unwrap();
    assert_eq!(d.settled(), NONE, "{on}");

    // Taking the two files back does nothing.
    let calls = all_calls(d);
    assert_eq!(d.unwatch(&[a, b]), NONE, "{on}");
    assert_eq!(all_calls(d), calls, "{on}");
    d.fs().remove("/p/node_modules/pkg/x.js").unwrap();
    assert_eq!(d.settled(), ["D /p/node_modules/pkg/x.js"], "{on}");
    d.caught_up();
  });
}

// ---- cases of the design review ----

/// The filter is asked for a watched path too, with the kind it has now.
#[test]
fn a_watched_path_that_the_filter_ignores() {
  let options = Options {
    ignored: Some(std::sync::Arc::new(|path: &Path, kind| {
      kind == pathwatch::EntryKind::File
        && path.extension().is_some_and(|extension| extension == "log")
    })),
    ..Options::default()
  };
  let mut d = Driver::sim(Mode::Inode, options);
  d.fs.mkdir("/p").unwrap();
  d.fs.write("/p/foo.log", "1").unwrap();
  d.watch(&["/p/foo.log"]);
  assert!(d.known.is_empty());
  d.fs.write("/p/foo.log", "12").unwrap();
  assert_eq!(settled(&mut d), NONE);

  // As a directory the same path is not ignored, and neither is what is in it.
  d.fs.remove("/p/foo.log").unwrap();
  d.fs.mkdir("/p/foo.log").unwrap();
  d.fs.write("/p/foo.log/a.js", "1").unwrap();
  d.fs.write("/p/foo.log/b.log", "1").unwrap();
  assert_eq!(settled(&mut d), ["C /p/foo.log/a.js"]);
  caught_up(&mut d);
}

/// A watched file below an ignored directory works: the directory gets a partial node, nothing
/// more.
#[test]
fn a_watched_file_below_an_ignored_directory() {
  for mode in Mode::ALL {
    let mut d = Driver::sim(mode, ignore_node_modules());
    d.fs.mkdir_all("/p/node_modules/foo").unwrap();
    d.fs.write("/p/node_modules/foo/index.js", "1").unwrap();
    d.fs.write("/p/node_modules/foo/other.js", "1").unwrap();
    d.fs.write("/p/a.js", "1").unwrap();
    d.watch(&["/p", "/p/node_modules/foo/index.js"]);
    assert_eq!(d.known.len(), 2);

    d.fs.write("/p/node_modules/foo/index.js", "12").unwrap();
    d.fs.write("/p/node_modules/foo/other.js", "12").unwrap();
    d.fs.write("/p/node_modules/new.js", "1").unwrap();
    assert_eq!(settled(&mut d), ["U /p/node_modules/foo/index.js"], "{mode:?}");

    // The ignored directory gets a visible name: all of it shows up, and the watched file under
    // the old name is gone.
    d.fs.rename("/p/node_modules", "/p/nm").unwrap();
    assert_eq!(
      settled(&mut d),
      [
        "C /p/nm/foo/index.js",
        "C /p/nm/foo/other.js",
        "C /p/nm/new.js",
        "D /p/node_modules/foo/index.js"
      ],
      "{mode:?}"
    );
    d.fs.write("/p/nm/foo/other.js", "123").unwrap();
    assert_eq!(settled(&mut d), ["U /p/nm/foo/other.js"], "{mode:?}");
    caught_up(&mut d);
  }
}

/// (i): an ignored directory gets no node, no watch and no listing. A watched file below it
/// works through partial nodes.
#[test]
fn case_i_an_ignored_directory() {
  on_all(&ignore_node_modules(), |d, on| {
    d.fs().mkdir_all("/p/node_modules/foo/lib").unwrap();
    d.fs().write("/p/node_modules/foo/index.js", "1").unwrap();
    d.fs().write("/p/node_modules/foo/lib/other.js", "1").unwrap();
    d.fs().write("/p/a.js", "1").unwrap();
    assert_eq!(d.watch(&["/p"]), NONE, "{on}");
    assert_eq!(d.known(), ["/p/a.js"], "{on}");
    // Only `/p` was listed.
    assert_eq!(d.fs().calls().read_dir, 1, "{on}");

    assert_eq!(d.watch(&["/p/node_modules/foo/index.js"]), NONE, "{on}");
    assert_eq!(d.fs().calls().read_dir, 1, "{on}");
    d.fs().write("/p/node_modules/foo/index.js", "22").unwrap();
    d.fs().write("/p/node_modules/foo/lib/other.js", "22").unwrap();
    assert_eq!(d.settled(), ["U /p/node_modules/foo/index.js"], "{on}");

    // To a visible name: every file is created. Back: every known file is deleted.
    d.fs().rename("/p/node_modules", "/p/vendor").unwrap();
    assert_eq!(
      sorted(d.settled()),
      [
        "C /p/vendor/foo/index.js",
        "C /p/vendor/foo/lib/other.js",
        "D /p/node_modules/foo/index.js"
      ],
      "{on}"
    );
    d.fs().rename("/p/vendor", "/p/node_modules").unwrap();
    assert_eq!(
      sorted(d.settled()),
      [
        "C /p/node_modules/foo/index.js",
        "D /p/vendor/foo/index.js",
        "D /p/vendor/foo/lib/other.js"
      ],
      "{on}"
    );
    d.caught_up();
  });
}

/// The filter is asked about an entry when it is found, and again when its kind changes. Not at
/// every check: for an entry that the tree has, the answer is known.
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
      // The two watched paths, `sub`, and four files. And two more: `watch` asks about a path
      // as a file first, so `/p/r` is asked for both kinds; and `/p` is the watch target of
      // `/p/f.js`.
      assert_eq!(d.watch(&["/p/r", "/p/f.js"]), NONE, "{on}");
      assert_eq!(asked(), 9, "{on}");

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
      d.caught_up();
    });
  }
}

/// A flood of hints from inside an ignored directory ends at its parent and costs
/// one `lstat` for a flush. The watched directory is checked with it, because the directory
/// above it is only polled: one more.
#[test]
fn hints_from_an_ignored_directory() {
  on_modes(&RECURSIVE, &ignore_node_modules(), |d, on| {
    d.fs().mkdir_all("/p/node_modules/x").unwrap();
    assert_eq!(d.watch(&["/p"]), NONE, "{on}");
    let before = d.fs().calls();
    for file in 0..50 {
      d.fs().write(format!("/p/node_modules/x/{file}.js"), "1").unwrap();
    }
    assert!(d.fs().pending_raw() >= 50, "{on}");
    assert_eq!(d.window(), NONE, "{on}");
    let after = d.fs().calls();
    assert_eq!((after.lstat - before.lstat, after.read_dir - before.read_dir), (2, 0), "{on}");
    d.caught_up();
  });
}

/// A flood from an ignored directory below a recursive watch costs one check for a flush, and a
/// flood from a place no watched path covers costs none.
#[test]
fn a_flood_from_places_nobody_watches() {
  for mode in [Mode::Prefix, Mode::Handle] {
    let mut d = Driver::sim(mode, ignore_node_modules());
    d.fs.mkdir_all("/p/r/node_modules/pkg").unwrap();
    d.fs.mkdir_all("/p/other").unwrap();
    d.fs.write("/p/f.js", "1").unwrap();
    d.watch(&["/p/r", "/p/f.js"]);

    let before = d.fs.calls();
    for file in 0..100 {
      d.fs.write(format!("/p/r/node_modules/pkg/{file}.js"), "1").unwrap();
      d.fs.write(format!("/p/other/{file}.js"), "1").unwrap();
    }
    assert!(d.fs.pending_raw() >= 200, "{mode:?}");
    flush(&mut d);
    assert_eq!(taken(&mut d), NONE, "{mode:?}");
    let after = d.fs.calls();
    // `node_modules`, and `/p`: its parent `/` is only polled, so it is checked with the mark.
    let checks = (after.lstat - before.lstat) + (after.stat - before.stat);
    assert_eq!((checks, after.read_dir - before.read_dir), (2, 0), "{mode:?}");
    caught_up(&mut d);
  }
}
