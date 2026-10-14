//! The simulated disk of `tests/sim/fixtures`: an engine on a disk in memory, with each simulated OS
//! watch API and with polling. Time passes only when the world lets everything settle, so a
//! story takes no time here, and its end is checked against the disk (`caught_up`).

use std::path::{Path, PathBuf};

use crate::fixtures::{Deliver, Driver, Mode, caught_up};
use pathwatch::{EventKind, engine::Options, poll::PollBackend};

use crate::{Settings, World, differences, expected_history, history_of};

/// The root of every story on the simulated disk.
const ROOT: &str = "/project";

/// Runs the story on each simulated backend and on the poll backend, each on a new disk.
pub fn each_backend(settings: &Settings, story: &(impl Fn(&mut dyn World) + Sync)) {
  let options = options(settings);
  for mode in Mode::ALL {
    let name = match mode {
      Mode::Inode => "sim inotify",
      Mode::Prefix => "sim FSEvents",
      Mode::Handle => "sim Windows",
    };
    let mut sim = Sim::new(Driver::sim(mode, options.clone()), name);
    story(&mut sim);
    sim.end();
  }
  let mut sim = Sim::new(Driver::<PollBackend>::poll(options), "sim polling");
  story(&mut sim);
  sim.end();
}

fn options(settings: &Settings) -> Options {
  let mut options = Options::default();
  if let Some(debounce) = settings.debounce {
    options.debounce = debounce;
  }
  if let Some(delay) = settings.delete_delay {
    options.delete_delay = delay;
  }
  options.ignored.clone_from(&settings.ignored);
  options
}

pub struct Sim<B: Deliver> {
  driver: Driver<B>,
  name: &'static str,
  passing: Vec<PathBuf>,
}

impl<B: Deliver> Sim<B> {
  fn new(driver: Driver<B>, name: &'static str) -> Self {
    driver.fs.mkdir_all(ROOT).unwrap();
    Self { driver, name, passing: Vec::new() }
  }

  /// The end of the story: everything settles, and the engine has caught up with the disk.
  fn end(&mut self) {
    caught_up(&mut self.driver);
  }

  fn paths(&self, rels: &[&str]) -> Vec<PathBuf> {
    rels.iter().map(|rel| self.path(rel)).collect()
  }
}

impl<B: Deliver> World for Sim<B> {
  fn path(&self, rel: &str) -> PathBuf {
    Path::new(ROOT).join(rel)
  }

  fn name(&self) -> &'static str {
    self.name
  }

  fn polls(&self) -> bool {
    self.name == "sim polling"
  }

  fn watch(&mut self, rels: &[&str]) {
    self.driver.watch(&self.paths(rels));
  }

  fn unwatch(&mut self, rels: &[&str]) {
    self.driver.unwatch(&self.paths(rels));
  }

  fn write(&mut self, rel: &str, content: &str) {
    let path = self.path(rel);
    self.driver.fs.mkdir_all(path.parent().unwrap()).unwrap();
    self.driver.fs.write(path, content).unwrap();
  }

  fn mkdir(&mut self, rel: &str) {
    self.driver.fs.mkdir_all(self.path(rel)).unwrap();
  }

  fn remove(&mut self, rel: &str) {
    let path = self.path(rel);
    let fs = &self.driver.fs;
    if fs.read_link(&path).is_none() && fs.dir_object(&path).is_some() {
      fs.remove_all(path).unwrap();
    } else {
      fs.remove(path).unwrap();
    }
  }

  fn rename(&mut self, from: &str, to: &str) {
    self.driver.fs.rename(self.path(from), self.path(to)).unwrap();
  }

  fn symlink(&mut self, target: &str, link: &str) -> bool {
    self.driver.fs.symlink(self.path(target), self.path(link)).unwrap();
    true
  }

  fn expect(&mut self, expected: &[(EventKind, &str)]) {
    self.driver.settle();
    let events = self.driver.take_events();
    let expected = expected_history(expected, |rel| self.path(rel));
    let got = history_of(&events, &self.passing);
    assert!(got == expected, "{}: {}", self.name, differences(&expected, &got));
  }

  fn may_pass(&mut self, rel: &str) {
    let path = self.path(rel);
    self.passing.push(path);
  }
}
