//! The six scenarios. Each one makes its tree, starts the adapter and runs its phases.

use std::{
  collections::BTreeMap,
  fs,
  io::{BufRead, BufReader},
  path::{Path, PathBuf},
  process::{Command, Stdio},
  thread,
  time::{Duration, Instant},
};

use crate::{
  adapter::{Adapter, Job, Places},
  record::Run,
  session::Session,
  tree::{Shape, files_below, spread, write, write_all},
};

/// How long the driver waits for the event of a single change.
const PATIENCE: Duration = Duration::from_secs(3);
/// The same when the watcher polls: a round over a big set of files takes its time.
const POLL_PATIENCE: Duration = Duration::from_secs(10);
/// The pause between making the tree and starting the adapter. FSEvents hands a new stream
/// the changes of the last moments, so a tree that was made just now would give events.
const SETTLE: Duration = Duration::from_millis(1500);

/// What the flags of the driver set for every scenario.
pub struct Settings<'a> {
  pub places: &'a Places,
  /// The directory in which the driver makes its trees.
  pub tmp: &'a Path,
  /// All sizes are multiplied by this.
  pub scale: f64,
  /// How long a quiet phase is.
  pub quiet: Duration,
  /// S3: write the noise next to the project and not inside it.
  pub noise_outside: bool,
}

impl Settings<'_> {
  /// A size of the full run, made smaller by the scale, but never below `least`.
  fn size(&self, full: usize, least: usize) -> usize {
    ((full as f64 * self.scale).round() as usize).max(least)
  }
}

/// A scenario by its short name.
pub struct Scenario {
  pub name: &'static str,
  pub title: &'static str,
  pub run: fn(&Settings, &Adapter, &Path, Run) -> Run,
}

pub const SCENARIOS: &[Scenario] = &[
  Scenario { name: "s1", title: "module files: every file is watched by its own path", run: s1 },
  Scenario { name: "s2", title: "project directory: one directory is watched", run: s2 },
  Scenario { name: "s3", title: "noise: module files while another process writes", run: s3 },
  Scenario { name: "s4", title: "ignored directory: writes into node_modules", run: s4 },
  Scenario { name: "s5", title: "polling: module files, polled every 100 ms", run: s5 },
  Scenario { name: "s6", title: "missing paths: watched before they exist", run: s6 },
];

/// Runs one scenario with one adapter, in a fresh directory that is removed at the end.
#[must_use]
pub fn run(settings: &Settings, scenario: &Scenario, adapter: &Adapter) -> Run {
  let root = settings.tmp.join(format!("{}-{}", scenario.name, adapter.name));
  fs::create_dir_all(&root).expect("the driver can write in its temporary directory");
  let run = Run {
    scenario: scenario.name.to_owned(),
    adapter: adapter.name.to_owned(),
    root: root.display().to_string(),
    ..Run::default()
  };
  let run = (scenario.run)(settings, adapter, &root, run);
  let _ = fs::remove_dir_all(&root);
  run
}

/// The numbers of an adapter that watches nothing.
#[must_use]
pub fn base(settings: &Settings, adapter: &Adapter) -> Option<crate::record::Base> {
  let session =
    Session::start(adapter, settings.places, &Job::default(), settings.tmp, Run::default());
  session.ok().map(Session::into_base)
}

fn sizes(run: &mut Run, entries: &[(&str, usize)]) {
  run.sizes = entries.iter().map(|(name, size)| ((*name).to_owned(), *size)).collect();
}

/// The tree of S1, S3 and S5: 20,000 files in 2,000 directories.
fn module_files(settings: &Settings) -> Shape {
  Shape { groups: settings.size(40, 2), dirs: 50, files: 10 }
}

