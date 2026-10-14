//! The seven scenarios. Each one makes its tree once; every adapter runs the phases on it in
//! turn, and after each run the tree is put back as it was made. A tree that is made and
//! removed for each run is work for the OS that the next run would meet (`os`).

use std::{
  collections::{BTreeMap, HashSet},
  fs,
  io::{self, BufRead, BufReader},
  path::{Path, PathBuf},
  process::{Command, Stdio},
  thread,
  time::{Duration, Instant},
};

use crate::{
  adapter::{Adapter, Job, Places},
  os::Daemon,
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
/// S5: a single change waits this long at most, a different time for each change and the same
/// for every adapter, so that it lands at a random place of a round of polling.
const POLL_PAUSE: Duration = Duration::from_millis(300);
/// S3: the noise runs alone this long before the edits. The start of a flood keeps the OS busy
/// for a while, for every library alike; the edits come when it writes at its usual pace.
const NOISE_LEAD: Duration = Duration::from_secs(5);
/// S4: how long the edit after the flood is waited for. It is one path, and its time is the
/// point of the phase: the OS may take seconds to digest 30,000 new files.
const FLOOD_PATIENCE: Duration = Duration::from_secs(30);

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
  /// The daemon whose backlog the driver waits for before a run and a phase (`os`).
  pub daemon: Option<Daemon>,
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
  /// Makes the tree in the directory of the scenario.
  pub make: fn(&Settings, &Path),
  /// Runs the phases with one adapter on the tree.
  pub run: fn(&Settings, &Adapter, &Path, Run) -> Run,
  /// The tree has a directory of tens of thousands of entries. fseventsd takes minutes to
  /// digest its removal (measured: 12 s for one, more than 2 minutes after a few), and the next
  /// scenario would start in that backlog, so the tree is removed with the temporary directory
  /// at the end of the driver. Every other tree is removed after its scenario.
  pub big_directory: bool,
}

pub const SCENARIOS: &[Scenario] = &[
  Scenario {
    name: "s1",
    title: "module files: every file is watched by its own path",
    make: make_s1,
    run: s1,
    big_directory: false,
  },
  Scenario {
    name: "s2",
    title: "project directory: one directory is watched",
    make: make_s2,
    run: s2,
    big_directory: false,
  },
  Scenario {
    name: "s3",
    title: "noise: module files while another process writes",
    make: make_s3,
    run: s3,
    big_directory: false,
  },
  Scenario {
    name: "s4",
    title: "ignored directory: writes into node_modules",
    make: make_s4,
    run: s4,
    big_directory: false,
  },
  Scenario {
    name: "s5",
    title: "polling: module files, polled every 100 ms",
    make: make_s5,
    run: s5,
    big_directory: false,
  },
  Scenario {
    name: "s6",
    title: "missing paths: watched before they exist",
    make: make_s6,
    run: s6,
    big_directory: false,
  },
  Scenario {
    name: "s7",
    title: "big directory: one watched directory holds every file",
    make: make_s7,
    run: s7,
    big_directory: true,
  },
];

/// The tree of a scenario as `make` left it: every run starts with it.
pub struct Tree {
  root: PathBuf,
  files: HashSet<PathBuf>,
  dirs: HashSet<PathBuf>,
}

impl Tree {
  /// Makes the tree of the scenario, in a directory of its own.
  #[must_use]
  pub fn make(settings: &Settings, scenario: &Scenario) -> Self {
    let root = settings.tmp.join(scenario.name);
    fs::create_dir_all(&root).expect("the driver can write in its temporary directory");
    (scenario.make)(settings, &root);
    let mut tree = Self { root, files: HashSet::new(), dirs: HashSet::new() };
    let mut dirs = vec![tree.root.clone()];
    while let Some(dir) = dirs.pop() {
      for entry in fs::read_dir(&dir).expect("the tree can be read").flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
          tree.dirs.insert(entry.path());
          dirs.push(entry.path());
        } else {
          tree.files.insert(entry.path());
        }
      }
    }
    tree
  }

  /// Runs the scenario with one adapter, and puts the tree back.
  #[must_use]
  pub fn run(&self, settings: &Settings, scenario: &Scenario, adapter: &Adapter) -> Run {
    let run = Run {
      scenario: scenario.name.to_owned(),
      adapter: adapter.name.to_owned(),
      root: self.root.display().to_string(),
      ..Run::default()
    };
    let run = (scenario.run)(settings, adapter, &self.root, run);
    self.restore().expect("the tree can be put back");
    run
  }

  /// What a run added goes, and what it removed comes back.
  fn restore(&self) -> io::Result<()> {
    let mut dirs = vec![self.root.clone()];
    while let Some(dir) = dirs.pop() {
      for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        let is_dir = entry.file_type()?.is_dir();
        if is_dir && self.dirs.contains(&path) {
          dirs.push(path);
        } else if is_dir {
          fs::remove_dir_all(&path)?;
        } else if !self.files.contains(&path) {
          fs::remove_file(&path)?;
        }
      }
    }
    let mut missing: Vec<PathBuf> =
      self.files.iter().filter(|file| fs::symlink_metadata(file).is_err()).cloned().collect();
    missing.sort();
    write_all(&missing)?;
    self.dirs.iter().try_for_each(fs::create_dir_all)
  }

  /// The end of the scenario: the tree is removed, or put aside for the end of the driver
  /// (`Scenario::big_directory`).
  pub fn remove(self, settings: &Settings, scenario: &Scenario) {
    if scenario.big_directory {
      let aside = settings.tmp.join("removed-at-the-end");
      let _ = fs::create_dir_all(&aside);
      let _ = fs::rename(&self.root, aside.join(scenario.name));
    }
    let _ = fs::remove_dir_all(&self.root);
  }
}

