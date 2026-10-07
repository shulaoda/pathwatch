//! The engine: it watches paths through a backend, keeps the tree in line with the disk, and
//! turns hints into file events. It has no thread and reads no clock: the caller gives the time.
//!
//! The way of one change of one file: `signal` makes a mark, `tick` runs `flush` (a `check` for
//! each mark, then the scans), and `finish` reads the events off the tree.

mod check; // one entry: the disk against the tree
mod events; // the events of one flush, and the delete delay
mod filter; // the questions to the ignore filter
mod os_watch; // what a directory asks from the backend, and what is polled
mod scan; // one directory: every entry in it
mod test_hooks; // what the tests read
mod watched_paths; // a path becomes a watched path, and the way back

use std::{
  collections::VecDeque,
  fmt, io, mem,
  path::{Path, PathBuf},
  sync::Arc,
  time::{Duration, Instant, SystemTime},
};

use rustc_hash::FxHashSet;

use self::scan::ScanEntry;
pub use crate::tree::Fingerprint;
use crate::{
  backend::{Backend, DirKey, Signal},
  config::EntryKind,
  error::{Error, ErrorKind},
  event::Event,
  fs::Fs,
  path::normalize,
  tree::{Depth, Kind, Node, Tree, emptied},
};

/// While hints keep coming, the disk is still checked this often, or every four quiet times if
/// that is longer.
const LONGEST_WAIT: Duration = Duration::from_millis(500);

/// After work that no hint asked for by name (a poll, or the scans after lost notifications),
/// the engine rests this many times as long as the work took. So about a tenth of the time
/// goes into such work, however big the watched set is.
const REST: u32 = 9;

/// The ignore filter: `true` for a path that must not be reported. An ignored directory is
/// never entered.
///
/// It must give the same answer for the same path and kind: a node keeps the answer, and the
/// engine does not ask again for that node. `Config::with_ignored` says what is asked, and when.
pub type Ignored = Arc<dyn Fn(&Path, EntryKind) -> bool + Send + Sync>;

/// The settings of an [`Engine`].
#[derive(Clone)]
pub struct Options {
  pub ignored: Option<Ignored>,
  /// The debounce: the disk is checked when no hint came for this long. Zero: at once.
  pub quiet_time: Duration,
  /// How long a `Delete` waits for the file to come back, so that "remove, then write again"
  /// is one `Update`. Zero turns it off.
  pub delete_delay: Duration,
  /// The shortest time between two polls of what no OS watch covers.
  pub poll_interval: Duration,
  /// Compare a hash of the content of every file at every check.
  pub compare_contents: bool,
  /// The disk takes two spellings of a name for one: another case of the letters, or another
  /// Unicode form (the usual disks of macOS and Windows).
  pub loose_spelling: bool,
}

impl Default for Options {
  fn default() -> Self {
    Self {
      ignored: None,
      quiet_time: Duration::from_millis(10),
      delete_delay: Duration::from_millis(100),
      poll_interval: Duration::from_secs(1),
      compare_contents: false,
      loose_spelling: false,
    }
  }
}

impl fmt::Debug for Options {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Options")
      .field("ignored", &self.ignored.is_some())
      .field("quiet_time", &self.quiet_time)
      .field("delete_delay", &self.delete_delay)
      .field("poll_interval", &self.poll_interval)
      .field("compare_contents", &self.compare_contents)
      .field("loose_spelling", &self.loose_spelling)
      .finish()
  }
}

/// What one call of [`Engine::watch`], [`Engine::unwatch`] or [`Engine::tick`] hands over.
///
/// The engine appends to it. The `events` of one call are one batch: one event for a path, in
/// no order that is promised. Take them out before the next call.
#[derive(Debug, Default)]
pub struct Output {
  pub events: Vec<Event>,
  pub errors: Vec<Error>,
}

/// Watches paths through a [`Backend`] and a disk ([`Fs`]), and turns hints into file events.
pub struct Engine<B: Backend, F: Fs> {
  backend: B,
  fs: F,
  options: Options,
  /// What the engine knows about the disk, and the marks and scans that wait.
  tree: Tree,

