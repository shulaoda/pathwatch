//! The real disk: a temporary directory, and a watcher with the watch API of this OS or with
//! polling. A story runs on both at the same time. No story sleeps: it waits for the events it
//! expects, and a marker file tells when everything before it has been reported.

use std::{
  collections::{BTreeMap, BTreeSet},
  env, fs,
  path::{Component, Path, PathBuf},
  sync::{
    Arc, Weak,
    mpsc::{self, Receiver, RecvTimeoutError},
  },
  thread,
  time::{Duration, Instant, SystemTime},
};

use pathwatch::{Config, Error, Event, EventKind, Watcher};
use tempfile::TempDir;

use crate::{History, Settings, World, differences, expected_history, history_of};

pub const PATIENCE: Duration = Duration::from_secs(30);
/// Longer than the default debounce and the delete delay together.
pub const QUIET: Duration = Duration::from_millis(300);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Runs inside the handler, with the watcher and the batch, before the story gets the batch.
pub type Inside = fn(&Watcher, &[Event]);

/// Runs the story with the watch API of this OS and with polling, at the same time.
pub fn each_backend(settings: &Settings, inside: Option<Inside>, story: impl Fn(&mut Disk) + Sync) {
  thread::scope(|scope| {
    for polling in [false, true] {
      let story = &story;
      scope.spawn(move || {
        let config = config(settings).with_polling(polling).with_poll_interval(POLL_INTERVAL);
        let mut disk = Disk::start(config, inside, if polling { "polling" } else { "native" });
        story(&mut disk);
        assert!(disk.events.is_empty(), "{}: a story ends with `expect`", disk.backend);
      });
    }
  });
}

fn config(settings: &Settings) -> Config {
  let mut config = Config::default();
  if let Some(debounce) = settings.debounce {
    config = config.with_debounce(debounce);
  }
  if let Some(delay) = settings.delete_delay {
    config = config.with_delete_delay(delay);
  }
  if let Some(ignored) = settings.ignored.clone() {
    config = config.with_ignored(move |path, kind| ignored(path, kind));
  }
  config
}

/// A temporary directory and a relative path to it from the current directory: in the target
/// directory, or in the current one when the target is on another drive (Windows).
pub fn relative_scratch() -> (TempDir, PathBuf) {
  let here = env::current_dir().unwrap();
  let mut dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
  if here.components().next() != dir.path().components().next() {
    dir = tempfile::tempdir_in(&here).unwrap();
  }
  let is_name = |part: &Component| matches!(part, Component::Normal(_));
  let mut rel: PathBuf = here.components().filter(is_name).map(|_| Component::ParentDir).collect();
  rel.extend(dir.path().components().filter(is_name));
  (dir, rel)
}

/// Makes a symbolic link at `link` to `target`, to a directory or to a file as the target is.
/// `false`: this user may not make one (Windows without the developer mode).
pub fn symlink(target: &Path, link: &Path) -> bool {
  #[cfg(unix)]
  {
    std::os::unix::fs::symlink(target, link).unwrap();
    true
  }
  #[cfg(windows)]
  {
    use std::os::windows::fs::{symlink_dir, symlink_file};
    const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;
    let made = if target.is_dir() { symlink_dir(target, link) } else { symlink_file(target, link) };
    match made {
      Ok(()) => true,
      Err(error) if error.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD) => false,
      Err(error) => panic!("a symbolic link cannot be made: {error}"),
    }
  }
  #[cfg(not(any(unix, windows)))]
  {
    let _ = (target, link);
    false
  }
}

pub struct Disk {
  // Dropped before the directories, so their removal is not watched.
  pub watcher: Arc<Watcher>,
  pub results: Receiver<Result<Vec<Event>, Error>>,
  /// Not made canonical: on macOS it lies behind a link, and event paths must be the given ones.
  pub dir: TempDir,
  /// A directory of its own for the markers of `sync`, watched from the start.
  markers: TempDir,
  /// The number of the last marker written, and of the last one reported.
  written: usize,
  reported: usize,
  /// The events since the last `expect`.
  pub events: Vec<Event>,
  /// The paths of `may_pass`.
  passing: Vec<PathBuf>,
  pub backend: &'static str,
}

/// Files with their content.
pub type Files = BTreeMap<PathBuf, Vec<u8>>;

/// What a handler reads at the path: nothing for a path that is gone or a directory.
fn content(path: &Path) -> Vec<u8> {
  fs::read(path).unwrap_or_default()
}

