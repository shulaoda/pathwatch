//! The rows S1 to S14 of the behavior contract in `docs/design.md`. Each row is one function
//! that tells its story on the driver from `make`; `on_every_backend!` at the end makes a test
//! of each row for each backend.

#![cfg(unix)]

use pathwatch::engine::Options;
use pathwatch_sim::{
  Deliver, Driver, Mode, NONE, caught_up, ignore_node_modules, settled, until_delete_pending,
};

/// S1: in-place write to file `R`.
fn s1_write_in_place<B: Deliver>(make: impl Fn(Options) -> Driver<B>) {
  let mut d = make(Options::default());
  d.fs.mkdir("/p").unwrap();
  d.fs.write("/p/a.js", "1").unwrap();
  d.watch(&["/p/a.js"]);
  assert_eq!(settled(&mut d), NONE);

  d.fs.write("/p/a.js", "22").unwrap();
  assert_eq!(settled(&mut d), ["U /p/a.js"]);
  // Same size as before.
  d.fs.write("/p/a.js", "33").unwrap();
  assert_eq!(settled(&mut d), ["U /p/a.js"]);
  assert_eq!(settled(&mut d), NONE);
  caught_up(&mut d);
}

/// S2: atomic save of `R` (write a temporary file, rename it over).
fn s2_atomic_save<B: Deliver>(make: impl Fn(Options) -> Driver<B>) {
  let mut d = make(Options::default());
  d.fs.mkdir("/p").unwrap();
  d.fs.write("/p/a.js", "old").unwrap();
  d.watch(&["/p/a.js"]);

  d.fs.write("/p/a.js.tmp", "new").unwrap();
  d.fs.rename("/p/a.js.tmp", "/p/a.js").unwrap();
  assert_eq!(settled(&mut d), ["U /p/a.js"]);
  caught_up(&mut d);
}

/// S3: `R` deleted and created again.
fn s3_delete_then_create<B: Deliver>(make: impl Fn(Options) -> Driver<B>) {
  let mut d = make(Options::default());
  d.fs.mkdir("/p").unwrap();
  d.fs.write("/p/a.js", "1").unwrap();
  d.watch(&["/p/a.js"]);

  d.fs.remove("/p/a.js").unwrap();
  assert_eq!(settled(&mut d), ["D /p/a.js"]);
  d.fs.write("/p/a.js", "1").unwrap();
  assert_eq!(settled(&mut d), ["C /p/a.js"]);

  // Inside the delete delay the pair is one update.
  d.fs.remove("/p/a.js").unwrap();
  until_delete_pending(&mut d, "/p/a.js");
  d.fs.write("/p/a.js", "1").unwrap();
  assert_eq!(settled(&mut d), ["U /p/a.js"]);
  caught_up(&mut d);
}

/// S4: `R` is missing, a file is created there, then written.
fn s4_missing_then_file<B: Deliver>(make: impl Fn(Options) -> Driver<B>) {
  let mut d = make(Options::default());
  d.fs.mkdir("/p").unwrap();
  d.watch(&["/p/new.js"]);
  assert_eq!(settled(&mut d), NONE);

  d.fs.write("/p/new.js", "1").unwrap();
  assert_eq!(settled(&mut d), ["C /p/new.js"]);
  d.fs.write("/p/new.js", "12").unwrap();
  assert_eq!(settled(&mut d), ["U /p/new.js"]);
  caught_up(&mut d);
}

/// S5: `R` is missing, a directory is created there, then a file inside.
fn s5_missing_then_dir<B: Deliver>(make: impl Fn(Options) -> Driver<B>) {
  let mut d = make(Options::default());
  d.fs.mkdir("/p").unwrap();
  d.watch(&["/p/content"]);

  d.fs.mkdir("/p/content").unwrap();
  d.fs.write("/p/content/f", "1").unwrap();
  assert_eq!(settled(&mut d), ["C /p/content/f"]);
  d.fs.write("/p/content/g", "1").unwrap();
  assert_eq!(settled(&mut d), ["C /p/content/g"]);
  caught_up(&mut d);
}

/// S6: `R` and several ancestors are missing; `mkdir -p` and a file at `R`.
fn s6_missing_chain<B: Deliver>(make: impl Fn(Options) -> Driver<B>) {
  let mut d = make(Options::default());
  d.fs.mkdir("/p").unwrap();
  d.watch(&["/p/a/b/c/x.js"]);
  assert_eq!(settled(&mut d), NONE);

  d.fs.mkdir_all("/p/a/b/c").unwrap();
  d.fs.write("/p/a/b/c/x.js", "1").unwrap();
  d.fs.write("/p/a/b/c/other.js", "1").unwrap();
  assert_eq!(settled(&mut d), ["C /p/a/b/c/x.js"]);

  // The chain goes away at the second level and comes back.
  d.fs.remove_all("/p/a/b").unwrap();
  assert_eq!(settled(&mut d), ["D /p/a/b/c/x.js"]);
  d.fs.mkdir_all("/p/a/b/c").unwrap();
  d.fs.write("/p/a/b/c/x.js", "1").unwrap();
  assert_eq!(settled(&mut d), ["C /p/a/b/c/x.js"]);
  caught_up(&mut d);
}

