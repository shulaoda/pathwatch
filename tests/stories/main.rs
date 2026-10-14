//! The stories: what a user does on the disk, and the events that pathwatch reports for it.
//!
//! A story is one function. It sets the disk up, watches, changes the disk and says what the
//! handler gets, with the words of [`World`]. The same story runs in every world that can
//! play it: [`everywhere`] runs it on the real disk, with the watch API of this OS and with
//! polling, and (on Unix) on the simulated disk, with each simulated OS watch API and with
//! polling. The events are the same everywhere: that is the contract.
//!
//! A story that needs more than the words of [`World`] (a socket, a handle that Windows holds
//! open, the clock of the disk, the watcher itself) runs on the real disk alone, with
//! [`on_disk`], and gets the [`Disk`] with everything it has.
//!
//! - `contract`: the rows S1 to S14 of `references/design.md`.
//! - `promises`: the promises of the README.
//! - `projects`: what editors, package managers and big projects do.
//! - `engine`: what the engine has to get right below the promises.
//! - `operations`: every way a user writes, renames and links a file.

mod contract;
mod disk;
mod engine;
/// The fixtures of `tests/sim`: the simulated disk and its backends. The stories use a part.
#[cfg(unix)]
#[path = "../sim/fixtures/mod.rs"]
#[expect(dead_code, unused_imports, reason = "the stories use a part of the fixtures")]
mod fixtures;
mod operations;
mod projects;
mod promises;
#[cfg(unix)]
mod sim;

use std::{
  collections::BTreeMap,
  path::{Path, PathBuf},
  sync::Arc,
  time::Duration,
};

use pathwatch::{EntryKind, Event, EventKind, engine::Ignored};

pub use crate::disk::{Disk, Inside};

/// The words of a story. Every path is relative to the root of the world, with `/` between
/// the names.
pub trait World {
  /// The absolute path of `rel` in this world.
  fn path(&self, rel: &str) -> PathBuf;
  /// `native` or `polling` on the real disk; the name of the simulated backend otherwise.
  fn name(&self) -> &'static str;
  /// Whether the world polls the disk in place of an OS watch.
  fn polls(&self) -> bool;
  /// Whether the disk takes two spellings of a name for one (the case of letters).
  fn folds_case(&self) -> bool;

  fn watch(&mut self, rels: &[&str]);
  fn unwatch(&mut self, rels: &[&str]);

  /// Writes a file, making the directories above it.
  fn write(&mut self, rel: &str, content: &str);
  /// Appends to a file that is there.
  fn append(&mut self, rel: &str, content: &str);
  fn mkdir(&mut self, rel: &str);
  /// Removes a file, a link, or a directory with everything in it.
  fn remove(&mut self, rel: &str);
  fn rename(&mut self, from: &str, to: &str);
  /// Makes a symbolic link at `link` to `target`. `false`: this user may not make one (Windows
  /// without the developer mode), so the story has nothing to check.
  fn symlink(&mut self, target: &str, link: &str) -> bool;

  /// Time passes. The one way to see that nothing comes after a while: a story uses it where a
  /// late event would be a mistake.
  fn pass(&mut self, time: Duration);

  /// Waits until the events since the last `expect` are the expected ones, and then nothing
  /// else comes. For one path the order counts; an `Update` right after a `Create` or an
  /// `Update` of the same path is fine.
  fn expect(&mut self, expected: &[(EventKind, &str)]);
  /// The file at `rel` comes and goes between two `expect` calls: a `Create` and a `Delete` of
  /// it are fine, and so is nothing.
  fn may_pass(&mut self, rel: &str);
}

/// What a story asks of the watcher and of the disk. `None`: the default.
#[derive(Clone, Default)]
pub struct Settings {
  pub debounce: Option<Duration>,
  /// A story that sets it is about the delete delay: it skips polling, which has none.
  pub delete_delay: Option<Duration>,
  pub ignored: Option<Ignored>,
  /// The story needs a disk that takes two spellings of a name for one. The simulated disk
  /// is made so; the real disk is probed, and the story is skipped where it is not so.
  pub loose_disk: bool,
}

impl Settings {
  pub fn loose() -> Self {
    Self { loose_disk: true, ..Self::default() }
  }