/// S1 "module files": 20,000 files in 2,000 directories, every file watched by its own path.
fn s1(settings: &Settings, adapter: &Adapter, root: &Path, mut run: Run) -> Run {
  let shape = module_files(settings);
  let project = root.join("project");
  let files = shape.make(&project).expect("the tree can be made");
  sizes(&mut run, &[("files", files.len()), ("dirs", shape.dir_count()), ("watched", files.len())]);
  thread::sleep(SETTLE);

  let job = Job { paths: files.clone(), ..Job::default() };
  let mut session = match Session::start(adapter, settings.places, &job, root, run) {
    Ok(session) => session,
    Err(run) => return *run,
  };
  session.single_edits("first edit, right after ready", &files[..1], PATIENCE);
  session.quiet("quiet", settings.quiet);
  session.single_edits("single edits", &spread(&files, settings.size(200, 10)), PATIENCE);
  let saved = spread(&files[3..], settings.size(50, 5));
  session.single_changes("single atomic saves", &saved, PATIENCE, atomic_save);
  // The file at the path is another one now. Is the path still watched?
  session.single_edits("single edits of the saved files", &saved, PATIENCE);

  let burst = spread(&files[1..], settings.size(2000, 50));
  session.burst("burst of edits", &burst, || {
    for file in &burst {
      write(file).expect("the driver can write its own files");
    }
  });

  let removed = spread(&files[2..], settings.size(200, 10));
  session.burst("files removed", &removed, || {
    for file in &removed {
      fs::remove_file(file).expect("the driver can remove its own files");
    }
  });
  session.burst("files written again", &removed, || {
    write_all(&removed).expect("the driver can write its own files");
  });
  session.end()
}

/// Saves a file the way many editors do: the new content goes to a temporary file in the
/// same directory, which is then renamed over the old file.
fn atomic_save(path: &Path) -> std::io::Result<()> {
  let temporary = path.with_extension("js.tmp");
  write(&temporary)?;
  fs::rename(&temporary, path)
}

/// S2 "project directory": one watched directory with 50,000 files in 5,000 directories.
fn s2(settings: &Settings, adapter: &Adapter, root: &Path, mut run: Run) -> Run {
  let shape = Shape { groups: 10, dirs: settings.size(500, 5), files: 10 };
  let project = root.join("project");
  let files = shape.make(&project).expect("the tree can be made");
  // The tree that is moved in later. It waits outside of the watched directory.
  let moved = Shape { groups: 1, dirs: shape.dirs, files: 10 };
  let outside = root.join("outside");
  moved.make(&outside).expect("the tree can be made");
  sizes(&mut run, &[("files", files.len()), ("dirs", shape.dir_count()), ("watched", 1)]);
  thread::sleep(SETTLE);

  let job = Job { paths: vec![project.clone()], ..Job::default() };
  let mut session = match Session::start(adapter, settings.places, &job, root, run) {
    Ok(session) => session,
    Err(run) => return *run,
  };
  session.single_edits("first edit, right after ready", &files[..1], PATIENCE);
  session.quiet("quiet", settings.quiet);
  session.single_edits("single edits", &spread(&files, settings.size(200, 10)), PATIENCE);

  // New files in new directories, spread over the groups: `g03/new017/m4.js`.
  let new_dirs = settings.size(500, 5);
  let new_files: Vec<PathBuf> = (0..new_dirs)
    .flat_map(|dir| {
      let dir = Shape::group(&project, dir % shape.groups).join(format!("new{dir:03}"));
      (0..10).map(move |file| dir.join(format!("m{file}.js")))
    })
    .collect();
  session.burst("new files in new directories", &new_files, || {
    write_all(&new_files).expect("the driver can write its own files");
  });

  // One group goes away, with everything that is in it by now.
  let group = Shape::group(&project, 1);
  let removed = files_below(&group);
  session.burst("a directory tree removed", &removed, || {
    fs::remove_dir_all(&group).expect("the driver can remove its own files");
  });

  // A tree comes in by one rename, and goes out by one rename.
  let inside = project.join("moved");
  let moved_files = moved.paths(&inside);
  session.burst("a directory tree moved in", &moved_files, || {
    fs::rename(&outside, &inside).expect("the driver can rename its own directory");
  });
  session.burst("a directory tree moved out", &moved_files, || {
    fs::rename(&inside, &outside).expect("the driver can rename its own directory");
  });
  session.end()
}