/// S7: file `R` is deleted, a directory with files appears at `R`.
fn s7_file_becomes_dir<B: Deliver>(make: impl Fn(Options) -> Driver<B>) {
  let mut d = make(Options::default());
  d.fs.mkdir("/p").unwrap();
  d.fs.write("/p/r", "file").unwrap();
  d.watch(&["/p/r"]);

  d.fs.remove("/p/r").unwrap();
  d.fs.mkdir("/p/r").unwrap();
  d.fs.write("/p/r/x", "1").unwrap();
  d.fs.write("/p/r/y", "1").unwrap();
  // One batch: the delete does not wait when the kind changes.
  assert_eq!(settled(&mut d), ["D /p/r", "C /p/r/x", "C /p/r/y"]);
  caught_up(&mut d);
}

/// S8: directory `R` is removed, a file is created at `R`.
fn s8_dir_becomes_file<B: Deliver>(make: impl Fn(Options) -> Driver<B>) {
  let mut d = make(Options::default());
  d.fs.mkdir_all("/p/r").unwrap();
  d.fs.write("/p/r/x", "1").unwrap();
  d.fs.write("/p/r/y", "1").unwrap();
  d.watch(&["/p/r"]);

  d.fs.remove_all("/p/r").unwrap();
  d.fs.write("/p/r", "file").unwrap();
  assert_eq!(settled(&mut d), ["C /p/r", "D /p/r/x", "D /p/r/y"]);
  caught_up(&mut d);
}

/// S9: a new file in directory `R`; then `mkdir` and a file written right away.
fn s9_new_entries<B: Deliver>(make: impl Fn(Options) -> Driver<B>) {
  let mut d = make(Options::default());
  d.fs.mkdir_all("/p/r").unwrap();
  d.watch(&["/p/r"]);

  d.fs.write("/p/r/f", "1").unwrap();
  assert_eq!(settled(&mut d), ["C /p/r/f"]);

  d.fs.mkdir("/p/r/sub").unwrap();
  d.fs.write("/p/r/sub/f", "1").unwrap();
  assert_eq!(settled(&mut d), ["C /p/r/sub/f"]);
  d.fs.write("/p/r/sub/f", "12").unwrap();
  assert_eq!(settled(&mut d), ["U /p/r/sub/f"]);
  caught_up(&mut d);
}

/// S10: a tree is moved into directory `R`.
fn s10_tree_moved_in<B: Deliver>(make: impl Fn(Options) -> Driver<B>) {
  let mut d = make(Options::default());
  d.fs.mkdir_all("/p/r").unwrap();
  d.fs.mkdir_all("/q/t/u").unwrap();
  d.fs.write("/q/t/a", "1").unwrap();
  d.fs.write("/q/t/u/b", "1").unwrap();
  d.watch(&["/p/r"]);

  d.fs.rename("/q/t", "/p/r/t").unwrap();
  assert_eq!(settled(&mut d), ["C /p/r/t/a", "C /p/r/t/u/b"]);
  d.fs.write("/p/r/t/u/b", "12").unwrap();
  assert_eq!(settled(&mut d), ["U /p/r/t/u/b"]);
  caught_up(&mut d);
}

/// S11: a sub-directory is moved out of `R`, or removed with everything in it.
fn s11_subdir_leaves<B: Deliver>(make: impl Fn(Options) -> Driver<B>) {
  let mut d = make(Options::default());
  d.fs.mkdir_all("/p/r/one/deep").unwrap();
  d.fs.mkdir_all("/p/r/two/deep").unwrap();
  d.fs.mkdir("/q").unwrap();
  for dir in ["one", "two"] {
    d.fs.write(format!("/p/r/{dir}/a"), "1").unwrap();
    d.fs.write(format!("/p/r/{dir}/deep/b"), "1").unwrap();
  }
  d.watch(&["/p/r"]);

  d.fs.rename("/p/r/one", "/q/one").unwrap();
  assert_eq!(settled(&mut d), ["D /p/r/one/a", "D /p/r/one/deep/b"]);
  // It is not watched where it went.
  d.fs.write("/q/one/a", "12").unwrap();
  assert_eq!(settled(&mut d), NONE);

  d.fs.remove_all("/p/r/two").unwrap();
  assert_eq!(settled(&mut d), ["D /p/r/two/a", "D /p/r/two/deep/b"]);
  caught_up(&mut d);
}

