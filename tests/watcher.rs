//! The watcher thread on a real directory, with the poll backend.

#[cfg(windows)]
use std::env;
use std::{
  collections::BTreeSet,
  fs, io,
  path::{Path, PathBuf},
  sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
  },
  thread,
  time::{Duration, Instant, SystemTime},
};

use pathwatch::{
  Config, Error, ErrorKind, Event, EventKind, Watcher,
  backend::{Backend, Coverage, DirKey, Hint, Sink, Want, watch_limit},
  engine::{Ignored, Options},
  poll::PollBackend,
};
use tempfile::TempDir;

const PATIENCE: Duration = Duration::from_secs(30);

type Results = Receiver<Result<Vec<Event>, Error>>;

fn quick() -> Options {
  Options {
    debounce: Duration::from_millis(2),
    delete_delay: Duration::from_millis(20),
    poll_interval: Duration::from_millis(5),
    ..Options::default()
  }
}

fn quick_config() -> Config {
  Config::default()
    .with_polling(true)
    .with_poll_interval(Duration::from_millis(5))
    .with_debounce(Duration::from_millis(2))
}

struct Watch {
  watcher: Watcher,
  results: Results,
  dir: TempDir,
  events: Vec<Event>,
}

fn watch() -> Watch {
  let (sender, results) = mpsc::channel();
  let watcher = Watcher::with_backend(
    quick(),
    move |result| {
      let _ = sender.send(result);
    },
    PollBackend::new,
  )
  .unwrap();
  Watch { watcher, results, dir: tempfile::tempdir().unwrap(), events: Vec::new() }
}

/// As the public API makes it.
fn watch_with(config: Config) -> Watch {
  let (sender, results) = mpsc::channel();
  let watcher = Watcher::with_config(config, move |result| {
    let _ = sender.send(result);
  })
  .unwrap();
  Watch { watcher, results, dir: tempfile::tempdir().unwrap(), events: Vec::new() }
}

impl Watch {
  fn path(&self, rel: &str) -> PathBuf {
    self.dir.path().join(rel)
  }

  /// Waits for the event, and drops the ones before it.
  fn expect(&mut self, kind: EventKind, rel: &str) {
    self.expect_all(kind, &[rel]);
  }

  /// Waits until each of `rels` had the event, in any order and any batches.
  fn expect_all(&mut self, kind: EventKind, rels: &[&str]) {
    let paths: Vec<PathBuf> = rels.iter().map(|rel| self.path(rel)).collect();
    let end = Instant::now() + PATIENCE;
    loop {
      let found: Vec<Option<usize>> = paths
        .iter()
        .map(|path| self.events.iter().position(|event| event.kind == kind && &event.path == path))
        .collect();
      if let Some(last) = found.iter().copied().collect::<Option<Vec<usize>>>() {
        self.events.drain(..=last.into_iter().max().unwrap_or(0));
        return;
      }
      match self.results.recv_timeout(end.saturating_duration_since(Instant::now())) {
        Ok(Ok(mut events)) => {
          assert!(!events.is_empty(), "a batch is never empty");
          // A batch has no promised order: sorted, so no test depends on it.
          events.sort_by(|a, b| a.path.cmp(&b.path));
          assert!(
            events.windows(2).all(|pair| pair[0].path < pair[1].path),
            "a batch has one event for a path: {events:?}"
          );
          self.events.extend(events);
        }
        Ok(Err(error)) => panic!("the watcher reported: {error}"),
        Err(timeout) => {
          panic!("no {kind:?} for each of {rels:?}: {timeout}; left over: {:?}", self.events)
        }
      }
    }
  }
}

