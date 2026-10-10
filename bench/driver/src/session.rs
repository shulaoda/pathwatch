//! One adapter at work: the driver changes files, reads the lines of the adapter and writes
//! down what it sees. The three kinds of phases are here: single changes, a burst, quiet time.

use std::{
  collections::{BTreeMap, HashMap, HashSet},
  io,
  path::{Path, PathBuf},
  time::{Duration, Instant},
};

use crate::{
  adapter::{Adapter, Child, Job, Line, Places, READY_PATIENCE},
  measure::{Sample, peak_rss_kb, sample, wait_for_end},
  record::{Base, Latency, Phase, Run, ms},
};

/// How many samples of missed and extra paths a phase keeps.
const SAMPLES: usize = 5;
/// The pause after a single change, so its late events are not taken for the next change.
const GAP: Duration = Duration::from_millis(30);
/// A phase is over when the adapter printed nothing for this long ...
const SILENCE: Duration = Duration::from_millis(250);
/// ... or when this much time has passed since the phase had all its events.
const LONGEST_DRAIN: Duration = Duration::from_secs(3);
/// A burst gives up when not one line came for this long, and in any case after `LONGEST_BURST`.
const STALL: Duration = Duration::from_secs(3);
const LONGEST_BURST: Duration = Duration::from_secs(60);
/// Single changes stop after this many changes in a row with no event.
const MISSES_IN_A_ROW: usize = 5;

/// What the adapter printed for one set of changes.
struct Tally {
  /// Each changed path, and the time and the word of its first event.
  changed: HashMap<PathBuf, Option<(Instant, String)>>,
  /// The directories above the changed paths, up to the root of the run.
  above: HashSet<PathBuf>,
  /// How many changed paths have an event.
  seen: usize,
  duplicates: usize,
  late: usize,
  dir_events: usize,
  extra: usize,
  extra_samples: Vec<String>,
  access: usize,
}

impl Tally {
  fn new(changed: &[PathBuf], root: &Path) -> Self {
    let mut above = HashSet::new();
    for path in changed {
      for dir in path.ancestors().skip(1) {
        // The directories further up are in the set already.
        if !dir.starts_with(root) || !above.insert(dir.to_path_buf()) {
          break;
        }
      }
    }
    Self {
      changed: changed.iter().map(|path| (path.clone(), None)).collect(),
      above,
      seen: 0,
      duplicates: 0,
      late: 0,
      dir_events: 0,
      extra: 0,
      extra_samples: Vec::new(),
      access: 0,
    }
  }

  fn complete(&self) -> bool {
    self.seen == self.changed.len()
  }

  /// `earlier` holds the paths that changed in the steps before, and the directories above
  /// them.
  fn take(&mut self, at: Instant, kind: String, path: PathBuf, earlier: &HashSet<PathBuf>) {
    // Opening or reading a file is not a change.
    if kind == "access" {
      self.access += 1;
    } else if let Some(first) = self.changed.get_mut(&path) {
      if first.is_none() {
        *first = Some((at, kind));
        self.seen += 1;
      } else {
        self.duplicates += 1;
      }
    } else if self.above.contains(&path) {
      self.dir_events += 1;
    } else if earlier.contains(&path) {
      self.late += 1;
    } else {
      self.extra += 1;
      if self.extra_samples.len() < SAMPLES {
        self.extra_samples.push(format!("{kind} {}", path.display()));
      }
    }
  }

  /// Adds what this tally saw to the phase. `start` is when the changes began. The changed
  /// paths and the directories above them go to `earlier`, for the steps that follow.
  fn add_to(
    self,
    phase: &mut Phase,
    times: &mut Vec<f64>,
    start: Instant,
    earlier: &mut HashSet<PathBuf>,
  ) {
    earlier.extend(self.above);
    earlier.extend(self.changed.keys().cloned());
    phase.changes += self.changed.len();
    phase.missed += self.changed.len() - self.seen;
    phase.duplicates += self.duplicates;
    phase.late += self.late;
    phase.dir_events += self.dir_events;
    phase.extra += self.extra;
    phase.access += self.access;
    for sample in self.extra_samples {
      if phase.extra_samples.len() < SAMPLES {
        phase.extra_samples.push(sample);
      }
    }
    for (path, first) in self.changed {
      match first {
        Some((at, kind)) => {
          times.push(ms(at.saturating_duration_since(start)));
          *phase.kinds.entry(kind).or_default() += 1;
        }
        None if phase.missed_samples.len() < SAMPLES => {
          phase.missed_samples.push(path.display().to_string());
        }
        None => {}
      }
    }
  }
}

/// A scenario with one adapter, from the start of the process to its end.
pub struct Session {
  child: Child,
  /// The directory of this run. Events for directories above it are never "above a change".
  root: PathBuf,
  run: Run,
  /// Every path that the driver changed so far, and the directories above them. A late event
  /// for one of them is not an event for a path that did not change.
  earlier: HashSet<PathBuf>,
  /// The adapter is gone: every later phase is skipped.
  dead: bool,
}

