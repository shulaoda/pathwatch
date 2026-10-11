//! notify-debouncer-full 0.7 on notify 8, used as its docs show: `new_debouncer` with a
//! timeout and a channel, and `watch` for each path. The timeout is 50 ms, the quiet time of
//! pathwatch, so the two wait for the same thing. With `poll`: `new_debouncer_opt` with a
//! `PollWatcher`. It has no ignore rule.

use std::{
  path::Path,
  sync::mpsc::{self, Receiver},
  time::Duration,
};

use adapters::{Job, Out, exit_when_stdin_closes, notify_word, read_job};
use notify::{Config, PollWatcher, RecursiveMode, Watcher};
use notify_debouncer_full::{
  DebounceEventResult, Debouncer, FileIdCache, RecommendedCache, new_debouncer, new_debouncer_opt,
};

/// The quiet time of pathwatch.
const TIMEOUT: Duration = Duration::from_millis(50);

fn main() {
  let mut out = Out::new();
  let job = read_job();
  if job.ignore.is_some() {
    out.note("notify-debouncer-full has no ignore rule: every event of the library is printed");
  }
  let (sender, events) = mpsc::channel();
  match job.poll {
    Some(pause) => {
      let config = Config::default().with_poll_interval(pause);
      let cache = RecommendedCache::new();
      let debouncer = new_debouncer_opt::<_, PollWatcher, _>(TIMEOUT, None, sender, cache, config)
        .expect("the poll watcher starts");
      run(&mut out, &job, debouncer, &events);
    }
    None => {
      let debouncer = new_debouncer(TIMEOUT, None, sender).expect("the watcher starts");
      run(&mut out, &job, debouncer, &events);
    }
  }
}

fn run<T: Watcher, C: FileIdCache>(
  out: &mut Out,
  job: &Job,
  mut debouncer: Debouncer<T, C>,
  events: &Receiver<DebounceEventResult>,
) {
  let mut failed = 0;
  for path in &job.paths {
    if let Err(error) = debouncer.watch(path, mode_for(path)) {
      failed += 1;
      // One error for each path would be thousands of equal lines.
      if failed <= 3 {
        out.error(&format!("watch {}: {error}", path.display()));
      }
    }
  }
  if failed > 0 {
    out.error(&format!("watch failed for {failed} of {} paths", job.paths.len()));
  }
  // The thread for stdin is started before `ready`, so the memory at `ready` holds
  // everything that the adapter needs.
  exit_when_stdin_closes();
  out.ready();

  while let Ok(first) = events.recv() {
    print(out, first);
    // Everything that waits goes out in one write.
    while let Ok(next) = events.try_recv() {
      print(out, next);
    }
    out.flush();
  }
}

/// A directory is watched with everything below it, a file by itself.
fn mode_for(path: &Path) -> RecursiveMode {
  if path.is_dir() { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive }
}

fn print(out: &mut Out, result: DebounceEventResult) {
  match result {
    Ok(events) => {
      for event in events {
        let kind = notify_word(&event.kind);
        for path in &event.paths {
          out.event(kind, path);
        }
      }
    }
    Err(errors) => {
      for error in errors {
        out.error(&error.to_string());
      }
    }
  }
}