  // When the next work is due.
  /// The next flush: the last hint and the quiet time. `None`: nothing waits for one.
  flush_at: Option<Instant>,
  /// The latest time for that flush, so that hints that keep coming do not hold it back.
  flush_by: Option<Instant>,
  /// The next poll. `None` while nothing is registered.
  poll_at: Option<Instant>,
  /// The held nodes in the order of their second looks. An entry can be stale.
  hold_queue: VecDeque<(Instant, DirKey)>,
  /// The scans that loss reports ask for do not start before this time.
  rest_until: Option<Instant>,
  /// A loss report waits for the end of that rest.
  loss_waits: bool,
  /// The backend said that it cannot go on. Every node is polled.
  backend_failed: bool,

  // The running flush.
  /// Its time, as the caller gave it. `None` before the first flush.
  now: Option<Instant>,
  /// The time of the disk at its start. The rule for young files compares with it.
  flush_time: SystemTime,
  /// How long the backend took to make watches live.
  commit_took: Duration,
  /// The nodes that it changed. `finish` reads each one for its event. A key can be stale.
  touched: Vec<DirKey>,
  /// The registered names whose watch target the filter ignores. The end of the flush drops
  /// the watched paths at and below them, with a `Delete` for each file that the handler knows.
  dropped: Vec<DirKey>,
  /// With loose spelling: the nodes that the listing of the running scan named.
  listed: FxHashSet<DirKey>,
  /// A read of the disk failed during the running scan.
  scan_had_error: bool,

  // Buffers. They are kept between the flushes, so that a flush asks for no memory for them
  // (see `emptied`). What they hold between two uses does not matter.
  /// Every path for the disk is built here, so that a check allocates nothing.
  path: PathBuf,
  /// The marks of a flush, in the order of their checks (see `Tree::take_marked`).
  marks: Vec<(usize, DirKey, DirKey)>,
  /// The scans of one round.
  scans: Vec<(DirKey, Depth)>,
  /// The nodes of the directory that is scanned.
  names: Vec<ScanEntry>,

  /// The errors that wait for the next output. One for each kind.
  errors: Vec<Error>,
}

impl<B: Backend, F: Fs> Engine<B, F> {
  pub fn new(backend: B, fs: F, options: Options) -> Self {
    let mut tree = Tree::new();
    tree.loose_spelling = options.loose_spelling;
    Self {
      backend,
      fs,
      options,
      tree,
      flush_at: None,
      flush_by: None,
      poll_at: None,
      hold_queue: VecDeque::new(),
      rest_until: None,
      loss_waits: false,
      backend_failed: false,
      now: None,
      flush_time: SystemTime::UNIX_EPOCH,
      commit_took: Duration::ZERO,
      touched: Vec::new(),
      dropped: Vec::new(),
      listed: FxHashSet::default(),
      scan_had_error: false,
      path: PathBuf::new(),
      marks: Vec::new(),
      scans: Vec::new(),
      names: Vec::new(),
      errors: Vec::new(),
    }
  }

  /// Makes every path a watched path and checks the disk once. When it returns, every path is
  /// watched, and what exists at that moment is known and was not reported: the baseline.
  ///
  /// A path that cannot be watched goes to `out.errors`, and the rest is registered.
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

  /// Takes back what `watch` did for every path. A file that only these paths covered is
  /// forgotten without an event, also when its `Delete` was waiting. A file that another
  /// watched path still covers stays as it is. A path that is not watched is skipped.
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

