//! The engine: it watches paths through a backend, keeps the tree in line with the disk, and
//! turns hints into file events. It has no thread and reads no clock: the caller gives the time.

mod check;
mod events;
mod filter;
mod os_watch;
mod scan;
mod test_hooks;
mod watched_paths;

use std::{
  cmp::Reverse,
  collections::BinaryHeap,
  fmt, io, mem,
  path::{Path, PathBuf},
  sync::Arc,
  time::{Duration, Instant, SystemTime},
};

use rustc_hash::FxHashMap;

use self::{check::Listed, scan::ScanEntry};
pub use crate::tree::Fingerprint;
use crate::{
  backend::{Backend, DirKey, Signal},
  config::EntryKind,
  error::{Error, ErrorKind},
  event::Event,
  fs::Fs,
  path::normalize,
  tree::{Depth, KEPT, Mark, Node, Tree, emptied},
};

/// A batch goes out at least this often while hints keep coming, or every four quiet times.
const LONGEST_WAIT: Duration = Duration::from_millis(500);

/// A poll, or the scans after a loss, rests this many times as long as it took: such work
/// takes a tenth of the time at most.
const REST: u32 = 9;

/// A flush counts for no more than this in the rest: the clock runs on while the machine
/// sleeps, and a flush that the sleep fell into would rest for hours.
const LONGEST_FLUSH: Duration = Duration::from_secs(60);

/// No time of the settings is longer: `Duration::MAX` is a way to say "never", and a time that
/// far ahead of now is no `Instant`.
const LONGEST_SETTING: Duration = Duration::from_hours(365 * 24);

/// The ignore filter: `true` for a path that must not be reported. It must give the same
/// answer for the same path and kind: the node keeps it.
pub type Ignored = Arc<dyn Fn(&Path, EntryKind) -> bool + Send + Sync>;

/// The settings of an [`Engine`]. `Options::default()` is for the tests (a quiet time of
/// 10 ms); `Config` makes the ones of a watcher.
#[derive(Clone)]
pub struct Options {
  pub ignored: Option<Ignored>,
  /// The quiet time: the disk is checked when no hint came for this long. Zero: at once.
  pub debounce: Duration,
  /// How long a `Delete` waits for the file to come back. Zero turns it off.
  pub delete_delay: Duration,
  /// The shortest time between two polls of what no OS watch covers.
  pub poll_interval: Duration,
  /// The same once the backend has stopped and everything is polled. `None`: no other time.
  pub poll_interval_alone: Option<Duration>,
  /// Every disk takes two spellings of a name for one (macOS, Windows). Without it, a directory
  /// counts as such a disk until a look proves otherwise.
  pub loose_spelling: bool,
}

impl Default for Options {
  fn default() -> Self {
    Self {
      ignored: None,
      debounce: Duration::from_millis(10),
      delete_delay: Duration::from_millis(100),
      poll_interval: Duration::from_secs(1),
      poll_interval_alone: None,
      loose_spelling: false,
    }
  }
}

impl fmt::Debug for Options {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Options")
      .field("ignored", &self.ignored.is_some())
      .field("debounce", &self.debounce)
      .field("delete_delay", &self.delete_delay)
      .field("poll_interval", &self.poll_interval)
      .field("poll_interval_alone", &self.poll_interval_alone)
      .field("loose_spelling", &self.loose_spelling)
      .finish()
  }
}

/// What one call of the engine hands over: one batch of events, one for a path, and the
/// errors. Take them out before the next call.
#[derive(Debug, Default)]
pub struct Output {
  pub events: Vec<Event>,
  pub errors: Vec<Error>,
}

/// Watches paths through a [`Backend`] and a disk ([`Fs`]), and turns hints into file events.
pub struct Engine<B: Backend, F: Fs> {
  /// `None`: the backend died and was dropped; every node is polled.
  backend: Option<B>,
  fs: F,
  options: Options,
  tree: Tree,

  // When the next work is due.
  /// The next flush. `None`: none waits.
  flush_at: Option<Instant>,
  /// The latest time for that flush, so hints that keep coming do not hold it back.
  flush_by: Option<Instant>,
  /// The next poll. `None` while nothing is registered.
  poll_at: Option<Instant>,
  /// The held nodes by the time of their second look, the earliest on top. An entry can be stale.
  hold_queue: BinaryHeap<Reverse<(Instant, DirKey)>>,
  /// The scans that loss reports ask for wait until then.
  rest_until: Option<Instant>,
  /// A loss report waits for the end of that rest.
  loss_waits: bool,

  // The running flush.
  /// The time of the running flush. `None` before the first.
  now: Option<Instant>,
  /// The time of the disk at the start of the flush: the rule for young files compares with it.
  flush_time: SystemTime,
  /// `Fs::ticks` at the start of the flush.
  flush_ticks: Duration,
  /// How long the backend took to make watches live.
  commit_took: Duration,
  /// The nodes the flush changed: `finish` reads their events. A key can be stale.
  touched: Vec<DirKey>,
  /// The registered names whose watch target the filter ignores: the end of the flush drops
  /// the watched paths below them.
  dropped: Vec<DirKey>,
  /// The directories that give their own watch back after the next commit.
  covered: Vec<DirKey>,
  /// With loose spelling: what the listing of the running scan said of the nodes it named.
  listed: FxHashMap<DirKey, Listed>,
  /// A read of the disk failed during the running scan.
  scan_had_error: bool,

