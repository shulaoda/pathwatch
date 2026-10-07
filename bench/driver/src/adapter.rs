//! The adapters, and one of them running as a child process.

use std::{
  io::{self, BufRead, BufReader, BufWriter, Read, Write},
  path::{Path, PathBuf},
  process::{ChildStdin, Command, Stdio},
  sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
  thread,
  time::{Duration, Instant},
};

/// One library behind the line protocol of `bench/adapters/src/lib.rs`.
pub struct Adapter {
  pub name: &'static str,
  program: Program,
}

enum Program {
  /// A program of the `adapters` crate, with its arguments.
  Rust(&'static str, &'static [&'static str]),
  /// A script of `bench/js`, run by `node`.
  Node(&'static str),
}

/// Every adapter, in the order of the tables.
pub const ADAPTERS: &[Adapter] = &[
  Adapter { name: "pathwatch", program: Program::Rust("adapter-pathwatch", &[]) },
  Adapter {
    name: "pathwatch-0",
    program: Program::Rust("adapter-pathwatch", &["--debounce-ms", "0"]),
  },
  Adapter { name: "notify", program: Program::Rust("adapter-notify", &[]) },
  Adapter { name: "notify-fork", program: Program::Rust("adapter-notify-fork", &[]) },
  Adapter { name: "chokidar", program: Program::Node("chokidar.mjs") },
  Adapter { name: "parcel", program: Program::Node("parcel.mjs") },
  Adapter { name: "fs-watch", program: Program::Node("fs-watch.mjs") },
  // Node with no watcher at all. It is only run for the table of bases.
  Adapter { name: "node-idle", program: Program::Node("idle.mjs") },
];

/// Where the programs of the adapters are.
pub struct Places {
  /// The directory of the Rust adapters: the one that holds the driver itself.
  pub rust: PathBuf,
  /// `bench/js`, or a copy of it with its own `node_modules`.
  pub js: PathBuf,
  /// Linux: leave the memory allocator of the system as it is. See `Adapter::command`.
  pub keep_huge_pages: bool,
}

impl Adapter {
  #[must_use]
  pub fn is_node(&self) -> bool {
    matches!(self.program, Program::Node(_))
  }

  /// The file that must exist for the adapter to run.
  #[must_use]
  pub fn file(&self, places: &Places) -> PathBuf {
    match self.program {
      Program::Rust(name, _) => places.rust.join(name),
      Program::Node(script) => places.js.join(script),
    }
  }

  fn command(&self, places: &Places) -> Command {
    let mut command = self.program(places);
    // With glibc 2.43 on Ubuntu 26.04, every thread that allocates gets one huge page of
    // 2 MiB, whatever it stores: an adapter with four threads that watches one file has a
    // resident size of 8.3 MiB, and of 2.3 MiB with this setting. So the memory of the data
    // structures can only be seen without the huge pages.
    if cfg!(target_os = "linux") && !places.keep_huge_pages {
      command.env("GLIBC_TUNABLES", "glibc.malloc.hugetlb=0");
    }
    command
  }

  fn program(&self, places: &Places) -> Command {
    match self.program {
      Program::Rust(_, args) => {
        let mut command = Command::new(self.file(places));
        command.args(args);
        command
      }
      Program::Node(_) => {
        let mut command = Command::new("node");
        command.arg(self.file(places));
        command
      }
    }
  }
}

/// What the driver asks an adapter to do.
#[derive(Default)]
pub struct Job {
  pub paths: Vec<PathBuf>,
  /// Ignore every path that has a part with this name.
  pub ignore: Option<&'static str>,
  /// Poll, with this pause in milliseconds.
  pub poll_ms: Option<u64>,
}

/// One line of an adapter, with the time at which the driver read it.
pub enum Line {
  Ready(Instant),
  Event {
    at: Instant,
    kind: String,
    path: PathBuf,
  },
  Note(String),
  Error(String),
  Unsupported(String),
  /// The adapter closed its output: it is gone.
  End,
}

/// An adapter that runs.
pub struct Child {
  pub pid: i32,
  /// Just before the process was started.
  pub spawned: Instant,
  lines: Receiver<Line>,
  /// Closing it tells the adapter to exit.
  stdin: Option<ChildStdin>,
}

impl Child {
  /// Starts the adapter and hands it the job.
  pub fn start(adapter: &Adapter, places: &Places, job: &Job) -> io::Result<Self> {
    let mut command = adapter.command(places);
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let spawned = Instant::now();
    let mut child = command.spawn()?;
    let (sender, lines) = mpsc::channel();

    let stdout = child.stdout.take().expect("stdout is a pipe");
    let to_driver = sender.clone();
    thread::spawn(move || read_lines(stdout, &to_driver));
    // What a program writes to stderr is an error of it: a panic, a warning of Node.
    let stderr = child.stderr.take().expect("stderr is a pipe");
    thread::spawn(move || {
      for line in BufReader::new(stderr).lines().map_while(Result::ok) {
        let _ = sender.send(Line::Error(format!("stderr: {line}")));
      }
    });

    let stdin = child.stdin.take().expect("stdin is a pipe");
    let mut writer = BufWriter::new(stdin);
    if let Some(name) = job.ignore {
      writeln!(writer, "ignore\t{name}")?;
    }
    if let Some(pause) = job.poll_ms {
      writeln!(writer, "poll\t{pause}")?;
    }
    for path in &job.paths {
      writeln!(writer, "watch\t{}", path.display())?;
    }
    writeln!(writer, "start")?;
    let stdin = writer.into_inner().map_err(io::IntoInnerError::into_error)?;

    let pid = i32::try_from(child.id()).expect("a process id fits");
    Ok(Self { pid, spawned, lines, stdin: Some(stdin) })
  }

  /// The next line, or `None` when there is none before `until`.
  pub fn next(&self, until: Instant) -> Option<Line> {
    match self.lines.recv_timeout(until.saturating_duration_since(Instant::now())) {
      Ok(line) => Some(line),
      Err(RecvTimeoutError::Timeout) => None,
      Err(RecvTimeoutError::Disconnected) => Some(Line::End),
    }
  }

  /// Tells the adapter to exit.
  pub fn close(&mut self) {
    self.stdin = None;
  }
}

/// Reads the output of an adapter, line by line, and notes the time of each line.
fn read_lines(stdout: impl Read, lines: &Sender<Line>) {
  for line in BufReader::with_capacity(256 * 1024, stdout).lines().map_while(Result::ok) {
    let at = Instant::now();
    let (word, rest) = line.split_once('\t').unwrap_or((line.as_str(), ""));
    let line = match word {
      "event" => {
        let (kind, path) = rest.split_once('\t').unwrap_or((rest, ""));
        Line::Event { at, kind: kind.to_owned(), path: PathBuf::from(path) }
      }
      "ready" => Line::Ready(at),
      "note" => Line::Note(rest.to_owned()),
      "error" => Line::Error(rest.to_owned()),
      "unsupported" => Line::Unsupported(rest.to_owned()),
      _ => Line::Error(format!("a line that the driver does not know: {line}")),
    };
    if lines.send(line).is_err() {
      return;
    }
  }
  let _ = lines.send(Line::End);
}

/// The directory of the running program: the Rust adapters are built next to it.
#[must_use]
pub fn own_directory() -> PathBuf {
  let program = std::env::current_exe().expect("the path of the driver is known");
  program.parent().map(Path::to_path_buf).expect("a program is in a directory")
}

/// How long the driver waits for `ready`. chokidar needs minutes for 20,000 polled files.
pub const READY_PATIENCE: Duration = Duration::from_secs(300);
