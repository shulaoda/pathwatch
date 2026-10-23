//! The engine: it watches paths through a backend, keeps the tree in line with the disk, and
//! turns hints into file events. It has no thread and reads no clock: the caller gives the time.

mod check;
mod events;
mod os_watch;
mod scan;
#[cfg(feature = "test-hooks")]
mod test_hooks;
mod watched_paths;

use std::{
  cmp::Reverse,
  collections::BinaryHeap,
  fmt, io, mem,
  path::{Path, PathBuf},
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
  time::{Duration, Instant},
};

use rustc_hash::{FxHashMap, FxHashSet};

use self::{check::Listed, scan::ScanEntry};
use crate::{
  backend::{Backend, DirKey, Hint, Signal},
  config::EntryKind,
  error::{Error, ErrorKind},
  event::Event,
  fs::Fs,
  tree::{Depth, KEPT, Mark, Marked, Node, Tree, emptied, nanos},
};

/// While hints keep coming, a batch goes out four quiet times after its first hint, and this
/// long at most, unless the quiet time itself is longer.
const LONGEST_WAIT: Duration = Duration::from_millis(500);

/// The scans after a loss report rest this many times as long as they took: such work takes a
/// tenth of the time at most.
const REST: u32 = 9;
/// The longest rest after the scans of a loss report: the rest is against a storm of them, and
/// one long scan must not put the next loss report off by seconds.
const LONGEST_LOSS_REST: Duration = Duration::from_millis(500);

/// A flush counts for no more than this in the rest: the clock runs on while the machine
/// sleeps, and a flush that the sleep fell into would rest for hours.
const LONGEST_FLUSH: Duration = Duration::from_secs(60);

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
  /// Every disk takes two spellings of a name for one (macOS, Windows). Without it, every
  /// directory tells them apart.
  pub loose_spelling: bool,
  /// A junction or a directory link is a plain directory first, and the second step gives no
  /// record (Windows): a young directory that the tree did not know is looked at once more at
  /// the next flush.
  pub two_step_links: bool,
  /// Every write to a file that an OS watch covers gives a `Written` hint (inotify, Windows).
  /// Then a young file is not read for a hash: the hint tells of a write that leaves the
  /// fingerprint as it is, and while the file is young a loss report counts as one (5.8).
  pub written_hints: bool,
}