/// The numbers of an adapter that watches nothing.
#[must_use]
pub fn base(settings: &Settings, adapter: &Adapter) -> Option<crate::record::Base> {
  let session = Session::start(
    adapter,
    settings.places,
    settings.daemon,
    &Job::default(),
    settings.tmp,
    Run::default(),
  );
  session.ok().map(Session::into_base)
}

/// The pause between making the tree and starting the adapter: `SETTLE`, and then until the
/// OS caught up with the tree and with the run before (`os`).
fn settle(settings: &Settings, run: &mut Run) {
  thread::sleep(SETTLE);
  if let Some(daemon) = settings.daemon {
    let waited = daemon.wait_idle(&mut thread::sleep);
    run.waited(&waited, "the start");
  }
}

fn sizes(run: &mut Run, entries: &[(&str, usize)]) {
  run.sizes = entries.iter().map(|(name, size)| ((*name).to_owned(), *size)).collect();
}

/// The tree of S1, S3 and S5: 20,000 files in 2,000 directories.
fn module_files(settings: &Settings) -> Shape {
  Shape { groups: settings.size(40, 2), dirs: 50, files: 10 }
}

fn make_s1(settings: &Settings, root: &Path) {
  module_files(settings).make(&root.join("project")).expect("the tree can be made");
}

