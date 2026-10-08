//! The watcher on the real disk. [`each_backend`] runs a test twice: with the watch API of this
//! system and with polling, both at the same time. A path in a test is relative to a new
//! temporary directory. No test sleeps: [`Watch::expect`] waits until the events since the last
//! `expect` are the expected ones, and then for a short quiet time in which nothing else may
//! come.

mod contract;
mod projects;
mod promises;

use std::{
  collections::{BTreeMap, BTreeSet},
  fs,
  path::PathBuf,
  sync::{
    Arc, Weak,
    mpsc::{self, Receiver, RecvTimeoutError},
  },
  thread,
  time::{Duration, Instant},
};

use pathwatch::{Config, Error, Event, EventKind, Watcher};
use tempfile::TempDir;

/// How long a test waits for events that have to come.
const PATIENCE: Duration = Duration::from_secs(30);
/// How long nothing else may come after the expected events. It is longer than the default
/// debounce and the delete delay together.
const QUIET: Duration = Duration::from_millis(300);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Runs `test` with the default settings on each backend.
fn each_backend(test: impl Fn(&mut Watch) + Sync) {
  each_backend_with(&Config::default(), test);
}

/// What a test does inside the handler. It gets the watcher and one batch, before the test
/// gets the batch.
type Inside = fn(&Watcher, &[Event]);

/// Runs `test` twice: with the backend for the watch API of this system, and with polling.
fn each_backend_with(config: &Config, test: impl Fn(&mut Watch) + Sync) {
  run_each_backend(config, None, test);
}

/// Runs `test` on each backend. The handler calls `inside` for every batch.
fn each_backend_inside(inside: Inside, test: impl Fn(&mut Watch) + Sync) {
  run_each_backend(&Config::default(), Some(inside), test);
}

fn run_each_backend(config: &Config, inside: Option<Inside>, test: impl Fn(&mut Watch) + Sync) {
  side_by_side(|polling| {
    let config = config.clone().with_polling(polling).with_poll_interval(POLL_INTERVAL);
    let mut watch = Watch::start(config, inside, backend_name(polling));
    test(&mut watch);
    assert!(watch.events.is_empty(), "{}: a test ends with `expect`", watch.backend);
  });
}

/// Runs `run` for the native backend (`false`) and for polling (`true`) at the same time. The
/// two runs share nothing, and a test waits most of its time, so this halves the time of a
/// test.
fn side_by_side(run: impl Fn(bool) + Sync) {
  thread::scope(|scope| {
    for polling in [false, true] {
      let run = &run;
      scope.spawn(move || run(polling));
    }
  });
}

fn backend_name(polling: bool) -> &'static str {
  if polling { "polling" } else { "native" }
}

/// A watcher and a temporary directory for it.
struct Watch {
  // Dropped first, so that the removal of the directory is not watched. It is in an `Arc`
  // because the handler reaches it too, for `Inside`.
  watcher: Arc<Watcher>,
  results: Receiver<Result<Vec<Event>, Error>>,
  /// Not made canonical: an event path has to be the path as the test gave it to `watch`, also
  /// when the temporary directory lies behind a symbolic link (`/var` on macOS).
  dir: TempDir,
  /// The events since the last `expect`.
  events: Vec<Event>,
  /// The paths of [`Watch::may_pass`].
  passing: Vec<PathBuf>,
  /// For the text of a failure.
  backend: &'static str,
}

/// The events of each path, in the order in which they came.
type History = BTreeMap<PathBuf, Vec<EventKind>>;

impl Watch {
  fn start(config: Config, inside: Option<Inside>, backend: &'static str) -> Self {
    let (sender, results) = mpsc::channel();
    let watcher = Arc::new_cyclic(|this: &Weak<Watcher>| {
      let this = Weak::clone(this);
      Watcher::with_config(config, move |result| {
        if let (Some(inside), Ok(events), Some(watcher)) = (inside, &result, this.upgrade()) {
          inside(&watcher, events);
        }
        // The test may be over already.
        let _ = sender.send(result);
      })
      .unwrap()
    });
    let dir = tempfile::tempdir().unwrap();
    Self { watcher, results, dir, events: Vec::new(), passing: Vec::new(), backend }
  }

  fn path(&self, rel: &str) -> PathBuf {
    self.dir.path().join(rel)
  }

  fn watch(&self, rels: &[&str]) {
    self.watcher.watch_all(rels.iter().map(|rel| self.path(rel))).unwrap();
  }

  fn unwatch(&self, rels: &[&str]) {
    self.watcher.unwatch_all(rels.iter().map(|rel| self.path(rel))).unwrap();
  }