/// Puts into `files` what a watched path at `path` stands for.
pub fn files_at(path: &Path, files: &mut Files) {
  if path.is_file() {
    files.insert(path.to_path_buf(), content(path));
  } else if let Ok(entries) = fs::read_dir(path) {
    for entry in entries {
      files_at(&entry.unwrap().path(), files);
    }
  }
}

fn unknown(known: &Files, on_disk: &Files) -> String {
  let other = |these: &Files, those: &Files| -> Vec<PathBuf> {
    these
      .iter()
      .filter(|(path, content)| those.get(*path) != Some(content))
      .map(|(path, _)| path.clone())
      .collect()
  };
  format!(
    "the handler does not know these files as they are: {:?}, and it knows these, which are gone or other: {:?}",
    other(on_disk, known),
    other(known, on_disk)
  )
}

impl Disk {
  fn start(config: Config, inside: Option<Inside>, backend: &'static str) -> Self {
    let (sender, results) = mpsc::channel();
    let watcher = Arc::new_cyclic(|this: &Weak<Watcher>| {
      let this = Weak::clone(this);
      Watcher::with_config(config, move |result| {
        if let (Some(inside), Ok(events), Some(watcher)) = (inside, &result, this.upgrade()) {
          inside(&watcher, events);
        }
        // The story may be over already.
        let _ = sender.send(result);
      })
      .unwrap()
    });
    let dir = tempfile::tempdir().unwrap();
    let markers = tempfile::tempdir().unwrap();
    watcher.watch(&[markers.path()]).unwrap();
    Self {
      watcher,
      results,
      dir,
      markers,
      written: 0,
      reported: 0,
      events: Vec::new(),
      passing: Vec::new(),
      backend,
    }
  }

  pub fn path(&self, rel: &str) -> PathBuf {
    self.dir.path().join(rel)
  }

  pub fn watch(&self, rels: &[&str]) {
    self.watcher.watch(&rels.iter().map(|rel| self.path(rel)).collect::<Vec<_>>()).unwrap();
  }

  pub fn unwatch(&self, rels: &[&str]) {
    self.watcher.unwatch(&rels.iter().map(|rel| self.path(rel)).collect::<Vec<_>>()).unwrap();
  }

