//! `bench`: runs the scenarios with the adapters and writes one JSON file.
//!
//! ```text
//! bench [--quick] [--scale <n>] [--quiet-secs <n>] [--repeat <n>] [--scenarios s1,s2]
//!       [--adapters a,b] [--tmp <dir>] [--js <dir>] [--noise-outside] [--keep-huge-pages]
//!       [--out <file.json>]
//! ```

use std::{
  collections::BTreeMap,
  env, fs,
  path::{Path, PathBuf},
  process,
  time::{Duration, Instant},
};

use driver::{
  adapter::{ADAPTERS, Adapter, Places, own_directory, venv_python},
  machine,
  os::Daemon,
  record::{self, Report},
  scenarios::{self, SCENARIOS, Settings, Tree},
};

const USAGE: &str = "\
bench [flags]

  --quick              small sizes for a try: the same as --scale 0.05 --quiet-secs 2
  --scale <n>          multiply every size by n (default 1: the full sizes)
  --quiet-secs <n>     how long the quiet phase is (default 5)
  --repeat <n>         run each scenario n times with each adapter (default 1); the tables
                       show the middle run, and how far the runs are apart
  --scenarios <list>   s1,s2,s3,s4,s5,s6,s7 (default: all)
  --adapters <list>    pathwatch,pathwatch-0,notify-fork,chokidar,fs-watch,fsnotify,watchdog
                       (default: all), or `rust` for the first three
  --tmp <dir>          where the trees are made (default: the temporary directory of the system)
  --js <dir>           the directory of the Node adapters with their node_modules
                       (default: bench/js)
  --noise-outside      S3: write the noise next to the project and not inside it
  --keep-huge-pages    Linux: do not turn off the huge pages of malloc for the adapters
  --out <file>         the JSON file (default: bench/results/<os>-<time>.json)
";

struct Flags {
  scale: f64,
  quiet_secs: f64,
  repeat: usize,
  scenarios: Option<Vec<String>>,
  adapters: Option<Vec<String>>,
  tmp: PathBuf,
  js: PathBuf,
  noise_outside: bool,
  keep_huge_pages: bool,
  out: Option<PathBuf>,
}

fn flags(bench: &Path) -> Flags {
  let mut flags = Flags {
    scale: 1.0,
    quiet_secs: 5.0,
    repeat: 1,
    scenarios: None,
    adapters: None,
    tmp: env::temp_dir(),
    js: bench.join("js"),
    noise_outside: false,
    keep_huge_pages: false,
    out: None,
  };
  let mut args = env::args().skip(1);
  let list = |value: String| value.split(',').map(str::to_owned).collect::<Vec<_>>();
  while let Some(flag) = args.next() {
    let mut value = || args.next().unwrap_or_else(|| fail(&format!("{flag} needs a value")));
    match flag.as_str() {
      "--quick" => (flags.scale, flags.quiet_secs) = (0.05, 2.0),
      "--scale" => flags.scale = value().parse().unwrap_or_else(|_| fail("--scale <number>")),
      "--quiet-secs" => {
        flags.quiet_secs = value().parse().unwrap_or_else(|_| fail("--quiet-secs <number>"));
      }
      "--repeat" => {
        flags.repeat = value().parse().unwrap_or_else(|_| fail("--repeat <number>"));
      }
      "--scenarios" => flags.scenarios = Some(list(value())),
      "--adapters" => flags.adapters = Some(list(value())),
      "--tmp" => flags.tmp = PathBuf::from(value()),
      "--js" => flags.js = PathBuf::from(value()),
      "--noise-outside" => flags.noise_outside = true,
      "--keep-huge-pages" => flags.keep_huge_pages = true,
      "--out" => flags.out = Some(PathBuf::from(value())),
      "--help" | "-h" => {
        print!("{USAGE}");
        process::exit(0);
      }
      _ => fail(&format!("unknown flag {flag}")),
    }
  }
  flags
}

fn fail(message: &str) -> ! {
  eprintln!("{message}\n\n{USAGE}");
  process::exit(2);
}