/// S3 "noise": S1, while another process writes 50,000 files in a directory nobody watches.
fn s3(settings: &Settings, adapter: &Adapter, root: &Path, mut run: Run) -> Run {
  let shape = module_files(settings);
  let project = root.join("project");
  let files = shape.make(&project).expect("the tree can be made");
  // Like the output directory of a build: inside the project, and not one of the watched
  // paths. A library that can only watch the directory above all files sees it.
  let noise = if settings.noise_outside { root.join("noise") } else { project.join("dist") };
  fs::create_dir_all(&noise).expect("the directory can be made");
  let noise_files = settings.size(50_000, 1000);
  sizes(&mut run, &[("files", files.len()), ("dirs", shape.dir_count()), ("watched", files.len())]);
  thread::sleep(SETTLE);

  let job = Job { paths: files.clone(), ..Job::default() };
  let mut session = match Session::start(adapter, settings.places, &job, root, run) {
    Ok(session) => session,
    Err(run) => return *run,
  };
  session.single_edits("first edit, right after ready", &files[..1], PATIENCE);

  let writer = NoiseWriter::start(&noise, noise_files);
  session.single_edits(
    "single edits under noise",
    &spread(&files, settings.size(100, 10)),
    PATIENCE,
  );
  let (writes, seconds) = writer.stop();
  session.info(BTreeMap::from([
    ("noise files".to_owned(), noise_files as f64),
    ("noise writes".to_owned(), writes),
    ("noise writes per second".to_owned(), (writes / seconds).round()),
  ]));
  // What the noise left behind in the watcher must be gone before the adapter is closed.
  session.quiet("quiet after the noise", Duration::from_secs(1));
  session.end()
}

/// Another process that writes files as fast as it can, until it is stopped.
struct NoiseWriter {
  process: std::process::Child,
  started: Instant,
}

impl NoiseWriter {
  fn start(dir: &Path, files: usize) -> Self {
    // The driver itself has the part of the noise writer: see `write_noise`.
    let process = Command::new(std::env::current_exe().expect("the driver has a path"))
      .arg("noise")
      .arg(dir)
      .arg(files.to_string())
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .spawn()
      .expect("the noise writer starts");
    let started = Instant::now();
    // The first files exist when the single edits begin.
    thread::sleep(Duration::from_millis(200));
    Self { process, started }
  }

  /// Stops the writer. Returns how many files it wrote, and in how many seconds.
  fn stop(mut self) -> (f64, f64) {
    // Closing its stdin is the sign to stop.
    drop(self.process.stdin.take());
    let seconds = self.started.elapsed().as_secs_f64();
    let mut line = String::new();
    let stdout = self.process.stdout.take().expect("stdout is a pipe");
    let _ = BufReader::new(stdout).read_line(&mut line);
    let _ = self.process.wait();
    (line.trim().parse().unwrap_or(0.0), seconds)
  }
}

/// The noise writer: `bench noise <dir> <files>`. It writes `files` files, 100 in a directory,
/// and then writes them again and again, until its stdin is closed. Then it prints how many
/// writes it made.
pub fn write_noise(dir: &Path, files: usize) {
  use std::sync::atomic::{AtomicBool, Ordering};
  static STOP: AtomicBool = AtomicBool::new(false);
  thread::spawn(|| {
    for line in std::io::stdin().lock().lines() {
      if line.is_err() {
        break;
      }
    }
    STOP.store(true, Ordering::Relaxed);
  });
  let mut writes = 0_u64;
  'all: loop {
    for file in 0..files {
      if STOP.load(Ordering::Relaxed) {
        break 'all;
      }
      let dir = dir.join(format!("n{:04}", file / 100));
      if file % 100 == 0 {
        let _ = fs::create_dir_all(&dir);
      }
      let _ = write(&dir.join(format!("f{:02}.txt", file % 100)));
      writes += 1;
    }
  }
  println!("{writes}");
}

