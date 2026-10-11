//! notify 8 from crates.io, used as its docs show: `recommended_watcher` with a channel, and
//! `paths_mut` to add many paths in one step. With `poll`: `PollWatcher`.
//!
//! notify has no ignore rule, so an `ignore` line only gives a note: the adapter prints what
//! the library gives.

use std::{path::Path, sync::mpsc};

use adapters::{Out, exit_when_stdin_closes, notify_word, read_job};
use notify::{Config, Event, PollWatcher, RecursiveMode, Watcher};

fn main() {
  let mut out = Out::new();
  let job = read_job();
  if job.ignore.is_some() {
    out.note("notify has no ignore rule: every event of the library is printed");
  }

  let (sender, events) = mpsc::channel();
  let mut watcher: Box<dyn Watcher> = match job.poll {
    Some(pause) => Box::new(
      PollWatcher::new(sender, Config::default().with_poll_interval(pause))
        .expect("the poll watcher starts"),
    ),
    None => Box::new(notify::recommended_watcher(sender).expect("the watcher starts")),
  };

  let mut failed = 0;
  let mut paths = watcher.paths_mut();
  for path in &job.paths {
    if let Err(error) = paths.add(path, mode_for(path)) {
      failed += 1;
      // One error for each path would be thousands of equal lines.
      if failed <= 3 {
        out.error(&format!("watch {}: {error}", path.display()));
      }
    }
  }
  if let Err(error) = paths.commit() {
    out.error(&format!("commit: {error}"));
  }
  if failed > 0 {
    out.error(&format!("watch failed for {failed} of {} paths", job.paths.len()));
  }
  // The thread for stdin is started before `ready`, so the memory at `ready` holds
  // everything that the adapter needs.
  exit_when_stdin_closes();
  out.ready();

  while let Ok(first) = events.recv() {
    print(&mut out, first);
    // Everything that waits goes out in one write.
    while let Ok(next) = events.try_recv() {
      print(&mut out, next);
    }
    out.flush();
  }
}

/// A directory is watched with everything below it, a file by itself.
fn mode_for(path: &Path) -> RecursiveMode {
  if path.is_dir() { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive }
}

fn print(out: &mut Out, result: notify::Result<Event>) {
  match result {
    Ok(event) => {
      let kind = notify_word(&event.kind);
      for path in &event.paths {
        out.event(kind, path);
      }
    }
    Err(error) => out.error(&error.to_string()),
  }
}