/// Windows: what `watch` makes of a path, seen through a filter that ignores the paths, so
/// that nothing is read. A share spelled with a separator too many is a share, not a path on the
/// current drive; a drive with no root is joined to the current directory on that drive.
#[cfg(windows)]
#[test]
fn the_paths_that_windows_takes() {
  let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
  let current = env::current_dir().unwrap();
  let share = PathBuf::from(r"\\server\share\f");
  let config = Config::default().with_ignored({
    let (seen, current, share) = (Arc::clone(&seen), current.clone(), share.clone());
    // The filter is asked about the directories above a path first, and nothing below an
    // ignored one: the paths themselves are ignored, so that nothing is read, and so is every
    // entry of the current directory, which is watched when a drive with no root is cleaned
    // to it, so that its listing reads nothing below.
    move |path: &Path, _| {
      seen.lock().unwrap().push(path.to_path_buf());
      path == share || (path.starts_with(&current) && path != current)
    }
  });
  let watcher = Watcher::with_config(config, |_| {}).unwrap();
  let drive = current.components().next().unwrap().as_os_str().to_owned();
  let other_drive = if drive.eq_ignore_ascii_case("Q:") { "R:" } else { "Q:" };

  watcher.watch(&[r"\\server\\share\f"]).unwrap();
  watcher.watch(&["//server//share/f"]).unwrap();
  let mut on_the_drive = drive.clone();
  on_the_drive.push("name");
  watcher.watch(&[&on_the_drive]).unwrap();
  watcher.watch(&[&drive]).unwrap();
  for bad in [r"\\\server\share\f", "///server/share/f", r"\\server", r"\\server\\share"] {
    assert_eq!(invalid(watcher.watch(&[bad])), Path::new(bad), "{bad}");
  }
  assert_eq!(
    invalid(watcher.watch(&[format!("{other_drive}name")])),
    Path::new(other_drive).join("name")
  );
  watcher.close();

  let seen = seen.lock().unwrap();
  // The filter is asked as a file and as a directory, for each of the two calls.
  assert_eq!(seen.iter().filter(|path| **path == share).count(), 4, "{seen:?}");
  assert!(seen.contains(&current.join("name")), "{seen:?}");
  assert!(seen.contains(&current), "{seen:?}");
  // Nothing else is asked about: the directories on the way, and the entries of the current
  // directory.
  let on_the_way = |path: &PathBuf| share.starts_with(path) || current.starts_with(path);
  assert!(seen.iter().all(|path| on_the_way(path) || path.starts_with(&current)), "{seen:?}");
}

/// Windows: a watcher that a thread-local holds is dropped while the process exits, when the OS
/// has ended the watcher thread already. The exit code is the one the program asked for; a
/// `join` of that thread would end the process with a crash code instead. The test runs itself
/// as a child for that.
#[cfg(windows)]
#[test]
#[expect(clippy::exit, reason = "the child ends the process with the watcher in a thread-local")]
fn a_watcher_dropped_while_the_process_exits() {
  thread_local! {
    static HELD: std::cell::RefCell<Option<Watcher>> = const { std::cell::RefCell::new(None) };
  }
  if env::var_os("PATHWATCH_TEST_CHILD").is_some() {
    let watcher = Watcher::new(|_| {}).unwrap();
    watcher.watch(&[env::temp_dir().join("pathwatch-not-there")]).unwrap();
    HELD.with(|held| *held.borrow_mut() = Some(watcher));
    std::process::exit(7);
  }
  let child = std::process::Command::new(env::current_exe().unwrap())
    .args(["--exact", "a_watcher_dropped_while_the_process_exits", "--nocapture"])
    .env("PATHWATCH_TEST_CHILD", "1")
    .output()
    .unwrap();
  let code = child.status.code();
  assert_eq!(code, Some(7), "exit code {code:#x?}: {}", String::from_utf8_lossy(&child.stderr));
}

#[cfg(windows)]
fn invalid(result: Result<(), Error>) -> PathBuf {
  let error = result.expect_err("the path cannot be used");
  assert_eq!(error.kind(), ErrorKind::InvalidPath);
  error.path().expect("the error names the path").to_path_buf()
}

fn closed(result: Result<(), Error>) {
  assert_eq!(result.expect_err("the watcher is closed").kind(), ErrorKind::Closed);
}

/// A watcher that its own handler can reach. The slot is filled after the watcher is made.
fn with_handler(
  mut handler: impl FnMut(&Watcher, Result<Vec<Event>, Error>) + Send + 'static,
) -> Arc<OnceLock<Watcher>> {
  let slot: Arc<OnceLock<Watcher>> = Arc::default();
  let watcher = {
    let slot = Arc::clone(&slot);
    Watcher::with_config(quick_config(), move |result| {
      handler(slot.get().expect("the test filled the slot before the first change"), result);
    })
    .unwrap()
  };
  slot.set(watcher).unwrap();
  slot
}

