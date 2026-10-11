//! What every Rust adapter shares: the lines that it reads and the lines that it prints.
//!
//! The driver writes the job to stdin, one command on a line, with tabs between the parts:
//!
//! ```text
//! ignore <name>     ignore every path that has a part with this name
//! poll <ms>         use the polling mode of the library, with this pause
//! watch <path>      one path to watch (many lines)
//! start             the job is complete: start watching
//! ```
//!
//! The adapter answers on stdout:
//!
//! ```text
//! note <text>            how the library is used here, when that needs a word
//! error <text>           an error that the library gave
//! unsupported <text>     the library cannot do this job; the adapter stops
//! ready                  the library says that it watches
//! event <kind> <path>    one event of the library
//! ```
//!
//! When stdin is closed the adapter exits.

use std::{
  io::{self, BufRead, BufWriter, Stdout, Write},
  path::{Path, PathBuf},
  process, thread,
  time::Duration,
};

/// What the driver asks for.
#[derive(Default)]
pub struct Job {
  pub paths: Vec<PathBuf>,
  /// Ignore every path that has a part with this name.
  pub ignore: Option<String>,
  /// Poll, with this pause between two rounds.
  pub poll: Option<Duration>,
}

/// Reads the job from stdin, up to the `start` line.
#[must_use]
pub fn read_job() -> Job {
  let mut job = Job::default();
  for line in io::stdin().lock().lines() {
    let Ok(line) = line else { break };
    let (command, rest) = line.split_once('\t').unwrap_or((line.as_str(), ""));
    match command {
      "ignore" => job.ignore = Some(rest.to_owned()),
      "poll" => job.poll = rest.parse().ok().map(Duration::from_millis),
      "watch" => job.paths.push(PathBuf::from(rest)),
      "start" => return job,
      _ => {}
    }
  }
  // The driver went away before the job was complete.
  process::exit(0);
}

/// The ignore rule of the benchmark: the path has a part with this name.
#[must_use]
pub fn has_part(path: &Path, name: &str) -> bool {
  path.components().any(|part| part.as_os_str() == name)
}

/// Exits the process when the driver closes stdin.
pub fn exit_when_stdin_closes() {
  thread::spawn(|| {
    for line in io::stdin().lock().lines() {
      if line.is_err() {
        break;
      }
    }
    process::exit(0);
  });
}

/// The lines of the adapter. Events are collected and written in one piece by `flush`, so a
/// batch of many events is one write.
pub struct Out(BufWriter<Stdout>);

impl Out {
  #[must_use]
  pub fn new() -> Self {
    Self(BufWriter::with_capacity(64 * 1024, io::stdout()))
  }

  pub fn note(&mut self, text: &str) {
    self.line("note", text);
  }

  pub fn error(&mut self, text: &str) {
    self.line("error", text);
  }

  /// The library cannot do this job.
  pub fn unsupported(&mut self, text: &str) -> ! {
    self.line("unsupported", text);
    process::exit(0);
  }

  pub fn ready(&mut self) {
    let _ = writeln!(self.0, "ready");
    self.flush();
  }

  /// One event. It is not written before `flush`.
  pub fn event(&mut self, kind: &str, path: &Path) {
    let _ = writeln!(self.0, "event\t{kind}\t{}", path.display());
  }

  pub fn flush(&mut self) {
    // The driver is gone when the pipe is closed.
    if self.0.flush().is_err() {
      process::exit(0);
    }
  }

  fn line(&mut self, word: &str, text: &str) {
    // A line is one message: an error text of many lines becomes one line.
    let text = text.replace(['\n', '\t'], " ");
    let _ = writeln!(self.0, "{word}\t{text}");
    self.flush();
  }
}

impl Default for Out {
  fn default() -> Self {
    Self::new()
  }
}