/// S12: directory `R` is renamed away, then it is back.
fn s12_watched_dir_away_and_back<B: Deliver>(make: impl Fn(Options) -> Driver<B>) {
  let mut d = make(Options::default());
  d.fs.mkdir_all("/p/r/s").unwrap();
  d.fs.write("/p/r/a", "1").unwrap();
  d.fs.write("/p/r/s/b", "1").unwrap();
  d.watch(&["/p/r"]);

  d.fs.rename("/p/r", "/p/gone").unwrap();
  assert_eq!(settled(&mut d), ["D /p/r/a", "D /p/r/s/b"]);
  d.fs.rename("/p/gone", "/p/r").unwrap();
  assert_eq!(settled(&mut d), ["C /p/r/a", "C /p/r/s/b"]);

  d.fs.remove_all("/p/r").unwrap();
  assert_eq!(settled(&mut d), ["D /p/r/a", "D /p/r/s/b"]);
  d.fs.mkdir("/p/r").unwrap();
  d.fs.write("/p/r/c", "1").unwrap();
  assert_eq!(settled(&mut d), ["C /p/r/c"]);
  caught_up(&mut d);
}

/// S13: file `R = /proj/lib/a.js`; `mv /proj/lib /proj/lib2`, and back.
fn s13_ancestor_away_and_back<B: Deliver>(make: impl Fn(Options) -> Driver<B>) {
  let mut d = make(Options::default());
  d.fs.mkdir_all("/proj/lib").unwrap();
  d.fs.write("/proj/lib/a.js", "1").unwrap();
  d.watch(&["/proj/lib/a.js"]);

  d.fs.rename("/proj/lib", "/proj/lib2").unwrap();
  assert_eq!(settled(&mut d), ["D /proj/lib/a.js"]);
  d.fs.rename("/proj/lib2", "/proj/lib").unwrap();
  assert_eq!(settled(&mut d), ["C /proj/lib/a.js"]);
  d.fs.write("/proj/lib/a.js", "12").unwrap();
  assert_eq!(settled(&mut d), ["U /proj/lib/a.js"]);
  caught_up(&mut d);
}

/// S14: an ignored directory below `R`.
fn s14_ignored_dir<B: Deliver>(make: impl Fn(Options) -> Driver<B>) {
  let mut d = make(ignore_node_modules());
  d.fs.mkdir_all("/p/r/node_modules/pkg").unwrap();
  d.fs.mkdir_all("/p/r/src").unwrap();
  d.fs.write("/p/r/node_modules/pkg/x.js", "1").unwrap();
  d.fs.write("/p/r/src/a.js", "1").unwrap();
  d.watch(&["/p/r"]);
  assert_eq!(
    d.known.iter().map(|path| path.to_str().unwrap()).collect::<Vec<_>>(),
    ["/p/r/src/a.js"]
  );

  d.fs.write("/p/r/node_modules/pkg/x.js", "12").unwrap();
  d.fs.write("/p/r/node_modules/y.js", "1").unwrap();
  d.fs.mkdir("/p/r/src/node_modules").unwrap();
  d.fs.write("/p/r/src/node_modules/z.js", "1").unwrap();
  assert_eq!(settled(&mut d), NONE);

  // Ignored name to visible name.
  d.fs.rename("/p/r/node_modules", "/p/r/vendor").unwrap();
  assert_eq!(settled(&mut d), ["C /p/r/vendor/pkg/x.js", "C /p/r/vendor/y.js"]);
  // Visible name to ignored name.
  d.fs.rename("/p/r/vendor", "/p/r/node_modules").unwrap();
  assert_eq!(settled(&mut d), ["D /p/r/vendor/pkg/x.js", "D /p/r/vendor/y.js"]);
  caught_up(&mut d);
}

macro_rules! on_every_backend {
  ($($scenario:ident),* $(,)?) => {
    mod inode {
      use super::*;
      $(#[test] fn $scenario() { super::$scenario(|options| Driver::sim(Mode::Inode, options)); })*
    }
    mod prefix {
      use super::*;
      $(#[test] fn $scenario() { super::$scenario(|options| Driver::sim(Mode::Prefix, options)); })*
    }
    mod handle {
      use super::*;
      $(#[test] fn $scenario() { super::$scenario(|options| Driver::sim(Mode::Handle, options)); })*
    }
    mod poll {
      use super::*;
      $(#[test] fn $scenario() { super::$scenario(Driver::poll); })*
    }
  };
}

on_every_backend!(
  s1_write_in_place,
  s2_atomic_save,
  s3_delete_then_create,
  s4_missing_then_file,
  s5_missing_then_dir,
  s6_missing_chain,
  s7_file_becomes_dir,
  s8_dir_becomes_file,
  s9_new_entries,
  s10_tree_moved_in,
  s11_subdir_leaves,
  s12_watched_dir_away_and_back,
  s13_ancestor_away_and_back,
  s14_ignored_dir,
);
