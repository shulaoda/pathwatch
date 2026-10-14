//! What a run writes down: the JSON file is these types, and `report` reads them back.

use std::{collections::BTreeMap, time::Duration};

use serde::{Deserialize, Serialize};

/// Everything of one call of the driver.
#[derive(Serialize, Deserialize)]
pub struct Report {
  /// UTC, when the driver started.
  pub started: String,
  pub machine: Machine,
  /// The version of every library, of Node and of the Rust compiler.
  pub versions: BTreeMap<String, String>,
  pub settings: Settings,
  /// Each adapter when it watches nothing: what the runtime and the library cost by themselves.
  pub bases: BTreeMap<String, Base>,
  pub runs: Vec<Run>,
  /// macOS: the CPU time that fseventsd had used, in seconds, when the driver started and when
  /// it ended (`os`).
  #[serde(default)]
  pub daemon_cpu_s: Option<[f64; 2]>,
}

#[derive(Serialize, Deserialize)]
pub struct Machine {
  pub cpu: String,
  pub cores: usize,
  pub memory_gb: f64,
  pub os: String,
  pub kernel: String,
  /// The file system of the temporary directory.
  pub tmp_filesystem: String,
  /// Linux: the setting of transparent huge pages (`always`, `madvise`, `never`).
  pub huge_pages: Option<String>,
  /// How many files a process may have open: the driver raises its own limit to this, and
  /// the adapters get it from the driver.
  pub open_files_limit: u64,
  /// The load average (1, 5, 15 minutes) when the driver started. Times are only worth
  /// something when it is far below the number of cores.
  pub load_average: [f64; 3],
}

#[derive(Serialize, Deserialize)]
pub struct Settings {
  /// All sizes are multiplied by this. 1 is the full size.
  pub scale: f64,
  /// How long a quiet phase is, in seconds.
  pub quiet_secs: f64,
  /// How often each scenario ran with each adapter. The tables show the middle run.
  #[serde(default = "once")]
  pub repeat: usize,
  pub tmp: String,
  /// S3: the noise is written next to the project and not inside it.
  pub noise_outside: bool,
  /// Linux: the adapters ran with the huge pages of malloc (the default of the system). When
  /// false, they ran with `GLIBC_TUNABLES=glibc.malloc.hugetlb=0`.
  pub keep_huge_pages: bool,
}

fn once() -> usize {
  1
}

/// An adapter that watches nothing.
#[derive(Serialize, Deserialize, Clone)]
pub struct Base {
  pub setup_ms: f64,
  pub cpu_ms: f64,
  pub rss_kb: u64,
  /// The memory that belongs to the process alone. macOS: the physical footprint (the
  /// "memory" of Activity Monitor). Linux: its private pages.
  pub footprint_kb: Option<u64>,
}

/// One scenario with one adapter.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Run {
  pub scenario: String,
  pub adapter: String,
  /// `ok`, or why there are no numbers: `unsupported: ...`, `failed: ...`.
  pub status: String,
  /// The directory of this run. Every path in the samples below starts with it.
  pub root: String,
  /// The sizes of this run: files, directories, watched paths.
  pub sizes: BTreeMap<String, usize>,
  /// What the adapter said about how it uses the library.
  pub notes: Vec<String>,
  /// How many error lines the adapter printed, and the first of them.
  pub errors: usize,
  pub error_samples: Vec<String>,
  /// From the start of the process to `ready`.
  pub setup_ms: f64,
  /// User plus system time of the process up to `ready`.
  pub setup_cpu_ms: f64,
  pub rss_ready_kb: u64,
  pub footprint_ready_kb: Option<u64>,
  /// After all phases.
  pub rss_end_kb: u64,
  pub footprint_end_kb: Option<u64>,
  /// The highest resident size of the whole life of the process.
  pub peak_rss_kb: u64,
  /// User plus system time of the whole life of the process.
  pub total_cpu_ms: f64,
  pub phases: Vec<Phase>,
  /// How long the driver waited for the OS to be idle before the start and before each phase
  /// (`os`), and how often it gave up.
  #[serde(default)]
  pub os_wait_ms: f64,
  #[serde(default)]
  pub os_waits_given_up: usize,
}

impl Run {
  /// Writes down one wait for the OS. `before`: what the wait was for.
  pub fn waited(&mut self, waited: &crate::os::Waited, before: &str) {
    self.os_wait_ms += ms(waited.time);
    if waited.gave_up {
      self.os_waits_given_up += 1;
      self.notes.push(format!(
        "{before}: fseventsd was still busy after {} s, and the driver went on",
        crate::os::LONGEST.as_secs()
      ));
    }
  }
}

/// One step of a scenario: some changes, and what the adapter printed for them.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Phase {
  pub name: String,
  /// How many paths changed. Each of them should get at least one event.
  pub changes: usize,
  /// Changed paths that got no event.
  pub missed: usize,
  pub missed_samples: Vec<String>,
  /// More events for a changed path, after its first one.
  pub duplicates: usize,
  /// Events for a path that changed in an earlier phase of the run, or for a directory above
  /// one: they came late, or the library says the same thing again.
  pub late: usize,
  /// Events for a directory that is above a changed path.
  pub dir_events: usize,
  /// Events for every other path: paths that the driver never changed, or that nobody asked
  /// for.
  pub extra: usize,
  pub extra_samples: Vec<String>,
  /// Events of the kind `access` (a file was opened or read). They are not changes and are
  /// never used for a time.
  pub access: usize,
  /// The word of the first event of each changed path, and how often it came.
  pub kinds: BTreeMap<String, usize>,
  /// From the start of a change to the first event of its path, over all paths that got one.
  /// In a burst every path counts from the start of the burst, so `max_ms` is the time until
  /// the events for ALL paths came (when nothing was missed).
  pub latency: Option<Latency>,
  /// The changes were made at once: every path counts from the start, and `max_ms` is when the
  /// events for all of them had come.
  #[serde(default)]
  pub burst: bool,
  /// How long the driver took to make the changes of a burst.
  pub write_ms: f64,
  /// How long the phase took, with the wait for late events.
  pub wall_ms: f64,
  /// User plus system time of the adapter process in the phase.
  pub cpu_ms: f64,
  /// The phase was stopped early because five changes in a row got no event.
  pub abandoned: bool,
  /// More numbers of this phase, by name.
  pub info: BTreeMap<String, f64>,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct Latency {
  pub p50_ms: f64,
  pub p95_ms: f64,
  pub max_ms: f64,
}

impl Latency {
  /// `None` for an empty list.
  #[must_use]
  pub fn of(mut times: Vec<f64>) -> Option<Self> {
    times.sort_by(f64::total_cmp);
    let at = |part: f64| {
      // The value at this part of the sorted list ("nearest rank").
      let rank = (part * times.len() as f64).ceil() as usize;
      times[rank.clamp(1, times.len()) - 1]
    };
    if times.is_empty() {
      return None;
    }
    Some(Self { p50_ms: at(0.50), p95_ms: at(0.95), max_ms: at(1.0) })
  }
}

/// Milliseconds with three digits after the point.
#[must_use]
pub fn ms(time: Duration) -> f64 {
  (time.as_secs_f64() * 1e6).round() / 1e3
}
