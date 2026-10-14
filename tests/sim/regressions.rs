//! Cases of the design review that fit no other file: a file and its directory as two watched
//! paths, a chain of directories that appears, and a name that is a file and then a directory.
//! A story makes the disk, calls `watch`, changes the disk and compares the events that come
//! out (`C`, `U`, `D` and the path).

use crate::fixtures::{NONE, ignore_node_modules, on_all};
use pathwatch::engine::Options;

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
/// the whole chain, all inside one window.
#[test]
fn case_f_a_chain_that_comes_and_goes() {
  on_all(&Options::default(), |d, on| {
    d.fs().mkdir("/p").unwrap();
    assert_eq!(d.watch(&["/p/a/b/c/R.js"]), NONE, "{on}");
    d.fs().mkdir("/p/a").unwrap();
    d.fs().remove("/p/a").unwrap();
    d.fs().mkdir_all("/p/a/b/c").unwrap();
    d.fs().write("/p/a/b/c/R.js", "1").unwrap();
    assert_eq!(d.window(), ["C /p/a/b/c/R.js"], "{on}");
    assert_eq!(d.settled(), NONE, "{on}");
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

    // 2. A chain that never existed goes through `check` and `vanish` without a watch: the
    // backend is never asked to forget one.
    d.fs().mkdir("/s").unwrap();
    let before = d.fs().calls();
    assert_eq!(d.watch(&["/s/a/b/c/x.js"]), NONE, "{on}");
    assert_eq!(d.settled(), NONE, "{on}");
    assert_eq!(d.fs().calls().unwatch, before.unwatch, "{on}");
    d.caught_up();
  });
}