  // Buffers, kept between flushes so that a flush asks for no memory (see `emptied`).
  /// Every path for the disk is built here.
  path: PathBuf,
  /// The marks of a flush, in the order of their checks.
  marks: Vec<(usize, DirKey, DirKey, Option<Instant>)>,
  /// The scans of one round.
  scans: Vec<(DirKey, Depth)>,
  /// The nodes of the directory that is scanned.
  names: Vec<ScanEntry>,

  /// The errors for the next output, one of each kind.
  errors: Vec<Error>,
}

impl<B: Backend, F: Fs> Engine<B, F> {
  pub fn new(backend: B, fs: F, mut options: Options) -> Self {
    for time in [&mut options.debounce, &mut options.delete_delay, &mut options.poll_interval] {
      *time = (*time).min(LONGEST_SETTING);
    }
    options.poll_interval_alone = options.poll_interval_alone.map(|time| time.min(LONGEST_SETTING));
    let mut tree = Tree::new();
    tree.loose_spelling = options.loose_spelling;
    Self {
      backend: Some(backend),
      fs,
      options,
      tree,
      flush_at: None,
      flush_by: None,
      poll_at: None,
      hold_queue: BinaryHeap::new(),
      rest_until: None,
      loss_waits: false,
      now: None,
      flush_time: SystemTime::UNIX_EPOCH,
      flush_ticks: Duration::ZERO,
      commit_took: Duration::ZERO,
      touched: Vec::new(),
      dropped: Vec::new(),
      covered: Vec::new(),
      listed: FxHashMap::default(),
      scan_had_error: false,
      path: PathBuf::new(),
      marks: Vec::new(),
      scans: Vec::new(),
      names: Vec::new(),
      errors: Vec::new(),
    }
  }

  /// Makes every path a watched path and checks the disk once: what exists then is the baseline,
  /// known without an event. A path that cannot be used goes to `out.errors`.
  pub fn watch(&mut self, paths: Vec<PathBuf>, now: Instant, out: &mut Output) {
    for path in paths {
      match normalize(&path) {
        Some(clean) => self.register(&clean),
        None => self.report(ErrorKind::InvalidPath, Some(path), None),
      }
    }
    self.flush(now);
    self.finish(out);
    self.poll_at.get_or_insert(now + self.options.poll_interval);
    self.stop_poll_if_idle();
  }

  /// Takes back what `watch` did for every path, and reports nothing for them.
  pub fn unwatch(&mut self, paths: Vec<PathBuf>, now: Instant, out: &mut Output) {
    for path in paths {
      if let Some(clean) = normalize(&path) {
        self.unregister(&clean);
      }
    }
    // What still stands may need another watch now, or another look.
    self.flush(now);
    self.finish(out);
    self.stop_poll_if_idle();
  }

  /// Takes one signal of the backend: it only makes marks, and [`Engine::tick`] reads the disk.
  pub fn signal(&mut self, signal: Signal<B::Raw>, now: Instant) {
    let mut left_work = false;
    match signal {
      Signal::Raw(raw) => {
        let tree = &mut self.tree;
        if let Some(backend) = &mut self.backend {
          tree.hint_time = Some(now);
          backend.translate(raw, &mut |hint| left_work |= tree.hint(hint));
          tree.hint_time = None;
        }
      }
      Signal::Failed(error) => {
        left_work = self.backend.is_some();
        self.backend_died(&error);
        // The poll that was set with the old interval may be far away.
        let next = now + self.options.poll_interval;
        self.poll_at = self.poll_at.map(|at| at.min(next));
      }
    }
    if left_work {
      self.schedule(now);
    }
  }

  /// Signals were dropped before they reached the engine: everything has to be checked.
  pub fn overflow(&mut self, now: Instant) {
    self.tree.lost = true;
    self.schedule(now);
  }

  /// The next time [`Engine::tick`] has work.
  pub fn deadline(&self) -> Option<Instant> {
    [self.flush_at, self.loss_wait(), self.next_hold(), self.poll_at].into_iter().flatten().min()
  }

  /// Does what is due at `now`.
  pub fn tick(&mut self, now: Instant, out: &mut Output) {
    let second_look = self.mark_due_holds(now);
    let poll = self.poll_at.is_some_and(|at| at <= now);
    if poll {
      self.mark_polled();
    }
    let flush = [self.flush_at, self.loss_wait()].into_iter().flatten().any(|at| at <= now);
    if !flush && !poll && !second_look {
      return;
    }
    let took = self.flush(now);
    self.finish(out);
    if poll {
      // The next poll counts from the end of this one, after a rest for a slow one.
      self.poll_at = Some(now + took + self.options.poll_interval.max(took * REST));
    }
    self.stop_poll_if_idle();
  }

