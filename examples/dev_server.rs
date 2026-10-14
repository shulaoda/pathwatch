//! A dev server in small: it watches the `src` directory of a project, and the extra paths
//! that the file `watch-list.txt` of the project names, one in a line.
//!
//! ```text
//! cargo run -p pathwatch --example dev_server -- path/to/project
//! ```
//!
//! Change `watch-list.txt` while it runs, and it watches other paths. Press Enter to stop.
#![expect(
  clippy::print_stdout,
  clippy::print_stderr,
  reason = "printing is what this example does"
)]

use std::{env, error::Error, fs, io, path::PathBuf, sync::mpsc, thread};

use pathwatch::{Config, Watcher};

fn main() -> Result<(), Box<dyn Error>> {
  let project = PathBuf::from(env::args_os().nth(1).unwrap_or_else(|| ".".into()));
  let list = project.join("watch-list.txt");
  let read_list = || -> Vec<PathBuf> {
    let text = fs::read_to_string(&list).unwrap_or_default();
    text.lines().filter(|line| !line.trim().is_empty()).map(|line| project.join(line)).collect()
  };

  let config = Config::default().with_ignored(|path, _kind| {
    path.components().any(|part| part.as_os_str() == "node_modules" || part.as_os_str() == ".git")
  });
  // The handler only passes each batch on. `None` on the same channel means "stop".
  let (sender, messages) = mpsc::channel();
  let batches = sender.clone();
  let watcher = Watcher::with_config(config, move |result| {
    let _ = batches.send(Some(result));
  })?;
  let mut extra = read_list();
  watcher.watch(&[project.join("src"), list.clone()])?;
  watcher.watch(&extra)?;
  thread::spawn(move || {
    let _ = io::stdin().read_line(&mut String::new());
    let _ = sender.send(None);
  });
  println!("watching {}, press Enter to stop", project.display());

  while let Some(result) = messages.recv()? {
    let events = match result {
      Ok(events) => events,
      Err(error) => {
        eprintln!("error: {error}");
        continue;
      }
    };
    if events.iter().any(|event| event.path.ends_with("watch-list.txt")) {
      watcher.unwatch(&extra)?;
      extra = read_list();
      watcher.watch(&extra)?;
      println!("{} extra paths are watched", extra.len());
    }
    println!("rebuild: {} files changed", events.len());
  }
  watcher.close();
  println!("closed");
  Ok(())
}
