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
  backend::{Backend, Hint, NodeKey, Signal},
  config::EntryKind,
  error::{Error, ErrorKind},
  event::Event,
  fs::Fs,
  tree::{Depth, KEPT, Mark, Marked, Node, Tree, emptied, nanos},
};

const LONGEST_WAIT: Duration = Duration::from_millis(500);

const REST: u32 = 9;
const LONGEST_LOSS_REST: Duration = Duration::from_millis(500);

const LONGEST_FLUSH: Duration = Duration::from_secs(60);

pub type Ignored = Arc<dyn Fn(&Path, EntryKind) -> bool + Send + Sync>;

#[derive(Clone)]
pub struct Options {
  pub ignored: Option<Ignored>,
  pub debounce: Duration,
  pub delete_delay: Duration,
  pub poll_interval: Duration,
  pub loose_spelling: bool,
  pub two_step_links: bool,
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

#[derive(Debug, Default)]
pub struct Output {
  pub events: Vec<Event>,
  pub errors: Vec<Error>,
}

#[expect(clippy::struct_excessive_bools, reason = "each one is a state of the engine")]
pub struct Engine<B: Backend, F: Fs> {
  backend: Option<B>,
  fs: F,
  options: Options,
  fallback: Option<Options>,
  tree: Tree,

  flush_at: Option<Instant>,
  flush_by: Option<Instant>,
  poll_at: Option<Instant>,
  hold_queue: BinaryHeap<Reverse<(Instant, NodeKey)>>,
  due_holds: Vec<NodeKey>,
  loss: Loss,
  pending: BinaryHeap<Reverse<(i64, NodeKey)>>,
  delivered: i64,
  asked: i64,

  now: Option<Instant>,
  flush_time: i64,
  flush_ticks: Duration,
  away_scan: bool,
  gave_back: bool,
  touched: Vec<NodeKey>,
  narrowing: Vec<NodeKey>,
  stop: Option<Arc<AtomicBool>>,
  listed: FxHashMap<NodeKey, Listed>,
  scan_had_error: bool,

  path: PathBuf,
  marks: Vec<Marked>,
  scans: Vec<(NodeKey, Depth, bool)>,
  entries: Vec<ScanEntry>,

  errors: Vec<Error>,
  told_watch_limit: bool,
  asked_disks: FxHashSet<u64>,
}

#[derive(Default)]
struct Loss {
  rest_until: Option<Instant>,
  waits: bool,
  scanning: bool,
}

impl<B: Backend, F: Fs> fmt::Debug for Engine<B, F> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Engine").field("backend", &self.backend.is_some()).finish_non_exhaustive()
  }
}