impl Default for Options {
  fn default() -> Self {
    Self {
      ignored: None,
      debounce: Duration::from_millis(10),
      delete_delay: Duration::from_millis(100),
      poll_interval: Duration::from_secs(1),
      loose_spelling: false,
      two_step_links: false,
      written_hints: false,
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
      .field("loose_spelling", &self.loose_spelling)
      .field("two_step_links", &self.two_step_links)
      .field("written_hints", &self.written_hints)
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
#[expect(clippy::struct_excessive_bools, reason = "each one is a state of the engine")]
pub struct Engine<B: Backend, F: Fs> {
  /// `None`: no backend (it did not start, or it died); every node is polled.
  backend: Option<B>,
  fs: F,
  options: Options,
  /// The settings once the backend has died: those of a watcher that polls everything
  /// ([`Engine::fall_back_to`]). `None`: the settings stay.
  fallback: Option<Options>,
  tree: Tree,

  // When the next work is due.
  /// The next flush. `None`: none waits.
  flush_at: Option<Instant>,
  /// The latest time for that flush, so hints that keep coming do not hold it back.
  flush_by: Option<Instant>,
  /// The next poll. `None` while a poll has nothing to look at (`has_poll_work`).
  poll_at: Option<Instant>,
  /// The held nodes by the time of their second look, the earliest on top. An entry can be stale.
  hold_queue: BinaryHeap<Reverse<(Instant, DirKey)>>,
  /// The held nodes whose second look is due in the running tick.
  due_holds: Vec<DirKey>,
  /// The pacing of the scans that loss reports ask for.
  loss: Loss,
  /// The young files that were not read (`Content::Pending`), by the time they get old, the
  /// earliest on top. An entry can be stale.
  pending: BinaryHeap<Reverse<(i64, DirKey)>>,
  /// Every record of a change before this time came (`Hint::Delivered`), in nanoseconds.
  delivered: i64,
  /// The time the backend was last asked about (`Backend::ask_delivered`).
  asked: i64,

  // The running flush.
  /// The time of the running flush. `None` before the first.
  now: Option<Instant>,
  /// The time of the disk at the start of the flush, in nanoseconds since 1970: the rule for
  /// young files compares with it.
  flush_time: i64,
  /// `Fs::ticks` at the start of the flush.
  flush_ticks: Duration,
  /// The running scan is of a directory that can have been away and back, or whose watch lost
  /// records (`Dir::away`): a young file in it that was not read counts as written (5.8).
  away_scan: bool,
  /// A watch was given back since the last commit: the backend may move its watches then.
  gave_back: bool,
  /// The nodes the flush changed: `finish` reads their events. A key can be stale.
  touched: Vec<DirKey>,
  /// The directories that may want less after an `unwatch` or a drop: asked once at the end of
  /// the call, not once for each path.
  narrowing: Vec<DirKey>,
  /// Set by the watcher when it closes: a flush ends at the next check, with the tree as it is.
  stop: Option<Arc<AtomicBool>>,
  /// With loose spelling: what the listing of the running scan said of the nodes it named.
  listed: FxHashMap<DirKey, Listed>,
  /// A read of the disk failed during the running scan.
  scan_had_error: bool,

  // Buffers, kept between flushes so that a flush asks for no memory (see `emptied`).
  /// Every path for the disk is built here.
  path: PathBuf,
  /// The marks of a flush, in the order of their checks (`Tree::take_marked`).
  marks: Vec<Marked>,
  /// The scans of one round.
  scans: Vec<(DirKey, Depth, bool)>,
  /// The nodes of the directory that is scanned.
  names: Vec<ScanEntry>,

  /// The errors for the next output, one of each kind (`report`).
  errors: Vec<Error>,
  /// `WatchLimit` was reported: not again for each directory that finds no room.
  told_watch_limit: bool,
  /// The disks, by device, that an OS watch was given on: each was asked once whether other
  /// machines change it too.
  asked_disks: FxHashSet<u64>,
}

/// The pacing of the scans that loss reports ask for (5.13).
#[derive(Default)]
struct Loss {
  /// The scans wait until then: the rest after the last ones.
  rest_until: Option<Instant>,
  /// A loss report waits for the end of that rest.
  waits: bool,
  /// The running flush makes the scans of a loss report: a young file counts as written (5.8).
  scanning: bool,
}

impl<B: Backend, F: Fs> fmt::Debug for Engine<B, F> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Engine").field("backend", &self.backend.is_some()).finish_non_exhaustive()
  }
}

/// Making an engine.
impl<B: Backend, F: Fs> Engine<B, F> {
  pub fn new(backend: B, fs: F, options: Options) -> Self {
    Self { backend: Some(backend), ..Self::without_backend(fs, options) }
  }

  /// An engine for a backend that did not start: every node is polled, as after its death.
  pub(crate) fn without_backend(fs: F, options: Options) -> Self {
    let mut tree = Tree::new();
    tree.loose_spelling = options.loose_spelling;
    Self {
      backend: None,
      fs,
      options,
      fallback: None,
      tree,
      flush_at: None,
      flush_by: None,
      poll_at: None,
      hold_queue: BinaryHeap::new(),
      due_holds: Vec::new(),
      loss: Loss::default(),
      pending: BinaryHeap::new(),
      delivered: i64::MIN,
      asked: i64::MIN,
      away_scan: false,
      now: None,
      flush_time: 0,
      flush_ticks: Duration::ZERO,
      gave_back: false,
      touched: Vec::new(),
      narrowing: Vec::new(),
      stop: None,
      listed: FxHashMap::default(),
      scan_had_error: false,
      path: PathBuf::new(),
      marks: Vec::new(),
      scans: Vec::new(),
      names: Vec::new(),
      errors: Vec::new(),
      told_watch_limit: false,
      asked_disks: FxHashSet::default(),
    }
  }

