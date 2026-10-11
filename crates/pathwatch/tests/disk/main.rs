//! The watcher on the real disk. Each test runs twice at the same time, with the watch API of
//! this system and with polling, in a fresh temporary directory. No test sleeps.

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

const PATIENCE: Duration = Duration::from_secs(30);
/// Longer than the default debounce and the delete delay together.
const QUIET: Duration = Duration::from_millis(300);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

fn each_backend(test: impl Fn(&mut Watch) + Sync) {
  each_backend_with(&Config::default(), test);
}

/// Runs inside the handler, with the watcher and the batch, before the test gets the batch.
type Inside = fn(&Watcher, &[Event]);

fn each_backend_with(config: &Config, test: impl Fn(&mut Watch) + Sync) {
  run_each_backend(config, None, test);
}

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

/// Runs `run` with `false` for the native backend and `true` for polling, at the same time.
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

struct Watch {
  // Dropped before `dir`, so the removal of the directory is not watched.
  watcher: Arc<Watcher>,
  results: Receiver<Result<Vec<Event>, Error>>,
  /// Not made canonical: on macOS it lies behind a link, and event paths must be the given ones.
  dir: TempDir,
  /// The events since the last `expect`.
  events: Vec<Event>,
  /// The paths of `may_pass`.
  passing: Vec<PathBuf>,
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

  /// Writes a file, making the directories above it.
  fn write(&self, rel: &str, content: &str) {
    let path = self.path(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
  }

  fn mkdir(&self, rel: &str) {
    fs::create_dir_all(self.path(rel)).unwrap();
  }

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

  /// The file at `rel` comes and goes between two `expect` calls: a `Create` and a `Delete` of
  /// it are fine, and so is nothing.
  fn may_pass(&mut self, rel: &str) {
    self.passing.push(self.path(rel));
  }

  /// Waits until the events since the last `expect` are the expected ones. For one path the
  /// order counts; an `Update` right after a `Create` or an `Update` of the same path is fine.
  fn wait(&mut self, expected: &[(EventKind, &str)]) {
    let expected = self.expected(expected);
    let end = Instant::now() + PATIENCE;
    while self.history() != expected {
      let left = end.saturating_duration_since(Instant::now());
      assert!(self.receive(left), "{}: {}", self.backend, differences(&expected, &self.history()));
    }
  }

  /// `wait`, and then nothing else may come for a quiet time. The events start empty after it.
  fn expect(&mut self, expected: &[(EventKind, &str)]) {
    self.wait(expected);
    let end = Instant::now() + QUIET;
    while self.receive(end.saturating_duration_since(Instant::now())) {}
    let expected = self.expected(expected);
    let history = self.history();
    assert!(history == expected, "{}: {}", self.backend, differences(&expected, &history));
    self.events.clear();
  }

  /// For a test that cannot tell which events come: every event must fit `known` and is applied
  /// to it, until `known` is `on_disk` and stays so for a quiet time.
  fn expect_known(&mut self, known: &mut BTreeSet<PathBuf>, on_disk: &BTreeSet<PathBuf>) {
    let patience = Instant::now() + PATIENCE;
    let mut end = patience;
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
      // Events still on their way can take `known` away from `on_disk` again: the quiet time
      // starts anew.
      if known != on_disk {
        (settled, end) = (false, patience);
      } else if !settled {
        (settled, end) = (true, Instant::now() + QUIET);
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

  /// Takes the next batch. False: none came in time.
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

fn each(kind: EventKind, paths: &[String]) -> Vec<(EventKind, &str)> {
  paths.iter().map(|path| (kind, path.as_str())).collect()
}

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
