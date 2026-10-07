//! Reading the CPU time and the memory of another process, by its process id.

use std::time::Duration;

/// One look at a process.
#[derive(Clone, Copy, Default)]
pub struct Sample {
  /// User plus system time since the process started.
  pub cpu: Duration,
  /// The resident size, in bytes.
  pub rss: u64,
  /// The memory that belongs to this process alone, in bytes. macOS: the physical footprint
  /// (the "memory" of Activity Monitor). Linux: the private pages (`Private_Clean` plus
  /// `Private_Dirty`).
  pub footprint: Option<u64>,
}

impl Sample {
  #[must_use]
  pub fn rss_kb(&self) -> u64 {
    self.rss / 1024
  }

  #[must_use]
  pub fn footprint_kb(&self) -> Option<u64> {
    self.footprint.map(|bytes| bytes / 1024)
  }
}

/// `None` when the process is gone.
#[cfg(target_os = "macos")]
#[must_use]
pub fn sample(pid: i32) -> Option<Sample> {
  #[repr(C)]
  struct Timebase {
    numer: u32,
    denom: u32,
  }
  unsafe extern "C" {
    fn mach_timebase_info(info: *mut Timebase) -> i32;
  }

  // SAFETY: the struct is plain numbers, so all zeros is a valid value.
  let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
  // SAFETY: the buffer is a `rusage_info_v2`, which is what the flavor `RUSAGE_INFO_V2` fills.
  let result = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, (&raw mut info).cast()) };
  if result != 0 {
    return None;
  }
  // The times are in the ticks of the Mach clock, which are not nanoseconds on Apple silicon.
  let mut timebase = Timebase { numer: 1, denom: 1 };
  // SAFETY: the pointer is to a struct of two `u32`, as the call wants.
  unsafe { mach_timebase_info(&raw mut timebase) };
  let ticks = u128::from(info.ri_user_time + info.ri_system_time);
  let nanos = ticks * u128::from(timebase.numer) / u128::from(timebase.denom);
  Some(Sample {
    cpu: Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX)),
    rss: info.ri_resident_size,
    footprint: Some(info.ri_phys_footprint),
  })
}

/// `None` when the process is gone.
#[cfg(target_os = "linux")]
#[must_use]
pub fn sample(pid: i32) -> Option<Sample> {
  // The CPU clock of the process: user plus system time of all its threads, in nanoseconds.
  let mut clock: libc::clockid_t = 0;
  // SAFETY: `clock` is a valid place for the result.
  if unsafe { libc::clock_getcpuclockid(pid, &raw mut clock) } != 0 {
    return None;
  }
  let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
  // SAFETY: `time` is a valid place for the result.
  if unsafe { libc::clock_gettime(clock, &raw mut time) } != 0 {
    return None;
  }
  // `smaps_rollup` adds up the pages of the process one by one. The cheaper `statm` is a
  // counter that the kernel keeps lazily, and it was off by 2 MiB for a process of 8 MiB.
  let rollup = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok()?;
  let kib = |name: &str| -> u64 {
    let line = rollup.lines().find(|line| line.starts_with(name));
    line.and_then(|line| line.split_whitespace().nth(1)?.parse().ok()).unwrap_or(0)
  };
  Some(Sample {
    cpu: Duration::new(u64::try_from(time.tv_sec).ok()?, u32::try_from(time.tv_nsec).ok()?),
    rss: kib("Rss:") * 1024,
    // The pages that no other process shares.
    footprint: Some((kib("Private_Clean:") + kib("Private_Dirty:")) * 1024),
  })
}

/// The highest resident size of a running process so far, in KiB, where the system tells it
/// for a running process.
///
/// Linux only. There, the number that `wait4` gives for a child (`ru_maxrss`) also counts the
/// memory of the driver, from the moment before the child became another program: it said
/// 6.4 MiB for an adapter that never had more than 2.7 MiB.
#[must_use]
pub fn peak_rss_kb(pid: i32) -> Option<u64> {
  let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
  let line = status.lines().find(|line| line.starts_with("VmHWM:"))?;
  line.split_whitespace().nth(1)?.parse().ok()
}

/// What the system says about a child process that ended.
pub struct Ended {
  /// User plus system time of its whole life.
  pub cpu: Duration,
  /// Its highest resident size, in KiB.
  pub peak_rss_kb: u64,
}

/// Waits until the child process ends, for at most `patience`; then it is killed.
#[must_use]
pub fn wait_for_end(pid: i32, patience: Duration) -> Option<Ended> {
  let start = std::time::Instant::now();
  let mut status = 0;
  // SAFETY: the struct is plain numbers, so all zeros is a valid value.
  let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
  loop {
    // SAFETY: `status` and `usage` are valid places for the results.
    let result = unsafe { libc::wait4(pid, &raw mut status, libc::WNOHANG, &raw mut usage) };
    if result == pid {
      break;
    }
    if result < 0 {
      return None;
    }
    if start.elapsed() > patience {
      // SAFETY: `pid` is our own child, and it has not been waited for yet.
      unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    std::thread::sleep(Duration::from_millis(5));
  }
  let time = |value: libc::timeval| {
    Duration::new(
      u64::try_from(value.tv_sec).unwrap_or(0),
      u32::try_from(value.tv_usec).unwrap_or(0) * 1000,
    )
  };
  // `ru_maxrss` is in bytes on macOS and in KiB on Linux.
  let peak = u64::try_from(usage.ru_maxrss).unwrap_or(0);
  let peak_rss_kb = if cfg!(target_os = "macos") { peak / 1024 } else { peak };
  Some(Ended { cpu: time(usage.ru_utime) + time(usage.ru_stime), peak_rss_kb })
}