  /// A flush ends at the next check once `stop` is set: `close` does not wait for one that
  /// reads thousands of files.
  pub fn stop_with(&mut self, stop: Arc<AtomicBool>) {
    self.stop = Some(stop);
  }

  /// The settings once the backend has died and every node is polled: the watcher then works
  /// as one that polls everything, as it does when the backend cannot start.
  pub fn fall_back_to(&mut self, options: Options) {
    self.fallback = Some(options);
  }

  pub(crate) fn stopped(&self) -> bool {
    self.stop.as_ref().is_some_and(|stop| stop.load(Ordering::Relaxed))
  }
}

/// The calls of the watcher thread, and when the next work is due.
impl<B: Backend, F: Fs> Engine<B, F> {
  /// Makes every path a watched path and checks the disk once: what exists then is the baseline,
  /// known without an event. Each path is clean, as `path::normalize` gives it: the watcher
  /// cleans them.
  pub fn watch(&mut self, paths: Vec<PathBuf>, now: Instant, out: &mut Output) {
    for path in paths {
      self.register(&path);
    }
    self.flush_now(now, out);
  }

  /// Takes back what `watch` did for every path, and reports nothing for them. Each path is
  /// clean, as for `watch`.
  pub fn unwatch(&mut self, paths: Vec<PathBuf>, now: Instant, out: &mut Output) {
    for path in paths {
      self.unregister(&path);
    }
    self.narrow_all();
    // What still stands may need another watch now, or another look.
    self.flush_now(now, out);
  }

  /// The flush of a `watch` or `unwatch` call: at once, without the quiet time, as its baseline
  /// is decided by a tree that is in line with the hints.
  fn flush_now(&mut self, now: Instant, out: &mut Output) {
    self.flush_at = None;
    let took = self.flush(now);
    self.finish(out);
    self.set_poll(now + took + self.options.poll_interval);
  }

  /// Takes one signal of the backend: it only makes marks, and [`Engine::tick`] reads the disk.
  pub fn signal(&mut self, signal: Signal<B::Raw>, now: Instant) {
    let mut left_work = false;
    match signal {
      Signal::Raw(raw) => {
        let tree = &mut self.tree;
        let mut delivered = None;
        if let Some(backend) = &mut self.backend {
          let first = tree.marked.len();
          backend.translate(raw, &mut |hint| match hint {
            Hint::Delivered { before } => delivered = Some(before),
            hint => left_work |= tree.hint(hint),
          });
          // Only `Tree::hint` added to the list meanwhile, with no time: these are the marks of
          // this signal, and a `Delete` counts its delay from their time.
          for (_, since) in &mut tree.marked[first..] {
            *since = Some(now);
          }
        }
        // No young file that was old by then can hide a write any more (5.8).
        if let Some(before) = delivered {
          self.delivered = self.delivered.max(nanos(before));
          self.forget_delivered();
        }
      }
      Signal::Failed(error) => {
        left_work = self.backend.is_some();
        self.backend_died(error);
        // The poll that was set with the old interval may be far away.
        let next = now + self.options.poll_interval;
        self.poll_at = self.poll_at.map(|at| at.min(next));
        self.set_poll(next);
      }
    }
    if left_work {
      self.schedule(now);
    }
  }

  /// Signals were dropped before they reached the engine: everything has to be checked.
  pub fn overflow(&mut self, now: Instant) {
    self.tree.lost = true;
    // The answer to the last question may be among them: the next one is asked again.
    self.asked = i64::MIN;
    self.schedule(now);
  }

  /// The next time [`Engine::tick`] has work.
  pub fn deadline(&self) -> Option<Instant> {
    let next_hold = self.hold_queue.peek().map(|Reverse((until, _))| *until);
    [self.next_flush(), next_hold, self.poll_at].into_iter().flatten().min()
  }