fn main() {
  // The driver is also the noise writer of S3: `bench noise <dir> <files>`.
  let args: Vec<String> = env::args().collect();
  if args.get(1).is_some_and(|arg| arg == "noise") {
    let files = args[3].parse().expect("a number of files");
    scenarios::write_noise(Path::new(&args[2]), files);
    return;
  }

  let bench = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("driver is in bench");
  let flags = flags(bench);
  let places = Places {
    rust: own_directory(),
    js: flags.js.clone(),
    py: bench.join("py"),
    go: bench.join("go"),
    keep_huge_pages: flags.keep_huge_pages,
  };

  // Links in the path are resolved once, here: some libraries report resolved paths, and
  // on macOS the temporary directory is behind a link.
  let tmp = fs::canonicalize(&flags.tmp).unwrap_or_else(|_| fail("--tmp must exist"));
  let tmp = plain(tmp);
  let tmp = tmp.join(format!("pathwatch-bench-{}", process::id()));
  fs::create_dir_all(&tmp).expect("the temporary directory can be made");

  let scenarios: Vec<_> = SCENARIOS
    .iter()
    .filter(|scenario| flags.scenarios.as_ref().is_none_or(|names| has(names, scenario.name)))
    .collect();
  let adapters = chosen_adapters(flags.adapters.as_deref(), &places);

  let settings = Settings {
    places: &places,
    tmp: &tmp,
    scale: flags.scale,
    quiet: Duration::from_secs_f64(flags.quiet_secs),
    noise_outside: flags.noise_outside,
    daemon: Daemon::find(),
  };
  let machine = machine::machine(&tmp);
  let started = machine::now_utc();
  let [load, ..] = machine.load_average;
  if load > machine.cores as f64 / 4.0 {
    eprintln!("the machine is busy (load {load:.1}): the times of this run are not worth much");
  }

  let mut bases = BTreeMap::new();
  for adapter in ADAPTERS {
    let wanted = adapters.iter().any(|chosen| chosen.name == adapter.name);
    // Node with no watcher stands in the table of the bases next to the Node adapters.
    let node_base = adapter.name == "node-idle" && adapters.iter().any(|chosen| chosen.is_node());
    if wanted || node_base {
      // The first start of a program is slow (macOS checks a file that it has not seen, and
      // nothing is in the file cache), so each adapter starts once before it is measured.
      let _ = scenarios::base(&settings, adapter);
      if let Some(base) = scenarios::base(&settings, adapter) {
        bases.insert(adapter.name.to_owned(), base);
      }
    }
  }

  let daemon_cpu = || settings.daemon.and_then(Daemon::cpu).map(|cpu| cpu.as_secs_f64());
  let cpu_at_start = daemon_cpu();
  let mut runs = Vec::new();
  for (index, scenario) in scenarios.iter().enumerate() {
    let tree = Tree::make(&settings, scenario);
    // One round is every adapter once: what disturbs the machine for a while hits a round of
    // all adapters, not every run of one. Each round starts with another adapter, so that what
    // the first run after another scenario meets is not always met by the same library.
    for round in 1..=flags.repeat.max(1) {
      let mut order = adapters.clone();
      let first = (index + round - 1) % order.len().max(1);
      order.rotate_left(first);
      for adapter in &order {
        eprint!("{} {:<12} {round}/{} ", scenario.name, adapter.name, flags.repeat.max(1));
        let start = Instant::now();
        let run = tree.run(&settings, scenario, adapter);
        eprintln!("{} ({:.1} s)", run.status, start.elapsed().as_secs_f64());
        runs.push(run);
      }
    }
    tree.remove(&settings, scenario);
  }
  let daemon_cpu_s = cpu_at_start.zip(daemon_cpu()).map(|(start, end)| [start, end]);
  let _ = fs::remove_dir_all(&tmp);

  let report = Report {
    started: started.clone(),
    machine,
    versions: machine::versions(bench, &flags.js),
    settings: record::Settings {
      scale: flags.scale,
      quiet_secs: flags.quiet_secs,
      repeat: flags.repeat.max(1),
      tmp: flags.tmp.display().to_string(),
      noise_outside: flags.noise_outside,
      keep_huge_pages: flags.keep_huge_pages,
    },
    bases,
    runs,
    daemon_cpu_s,
  };
  let out = flags.out.unwrap_or_else(|| {
    let name = format!("{}-{}.json", env::consts::OS, started.replace(':', ""));
    bench.join("results").join(name)
  });
  if let Some(dir) = out.parent() {
    fs::create_dir_all(dir).expect("the directory of the JSON file can be made");
  }
  let json = serde_json::to_string_pretty(&report).expect("the report is plain data");
  fs::write(&out, json + "\n").expect("the JSON file can be written");
  eprintln!("wrote {}", out.display());
  eprintln!("tables: cargo run --release --bin report -- {}", out.display());
}

/// Windows: `canonicalize` gives the long form of a path, with `\\?\` in front. Node and
/// Python take that form for another path than the plain one, and no library reports it. So
/// a path on a drive loses the mark.
fn plain(path: PathBuf) -> PathBuf {
  let rest = {
    let text = path.to_string_lossy();
    let rest = text.strip_prefix(r"\\?\").filter(|rest| !rest.starts_with("UNC"));
    rest.filter(|_| cfg!(windows)).map(PathBuf::from)
  };
  rest.unwrap_or(path)
}

fn has(names: &[String], name: &str) -> bool {
  names.iter().any(|one| one == name)
}

/// The adapters that the flags ask for and that can run. An adapter whose program is missing
/// is skipped with a word about it.
fn chosen_adapters(names: Option<&[String]>, places: &Places) -> Vec<&'static Adapter> {
  let rust = ["pathwatch", "pathwatch-0", "notify-fork"];
  let wanted = |adapter: &Adapter| match names {
    None => true,
    Some(names) => has(names, adapter.name) || (has(names, "rust") && rust.contains(&adapter.name)),
  };
  let mut adapters = Vec::new();
  for adapter in ADAPTERS.iter().filter(|adapter| adapter.name != "node-idle" && wanted(adapter)) {
    if !adapter.file(places).exists() {
      eprintln!("skipped {}: {} is missing", adapter.name, adapter.file(places).display());
      eprintln!(
        "  (Rust: `cargo build --release`; Node: `npm install` in js; Go: `go build` in go)"
      );
    } else if adapter.is_node() && !places.js.join("node_modules").exists() {
      eprintln!("skipped {}: run `npm install` in {}", adapter.name, places.js.display());
    } else if adapter.is_python() && !venv_python(&places.py).exists() {
      eprintln!("skipped {}: make the environment in {}", adapter.name, places.py.display());
      eprintln!("  (`python -m venv .venv`, then `pip install -r requirements.txt` with its pip)");
    } else {
      adapters.push(adapter);
    }
  }
  adapters
}