  /// Writes a file. The directories above it are made when they are missing.
  fn write(&self, rel: &str, content: &str) {
    let path = self.path(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
  }

  /// Makes a directory and the missing ones above it, like `mkdir -p`.
  fn mkdir(&self, rel: &str) {
    fs::create_dir_all(self.path(rel)).unwrap();
  }

  /// Removes a file, or a directory with all that is in it.
  fn remove(&self, rel: &str) {
    let path = self.path(rel);
    // A link to a directory is removed as a file.
    if fs::symlink_metadata(&path).unwrap().is_dir() {
      fs::remove_dir_all(path).unwrap();
    } else {
      fs::remove_file(path).unwrap();
    }
  }

  fn rename(&self, from: &str, to: &str) {
    fs::rename(self.path(from), self.path(to)).unwrap();
  }

  /// The file at `rel` is made and removed between two `expect` calls, so the watcher may find
  /// it or not: a `Create` and then a `Delete` of it are fine, and so is nothing.
  fn may_pass(&mut self, rel: &str) {
    self.passing.push(self.path(rel));
  }

  /// Waits until the events since the last `expect` are the expected ones.
  ///
  /// For one path the order counts. An `Update` right after a `Create` or an `Update` of the
  /// same path does not: the watcher may report more updates than there were writes.
  fn wait(&mut self, expected: &[(EventKind, &str)]) {
    let expected = self.expected(expected);
    let end = Instant::now() + PATIENCE;
    while self.history() != expected {
      let left = end.saturating_duration_since(Instant::now());
      assert!(self.receive(left), "{}: {}", self.backend, differences(&expected, &self.history()));
    }
  }

  /// [`Watch::wait`], and then nothing else may come for a quiet time. After that the list of
  /// events starts empty.
  fn expect(&mut self, expected: &[(EventKind, &str)]) {
    self.wait(expected);
    let end = Instant::now() + QUIET;
    while self.receive(end.saturating_duration_since(Instant::now())) {}
    let expected = self.expected(expected);
    let history = self.history();
    assert!(history == expected, "{}: {}", self.backend, differences(&expected, &history));
    self.events.clear();
  }

  /// For a test that cannot tell which events come. `known` is the set of files that the
  /// handler knows. Every event since the last `expect` must fit it (a `Create` of a file that
  /// it does not hold, an `Update` or a `Delete` of one that it holds), and is applied to it.
  /// This waits until `known` is `on_disk`, and then it must stay so for a quiet time.
  fn expect_known(&mut self, known: &mut BTreeSet<PathBuf>, on_disk: &BTreeSet<PathBuf>) {
    let mut end = Instant::now() + PATIENCE;
    let mut settled = false;
    loop {
      for event in self.events.drain(..) {
        let fits = match event.kind {
          EventKind::Create => known.insert(event.path.clone()),
          EventKind::Update => known.contains(&event.path),
          EventKind::Delete => known.remove(&event.path),
        };
        assert!(fits, "{}: {event:?} does not fit what the handler knows", self.backend);
      }
      if !settled && known == on_disk {
        settled = true;
        end = Instant::now() + QUIET;
      }
      if !self.receive(end.saturating_duration_since(Instant::now())) {
        break;
      }
    }
    assert!(
      known == on_disk,
      "{}: the handler does not know these files: {:?}, and it knows these, which are gone: {:?}",
      self.backend,
      on_disk.difference(known).collect::<Vec<_>>(),
      known.difference(on_disk).collect::<Vec<_>>()
    );
  }

  fn expected(&self, expected: &[(EventKind, &str)]) -> History {
    let mut history = History::new();
    for &(kind, rel) in expected {
      history.entry(self.path(rel)).or_default().push(kind);
    }
    history
  }

  fn history(&self) -> History {
    let mut history = History::new();
    for event in &self.events {
      let kinds = history.entry(event.path.clone()).or_default();
      let extra_update = event.kind == EventKind::Update
        && matches!(kinds.last(), Some(EventKind::Create | EventKind::Update));
      if !extra_update {
        kinds.push(event.kind);
      }
    }
    for path in &self.passing {
      if history.get(path).is_some_and(|kinds| kinds == &[EventKind::Create, EventKind::Delete]) {
        history.remove(path);
      }
    }
    history
  }

  /// Takes the next batch, and checks the rules for a batch. False: none came in time.
  fn receive(&mut self, timeout: Duration) -> bool {
    match self.results.recv_timeout(timeout) {
      Ok(Ok(events)) => {
        assert!(!events.is_empty(), "{}: a batch is never empty", self.backend);
        // The order of a batch is not promised.
        let paths: BTreeSet<&PathBuf> = events.iter().map(|event| &event.path).collect();
        assert_eq!(
          paths.len(),
          events.len(),
          "{}: a batch has one event for a path: {events:?}",
          self.backend
        );
        self.events.extend(events);
        true
      }
      Ok(Err(error)) => panic!("{}: the watcher reported: {error}", self.backend),
      Err(RecvTimeoutError::Timeout) => false,
      Err(RecvTimeoutError::Disconnected) => panic!("{}: the watcher thread is gone", self.backend),
    }
  }
}

/// One event of `kind` for each of `paths`.
fn each(kind: EventKind, paths: &[String]) -> Vec<(EventKind, &str)> {
  paths.iter().map(|path| (kind, path.as_str())).collect()
}

/// The paths whose events are not the expected ones, for the text of a failure.
fn differences(expected: &History, got: &History) -> String {
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
  // A big test can differ in thousands of paths.
  let shown = lines.iter().take(20).cloned().collect::<Vec<_>>().join("\n");
  format!("the events of {} paths are not the expected ones:\n{shown}", lines.len())
}