  /// Does what is due at `now`.
  pub fn tick(&mut self, now: Instant, out: &mut Output) {
    let second_look = self.mark_due_holds(now);
    let poll = self.poll_at.is_some_and(|at| at <= now);
    if poll {
      self.mark_polled();
    }
    let flush = self.next_flush().is_some_and(|at| at <= now);
    if !flush && !poll && !second_look {
      return;
    }
    let took = self.flush(now);
    self.finish(out);
    if poll {
      // The next poll counts from the end of this one: a pause of the poll interval, and at
      // least as long as the round took, so that polling takes half of the time at most.
      self.poll_at = Some(now + took + self.options.poll_interval.max(took));
    }
    self.set_poll(now + took + self.options.poll_interval);
  }

  /// Keeps the timer running while a poll has something to look at, and stops it otherwise:
  /// with inotify every directory above a watched path has a watch of its own, and the thread
  /// sleeps until a notification comes.
  fn set_poll(&mut self, at: Instant) {
    if !self.tree[Tree::ROOT].is_registered() {
      // Only a poll tidies the list of the timer: without one, every `watch` would leave its
      // directories on it.
      self.tree.tidy_polled();
    }
    if self.has_poll_work() {
      self.poll_at.get_or_insert(at);
    } else {
      self.poll_at = None;
    }
  }

  /// Whether a poll has anything to look at: a directory on the list other than the root (a
  /// stale key keeps the timer for one round, which tidies the list), a child of the root that
  /// is not on disk (a drive that is not there yet), a registered name that is a link (5.14),
  /// or work that a flush left (the disk stopped answering): the next poll takes it up.
  fn has_poll_work(&self) -> bool {
    self.tree.polled.iter().any(|&key| key != Tree::ROOT)
      || self.tree.children(Tree::ROOT).any(|top| !self.tree[top].is_dir())
      || self.tree.links().next().is_some()
      || self.tree.has_work()
  }

  /// Moves the flush to one quiet time from now, but not past the longest wait.
  fn schedule(&mut self, now: Instant) {
    let debounce = self.options.debounce;
    let longest = debounce.saturating_mul(4).min(LONGEST_WAIT).max(debounce);
    let latest = *self.flush_by.get_or_insert(now + longest);
    self.flush_at = Some((now + debounce).min(latest));
  }

  /// The time of the next flush: the end of the quiet time, or else the end of the rest that a
  /// loss report waits for. Its scans come then, and not before the quiet time, which its mark
  /// waits for like every hint.
  fn next_flush(&self) -> Option<Instant> {
    self.flush_at.or(self.loss.rest_until.filter(|_| self.loss.waits))
  }
}

/// A flush: the checks of the marks, then rounds of scans.
impl<B: Backend, F: Fs> Engine<B, F> {
  /// A check for every mark, then rounds of scans until nothing is queued. Says how long the
  /// looks at the disk took.
  fn flush(&mut self, now: Instant) -> Duration {
    // The marks of hints wait for the quiet time, which can be longer than the poll interval:
    // a poll or a second look that runs before it checks its own work and leaves them, but for
    // a hint above a held node, which decides for it. A `watch` or `unwatch` call ends the
    // quiet time: its baseline is decided by a tree that is in line with the hints. A loss
    // report waits for the quiet time too (`next_flush`).
    let hints_due = self.flush_at.is_none_or(|at| at <= now);
    if hints_due {
      self.flush_at = None;
      self.flush_by = None;
    }
    self.now = Some(now);
    self.flush_time = nanos(self.fs.now());
    self.flush_ticks = self.fs.ticks();
    // The scans that loss reports ask for are paced: after one flush of them, the next wait
    // for a rest.
    let rest_is_over = self.loss.rest_until.is_none_or(|until| until <= now);
    let mut loss_scans_ran = rest_is_over && mem::take(&mut self.tree.lost);
    if loss_scans_ran {
      self.tree.queue_scan(Tree::ROOT, Depth::Deep);
    }
    self.loss.scanning = loss_scans_ran;
    self.loss.waits = self.tree.lost;

    let mut path = mem::take(&mut self.path);
    loss_scans_ran |= self.check_marks(&mut path, rest_is_over, hints_due);
    self.due_holds = emptied(mem::take(&mut self.due_holds));
    let mut names = mem::take(&mut self.names);
    // A flush that only gave watches back commits too: the backend moves its watches then.
    while (!self.tree.to_scan.is_empty() || self.gave_back) && !self.stopped() {
      self.round(&mut path, &mut names);
    }
    // The scans took the marks of the nodes they looked at: what is left on the list is work.
    self.tree.tidy_marked();
    (self.path, self.names) = (path, emptied(names));
    // A big listing is not kept for ever, as `emptied` does for a list.
    self.listed.clear();
    self.listed.shrink_to(KEPT / size_of::<ScanEntry>());

    let took = self.fs.ticks().saturating_sub(self.flush_ticks).min(LONGEST_FLUSH);
    if loss_scans_ran {
      self.loss.rest_until = Some(now + took + (took * REST).min(LONGEST_LOSS_REST));
    }
    took
  }