  pub fn delete_delay(delay: Duration) -> Self {
    Self { delete_delay: Some(delay), ..Self::default() }
  }

  pub fn debounce(debounce: Duration) -> Self {
    Self { debounce: Some(debounce), ..Self::default() }
  }

  pub fn ignored(ignored: impl Fn(&Path, EntryKind) -> bool + Send + Sync + 'static) -> Self {
    Self { ignored: Some(Arc::new(ignored)), ..Self::default() }
  }

  pub fn ignore_node_modules() -> Self {
    Self::ignored(|path, kind| kind == EntryKind::Dir && path.ends_with("node_modules"))
  }
}

/// Runs the story in every world, with the default settings.
pub fn everywhere(story: impl Fn(&mut dyn World) + Sync) {
  everywhere_with(&Settings::default(), story);
}

/// Runs the story in every world: on the simulated disk first (it takes no time), then on the
/// real disk.
pub fn everywhere_with(settings: &Settings, story: impl Fn(&mut dyn World) + Sync) {
  #[cfg(unix)]
  sim::each_backend(settings, &story);
  disk::each_backend(settings, None, |disk| story(disk));
}

/// Runs the story on the real disk alone, with the watch API of this OS and with polling.
pub fn on_disk(story: impl Fn(&mut Disk) + Sync) {
  disk::each_backend(&Settings::default(), None, story);
}

pub fn on_disk_with(settings: &Settings, story: impl Fn(&mut Disk) + Sync) {
  disk::each_backend(settings, None, story);
}

/// `on_disk`, with `inside` run inside the handler for each batch, before the story gets it.
pub fn on_disk_inside(inside: Inside, story: impl Fn(&mut Disk) + Sync) {
  disk::each_backend(&Settings::default(), Some(inside), story);
}

/// Says on stderr that the running story skips a world, and why: a skipped world is never
/// silent. The name of the story is the name of its thread.
#[expect(clippy::print_stderr, reason = "a skipped world must be seen")]
pub fn skipped(why: &str) {
  let thread = std::thread::current();
  eprintln!("skipped: {}: {why}", thread.name().unwrap_or("a story"));
}

/// The events of each path, in the order in which they came.
pub type History = BTreeMap<PathBuf, Vec<EventKind>>;

/// The history of these events. An `Update` right after a `Create` or an `Update` of the same
/// path is left out, and a path of `passing` that only came and went is left out.
pub fn history_of(events: &[Event], passing: &[PathBuf]) -> History {
  let mut history = History::new();
  for event in events {
    let kinds = history.entry(event.path.clone()).or_default();
    let extra_update = event.kind == EventKind::Update
      && matches!(kinds.last(), Some(EventKind::Create | EventKind::Update));
    if !extra_update {
      kinds.push(event.kind);
    }
  }
  for path in passing {
    if history.get(path).is_some_and(|kinds| kinds == &[EventKind::Create, EventKind::Delete]) {
      history.remove(path);
    }
  }
  history
}

/// The history that a story expects, with its paths made absolute by `path`.
pub fn expected_history(expected: &[(EventKind, &str)], path: impl Fn(&str) -> PathBuf) -> History {
  let mut history = History::new();
  for &(kind, rel) in expected {
    history.entry(path(rel)).or_default().push(kind);
  }
  history
}

pub fn differences(expected: &History, got: &History) -> String {
  let none = Vec::new();
  let lines: Vec<String> = expected
    .keys()
    .chain(got.keys().filter(|path| !expected.contains_key(*path)))
    .map(|path| (path, expected.get(path).unwrap_or(&none), got.get(path).unwrap_or(&none)))
    .filter(|(_, expected, got)| expected != got)
    .map(|(path, expected, got)| {
      format!("  {}: expected {expected:?}, got {got:?}", path.display())
    })
    .collect();
  // A big story can differ in thousands of paths.
  let shown = lines.iter().take(20).cloned().collect::<Vec<_>>().join("\n");
  format!("the events of {} paths are not the expected ones:\n{shown}", lines.len())
}

/// One event of `kind` for each path.
pub fn each(kind: EventKind, paths: &[String]) -> Vec<(EventKind, &str)> {
  paths.iter().map(|path| (kind, path.as_str())).collect()
}
