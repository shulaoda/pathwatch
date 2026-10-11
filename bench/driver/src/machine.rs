//! What the JSON file says about the machine and about the versions of the libraries.

use std::{collections::BTreeMap, fs, path::Path, process::Command};

use crate::{
  adapter::{node, venv_python},
  record::Machine,
};

/// The output of a command, or an empty text when it cannot run.
fn output(program: &str, args: &[&str]) -> String {
  Command::new(program)
    .args(args)
    .output()
    .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    .unwrap_or_default()
}

/// The value of a `name: value` or `name=value` line of a text file.
#[cfg(unix)]
fn field(file: &str, name: &str) -> Option<String> {
  let text = fs::read_to_string(file).ok()?;
  let line = text.lines().find(|line| line.starts_with(name))?;
  let value = line[name.len()..].trim_start_matches([' ', '\t', ':', '=']);
  Some(value.trim().trim_matches('"').to_owned())
}

#[cfg(unix)]
#[must_use]
pub fn now_utc() -> String {
  output("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"])
}

/// Windows has no `date` program that prints this form.
#[cfg(windows)]
#[must_use]
pub fn now_utc() -> String {
  let seconds = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map_or(0, |since| since.as_secs());
  let (days, rest) = (seconds / 86400, seconds % 86400);
  // The day number to a date of the calendar, by the algorithm of Howard Hinnant
  // (`civil_from_days`). Its years begin in March, so a leap day is the last day of a year.
  let days = days + 719_468;
  let (era, day_of_era) = (days / 146_097, days % 146_097);
  let year_of_era =
    (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146_096) / 365;
  let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
  let month = (5 * day_of_year + 2) / 153;
  let day = day_of_year - (153 * month + 2) / 5 + 1;
  let month = if month < 10 { month + 3 } else { month - 9 };
  let year = year_of_era + era * 400 + u64::from(month <= 2);
  let (hour, minute, second) = (rest / 3600, rest % 3600 / 60, rest % 60);
  format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Windows has no load average and no limit of open files that a process can raise. In the
/// place of the load average stands the number of cores that were busy in the half second
/// before the run, three times.
#[cfg(windows)]
#[must_use]
pub fn machine(tmp: &Path) -> Machine {
  use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

  let cores = std::thread::available_parallelism().map_or(0, usize::from);
  // SAFETY: the struct is plain numbers, so all zeros is a valid value.
  let mut memory: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
  memory.dwLength = u32::try_from(size_of::<MEMORYSTATUSEX>()).unwrap_or(0);
  // SAFETY: the pointer is to a `MEMORYSTATUSEX` with its length set, as the call wants.
  unsafe { GlobalMemoryStatusEx(&raw mut memory) };
  let registry = |key: &str, value: &str| {
    // `reg query` prints `    <value>    REG_SZ    <data>`.
    let text = output("reg", &["query", key, "/v", value]);
    let line = text.lines().find(|line| line.trim_start().starts_with(value));
    line.and_then(|line| Some(line.split_once("REG_SZ")?.1.trim().to_owned())).unwrap_or_default()
  };
  let version = r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion";
  let processor = r"HKLM\HARDWARE\DESCRIPTION\System\CentralProcessor\0";
  // `ver` prints `Microsoft Windows [Version 10.0.26200.1234]`, in the words of the system
  // language. The number is after the last space.
  let ver = output("cmd", &["/c", "ver"]);
  let build = ver.rsplit(' ').next().unwrap_or_default().trim_end_matches(']');
  // The registry says `Windows 10 Pro` on Windows 11 too. Windows 11 starts at build 22000.
  let os = registry(version, "ProductName");
  let number = build.split('.').nth(2).and_then(|number| number.parse::<u32>().ok());
  let os = if number >= Some(22000) { os.replace("Windows 10", "Windows 11") } else { os };
  let busy = busy_cores(cores);
  Machine {
    cpu: registry(processor, "ProcessorNameString"),
    cores,
    memory_gb: (memory.ullTotalPhys as f64 / f64::from(1 << 30)).round(),
    os,
    kernel: format!("Windows NT {build} {}", std::env::consts::ARCH),
    tmp_filesystem: filesystem(tmp),
    open_files_limit: 0,
    huge_pages: None,
    load_average: [busy; 3],
  }
}

/// How many cores were busy in the last half second, over the whole machine.
#[cfg(windows)]
fn busy_cores(cores: usize) -> f64 {
  use windows_sys::Win32::{Foundation::FILETIME, System::Threading::GetSystemTimes};

  let times = || {
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut idle, mut kernel, mut user) = (zero, zero, zero);
    // SAFETY: the three pointers are valid places for the results.
    unsafe { GetSystemTimes(&raw mut idle, &raw mut kernel, &raw mut user) };
    let ticks = |time: FILETIME| {
      ((u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)) as f64
    };
    // The kernel time holds the idle time.
    (ticks(idle), ticks(kernel) + ticks(user))
  };
  let (idle_before, all_before) = times();
  std::thread::sleep(std::time::Duration::from_millis(500));
  let (idle_after, all_after) = times();
  let all = all_after - all_before;
  if all <= 0.0 {
    return 0.0;
  }
  (1.0 - (idle_after - idle_before) / all) * cores as f64
}