  /// Checks every marked node. `true`: a check that only a loss report asked for ran.
  fn check_marks(&mut self, path: &mut PathBuf, rest_is_over: bool, hints_due: bool) -> bool {
    self.check_moved(hints_due);
    // From the top down: a directory is checked before the names in it.
    let mut marks = mem::take(&mut self.marks);
    self.tree.take_marked(&mut marks);
    let mut loss_checks_ran = false;
    // The directories above the held nodes whose second look is due, sorted: a hint for one of
    // them decides for the held node, and does not wait.
    let mut above = Vec::new();
    if !hints_due {
      above.extend(self.due_holds.iter().flat_map(|&held| self.tree.ancestors(held)));
      above.sort_unstable();
      above.dedup();
    }
    // The path of one directory is built once for all of its marks.
    let mut path_of = None;
    for &(_, _, parent, key, since) in &marks {
      if self.stopped() {
        break;
      }
      // No mark: the directory of the node went away earlier in this flush.
      let Some(mark) = self.tree.take_mark(key) else { continue };
      let mark = Mark { since, ..mark };
      // Below a held node nothing is checked: its second look decides for this node too.
      if self.tree.frozen(key) {
        self.tree.unhold(key);
        self.tree.prune(key);
        continue;
      }
      // A mark that a hint made waits for the quiet time; one that only a loss report asked
      // for waits for the end of the rest.
      let waits = since.is_some() && !hints_due && above.binary_search(&key).is_err();
      if waits || (mark.lost && !rest_is_over) {
        self.tree.mark(key, mark);
        self.loss.waits |= mark.lost;
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
    self.gave_back = false;
    let mut scans = mem::take(&mut self.scans);
    self.tree.take_scans(&mut scans);
    // After `take_scans`: what this queues waits for the next commit.
    self.ask_again(now_watched);
    for &(key, depth, away) in &scans {
      if self.stopped() {
        break;
      }
      let Some(dir) = self.tree.get_mut(key).and_then(Node::dir_mut) else { continue };
      if let Some(queued) = &mut dir.scan {
        // Queued again in this round: its new watch is live only after the next commit.
        *queued = depth.max(*queued);
        dir.away |= away;
        continue;
      }
      self.away_scan = away;
      self.scan(key, depth, path, names);
    }
    self.away_scan = false;
    self.scans = emptied(scans);
  }

  /// Keeps one error of each kind for the next output, but every `RemoteDisk`: each is about
  /// another disk, and each disk is told once (`asked_disks`).
  fn report(&mut self, kind: ErrorKind, path: &Path, source: io::Error) {
    let kept =
      kind != ErrorKind::RemoteDisk && self.errors.iter().any(|error| error.kind() == kind);
    if !kept {
      self.errors.push(Error::new(kind, Some(path.to_path_buf()), Some(source)));
    }
  }
}