/// S1 "module files": 20,000 files in 2,000 directories, every file watched by its own path.
fn s1(settings: &Settings, adapter: &Adapter, root: &Path, mut run: Run) -> Run {
  let shape = module_files(settings);
  let files = shape.paths(&root.join("project"));
  sizes(&mut run, &[("files", files.len()), ("dirs", shape.dir_count()), ("watched", files.len())]);
  settle(settings, &mut run);

  let job = Job { paths: files.clone(), ..Job::default() };
  let mut session = match Session::start(adapter, settings.places, settings.daemon, &job, root, run)
  {
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
  session.burst("burst of edits", &burst, || burst.iter().try_for_each(|file| write(file)));

  let removed = spread(&files[2..], settings.size(200, 10));
  session.burst("files removed", &removed, || removed.iter().try_for_each(fs::remove_file));
  session.burst("files written again", &removed, || write_all(&removed));
  session.end()
}

/// Saves a file the way many editors do: the new content goes to a temporary file in the
/// same directory, which is then renamed over the old file.
fn atomic_save(path: &Path) -> std::io::Result<()> {
  let temporary = path.with_extension("js.tmp");
  write(&temporary)?;
  fs::rename(&temporary, path)
}

/// The project of S2, and the tree that is moved in later. It waits outside of the watched
/// directory.
fn s2_shapes(settings: &Settings) -> (Shape, Shape) {
  let shape = Shape { groups: 10, dirs: settings.size(500, 5), files: 10 };
  (shape, Shape { groups: 1, dirs: shape.dirs, files: 10 })
}

fn make_s2(settings: &Settings, root: &Path) {
  let (shape, moved) = s2_shapes(settings);
  shape.make(&root.join("project")).expect("the tree can be made");
  moved.make(&root.join("outside")).expect("the tree can be made");
}

/// S2 "project directory": one watched directory with 50,000 files in 5,000 directories.
fn s2(settings: &Settings, adapter: &Adapter, root: &Path, mut run: Run) -> Run {
  let (shape, moved) = s2_shapes(settings);
  let project = root.join("project");
  let files = shape.paths(&project);
  let outside = root.join("outside");
  sizes(&mut run, &[("files", files.len()), ("dirs", shape.dir_count()), ("watched", 1)]);
  settle(settings, &mut run);

  let job = Job { paths: vec![project.clone()], ..Job::default() };
  let mut session = match Session::start(adapter, settings.places, settings.daemon, &job, root, run)
  {
    Ok(session) => session,
    Err(run) => return *run,
  };
  session.single_edits("first edit, right after ready", &files[..1], PATIENCE);
  session.quiet("quiet", settings.quiet);
  session.single_edits("single edits", &spread(&files, settings.size(200, 10)), PATIENCE);

  // New files in new directories, spread over the groups: `group-003/new-directory-017/module-04.js`.
  let new_dirs = settings.size(500, 5);
  let new_files: Vec<PathBuf> = (0..new_dirs)
    .flat_map(|dir| {
      let dir = Shape::group(&project, dir % shape.groups).join(format!("new-directory-{dir:03}"));
      (0..10).map(move |file| dir.join(format!("module-{file:02}.js")))
    })
    .collect();
  session.burst("new files in new directories", &new_files, || write_all(&new_files));

  // One group goes away, with everything that is in it by now.
  let group = Shape::group(&project, 1);
  let removed = files_below(&group);
  session.burst("a directory tree removed", &removed, || fs::remove_dir_all(&group));

  // A tree comes in by one rename, and goes out by one rename.
  let inside = project.join("moved");
  let moved_files = moved.paths(&inside);
  session.burst("a directory tree moved in", &moved_files, || fs::rename(&outside, &inside));
  session.burst("a directory tree moved out", &moved_files, || fs::rename(&inside, &outside));
  session.end()
}

/// The directory of the noise of S3. Like the output directory of a build: inside the project,
/// and not one of the watched paths. A library that can only watch the directory above all
/// files sees it.
fn noise_dir(settings: &Settings, root: &Path) -> PathBuf {
  if settings.noise_outside { root.join("noise") } else { root.join("project").join("dist") }
}

/// The noise files are there from the start, as the output of the build before: the writer
/// writes them again, at the same pace for every adapter.
fn make_s3(settings: &Settings, root: &Path) {
  make_s1(settings, root);
  write_all(&noise_paths(&noise_dir(settings, root), settings.size(50_000, 1000)))
    .expect("the tree can be made");
}

/// S3 "noise": S1, while another process writes 50,000 files in a directory nobody watches.
fn s3(settings: &Settings, adapter: &Adapter, root: &Path, mut run: Run) -> Run {
  let shape = module_files(settings);
  let files = shape.paths(&root.join("project"));
  let noise = noise_dir(settings, root);
  let noise_files = settings.size(50_000, 1000);
  sizes(&mut run, &[("files", files.len()), ("dirs", shape.dir_count()), ("watched", files.len())]);
  settle(settings, &mut run);

  let job = Job { paths: files.clone(), ..Job::default() };
  let mut session = match Session::start(adapter, settings.places, settings.daemon, &job, root, run)
  {
    Ok(session) => session,
    Err(run) => return *run,
  };
  session.single_edits("first edit, right after ready", &files[..1], PATIENCE);

  // The noise is the point of the next phases: the wait for an idle OS comes before it starts,
  // and not before the phases that it keeps busy.
  session.settle("single edits under noise");
  session.settle_before_phases = false;
  let writer = NoiseWriter::start(&noise, noise_files);
  session.listen("noise alone", NOISE_LEAD);
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
    Self { process, started: Instant::now() }
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

/// `files` files, 100 in a directory.
fn noise_paths(dir: &Path, files: usize) -> Vec<PathBuf> {
  (0..files)
    .map(|file| {
      dir.join(format!("noise-{:04}", file / 100)).join(format!("noise-file-{:02}.txt", file % 100))
    })
    .collect()
}

/// The noise writer: `bench noise <dir> <files>`. It writes the `files` files of `noise_paths`
/// again and again, until its stdin is closed. Then it prints how many writes it made.
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
  let paths = noise_paths(dir, files);
  let mut writes = 0_u64;
  'all: loop {
    for path in &paths {
      if STOP.load(Ordering::Relaxed) {
        break 'all;
      }
      let _ = write(path);
      writes += 1;
    }
  }
  println!("{writes}");
}

/// The sources of S4, and a package in `node_modules`: 10 directories of 10 files.
fn s4_shapes(settings: &Settings) -> (Shape, Shape) {
  (
    Shape { groups: settings.size(10, 1), dirs: 50, files: 10 },
    Shape { groups: 1, dirs: 10, files: 10 },
  )
}

/// One package is there from the start.
fn make_s4(settings: &Settings, root: &Path) {
  let (shape, package) = s4_shapes(settings);
  let project = root.join("project");
  shape.make(&project.join("src")).expect("the tree can be made");
  package.make(&project.join("node_modules").join("first")).expect("the tree can be made");
}

