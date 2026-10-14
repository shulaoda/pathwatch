//! The work that the OS does for a change outside the adapter. On macOS every change of the file
//! system goes through one daemon, fseventsd, before any FSEvents client hears of it, and a big
//! directory keeps it busy for seconds after it is made or removed (measured: a directory of
//! 20,000 files, 4 s after it is made and 12 s after it is removed; a burst of 2,000 new files
//! in it, 4 to 5 s). A run or a phase that starts meanwhile measures that backlog, and an
//! adapter that runs after a big tree pays for the one before it. So the driver waits until the
//! daemon is idle before each run and each phase. Elsewhere no daemon stands between the file
//! system and the watcher, and nothing is waited for.

use std::{
  process::Command,
  time::{Duration, Instant},
};

/// The daemon counts as idle when it used less than this share of a core over `WINDOW`.
const WINDOW: Duration = Duration::from_millis(500);
const IDLE_SHARE: f64 = 0.05;
/// The wait gives up after this long: the run goes on, and the report says so.
pub const LONGEST: Duration = Duration::from_secs(120);

/// The daemon whose backlog the driver waits for: fseventsd on macOS.
#[derive(Clone, Copy)]
pub struct Daemon {
  pid: u32,
}

/// How long one wait took, and whether it gave up.
pub struct Waited {
  pub time: Duration,
  pub gave_up: bool,
}

impl Daemon {
  /// fseventsd on macOS, and `None` on every other system.
  #[must_use]
  pub fn find() -> Option<Self> {
    if !cfg!(target_os = "macos") {
      return None;
    }
    let out = Command::new("pgrep").args(["-x", "fseventsd"]).output().ok()?;
    let pid = String::from_utf8_lossy(&out.stdout).lines().next()?.trim().parse().ok()?;
    Some(Self { pid })
  }

  /// The CPU time of the daemon. `ps` reads it: the daemon belongs to root, and the calls of
  /// `measure` may not read such a process.
  fn cpu(self) -> Option<Duration> {
    let pid = self.pid.to_string();
    let out = Command::new("ps").args(["-o", "time=", "-p", &pid]).output().ok()?;
    // `minutes:seconds.hundredths`, with hours in front for a long life.
    let text = String::from_utf8_lossy(&out.stdout);
    let seconds = text
      .trim()
      .split(':')
      .try_fold(0.0, |total: f64, part| part.parse::<f64>().ok().map(|part| total * 60.0 + part))?;
    Some(Duration::from_secs_f64(seconds))
  }

  /// Waits until the daemon is idle, and `LONGEST` at the most. `pass` spends each slice of
  /// the wait: the caller reads the lines of its adapter meanwhile.
  pub fn wait_idle(self, pass: &mut dyn FnMut(Duration)) -> Waited {
    let start = Instant::now();
    while let (Some(before), began) = (self.cpu(), Instant::now()) {
      pass(WINDOW);
      let Some(after) = self.cpu() else { break };
      let share = after.saturating_sub(before).as_secs_f64() / began.elapsed().as_secs_f64();
      if share < IDLE_SHARE {
        break;
      }
      if start.elapsed() >= LONGEST {
        return Waited { time: start.elapsed(), gave_up: true };
      }
    }
    Waited { time: start.elapsed(), gave_up: false }
  }
}