/// The name of the file system that holds `path`: `NTFS`, `ReFS`, ...
#[cfg(windows)]
fn filesystem(path: &Path) -> String {
  use std::os::windows::ffi::OsStrExt;

  use windows_sys::Win32::Storage::FileSystem::{GetVolumeInformationW, GetVolumePathNameW};

  let path: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
  let mut volume = [0_u16; 261];
  let mut name = [0_u16; 261];
  // SAFETY: `path` ends with a zero, and `volume` is as long as the call is told.
  if unsafe { GetVolumePathNameW(path.as_ptr(), volume.as_mut_ptr(), 261) } == 0 {
    return String::new();
  }
  let null = std::ptr::null_mut::<u32>();
  // SAFETY: `volume` ends with a zero, `name` is as long as the call is told, and the other
  // answers are not asked for.
  let ok = unsafe {
    let no_label = std::ptr::null_mut();
    GetVolumeInformationW(volume.as_ptr(), no_label, 0, null, null, null, name.as_mut_ptr(), 261)
  };
  if ok == 0 {
    return String::new();
  }
  let end = name.iter().position(|&unit| unit == 0).unwrap_or(name.len());
  String::from_utf16_lossy(&name[..end])
}

#[cfg(unix)]
#[must_use]
pub fn machine(tmp: &Path) -> Machine {
  let mut load_average = [0.0; 3];
  // SAFETY: the buffer has room for the three numbers that are asked for.
  unsafe { libc::getloadavg(load_average.as_mut_ptr(), 3) };
  let cores = std::thread::available_parallelism().map_or(0, usize::from);
  let kernel = output("uname", &["-srm"]);
  let tmp_filesystem = filesystem(tmp);
  let open_files_limit = raise_open_files_limit();
  // The file reads `always [madvise] never`: the word in brackets is the setting.
  let huge_pages = fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled")
    .ok()
    .and_then(|text| Some(text.split_once('[')?.1.split_once(']')?.0.to_owned()));

  if cfg!(target_os = "macos") {
    let bytes: f64 = output("sysctl", &["-n", "hw.memsize"]).parse().unwrap_or(0.0);
    Machine {
      cpu: output("sysctl", &["-n", "machdep.cpu.brand_string"]),
      cores,
      memory_gb: (bytes / f64::from(1 << 30)).round(),
      os: format!("macOS {}", output("sw_vers", &["-productVersion"])),
      kernel,
      tmp_filesystem,
      open_files_limit,
      huge_pages,
      load_average,
    }
  } else {
    let kib: f64 = field("/proc/meminfo", "MemTotal")
      .and_then(|value| value.split_whitespace().next()?.parse().ok())
      .unwrap_or(0.0);
    Machine {
      // ARM machines have no `model name` line.
      cpu: field("/proc/cpuinfo", "model name").unwrap_or_else(|| output("uname", &["-m"])),
      cores,
      memory_gb: (kib / f64::from(1 << 20) * 10.0).round() / 10.0,
      os: field("/etc/os-release", "PRETTY_NAME").unwrap_or_default(),
      kernel,
      tmp_filesystem,
      open_files_limit,
      huge_pages,
      load_average,
    }
  }
}

/// A library that opens one file for each watched file needs more than the 256 open files
/// that a shell on macOS allows. The limit of the shell must not decide a result, so the
/// driver raises its own limit, and every adapter gets the same one. Returns the limit.
#[cfg(unix)]
fn raise_open_files_limit() -> u64 {
  const WANTED: libc::rlim_t = 65536;
  let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
  // SAFETY: `limit` is a valid place for the result.
  if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) } != 0 {
    return 0;
  }
  if limit.rlim_cur < WANTED {
    let raised = libc::rlimit { rlim_cur: WANTED.min(limit.rlim_max), ..limit };
    // SAFETY: the pointer is to a valid `rlimit`.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const raised) } == 0 {
      limit = raised;
    }
  }
  limit.rlim_cur
}