impl<B: Backend, F: Fs> Engine<B, F> {
  pub fn new(backend: B, fs: F, options: Options) -> Self {
    Self { backend: Some(backend), ..Self::without_backend(fs, options) }
  }

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
      now: None,
      flush_time: 0,
      flush_ticks: Duration::ZERO,
      away_scan: false,
      gave_back: false,
      touched: Vec::new(),
      narrowing: Vec::new(),
      stop: None,
      listed: FxHashMap::default(),
      scan_had_error: false,
      path: PathBuf::new(),
      marks: Vec::new(),
      scans: Vec::new(),
      entries: Vec::new(),
      errors: Vec::new(),
      told_watch_limit: false,
      asked_disks: FxHashSet::default(),
    }
  }

  pub fn stop_with(&mut self, stop: Arc<AtomicBool>) {
    self.stop = Some(stop);
  }

  pub fn fall_back_to(&mut self, options: Options) {
    self.fallback = Some(options);
  }

  pub(crate) fn stopped(&self) -> bool {
    self.stop.as_ref().is_some_and(|stop| stop.load(Ordering::Relaxed))
  }

  pub fn watch(&mut self, paths: Vec<PathBuf>, now: Instant, out: &mut Output) {
    for path in paths {
      self.register(&path);
    }
    self.flush_now(now, out);
  }

  pub fn unwatch(&mut self, paths: Vec<PathBuf>, now: Instant, out: &mut Output) {
    for path in paths {
      self.unregister(&path);
    }
    self.narrow_all();
    self.flush_now(now, out);
  }

  fn flush_now(&mut self, now: Instant, out: &mut Output) {
    self.flush_at = None;
    let took = self.flush(now);
    self.finish(out);
    self.set_poll(now + took + self.options.poll_interval);
  }

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
          for (_, since) in &mut tree.marked[first..] {
            *since = Some(now);
          }
        }
        if let Some(before) = delivered {
          self.delivered = self.delivered.max(nanos(before));
          self.forget_delivered();
        }
      }
      Signal::Failed(error) => {
        left_work = self.backend.is_some();
        self.backend_died(error);
        let next = now + self.options.poll_interval;
        self.poll_at = self.poll_at.map(|at| at.min(next));
        self.set_poll(next);
      }
    }
    if left_work {
      self.schedule(now);
    }
  }

  pub fn overflow(&mut self, now: Instant) {
    self.tree.lost = true;
    self.asked = i64::MIN;
    self.schedule(now);
  }

  pub fn deadline(&self) -> Option<Instant> {
    let next_hold = self.hold_queue.peek().map(|Reverse((until, _))| *until);
    [self.next_flush(), next_hold, self.poll_at].into_iter().flatten().min()
  }

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
      self.poll_at = Some(now + took + self.options.poll_interval.max(took));
    }
    self.set_poll(now + took + self.options.poll_interval);
  }

  fn set_poll(&mut self, at: Instant) {
    if !self.tree[Tree::ROOT].is_registered() {
      self.tree.tidy_polled();
    }
    if self.has_poll_work() {
      self.poll_at.get_or_insert(at);
    } else {
      self.poll_at = None;
    }
  }

  fn has_poll_work(&self) -> bool {
    self.tree.polled.iter().any(|&key| key != Tree::ROOT)
      || self.tree.children(Tree::ROOT).any(|top| !self.tree[top].is_dir())
      || self.tree.links().next().is_some()
      || self.tree.has_work()
  }

  fn schedule(&mut self, now: Instant) {
    let debounce = self.options.debounce;
    let longest = debounce.saturating_mul(4).min(LONGEST_WAIT).max(debounce);
    let latest = *self.flush_by.get_or_insert(now + longest);
    self.flush_at = Some((now + debounce).min(latest));
  }

  fn next_flush(&self) -> Option<Instant> {
    self.flush_at.or(self.loss.rest_until.filter(|_| self.loss.waits))
  }

  fn flush(&mut self, now: Instant) -> Duration {
    let hints_due = self.flush_at.is_none_or(|at| at <= now);
    if hints_due {
      self.flush_at = None;
      self.flush_by = None;
    }
    self.now = Some(now);
    self.flush_time = nanos(self.fs.now());
    self.flush_ticks = self.fs.ticks();
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
    let mut entries = mem::take(&mut self.entries);
    while (!self.tree.to_scan.is_empty() || self.gave_back) && !self.stopped() {
      self.round(&mut path, &mut entries);
    }
    self.tree.tidy_marked();
    (self.path, self.entries) = (path, emptied(entries));
    self.listed.clear();
    self.listed.shrink_to(KEPT / size_of::<ScanEntry>());

    let took = self.fs.ticks().saturating_sub(self.flush_ticks).min(LONGEST_FLUSH);
    if loss_scans_ran {
      self.loss.rest_until = Some(now + took + (took * REST).min(LONGEST_LOSS_REST));
    }
    took
  }

  fn check_marks(&mut self, path: &mut PathBuf, rest_is_over: bool, hints_due: bool) -> bool {
    self.mark_moved(hints_due);
    let mut marks = mem::take(&mut self.marks);
    self.tree.take_marked(&mut marks);
    let mut loss_checks_ran = false;
    let mut above = Vec::new();
    if !hints_due {
      above.extend(self.due_holds.iter().flat_map(|&held| self.tree.ancestors(held)));
      above.sort_unstable();
      above.dedup();
    }
    let mut path_of = None;
    for &(_, _, parent, key, since) in &marks {
      if self.stopped() {
        break;
      }
      let Some(mark) = self.tree.take_mark(key) else { continue };
      let mark = Mark { since, ..mark };
      if self.tree.frozen(key) {
        self.tree.unhold(key);
        self.tree.prune(key);
        continue;
      }
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
      self.check(key, path, Listed::NotAsked, mark);
    }
    self.marks = emptied(marks);
    loss_checks_ran
  }

  fn round(&mut self, path: &mut PathBuf, entries: &mut Vec<ScanEntry>) {
    let now_watched = self.commit();
    self.gave_back = false;
    let mut scans = mem::take(&mut self.scans);
    self.tree.take_scans(&mut scans);
    self.ask_again(now_watched);
    for &(key, depth, away) in &scans {
      if self.stopped() {
        break;
      }
      let Some(dir) = self.tree.get_mut(key).and_then(Node::dir_mut) else { continue };
      if let Some(queued) = &mut dir.scan {
        *queued = depth.max(*queued);
        dir.away |= away;
        continue;
      }
      self.away_scan = away;
      self.scan(key, depth, path, entries);
    }
    self.away_scan = false;
    self.scans = emptied(scans);
  }

  fn report(&mut self, kind: ErrorKind, path: &Path, source: io::Error) {
    if kind == ErrorKind::RemoteDisk || !self.errors.iter().any(|error| error.kind() == kind) {
      self.errors.push(Error::new(kind, Some(path.to_path_buf()), Some(source)));
    }
  }
}