/// S4 "ignored directory": a watched project with a rule that ignores `node_modules`, and
/// 30,000 files that are written into `node_modules`.
fn s4(settings: &Settings, adapter: &Adapter, root: &Path, mut run: Run) -> Run {
  let (shape, package) = s4_shapes(settings);
  let project = root.join("project");
  let files = shape.paths(&project.join("src"));
  let node_modules = project.join("node_modules");
  // 300 packages follow the first one.
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
  settle(settings, &mut run);

  let job = Job { paths: vec![project.clone()], ignore: Some("node_modules"), ..Job::default() };
  let mut session = match Session::start(adapter, settings.places, settings.daemon, &job, root, run)
  {
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
    FLOOD_PATIENCE,
    || {
      write_all(&written)?;
      let edit = Instant::now();
      write(&edited)?;
      Ok(edit)
    },
  );
  session.single_edits("single edits in src", &spread(&files, settings.size(20, 5)), PATIENCE);
  session.end()
}

fn make_s5(settings: &Settings, root: &Path) {
  make_s1(settings, root);
}

/// S5 "polling": the tree of S1 with the polling mode of each library, at 100 ms.
fn s5(settings: &Settings, adapter: &Adapter, root: &Path, mut run: Run) -> Run {
  let shape = module_files(settings);
  let files = shape.paths(&root.join("project"));
  sizes(&mut run, &[("files", files.len()), ("dirs", shape.dir_count()), ("watched", files.len())]);
  settle(settings, &mut run);

  let job = Job { paths: files.clone(), poll_ms: Some(100), ..Job::default() };
  let mut session = match Session::start(adapter, settings.places, settings.daemon, &job, root, run)
  {
    Ok(session) => session,
    Err(run) => return *run,
  };
  session.single_edits("first edit, right after ready", &files[..1], POLL_PATIENCE);
  session.quiet("quiet", settings.quiet);
  session.pause_before_a_change = POLL_PAUSE;
  session.single_edits(
    "single edits",
    &spread(&files, settings.size(50, 5)),
    POLL_PAUSE + POLL_PATIENCE,
  );
  session.end()
}

fn make_s6(_settings: &Settings, root: &Path) {
  fs::create_dir_all(root.join("project")).expect("the directory can be made");
}

/// S6 "missing paths": 1,000 watched paths that do not exist, in directories that do not
/// exist. Then they are made.
fn s6(settings: &Settings, adapter: &Adapter, root: &Path, mut run: Run) -> Run {
  let shape = Shape { groups: settings.size(10, 2), dirs: 10, files: 10 };
  let files = shape.paths(&root.join("project"));
  sizes(&mut run, &[("files", 0), ("dirs", 0), ("watched", files.len())]);
  settle(settings, &mut run);

  let job = Job { paths: files.clone(), ..Job::default() };
  let mut session = match Session::start(adapter, settings.places, settings.daemon, &job, root, run)
  {
    Ok(session) => session,
    Err(run) => return *run,
  };
  session.quiet("quiet", settings.quiet.min(Duration::from_secs(3)));
  session.burst("the missing paths are made", &files, || write_all(&files));
  session.single_edits("single edits after", &spread(&files, settings.size(20, 5)), PATIENCE);
  session.end()
}

/// The files of S7 with this name in front, all in the one directory.
fn s7_named(root: &Path, name: &str, count: usize) -> Vec<PathBuf> {
  (0..count).map(|file| root.join("project").join(format!("{name}{file:05}.js"))).collect()
}

fn make_s7(settings: &Settings, root: &Path) {
  write_all(&s7_named(root, "module-", settings.size(20_000, 500))).expect("the tree can be made");
}

/// S7 "big directory": one watched directory that holds 20,000 files itself, like the output
/// of a build or a big package. A watcher that lists the directory for a change pays for
/// every entry in it.
fn s7(settings: &Settings, adapter: &Adapter, root: &Path, mut run: Run) -> Run {
  let project = root.join("project");
  let named = |name: &str, count: usize| s7_named(root, name, count);
  let files = named("module-", settings.size(20_000, 500));
  sizes(&mut run, &[("files", files.len()), ("dirs", 1), ("watched", 1)]);
  settle(settings, &mut run);

  let job = Job { paths: vec![project.clone()], ..Job::default() };
  let mut session = match Session::start(adapter, settings.places, settings.daemon, &job, root, run)
  {
    Ok(session) => session,
    Err(run) => return *run,
  };
  session.single_edits("first edit, right after ready", &files[..1], PATIENCE);
  session.single_edits("single edits", &spread(&files, settings.size(100, 10)), PATIENCE);
  session.single_edits("single new files", &named("new-file-", settings.size(100, 10)), PATIENCE);
  let burst = named("burst-file-", settings.size(2000, 50));
  session.burst("burst of new files", &burst, || write_all(&burst));
  let removed = spread(&files[1..], settings.size(100, 10));
  session.single_changes("single files removed", &removed, PATIENCE, |file| fs::remove_file(file));
  session.end()
}