/// The name of the file system that holds `path`: `apfs`, `ext4`, `tmpfs`, ...
#[cfg(unix)]
fn filesystem(path: &Path) -> String {
  if cfg!(target_os = "macos") {
    // `mount` prints `/dev/disk3s5 on /System/Volumes/Data (apfs, local, ...)`.
    let device = output("df", &[&path.to_string_lossy()]);
    let device = device.lines().nth(1).and_then(|line| line.split_whitespace().next());
    let mounts = output("mount", &[]);
    let line = device.and_then(|device| {
      mounts.lines().find(|line| line.split_whitespace().next() == Some(device))
    });
    line
      .and_then(|line| Some(line.rsplit_once('(')?.1.split([',', ')']).next()?.to_owned()))
      .unwrap_or_default()
  } else {
    output("stat", &["-f", "-c", "%T", &path.to_string_lossy()])
  }
}

/// The versions of everything that is compared, by the name of the adapter.
#[must_use]
pub fn versions(bench: &Path, js: &Path) -> BTreeMap<String, String> {
  let mut versions = BTreeMap::new();
  versions.insert("rustc".to_owned(), output("rustc", &["--version"]));
  versions.insert("node".to_owned(), output(&node().to_string_lossy(), &["--version"]));
  let python = venv_python(&bench.join("py"));
  if python.exists() {
    let python = python.to_string_lossy();
    versions.insert("python".to_owned(), output(&python, &["--version"]));
    for package in ["watchdog", "watchfiles"] {
      let code = format!("import importlib.metadata as m; print(m.version('{package}'))");
      versions.insert(package.to_owned(), output(&python, &["-c", &code]));
    }
  }
  let go_mod = fs::read_to_string(bench.join("go/go.mod")).unwrap_or_default();
  let mut words =
    go_mod.split_whitespace().skip_while(|word| *word != "github.com/fsnotify/fsnotify");
  let fsnotify = words.nth(1).map_or("not installed", |version| version.trim_start_matches('v'));
  versions.insert("fsnotify".to_owned(), fsnotify.to_owned());

  let lock = fs::read_to_string(bench.join("Cargo.lock")).unwrap_or_default();
  let pathwatch = locked(&lock, "pathwatch").unwrap_or_default();
  let commit = commit(&bench.join("..")).unwrap_or_else(|| "no commit".to_owned());
  versions.insert("pathwatch".to_owned(), format!("{pathwatch} (working tree, {commit})"));
  versions.insert("notify".to_owned(), locked(&lock, "notify").unwrap_or_default());
  versions.insert("notify-fork".to_owned(), locked(&lock, "rolldown-notify").unwrap_or_default());
  let debouncer = locked(&lock, "notify-debouncer-full").unwrap_or_default();
  versions.insert("notify-debouncer".to_owned(), debouncer);

  let packages =
    [("chokidar", "chokidar"), ("parcel", "@parcel/watcher"), ("watchpack", "watchpack")];
  for (name, package) in packages {
    let file = js.join("node_modules").join(package).join("package.json");
    let version = fs::read_to_string(file)
      .ok()
      .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
      .and_then(|json| Some(json.get("version")?.as_str()?.to_owned()))
      .unwrap_or_else(|| "not installed".to_owned());
    versions.insert(name.to_owned(), version);
  }
  versions.insert("fs-watch".to_owned(), "part of node".to_owned());
  versions
}

/// The version of a package in `Cargo.lock`, with its git commit when it comes from git.
fn locked(lock: &str, name: &str) -> Option<String> {
  let mut lines = lock.lines().skip_while(|line| *line != format!("name = \"{name}\""));
  lines.next()?;
  let mut version = String::new();
  // The lines of one package end at an empty line.
  for line in lines.take_while(|line| !line.is_empty()) {
    if let Some(value) = line.strip_prefix("version = ") {
      version = value.trim_matches('"').to_owned();
    }
    if let Some(value) = line.strip_prefix("source = \"git+") {
      let commit = value.trim_matches('"').rsplit_once('#').map_or("", |(_, commit)| commit);
      version = format!("{version} (git {})", &commit[..commit.len().min(8)]);
    }
  }
  Some(version)
}

/// The commit that the repository is at, read from its files (no git command is run).
fn commit(repository: &Path) -> Option<String> {
  let head = fs::read_to_string(repository.join(".git/HEAD")).ok()?;
  let head = head.trim();
  let hash = match head.strip_prefix("ref: ") {
    Some(name) => fs::read_to_string(repository.join(".git").join(name)).ok()?,
    None => head.to_owned(),
  };
  Some(hash.trim().chars().take(8).collect())
}
