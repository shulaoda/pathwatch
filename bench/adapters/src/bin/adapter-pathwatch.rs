//! pathwatch, used as its crate docs show: a handler that sends each batch to a channel, and
//! `watch` for the paths.
//!
//! `--debounce-ms <n>` sets `Config::with_debounce`; without it the default is used (none on
//! macOS, 10 ms elsewhere).

use std::{env, sync::mpsc, time::Duration};

use adapters::{Out, exit_when_stdin_closes, has_part, read_job};
use pathwatch::{Config, EventKind, Watcher};

fn main() {
  let mut out = Out::new();
  let job = read_job();

  let mut config = Config::default();
  let args: Vec<String> = env::args().collect();
  if let Some(at) = args.iter().position(|arg| arg == "--debounce-ms") {
    let ms: u64 = args.get(at + 1).and_then(|ms| ms.parse().ok()).expect("--debounce-ms <n>");
    config = config.with_debounce(Duration::from_millis(ms));
  }
  if let Some(name) = job.ignore {
    config = config.with_ignored(move |path, _kind| has_part(path, &name));
  }
  if let Some(pause) = job.poll {
    config = config.with_polling(true).with_poll_interval(pause);
  }

  let (sender, batches) = mpsc::channel();
  let watcher = Watcher::with_config(config, move |result| {
    let _ = sender.send(result);
  })
  .expect("the watcher thread starts");
  if let Err(error) = watcher.watch(&job.paths) {
    out.error(&error.to_string());
  }
  // The thread for stdin is started before `ready`, so the memory at `ready` holds
  // everything that the adapter needs.
  exit_when_stdin_closes();
  out.ready();

  for result in batches {
    match result {
      Ok(events) => {
        for event in events {
          let kind = match event.kind {
            EventKind::Create => "create",
            EventKind::Update => "update",
            EventKind::Delete => "delete",
          };
          out.event(kind, &event.path);
        }
        out.flush();
      }
      Err(error) => out.error(&error.to_string()),
    }
  }
}