/// Hints nothing: it watches everything, or nothing, so that the poll looks. A raw notification
/// of one byte holds the watcher thread, and a 2 with the eight bytes of a time in nanoseconds
/// (`delivery`) is a `Hint::Delivered`; every other one is counted.
struct Silent {
  polled: bool,
  translated: Arc<AtomicUsize>,
  entered: Sender<()>,
  gate: Receiver<()>,
}

impl Backend for Silent {
  type Raw = Vec<u8>;

  fn watch(&mut self, _key: DirKey, _dir: &Path, _want: Want) -> Coverage {
    if self.polled { Coverage::NotWatched } else { Coverage::Watched }
  }

  fn unwatch(&mut self, _key: DirKey) {}

  fn translate(&mut self, raw: Self::Raw, hint: &mut dyn FnMut(Hint<'_>)) {
    if raw == [1] {
      self.entered.send(()).unwrap();
      self.gate.recv().unwrap();
    } else if let [2, nanos @ ..] = raw.as_slice()
      && let Ok(nanos) = <[u8; 8]>::try_from(nanos)
    {
      let before = SystemTime::UNIX_EPOCH + Duration::from_nanos(u64::from_ne_bytes(nanos));
      hint(Hint::Delivered { before });
    } else {
      self.translated.fetch_add(1, Ordering::Relaxed);
    }
  }

  fn raw_bytes(raw: &Self::Raw) -> usize {
    raw.len()
  }
}

#[test]
fn drop_waits_for_the_thread() {
  let w = watch();
  w.watcher.watch(&[w.path("a.txt")]).unwrap();
  drop(w.watcher);
  // The handler is gone with the thread: nothing can come after `drop`.
  loop {
    match w.results.try_recv() {
      Ok(_) => {}
      Err(TryRecvError::Disconnected) => break,
      Err(TryRecvError::Empty) => panic!("the watcher thread is still alive"),
    }
  }
}

#[test]
fn close_twice() {
  let w = watch();
  w.watcher.watch(&[w.path("a.txt")]).unwrap();
  w.watcher.close();
  // `close` waited for the thread, so the handler is gone.
  assert!(matches!(w.results.try_recv(), Err(TryRecvError::Disconnected)));
  w.watcher.close();
  w.watcher.close();
  drop(w.watcher);
}

#[test]
fn a_call_after_close() {
  let w = watch();
  w.watcher.watch(&[w.path("a.txt")]).unwrap();
  w.watcher.close();
  closed(w.watcher.watch(&[w.path("b.txt")]));
  closed(w.watcher.watch(&[w.path("b.txt"), w.path("c.txt")]));
  closed(w.watcher.unwatch(&[w.path("a.txt")]));
  closed(w.watcher.unwatch(&[w.path("a.txt")]));
  // `Closed` comes first: the path is not even looked at.
  closed(w.watcher.watch(&[""]));
  assert_eq!(w.watcher.watch(&["x"]).unwrap_err().to_string(), "the watcher is closed");
}

#[test]
fn close_inside_the_handler() {
  let dir = tempfile::tempdir().unwrap();
  let (sender, answers) = mpsc::channel();
  let extra = dir.path().join("b.txt");
  let slot = with_handler(move |watcher, _result| {
    // It must not wait for the thread that runs it.
    watcher.close();
    watcher.close();
    let _ = sender.send(watcher.watch(&[&extra]));
  });
  let watcher = slot.get().unwrap();
  watcher.watch(&[dir.path()]).unwrap();
  fs::write(dir.path().join("a.txt"), "1").unwrap();

  closed(answers.recv_timeout(PATIENCE).expect("the handler ran"));
  // The thread ends and drops the handler with its sender.
  fs::write(dir.path().join("c.txt"), "1").unwrap();
  assert!(matches!(answers.recv_timeout(PATIENCE), Err(RecvTimeoutError::Disconnected)));
  closed(watcher.watch(&[dir.path().join("d.txt")]));
  // From another thread it waits for the thread, which is gone or going.
  watcher.close();
}

#[test]
fn watch_fails_when_the_thread_is_gone() {
  let dir = tempfile::tempdir().unwrap();
  let handler = |_result| panic!("the handler of this test panics");
  let watcher = Watcher::with_backend(quick(), handler, PollBackend::new).unwrap();
  watcher.watch(&[dir.path().join("a.txt")]).unwrap();
  fs::write(dir.path().join("a.txt"), "1").unwrap();

  let end = Instant::now() + PATIENCE;
  loop {
    match watcher.watch(&[dir.path().join("b.txt")]) {
      Err(error) => {
        assert_eq!(error.kind(), ErrorKind::Closed);
        break;
      }
      Ok(()) => assert!(Instant::now() < end, "the watcher thread did not stop"),
    }
    thread::yield_now();
  }
  closed(watcher.unwatch(&[dir.path().join("a.txt")]));
  watcher.close();
}

/// Only the error that a backend made for a limit counts as one, not any error of the OS.
#[test]
fn a_backend_that_cannot_start_is_replaced_by_polling() {
  let cases = [
    (
      io::Error::other("no watch API"),
      ErrorKind::Io,
      "input or output error: the OS watcher did not start, so every path is polled: no watch API",
    ),
    (
      io::Error::new(io::ErrorKind::StorageFull, "disk full"),
      ErrorKind::Io,
      "input or output error: the OS watcher did not start, so every path is polled: disk full",
    ),
    (
      watch_limit("no room"),
      ErrorKind::WatchLimit,
      "the OS has no room for one more watch, polling instead: no room",
    ),
  ];
  for (error, kind, text) in cases {
    let dir = tempfile::tempdir().unwrap();
    let (sender, results) = mpsc::channel();
    let watcher = Watcher::with_backend(
      quick(),
      move |result| {
        let _ = sender.send(result);
      },
      |_sink| Err::<PollBackend, _>(error),
    )
    .unwrap();
    let told = results.recv_timeout(PATIENCE).expect("the handler is told").unwrap_err();
    assert_eq!((told.kind(), told.path()), (kind, None));
    assert_eq!(told.to_string(), text);

    let path = dir.path().join("a.txt");
    watcher.watch(&[&path]).unwrap();
    fs::write(&path, "1").unwrap();
    let batch = results.recv_timeout(PATIENCE).expect("the watcher polls");
    assert_eq!(batch.unwrap(), [Event { path, kind: EventKind::Create }]);
  }
}

#[test]
fn a_watcher_shared_by_two_threads() {
  fn shared<T: Send + Sync>() {}
  shared::<Watcher>();

  let dir = tempfile::tempdir().unwrap();
  let (sender, results) = mpsc::channel();
  let handler = move |result: Result<Vec<Event>, Error>| {
    let _ = sender.send(result);
  };
  let watcher = Arc::new(Watcher::with_config(quick_config(), handler).unwrap());
  let work = |thread: usize| {
    let (watcher, dir) = (Arc::clone(&watcher), dir.path().to_path_buf());
    move || {
      for file in 0..50 {
        let (kept, dropped) = (format!("{thread}-{file}.txt"), format!("{thread}-{file}.tmp"));
        watcher.watch(&[dir.join(&kept), dir.join(&dropped)]).unwrap();
        watcher.unwatch(&[dir.join(&dropped)]).unwrap();
        fs::write(dir.join(&kept), "1").unwrap();
        fs::write(dir.join(&dropped), "1").unwrap();
      }
    }
  };
  for thread in [thread::spawn(work(0)), thread::spawn(work(1))] {
    thread.join().unwrap();
  }

  let mut created = BTreeSet::new();
  while created.len() < 100 {
    for event in results.recv_timeout(PATIENCE).expect("100 files are reported").unwrap() {
      assert_eq!(event.path.extension().unwrap(), "txt", "an unwatched path was reported");
      // A poll between "made" and "written" sees the file empty: then an `Update` follows.
      match event.kind {
        EventKind::Create => assert!(created.insert(event.path.clone()), "{event:?}"),
        EventKind::Update => assert!(created.contains(&event.path), "{event:?}"),
        EventKind::Delete => panic!("{event:?}"),
      }
    }
  }
  watcher.close();
}

/// It is taken as 1 ms: zero would make the watcher thread spin.
#[test]
fn a_poll_interval_of_zero() {
  let config = Config::default().with_polling(true).with_poll_interval(Duration::ZERO);
  let shown = format!("{config:?}");
  assert!(shown.contains("poll_interval: 1ms"), "{shown}");

  let dir = tempfile::tempdir().unwrap();
  let (sender, results) = mpsc::channel();
  let watcher = Watcher::with_config(config, move |result| {
    let _ = sender.send(result);
  })
  .unwrap();
  let path = dir.path().join("a.txt");
  watcher.watch(&[&path]).unwrap();
  fs::write(&path, "1").unwrap();
  let batch = results.recv_timeout(PATIENCE).expect("the file is reported");
  assert_eq!(batch.unwrap(), [Event { path, kind: EventKind::Create }]);
}

/// Dropped signals make the engine check everything; the word that the backend died is kept.
#[test]
fn the_queue_is_bounded_by_bytes() {
  const MIB: usize = 1 << 20;

  let dir = tempfile::tempdir().unwrap();
  let translated = Arc::new(AtomicUsize::new(0));
  let (entered, enter) = mpsc::channel();
  let (open, gate) = mpsc::channel::<()>();
  let (sender, results) = mpsc::channel();
  let (sink_out, sink_in) = mpsc::channel::<Sink<Vec<u8>>>();
  let backend = Silent { polled: false, translated: Arc::clone(&translated), entered, gate };
  let watcher = Watcher::with_backend(
    quick(),
    move |result: Result<Vec<Event>, Error>| {
      let _ = sender.send(result);
    },
    move |sink| {
      sink_out.send(sink).unwrap();
      Ok(backend)
    },
  )
  .unwrap();
  let sink = sink_in.recv().unwrap();
  watcher.watch(&[dir.path()]).unwrap();
  // Holds the watcher thread, lets `while_held` run, then lets the thread go.
  let hold = |while_held: &dyn Fn()| {
    sink.raw(vec![1]);
    enter.recv_timeout(PATIENCE).expect("the watcher thread took the empty notification");
    while_held();
    open.send(()).unwrap();
  };
  let wait_for = |count: usize| {
    let end = Instant::now() + PATIENCE;
    while translated.load(Ordering::Relaxed) < count {
      assert!(Instant::now() < end, "{} of {count}", translated.load(Ordering::Relaxed));
      thread::yield_now();
    }
  };

  let flood = |count: usize, bytes: usize| {
    for _ in 0..count {
      sink.raw(vec![0; bytes]);
    }
  };

  // Far below the bound: every one is translated.
  hold(&|| flood(100_000, 1));
  wait_for(100_000);

  // Above the bound. Nothing will ever hint at this file: only the scan after the overflow
  // finds it.
  hold(&|| {
    fs::write(dir.path().join("a.txt"), "1").unwrap();
    flood(20, MIB);
  });
  let created = [Event { path: dir.path().join("a.txt"), kind: EventKind::Create }];
  while results.recv_timeout(PATIENCE).expect("the scan finds the file").ok().as_deref()
    != Some(&created)
  {}
  // 15 of them fit, with the few bytes that a message takes itself.
  wait_for(100_015);
  assert_eq!(translated.load(Ordering::Relaxed), 100_015);

  // The queue is filled to the last byte: a message with no bytes of its own fits as long as
  // any message does. So the word that the backend died is dropped too, and kept all the same.
  // It is taken with the overflow, where the first drop was in the queue: nothing behind that
  // reaches the backend, which is gone. Had the word fit, every empty message that fit would
  // have been translated before it.
  hold(&|| {
    flood(20, MIB);
    flood(100_000, 0);
    sink.failed(io::Error::other("the backend thread stopped"));
  });
  loop {
    match results.recv_timeout(PATIENCE).expect("the error comes") {
      Err(error) if error.kind() == ErrorKind::Io => {
        assert!(error.to_string().contains("the backend thread stopped"), "{error}");
        break;
      }
      _ => {}
    }
  }
  // The backend held the other end of this channel.
  assert_eq!(enter.recv_timeout(PATIENCE), Err(RecvTimeoutError::Disconnected));
  assert_eq!(translated.load(Ordering::Relaxed), 100_030);
  drop(watcher);
}

/// A flood of notifications does not starve the engine: after 512 messages the thread does what
/// is due before it takes the next one.
#[test]
fn a_turn_takes_at_most_512_messages() {
  const TURN: usize = 512;
  const FLOOD: usize = 600;

  let dir = tempfile::tempdir().unwrap();
  let (entered, enter) = mpsc::channel();
  let (open, gate) = mpsc::channel::<()>();
  let (sender, results) = mpsc::channel();
  let (sink_out, sink_in) = mpsc::channel::<Sink<Vec<u8>>>();
  let backend = Silent { polled: true, translated: Arc::default(), entered, gate };
  let watcher = Watcher::with_backend(
    quick(),
    move |result: Result<Vec<Event>, Error>| {
      let _ = sender.send(result);
    },
    move |sink| {
      sink_out.send(sink).unwrap();
      Ok(backend)
    },
  )
  .unwrap();
  // Dropped before the watcher: a failed test ends the held thread instead of waiting for it.
  let open = open;
  let sink = sink_in.recv().unwrap();
  watcher.watch(&[dir.path()]).unwrap();

  // Each one holds the thread until the test lets it go on.
  for _ in 0..FLOOD {
    sink.raw(vec![1]);
  }
  let pass = || {
    enter.recv_timeout(PATIENCE).expect("the watcher thread took the next notification");
    open.send(()).unwrap();
  };
  // Nothing hints at this file: the poll finds it, which comes due while the first
  // notification holds the thread.
  let path = dir.path().join("a.txt");
  fs::write(&path, "1").unwrap();
  enter.recv_timeout(PATIENCE).expect("the watcher thread took the first notification");
  thread::sleep(quick().poll_interval * 4);
  open.send(()).unwrap();
  for _ in 1..TURN {
    pass();
  }
  // The next notification waits: the poll runs first.
  let batch = results.recv_timeout(PATIENCE).expect("the poll runs between two turns");
  assert_eq!(batch.unwrap(), [Event { path, kind: EventKind::Create }]);
  for _ in TURN..FLOOD {
    pass();
  }
}

#[test]
fn a_dropped_signal_wakes_the_thread() {
  let dir = tempfile::tempdir().unwrap();
  let (entered, _enter) = mpsc::channel();
  let (_open, gate) = mpsc::channel::<()>();
  let (sender, results) = mpsc::channel();
  let (sink_out, sink_in) = mpsc::channel::<Sink<Vec<u8>>>();
  let backend = Silent { polled: false, translated: Arc::default(), entered, gate };
  let watcher = Watcher::with_backend(
    // Nothing else wakes the thread: the next poll is an hour away.
    Options { poll_interval: Duration::from_secs(3600), ..quick() },
    move |result: Result<Vec<Event>, Error>| {
      let _ = sender.send(result);
    },
    move |sink| {
      sink_out.send(sink).unwrap();
      Ok(backend)
    },
  )
  .unwrap();
  let sink = sink_in.recv().unwrap();
  watcher.watch(&[dir.path()]).unwrap();

  // Nothing will ever hint at this file: only the scan after the overflow finds it.
  let path = dir.path().join("a.txt");
  fs::write(&path, "1").unwrap();
  // One notification that is bigger than the whole queue: it is dropped.
  sink.raw(vec![0; (16 << 20) + 1]);
  let batch = results.recv_timeout(PATIENCE).expect("the thread wakes up and scans");
  assert_eq!(batch.unwrap(), [Event { path, kind: EventKind::Create }]);
}

/// A delivery that comes behind a dropped signal is no delivery of that signal: the overflow is
/// taken where the drop was in the queue. Taken at the end of the turn, the delivery ended the
/// doubt of a young file that was not read (design.md 5.8), and the scan after the overflow did
/// not count it as written, though the dropped signal can have been its write.
#[test]
fn a_delivery_behind_a_dropped_signal_does_not_end_the_doubt() {
  let dir = tempfile::tempdir().unwrap();
  let (entered, enter) = mpsc::channel();
  let (open, gate) = mpsc::channel::<()>();
  let (sender, results) = mpsc::channel();
  let (sink_out, sink_in) = mpsc::channel::<Sink<Vec<u8>>>();
  let backend = Silent { polled: false, translated: Arc::default(), entered, gate };
  let watcher = Watcher::with_backend(
    Options { written_hints: true, poll_interval: Duration::from_secs(3600), ..quick() },
    move |result: Result<Vec<Event>, Error>| {
      let _ = sender.send(result);
    },
    move |sink| {
      sink_out.send(sink).unwrap();
      Ok(backend)
    },
  )
  .unwrap();
  let sink = sink_in.recv().unwrap();

  // Young at the look of `watch` however slow the machine: its time is a second ahead.
  let path = dir.path().join("a.txt");
  fs::write(&path, "1").unwrap();
  let modified = SystemTime::now() + Duration::from_secs(1);
  let set_time = || fs::File::options().write(true).open(&path).unwrap().set_modified(modified);
  set_time().unwrap();
  watcher.watch(&[dir.path()]).unwrap();
  // Written again with the same fingerprint, and nothing hints at it: the hint was dropped.
  fs::write(&path, "2").unwrap();
  set_time().unwrap();
  // Nothing will ever hint at this file either: only the scan after the overflow finds it.
  let other = dir.path().join("b.txt");
  fs::write(&other, "1").unwrap();

  // In one turn of the thread: the dropped signal, then a delivery of every record until long
  // after the file is old.
  sink.raw(vec![1]);
  enter.recv_timeout(PATIENCE).expect("the watcher thread took the notification");
  sink.raw(vec![0; (16 << 20) + 1]);
  let later = modified + Duration::from_secs(3600);
  let nanos = later.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
  let mut delivery = vec![2];
  delivery.extend_from_slice(&u64::try_from(nanos).unwrap().to_ne_bytes());
  sink.raw(delivery);
  open.send(()).unwrap();

  let mut batch = results.recv_timeout(PATIENCE).expect("the scan after the overflow").unwrap();
  batch.sort_by(|a, b| a.path.cmp(&b.path));
  let expected =
    [Event { path, kind: EventKind::Update }, Event { path: other, kind: EventKind::Create }];
  assert_eq!(batch, expected);
}

/// A `watch` from inside the handler takes effect when the handler has returned, before the
/// messages that came into the queue meanwhile.
#[test]
fn a_watch_inside_the_handler_comes_before_the_queue() {
  let dir = tempfile::tempdir().unwrap();
  let other = tempfile::tempdir().unwrap();
  fs::write(other.path().join("old.txt"), "1").unwrap();
  let (entered, enter) = mpsc::channel();
  let (open, gate) = mpsc::channel::<()>();
  let (sender, results) = mpsc::channel();
  let (sink_out, sink_in) = mpsc::channel::<Sink<Vec<u8>>>();
  let (in_handler, handler_waits) = mpsc::channel::<()>();
  let (go, may_go) = mpsc::channel::<()>();
  let slot: Arc<OnceLock<Watcher>> = Arc::default();
  let backend = Silent { polled: true, translated: Arc::default(), entered, gate };
  let watcher = {
    let (slot, other) = (Arc::clone(&slot), other.path().to_owned());
    Watcher::with_backend(
      quick(),
      move |result: Result<Vec<Event>, Error>| {
        let events = result.unwrap();
        if events.iter().any(|event| event.path.ends_with("a.txt")) {
          in_handler.send(()).unwrap();
          may_go.recv().unwrap();
          slot.get().expect("the slot is filled").watch(&[&other]).unwrap();
        }
        let _ = sender.send(events);
      },
      move |sink| {
        sink_out.send(sink).unwrap();
        Ok(backend)
      },
    )
    .unwrap()
  };
  let sink = sink_in.recv().unwrap();
  watcher.watch(&[dir.path()]).unwrap();
  slot.set(watcher).unwrap();

  fs::write(dir.path().join("a.txt"), "1").unwrap();
  handler_waits.recv_timeout(PATIENCE).expect("the handler runs");
  // Two notifications come into the queue while the handler runs; each holds the thread.
  sink.raw(vec![1]);
  sink.raw(vec![1]);
  go.send(()).unwrap();
  // The thread holds at the first one: the watch of `other` is done by then, or it waits
  // behind them, and the file written now would be in its baseline.
  enter.recv_timeout(PATIENCE).expect("the thread took the first notification");
  let path = other.path().join("new.txt");
  fs::write(&path, "1").unwrap();
  open.send(()).unwrap();
  enter.recv_timeout(PATIENCE).expect("the thread took the second notification");
  open.send(()).unwrap();
  let created = Event { path, kind: EventKind::Create };
  while !results.recv_timeout(PATIENCE).expect("the new file is reported").contains(&created) {}
}

/// Each call from inside the handler hands over a batch of its own: the disk can change between
/// two of them, and a batch has at most one event for a path. Here the first `watch` finds the
/// file, and the filter writes it again when the last one asks about its first path.
#[test]
fn each_call_inside_the_handler_is_a_batch_of_its_own() {
  let dir = tempfile::tempdir().unwrap();
  let (file, trigger) = (dir.path().join("p.txt"), dir.path().join("trigger"));
  let ignored: Ignored = {
    let (file, trigger, written) = (file.clone(), trigger.clone(), AtomicBool::new(false));
    Arc::new(move |path: &Path, _| {
      if path == trigger && !written.swap(true, Ordering::Relaxed) {
        fs::write(&file, "22").unwrap();
      }
      false
    })
  };
  let (sender, results) = mpsc::channel();
  let slot: Arc<OnceLock<Watcher>> = Arc::default();
  let watcher = {
    let (slot, file, trigger) = (Arc::clone(&slot), file.clone(), trigger.clone());
    Watcher::with_backend(
      Options { ignored: Some(ignored), ..quick() },
      move |result: Result<Vec<Event>, Error>| {
        let events = result.unwrap();
        if events.iter().any(|event| event.path.ends_with("a.txt")) {
          fs::write(&file, "1").unwrap();
          let watcher = slot.get().expect("the slot is filled");
          watcher.watch(&[&file]).unwrap();
          watcher.unwatch(&[&file]).unwrap();
          watcher.watch(&[&trigger, &file]).unwrap();
        }
        let _ = sender.send(events);
      },
      PollBackend::new,
    )
    .unwrap()
  };
  watcher.watch(&[dir.path()]).unwrap();
  slot.set(watcher).unwrap();
  fs::write(dir.path().join("a.txt"), "1").unwrap();

  let mut seen = Vec::new();
  while !seen.contains(&Event { path: file.clone(), kind: EventKind::Update }) {
    let events = results.recv_timeout(PATIENCE).expect("the second write is reported");
    let on_file = events.iter().filter(|event| event.path == file).count();
    assert!(on_file <= 1, "a batch has one event for a path: {events:?}");
    seen.extend(events);
  }
  assert!(seen.contains(&Event { path: file, kind: EventKind::Create }));
}

/// The settings reach the thread, at both ends. With a delete delay of zero, a file that is
/// removed and written again is a `Delete` and a `Create`, not one `Update`. `Duration::MAX` is
/// a way to say "never": the thread must not die of a time that no clock has.
#[test]
fn a_setting_of_zero_and_a_time_too_long_for_a_clock() {
  let mut w = watch_with(quick_config().with_delete_delay(Duration::ZERO));
  fs::write(w.path("a.txt"), "1").unwrap();
  w.watcher.watch(&[w.path("a.txt")]).unwrap();
  fs::remove_file(w.path("a.txt")).unwrap();
  w.expect(EventKind::Delete, "a.txt");
  fs::write(w.path("a.txt"), "2").unwrap();
  w.expect(EventKind::Create, "a.txt");

  // The poll flushes by itself, whatever the quiet time is.
  let mut w = watch_with(quick_config().with_debounce(Duration::MAX));
  w.watcher.watch(&[w.path("a.txt")]).unwrap();
  fs::write(w.path("a.txt"), "1").unwrap();
  w.expect(EventKind::Create, "a.txt");

  // Nothing is ever due here. The thread still takes every call.
  for config in [
    Config::default().with_debounce(Duration::MAX),
    Config::default().with_delete_delay(Duration::MAX),
    Config::default().with_polling(true).with_poll_interval(Duration::MAX),
  ] {
    let watcher = Watcher::with_config(config, |_| {}).unwrap();
    watcher.watch(&[w.dir.path()]).unwrap();
    fs::write(w.path("a.txt"), "22").unwrap();
    watcher.watch(&[w.path("later")]).unwrap();
    watcher.unwatch(&[w.dir.path()]).unwrap();
  }
}

/// Windows finds no entry under such a name as it is given, so the path would never get an event.
#[cfg(windows)]
#[test]
fn a_name_that_windows_does_not_take_as_it_is() {
  let mut w = watch();
  for name in ["a.", "a ", "a.txt:stream", "*.js", "a?", r"dir.\a.txt"] {
    let path = w.path(name);
    assert_eq!(invalid(w.watcher.watch(&[&path])), path);
    assert_eq!(invalid(w.watcher.unwatch(&[&path])), path);
  }
  w.watcher.watch(&[w.path(".a b.txt")]).unwrap();
  fs::write(w.path(".a b.txt"), "1").unwrap();
  w.expect(EventKind::Create, ".a b.txt");
}