  /// Writes a file, making the directories above it.
  pub fn write(&self, rel: &str, content: &str) {
    let path = self.path(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
  }

  pub fn mkdir(&self, rel: &str) {
    fs::create_dir_all(self.path(rel)).unwrap();
  }

  pub fn remove(&self, rel: &str) {
    let path = self.path(rel);
    let kind = fs::symlink_metadata(&path).unwrap().file_type();
    // A link to a directory is removed as a file; Windows removes it as a directory.
    #[cfg(windows)]
    if std::os::windows::fs::FileTypeExt::is_symlink_dir(&kind) {
      return fs::remove_dir(path).unwrap();
    }
    if kind.is_dir() {
      fs::remove_dir_all(path).unwrap();
    } else {
      fs::remove_file(path).unwrap();
    }
  }

  pub fn rename(&self, from: &str, to: &str) {
    fs::rename(self.path(from), self.path(to)).unwrap();
  }

  pub fn symlink(&self, target: &str, link: &str) -> bool {
    symlink(&self.path(target), &self.path(link))
  }

  pub fn set_modified(&self, rel: &str, time: SystemTime) {
    fs::File::options().write(true).open(self.path(rel)).unwrap().set_modified(time).unwrap();
  }

  /// Returns when every event of the changes made before the call has come: a marker file is
  /// reported, twice, since the poll that finds the first one may have started before the
  /// changes and looked at their places already. The second one starts after that poll.
  pub fn sync(&mut self) {
    for _ in 0..2 {
      self.written += 1;
      fs::write(self.markers.path().join(self.written.to_string()), "").unwrap();
      let end = Instant::now() + PATIENCE;
      while self.reported < self.written {
        let left = end.saturating_duration_since(Instant::now());
        assert!(self.receive(left), "{}: marker {} is not reported", self.backend, self.written);
      }
    }
  }

  pub fn may_pass(&mut self, rel: &str) {
    self.passing.push(self.path(rel));
  }

  /// Waits until the events since the last `expect` are the expected ones.
  pub fn wait(&mut self, expected: &[(EventKind, &str)]) {
    let expected = self.expected(expected);
    let end = Instant::now() + PATIENCE;
    while self.history() != expected {
      let left = end.saturating_duration_since(Instant::now());
      assert!(self.receive(left), "{}: {}", self.backend, differences(&expected, &self.history()));
    }
  }

  /// `wait`, and then nothing else may come for a quiet time. The events start empty after it.
  pub fn expect(&mut self, expected: &[(EventKind, &str)]) {
    self.wait(expected);
    self.sync();
    let end = Instant::now() + QUIET;
    while self.receive(end.saturating_duration_since(Instant::now())) {}
    let expected = self.expected(expected);
    let history = self.history();
    assert!(history == expected, "{}: {}", self.backend, differences(&expected, &history));
    self.events.clear();
  }

  /// For a story that cannot tell which events come: every event must fit what the handler
  /// knows, with the content it read when the event came, until that is `on_disk` and stays so
  /// for a quiet time. An `Update` that never came leaves the content the handler read before.
  pub fn expect_known(&mut self, known: &mut Files, on_disk: &Files) {
    let patience = Instant::now() + PATIENCE;
    loop {
      self.learn(known);
      if known != on_disk {
        let left = patience.saturating_duration_since(Instant::now());
        assert!(self.receive(left), "{}: {}", self.backend, unknown(known, on_disk));
        continue;
      }
      // Events still on their way can take `known` away from `on_disk` again.
      self.sync();
      self.learn(known);
      if known != on_disk {
        continue;
      }
      let end = Instant::now() + QUIET;
      while self.receive(end.saturating_duration_since(Instant::now())) {
        self.learn(known);
      }
      if known == on_disk {
        return;
      }
    }
  }

  /// Takes the events since the last call into what the handler knows.
  fn learn(&mut self, known: &mut Files) {
    for event in self.events.drain(..) {
      let fits = match event.kind {
        EventKind::Create => known.insert(event.path.clone(), content(&event.path)).is_none(),
        EventKind::Update => known.insert(event.path.clone(), content(&event.path)).is_some(),
        EventKind::Delete => known.remove(&event.path).is_some(),
      };
      assert!(fits, "{}: {event:?} does not fit what the handler knows", self.backend);
    }
  }

  fn expected(&self, expected: &[(EventKind, &str)]) -> History {
    expected_history(expected, |rel| self.path(rel))
  }

  fn history(&self) -> History {
    history_of(&self.events, &self.passing)
  }

  /// Takes the next batch, with the markers taken out. False: none came in time.
  pub fn receive(&mut self, timeout: Duration) -> bool {
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
        for event in events {
          match event.path.strip_prefix(self.markers.path()) {
            Ok(marker) => {
              let number = marker.to_str().and_then(|text| text.parse().ok()).expect("a marker");
              self.reported = self.reported.max(number);
            }
            Err(_) => self.events.push(event),
          }
        }
        true
      }
      Ok(Err(error)) => panic!("{}: the watcher reported: {error}", self.backend),
      Err(RecvTimeoutError::Timeout) => false,
      Err(RecvTimeoutError::Disconnected) => panic!("{}: the watcher thread is gone", self.backend),
    }
  }
}

impl World for Disk {
  fn path(&self, rel: &str) -> PathBuf {
    Disk::path(self, rel)
  }

  fn name(&self) -> &'static str {
    self.backend
  }

  fn polls(&self) -> bool {
    self.backend == "polling"
  }

  fn watch(&mut self, rels: &[&str]) {
    Disk::watch(self, rels);
  }

  fn unwatch(&mut self, rels: &[&str]) {
    Disk::unwatch(self, rels);
  }

  fn write(&mut self, rel: &str, content: &str) {
    Disk::write(self, rel, content);
  }

  fn mkdir(&mut self, rel: &str) {
    Disk::mkdir(self, rel);
  }

  fn remove(&mut self, rel: &str) {
    Disk::remove(self, rel);
  }

  fn rename(&mut self, from: &str, to: &str) {
    Disk::rename(self, from, to);
  }

  fn symlink(&mut self, target: &str, link: &str) -> bool {
    Disk::symlink(self, target, link)
  }

  fn expect(&mut self, expected: &[(EventKind, &str)]) {
    Disk::expect(self, expected);
  }

  fn may_pass(&mut self, rel: &str) {
    Disk::may_pass(self, rel);
  }
}