impl Session {
  /// Starts the adapter and waits for `ready`. When the adapter cannot do the job, the `Run`
  /// comes back with the reason in its `status`.
  pub fn start(
    adapter: &Adapter,
    places: &Places,
    job: &Job,
    root: &Path,
    mut run: Run,
  ) -> Result<Self, Box<Run>> {
    let child = match Child::start(adapter, places, job) {
      Ok(child) => child,
      Err(error) => {
        run.status = format!("failed: the adapter did not start: {error}");
        return Err(Box::new(run));
      }
    };
    let mut session =
      Self { child, root: root.to_path_buf(), run, earlier: HashSet::new(), dead: false };
    let until = Instant::now() + READY_PATIENCE;
    let ready = loop {
      match session.child.next(until) {
        Some(Line::Ready(at)) => break at,
        Some(Line::Unsupported(why)) => {
          session.run.status = format!("unsupported: {why}");
          return Err(Box::new(session.end()));
        }
        Some(Line::End) => {
          session.run.status = "failed: the adapter ended before it was ready".to_owned();
          return Err(Box::new(session.end()));
        }
        Some(line) => session.note(line),
        None => {
          session.run.status = format!("failed: not ready after {} s", READY_PATIENCE.as_secs());
          return Err(Box::new(session.end()));
        }
      }
    };
    let now = session.sample();
    session.run.status = "ok".to_owned();
    session.run.setup_ms = ms(ready - session.child.spawned);
    session.run.setup_cpu_ms = ms(now.cpu);
    session.run.rss_ready_kb = now.rss_kb();
    session.run.footprint_ready_kb = now.footprint_kb();
    Ok(session)
  }

  /// Single changes: one file is written, and the driver waits for the first event of its
  /// path, for at most `patience`. Then the next file.
  pub fn single_edits(&mut self, name: &str, files: &[PathBuf], patience: Duration) {
    self.single_changes(name, files, patience, crate::tree::write);
  }

  /// As `single_edits`, with another way to change a file.
  pub fn single_changes(
    &mut self,
    name: &str,
    files: &[PathBuf],
    patience: Duration,
    change: impl Fn(&Path) -> std::io::Result<()>,
  ) {
    if self.dead {
      return;
    }
    let mut phase = Phase { name: name.to_owned(), ..Phase::default() };
    let mut times = Vec::new();
    let phase_start = Instant::now();
    let before = self.sample();
    let mut misses_in_a_row = 0;
    for file in files {
      let mut tally = Tally::new(std::slice::from_ref(file), &self.root);
      let start = Instant::now();
      match change(file) {
        Ok(()) => self.pump(&mut tally, start + patience, None),
        Err(error) => self.refused(&mut phase, &error),
      }
      misses_in_a_row = if tally.complete() { 0 } else { misses_in_a_row + 1 };
      // The late events of this change belong to it and not to the next one.
      self.pump_for(&mut tally, GAP);
      tally.add_to(&mut phase, &mut times, start, &mut self.earlier);
      if self.dead || misses_in_a_row == MISSES_IN_A_ROW {
        phase.abandoned = phase.changes < files.len();
        break;
      }
    }
    self.finish_phase(phase, times, phase_start, before);
  }

  /// A burst: `change` makes all changes as fast as it can, and the driver waits until every
  /// path in `changed` had an event. Every path counts its time from the start of the burst.
  pub fn burst(
    &mut self,
    name: &str,
    changed: &[PathBuf],
    change: impl FnOnce() -> io::Result<()>,
  ) {
    self.burst_from(name, changed, || {
      let start = Instant::now();
      change().map(|()| start)
    });
  }

  /// As `burst`, but `change` says from which moment the times of the changed paths count.
  pub fn burst_from(
    &mut self,
    name: &str,
    changed: &[PathBuf],
    change: impl FnOnce() -> io::Result<Instant>,
  ) {
    if self.dead {
      return;
    }
    let mut phase = Phase { name: name.to_owned(), ..Phase::default() };
    let mut times = Vec::new();
    let mut tally = Tally::new(changed, &self.root);
    let before = self.sample();
    let start = Instant::now();
    let changed = change();
    let written = Instant::now();
    phase.write_ms = ms(written - start);
    let count_from = match changed {
      Ok(count_from) => {
        self.pump(&mut tally, written + LONGEST_BURST, Some(STALL));
        count_from
      }
      Err(error) => {
        self.refused(&mut phase, &error);
        start
      }
    };
    self.drain(&mut tally);
    tally.add_to(&mut phase, &mut times, count_from, &mut self.earlier);
    self.finish_phase(phase, times, start, before);
  }

