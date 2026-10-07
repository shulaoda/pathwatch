//! What the JSON file says about the machine and about the versions of the libraries.

use std::{collections::BTreeMap, fs, path::Path, process::Command};

use crate::record::Machine;

/// The output of a command, or an empty text when it cannot run.
fn output(program: &str, args: &[&str]) -> String {
  Command::new(program)
    .args(args)
    .output()
    .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    .unwrap_or_default()
}

/// The value of a `name: value` or `name=value` line of a text file.
fn field(file: &str, name: &str) -> Option<String> {
  let text = fs::read_to_string(file).ok()?;
  let line = text.lines().find(|line| line.starts_with(name))?;
  let value = line[name.len()..].trim_start_matches([' ', '\t', ':', '=']);
  Some(value.trim().trim_matches('"').to_owned())
}

#[must_use]
pub fn now_utc() -> String {
  output("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"])
}

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
  versions.insert("node".to_owned(), output("node", &["--version"]));

  let lock = fs::read_to_string(bench.join("Cargo.lock")).unwrap_or_default();
  let pathwatch = locked(&lock, "pathwatch").unwrap_or_default();
  let commit = commit(&bench.join("..")).unwrap_or_else(|| "no commit".to_owned());
  versions.insert("pathwatch".to_owned(), format!("{pathwatch} (working tree, {commit})"));
  versions.insert("notify".to_owned(), locked(&lock, "notify").unwrap_or_default());
  versions.insert("notify-fork".to_owned(), locked(&lock, "rolldown-notify").unwrap_or_default());

  for (name, package) in [("chokidar", "chokidar"), ("parcel", "@parcel/watcher")] {
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