  /// Takes one signal of the backend. It only makes marks: the disk is read by [`tick`].
  ///
  /// [`tick`]: Engine::tick
  pub fn signal(&mut self, signal: Signal<B::Raw>, now: Instant) {
    let mut left_work = false;
    match signal {
      Signal::Raw(raw) => {
        let tree = &mut self.tree;
        self.backend.translate(raw, &mut |hint| left_work |= tree.hint(hint));
      }
      Signal::Failed(error) => {
        left_work = !self.backend_failed;
        self.backend_died(error);
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

  /// The next time [`tick`] has work: the earliest of the flush, the second look of a held
  /// node, the poll, and a loss report that waits for the end of a rest.
  ///
  /// [`tick`]: Engine::tick
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
      // The next poll is counted from the end of this one, and a poll that took long is
      // followed by a rest: a slow disk, or a big watched set, is not polled without a pause.
      self.poll_at = Some(now + took + self.options.poll_interval.max(took * REST));
    }
    self.stop_poll_if_idle();
  }

  /// Nothing is registered: `unwatch` took every watched path back, or each one was dropped.
  /// Then there is nothing to poll, and the caller can sleep until the next `watch`.
  fn stop_poll_if_idle(&mut self) {
    if !self.tree[Tree::ROOT].is_registered() {
      self.poll_at = None;
    }
  }

  /// A hint left work for a flush. The flush comes when no further hint came for the quiet
  /// time. Hints that keep coming move it, but not past the longest wait.
  fn schedule(&mut self, now: Instant) {
    let quiet = self.options.quiet_time;
    let longest = quiet.saturating_mul(4).max(LONGEST_WAIT);
    let latest = *self.flush_by.get_or_insert(now + longest);
    self.flush_at = Some((now + quiet).min(latest));
  }

  /// The time at which a loss report that had to wait gets its scans.
  fn loss_wait(&self) -> Option<Instant> {
    if self.loss_waits { self.rest_until } else { None }
  }

  /// Brings the tree in line with the disk: a `check` for every mark, then rounds of scans
  /// until nothing is queued. The events are not made here: `finish` reads them afterwards.
  /// Says how long the looks at the disk took, by the clock of the disk.
  fn flush(&mut self, now: Instant) -> Duration {
    self.flush_at = None;
    self.flush_by = None;
    self.now = Some(now);
    self.flush_time = self.fs.now();
    self.commit_took = Duration::ZERO;
    // A burst can make a backend report a loss many times a second, and each report asks for
    // a scan of everything below a place, or of everything. So these scans are paced: after
    // the ones of one flush, the next ones wait for the end of a rest. Nothing else waits.
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

    let took = self.fs.now().duration_since(self.flush_time).unwrap_or_default();
    let mut took = took.saturating_sub(self.commit_took);
    if loss_scans_ran {
      self.rest_until = Some(now + took + took * REST);
    }
    // Like `unwatch`, a drop can leave work, and one more flush does it.
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
    // The directory whose path is in `path`: the marks of one directory come one after the
    // other, and its path is built once for all of them.
    let mut path_of = None;
    for &(_, parent, key) in &marks {
      // No mark: the directory of the node went away earlier in this flush and took the mark
      // with it.
      let Some(mark) = self.tree.take_mark(key) else { continue };
      // A node above is held: nothing below it is checked before its second look. That look
      // decides for this node too, so a hold of its own ends here.
      if self.tree.frozen(key) {
        self.tree.unhold(key);
        self.tree.prune(key);
        continue;
      }
      if mark.lost && !rest_is_over {
        // Only a loss report asked for this check: it waits for the end of the rest.
        self.tree.mark(key, mark);
        self.loss_waits = true;
        continue;
      }
      loss_checks_ran |= mark.lost;
      // With loose spelling, `lstat` finds an entry under every spelling of its name. So when
      // this look finds an entry where the tree had none, or a held one, only a listing tells
      // how the disk spells it, and whether another node is gone for it: the names of the
      // directory are scanned in this flush (see `has_another_spelling`).
      let node = &self.tree[key];
      let was_absent = matches!(node.kind, Kind::Missing) || node.is_held();
      let name_is_unsure = self.options.loose_spelling && was_absent && !node.is_registered();
      if path_of != Some(parent) {
        self.tree.path_into(parent, path);
        path_of = Some(parent);
      }
      // A check leaves the path of the directory in `path`.
      self.check(key, path, None, mark);
      let is_there = |node: &Node| !matches!(node.kind, Kind::Missing) && !node.is_held();
      if name_is_unsure && self.tree.get(key).is_some_and(is_there) {
        // The entry was just looked at, and so were the others at their last change: only
        // the names are compared.
        self.tree.queue_scan(parent, Depth::Names);
      }
    }
    self.marks = emptied(marks);
    loss_checks_ran
  }

  /// One round: makes the watches asked for so far live, then scans what is queued. A new
  /// directory that a scan finds is asked from the backend and queued, for the next round.
  /// So the order holds for every directory: watch, commit, then scan.
  fn round(&mut self, path: &mut PathBuf, names: &mut Vec<ScanEntry>) {
    let now_watched = self.commit();
    let mut scans = mem::take(&mut self.scans);
    self.tree.take_scans(&mut scans);
    // After `take_scans`: what this queues waits for the commit of the next round.
    self.ask_again(now_watched);
    for &(key, depth) in &scans {
      let Some(dir) = self.tree.get_mut(key).and_then(Node::dir_mut) else { continue };
      if let Some(queued) = &mut dir.scan {
        // A scan of this round queued it again. Its watch may be new, and that watch is live
        // only after the next commit: the node waits for the next round.
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