  /// Nothing changes for `time`: what the watcher costs when it only waits. Every event in
  /// this time is one too many.
  pub fn quiet(&mut self, name: &str, time: Duration) {
    if self.dead {
      return;
    }
    // What is still on its way from the phase before is not part of the quiet time.
    let mut earlier = Tally::new(&[], &self.root);
    self.drain(&mut earlier);
    let mut phase = Phase { name: name.to_owned(), ..Phase::default() };
    let mut tally = Tally::new(&[], &self.root);
    let before = self.sample();
    let start = Instant::now();
    self.pump_for(&mut tally, time);
    tally.add_to(&mut phase, &mut Vec::new(), start, &mut self.earlier);
    self.finish_phase(phase, Vec::new(), start, before);
  }

  /// The system refused a change of the driver. On Windows that happens while the watcher
  /// holds a file or a directory open: a directory with an open handle below it cannot be
  /// renamed. The phase counts as stopped early, and a note says why.
  fn refused(&mut self, phase: &mut Phase, error: &io::Error) {
    phase.abandoned = true;
    let note = format!("{}: the system refused a change of the driver: {error}", phase.name);
    if !self.run.notes.contains(&note) {
      self.run.notes.push(note);
    }
  }

  /// Closes the adapter and returns everything that was written down.
  pub fn end(mut self) -> Run {
    let last = self.sample();
    self.run.rss_end_kb = last.rss_kb();
    self.run.footprint_end_kb = last.footprint_kb();
    let peak = peak_rss_kb(self.child.pid);
    self.child.close();
    if let Some(ended) = wait_for_end(self.child.pid, Duration::from_secs(10)) {
      self.run.peak_rss_kb = peak.unwrap_or(ended.peak_rss_kb);
      self.run.total_cpu_ms = ms(ended.cpu);
    }
    if self.dead && self.run.status == "ok" {
      self.run.status = "failed: the adapter ended in the middle of the run".to_owned();
    }
    self.run
  }

  /// The numbers of an adapter that watches nothing. They are taken at `ready`, at the same
  /// moment as the numbers of a run, so the two can be compared.
  #[must_use]
  pub fn into_base(self) -> Base {
    let run = self.end();
    Base {
      setup_ms: run.setup_ms,
      cpu_ms: run.setup_cpu_ms,
      rss_kb: run.rss_ready_kb,
      footprint_kb: run.footprint_ready_kb,
    }
  }

  /// Reads lines into the tally until it is complete or `until` has come. With `stall`, the
  /// wait also ends when no line came for that long.
  fn pump(&mut self, tally: &mut Tally, until: Instant, stall: Option<Duration>) {
    while !tally.complete() && !self.dead {
      let wait = stall.map_or(until, |stall| until.min(Instant::now() + stall));
      if !self.take_line(tally, wait) {
        return;
      }
    }
  }

  /// Reads lines into the tally for a fixed time.
  fn pump_for(&mut self, tally: &mut Tally, time: Duration) {
    let until = Instant::now() + time;
    while !self.dead && Instant::now() < until {
      self.take_line(tally, until);
    }
  }

  /// Reads lines into the tally until the adapter is silent.
  fn drain(&mut self, tally: &mut Tally) {
    let until = Instant::now() + LONGEST_DRAIN;
    while !self.dead && Instant::now() < until {
      if !self.take_line(tally, until.min(Instant::now() + SILENCE)) {
        return;
      }
    }
  }

  /// Takes one line. False: none came before `until`.
  fn take_line(&mut self, tally: &mut Tally, until: Instant) -> bool {
    match self.child.next(until) {
      Some(Line::Event { at, kind, path }) => tally.take(at, kind, path, &self.earlier),
      Some(line) => self.note(line),
      None => return false,
    }
    true
  }

  /// A line that is not an event.
  fn note(&mut self, line: Line) {
    match line {
      Line::Note(text) => self.run.notes.push(text),
      Line::Error(text) => {
        self.run.errors += 1;
        if self.run.error_samples.len() < SAMPLES {
          self.run.error_samples.push(text);
        }
      }
      Line::End => self.dead = true,
      Line::Ready(_) | Line::Unsupported(_) | Line::Event { .. } => {}
    }
  }

  fn finish_phase(&mut self, mut phase: Phase, times: Vec<f64>, start: Instant, before: Sample) {
    phase.wall_ms = ms(start.elapsed());
    phase.cpu_ms = ms(self.sample().cpu.saturating_sub(before.cpu));
    phase.latency = Latency::of(times);
    self.run.phases.push(phase);
  }

  fn sample(&self) -> Sample {
    sample(self.child.pid).unwrap_or_default()
  }

  /// More numbers for the phase that was written down last.
  pub fn info(&mut self, numbers: BTreeMap<String, f64>) {
    if let Some(phase) = self.run.phases.last_mut() {
      phase.info.extend(numbers);
    }
  }
}