/// S4 "ignored directory": a watched project with a rule that ignores `node_modules`, and
/// 30,000 files that are written into `node_modules`.
fn s4(settings: &Settings, adapter: &Adapter, root: &Path, mut run: Run) -> Run {
  let shape = Shape { groups: settings.size(10, 1), dirs: 50, files: 10 };
  let project = root.join("project");
  let files = shape.make(&project.join("src")).expect("the tree can be made");
  let node_modules = project.join("node_modules");
  // One package is there from the start. 300 more follow, each 10 directories of 10 files.
  let package = Shape { groups: 1, dirs: 10, files: 10 };
  package.make(&node_modules.join("first")).expect("the tree can be made");
  let packages = settings.size(300, 3);
  let written: Vec<PathBuf> = (0..packages)
    .flat_map(|index| package.paths(&node_modules.join(format!("package{index:03}"))))
    .collect();
  sizes(
    &mut run,
    &[
      ("files", files.len()),
      ("dirs", shape.dir_count()),
      ("watched", 1),
      ("files written into node_modules", written.len()),
    ],
  );
  thread::sleep(SETTLE);

  let job = Job { paths: vec![project.clone()], ignore: Some("node_modules"), ..Job::default() };
  let mut session = match Session::start(adapter, settings.places, &job, root, run) {
    Ok(session) => session,
    Err(run) => return *run,
  };
  session.single_edits("first edit, right after ready", &files[..1], PATIENCE);

  // The one changed path is the file in `src`, and its time counts from its own write.
  // Every event for `node_modules` is one too many.
  let edited = files[files.len() / 2].clone();
  session.burst_from(
    "node_modules written, then one edit in src",
    std::slice::from_ref(&edited),
    || {
      write_all(&written).expect("the driver can write its own files");
      let edit = Instant::now();
      write(&edited).expect("the driver can write its own files");
      edit
    },
  );
  session.single_edits("single edits in src", &spread(&files, settings.size(20, 5)), PATIENCE);
  session.end()
}

/// S5 "polling": the tree of S1 with the polling mode of each library, at 100 ms.
fn s5(settings: &Settings, adapter: &Adapter, root: &Path, mut run: Run) -> Run {
  let shape = module_files(settings);
  let project = root.join("project");
  let files = shape.make(&project).expect("the tree can be made");
  sizes(&mut run, &[("files", files.len()), ("dirs", shape.dir_count()), ("watched", files.len())]);
  thread::sleep(SETTLE);

  let job = Job { paths: files.clone(), poll_ms: Some(100), ..Job::default() };
  let mut session = match Session::start(adapter, settings.places, &job, root, run) {
    Ok(session) => session,
    Err(run) => return *run,
  };
  session.single_edits("first edit, right after ready", &files[..1], POLL_PATIENCE);
  session.quiet("quiet", settings.quiet);
  session.single_edits("single edits", &spread(&files, settings.size(50, 5)), POLL_PATIENCE);
  session.end()
}

/// S6 "missing paths": 1,000 watched paths that do not exist, in directories that do not
/// exist. Then they are made.
fn s6(settings: &Settings, adapter: &Adapter, root: &Path, mut run: Run) -> Run {
  let shape = Shape { groups: settings.size(10, 2), dirs: 10, files: 10 };
  let project = root.join("project");
  fs::create_dir_all(&project).expect("the directory can be made");
  let files = shape.paths(&project);
  sizes(&mut run, &[("files", 0), ("dirs", 0), ("watched", files.len())]);
  thread::sleep(SETTLE);

  let job = Job { paths: files.clone(), ..Job::default() };
  let mut session = match Session::start(adapter, settings.places, &job, root, run) {
    Ok(session) => session,
    Err(run) => return *run,
  };
  session.quiet("quiet", settings.quiet.min(Duration::from_secs(3)));
  session.burst("the missing paths are made", &files, || {
    write_all(&files).expect("the driver can write its own files");
  });
  session.single_edits("single edits after", &spread(&files, settings.size(20, 5)), PATIENCE);
  session.end()
}
