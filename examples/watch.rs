//! Prints the events for the paths given on the command line. `PATHWATCH_POLLING=1` polls
//! instead of using the watch API of the OS.
//!
//! ```text
//! cargo run -p pathwatch --example watch -- src package.json not-there-yet.json
//! ```
#![expect(
  clippy::print_stdout,
  clippy::print_stderr,
  reason = "printing is what this example does"
)]

use std::{env, ffi::OsString, sync::mpsc};

use pathwatch::{Config, EventKind, Watcher};

fn main() -> Result<(), pathwatch::Error> {
  let paths: Vec<OsString> = env::args_os().skip(1).collect();
  if paths.is_empty() {
    eprintln!("usage: watch <path>...");
    return Ok(());
  }

  let config = Config::default().with_polling(env::var_os("PATHWATCH_POLLING").is_some());
  // The handler runs on the watcher thread: it only passes the batch on.
  let (sender, batches) = mpsc::channel();
  let watcher = Watcher::with_config(config, move |result| {
    let _ = sender.send(result);
  })?;
  watcher.watch(&paths)?;
  println!("watching, stop with Ctrl-C");

  for result in batches {
    match result {
      Ok(events) => {
        for event in events {
          let kind = match event.kind {
            EventKind::Create => "create",
            EventKind::Update => "update",
            EventKind::Delete => "delete",
          };
          println!("{kind} {}", event.path.display());
        }
      }
      Err(error) => eprintln!("error: {error}"),
    }
  }
  Ok(())
}