  fn stop_poll_if_idle(&mut self) {
    if !self.tree[Tree::ROOT].is_registered() {
      self.poll_at = None;
    }
  }

  /// Moves the flush to one quiet time from now, but not past the longest wait.
  fn schedule(&mut self, now: Instant) {
    let debounce = self.options.debounce;
    let longest = debounce.saturating_mul(4).max(LONGEST_WAIT);
    let latest = *self.flush_by.get_or_insert(now + longest);
    self.flush_at = Some((now + debounce).min(latest));
  }

  /// The time at which a loss report that had to wait gets its scans.
  fn loss_wait(&self) -> Option<Instant> {
    if self.loss_waits { self.rest_until } else { None }
  }

  /// A check for every mark, then rounds of scans until nothing is queued. Says how long the
  /// looks at the disk took.
  fn flush(&mut self, now: Instant) -> Duration {
    self.flush_at = None;
    self.flush_by = None;
    self.now = Some(now);
    self.flush_time = self.fs.now();
    self.flush_ticks = self.fs.ticks();
    self.commit_took = Duration::ZERO;
    // The scans that loss reports ask for are paced: after one flush of them, the next wait
    // for a rest.
    let rest_is_over = self.rest_until.is_none_or(|until| until <= now);
    let mut loss_scans_ran = rest_is_over && mem::take(&mut self.tree.lost);
    if loss_scans_ran {
      self.tree.scan_all();
    }
    self.loss_waits = self.tree.lost;

    let mut path = mem::take(&mut self.path);
    loss_scans_ran |= self.check_marks(&mut path, rest_is_over);
    let mut names = mem::take(&mut self.names);
    while !self.tree.to_scan.is_empty() {
      self.round(&mut path, &mut names);
    }
    (self.path, self.names) = (path, emptied(names));
    // A big listing is not kept for ever, as `emptied` does for a list.
    self.listed.clear();
    self.listed.shrink_to(KEPT / size_of::<ScanEntry>());

    let took = self.fs.ticks().saturating_sub(self.flush_ticks);
    let mut took = took.saturating_sub(self.commit_took).min(LONGEST_FLUSH);
    if loss_scans_ran {
      self.rest_until = Some(now + took + took * REST);
    }
    // A drop can leave work, like `unwatch`: one more flush does it.
    if self.drop_ignored() {
      took += self.flush(now);
    }
    took
  }

  /// Checks every marked node. `true`: a check that only a loss report asked for ran.
  fn check_marks(&mut self, path: &mut PathBuf, rest_is_over: bool) -> bool {
    self.check_moved();
    // From the top down: a directory is checked before the names in it.
    let mut marks = mem::take(&mut self.marks);
    self.tree.take_marked(&mut marks);
    let mut loss_checks_ran = false;
    // The path of one directory is built once for all of its marks.
    let mut path_of = None;
    for &(_, parent, key, since) in &marks {
      // No mark: the directory of the node went away earlier in this flush.
      let Some(mark) = self.tree.take_mark(key) else { continue };
      let mark = Mark { since, ..mark };
      // Below a held node nothing is checked: its second look decides for this node too.
      if self.tree.frozen(key) {
        self.tree.unhold(key);
        self.tree.prune(key);
        continue;
      }
      if mark.lost && !rest_is_over {
        self.tree.mark(key, mark);
        self.loss_waits = true;
        continue;
      }
      loss_checks_ran |= mark.lost;
      if path_of != Some(parent) {
        self.tree.path_into(parent, path);
        path_of = Some(parent);
      }
      // A check leaves the path of the directory in `path`.
      self.check(key, path, Listed::NotAsked, mark);
    }
    self.marks = emptied(marks);
    loss_checks_ran
  }

  /// One round: commit, then the scans queued before it. Watch, commit, then scan, for every
  /// directory.
  fn round(&mut self, path: &mut PathBuf, names: &mut Vec<ScanEntry>) {
    let now_watched = self.commit();
    let mut scans = mem::take(&mut self.scans);
    self.tree.take_scans(&mut scans);
    // After `take_scans`: what these two queue waits for the next commit.
    self.give_back_covered();
    self.ask_again(now_watched);
    for &(key, depth) in &scans {
      let Some(dir) = self.tree.get_mut(key).and_then(Node::dir_mut) else { continue };
      if let Some(queued) = &mut dir.scan {
        // Queued again in this round: its new watch is live only after the next commit.
        *queued = depth.max(*queued);
        continue;
      }
      self.scan(key, depth, path, names);
    }
    self.scans = emptied(scans);
  }

  /// Keeps one error of each kind for the next output.
  fn report(&mut self, kind: ErrorKind, path: Option<PathBuf>, source: Option<io::Error>) {
    if !self.errors.iter().any(|error| error.kind() == kind) {
      self.errors.push(Error::new(kind, path, source));
    }
  }
}
