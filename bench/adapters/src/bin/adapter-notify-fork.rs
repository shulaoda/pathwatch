//! The fork of notify that rolldown uses today (github.com/rolldown/notify), used the way
//! rolldown uses it: `RecommendedWatcher::new` with a `Config`, `paths_mut`, and every path
//! with `Recursive` and `TrackPath`. With `poll`: `PollWatcher`. With `ignore`:
//! `Config::with_ignored`.
//!
//! rolldown puts its own code on top of this (a file check for every event and a walk of each
//! new directory). The adapter does not: it prints what the library gives.

use std::sync::mpsc;

use adapters::{Out, exit_when_stdin_closes, has_part, read_job};
use notify_fork::{
  Config, Event, EventKind, PollWatcher, RecommendedWatcher, RecursiveMode, TargetMode, WatchMode,
  Watcher,
  event::{AccessKind, AccessMode, ModifyKind},
};

fn main() {
  let mut out = Out::new();
  let job = read_job();

  let mut config = Config::default();
  if let Some(name) = job.ignore {
    config = config.with_ignored(move |path, _kind| has_part(path, &name));
  }
  let (sender, events) = mpsc::channel();
  let mut watcher: Box<dyn Watcher> = match job.poll {
    Some(pause) => Box::new(
      PollWatcher::new(sender, config.with_poll_interval(pause)).expect("the poll watcher starts"),
    ),
    None => Box::new(RecommendedWatcher::new(sender, config).expect("the watcher starts")),
  };

  let mode =
    WatchMode { recursive_mode: RecursiveMode::Recursive, target_mode: TargetMode::TrackPath };
  let mut failed = 0;
  let mut paths = watcher.paths_mut();
  for path in &job.paths {
    if let Err(error) = paths.add(path, mode) {
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

fn print(out: &mut Out, result: notify_fork::Result<Event>) {
  match result {
    Ok(event) => {
      let kind = word(&event.kind);
      for path in &event.paths {
        out.event(kind, path);
      }
    }
    Err(error) => out.error(&error.to_string()),
  }
}

/// The event kinds of notify in one word. The driver does not count `access` as a change.
fn word(kind: &EventKind) -> &'static str {
  match kind {
    EventKind::Create(_) => "create",
    EventKind::Remove(_) => "delete",
    EventKind::Modify(ModifyKind::Name(_)) => "rename",
    EventKind::Modify(ModifyKind::Metadata(_)) => "metadata",
    EventKind::Modify(_) => "update",
    EventKind::Access(AccessKind::Close(AccessMode::Write)) => "close-write",
    EventKind::Access(_) => "access",
    EventKind::Any | EventKind::Other => "other",
  }
}
