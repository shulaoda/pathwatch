//! The watcher thread on a real directory, with the poll backend.

use std::{
  collections::BTreeSet,
  env, fs, io,
  path::{Path, PathBuf},
  sync::{
    Arc, OnceLock,
    atomic::{AtomicUsize, Ordering},
    mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
  },
  thread,
  time::{Duration, Instant},
};

use pathwatch::{
  Config, Error, ErrorKind, Event, EventKind, Watcher,
  backend::{Backend, Coverage, DirKey, Hint, Sink, Want, watch_limit},
  engine::Options,
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
  /// Every event that `expect` took, also the ones it dropped.
  log: Vec<Event>,
}

fn watch() -> Watch {
  watch_in(tempfile::tempdir().unwrap())
}

fn watch_in(dir: TempDir) -> Watch {
  let (sender, results) = mpsc::channel();
  let watcher = Watcher::with_backend(
    quick(),
    move |result| {
      let _ = sender.send(result);
    },
    PollBackend::new,
  )
  .unwrap();
  Watch { watcher, results, dir, events: Vec::new(), log: Vec::new() }
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
          self.log.extend(events.iter().cloned());
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

impl Watch {
  /// Forgets the events so far. After a call of the watcher nothing from before it is on its way.
  fn forget(&mut self) {
    while self.results.try_recv().is_ok() {}
    self.events.clear();
    self.log.clear();
  }

  /// No event for this path since the last `forget`.
  fn nothing_for(&self, rel: &str) {
    let path = self.path(rel);
    let found: Vec<&Event> = self.log.iter().filter(|event| event.path == path).collect();
    assert!(found.is_empty(), "{found:?}");
  }
}

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
  assert!(slot.set(watcher).is_ok());
  slot
}

/// Watches everything, hints nothing. A raw notification with no bytes holds the watcher thread.
struct Silent {
  translated: Arc<AtomicUsize>,
  entered: Sender<()>,
  gate: Receiver<()>,
}

impl Backend for Silent {
  type Raw = Vec<u8>;

  fn watch(&mut self, _key: DirKey, _dir: &Path, _want: Want) -> Coverage {
    Coverage::Watched
  }

  fn unwatch(&mut self, _key: DirKey) {}

  fn translate(&mut self, raw: Self::Raw, _hint: &mut dyn FnMut(Hint<'_>)) {
    if raw.is_empty() {
      self.entered.send(()).unwrap();
      self.gate.recv().unwrap();
    } else {
      self.translated.fetch_add(1, Ordering::Relaxed);
    }
  }

  fn raw_bytes(raw: &Self::Raw) -> usize {
    raw.len()
  }
}

#[test]
fn a_watched_file() {
  let mut w = watch();
  fs::write(w.path("a.txt"), "1").unwrap();
  w.watcher.watch(w.path("a.txt")).unwrap();

  fs::write(w.path("a.txt"), "22").unwrap();
  w.expect(EventKind::Update, "a.txt");
  // The same size, most likely inside one tick of the clock.
  fs::write(w.path("a.txt"), "33").unwrap();
  w.expect(EventKind::Update, "a.txt");
  fs::remove_file(w.path("a.txt")).unwrap();
  w.expect(EventKind::Delete, "a.txt");
  fs::write(w.path("a.txt"), "1").unwrap();
  w.expect(EventKind::Create, "a.txt");
}

#[test]
fn a_directory_that_appears_and_vanishes() {
  let mut w = watch();
  w.watcher.watch(w.path("content")).unwrap();

  fs::create_dir_all(w.path("content/sub")).unwrap();
  fs::write(w.path("content/sub/f.txt"), "1").unwrap();
  fs::write(w.path("content/g.txt"), "1").unwrap();
  w.expect_all(EventKind::Create, &["content/g.txt", "content/sub/f.txt"]);

  fs::remove_dir_all(w.path("content")).unwrap();
  w.expect_all(EventKind::Delete, &["content/g.txt", "content/sub/f.txt"]);

  fs::create_dir(w.path("content")).unwrap();
  fs::write(w.path("content/h.txt"), "1").unwrap();
  w.expect(EventKind::Create, "content/h.txt");
}

#[test]
fn a_missing_parent_chain() {
  let mut w = watch();
  w.watcher.watch(w.path("a/b/c/x.txt")).unwrap();

  fs::create_dir_all(w.path("a/b/c")).unwrap();
  fs::write(w.path("a/b/c/x.txt"), "1").unwrap();
  w.expect(EventKind::Create, "a/b/c/x.txt");

  fs::rename(w.path("a"), w.path("gone")).unwrap();
  w.expect(EventKind::Delete, "a/b/c/x.txt");
  fs::rename(w.path("gone"), w.path("a")).unwrap();
  w.expect(EventKind::Create, "a/b/c/x.txt");
}

#[test]
fn unwatch_takes_a_watch_back() {
  let mut w = watch();
  w.watcher.watch_all([w.path("a.txt"), w.path("b.txt")]).unwrap();
  w.watcher.unwatch(w.path("a.txt")).unwrap();
  w.watcher.unwatch_all([w.path("never.txt"), w.path("a.txt")]).unwrap();

  fs::write(w.path("a.txt"), "1").unwrap();
  fs::write(w.path("b.txt"), "1").unwrap();
  w.expect(EventKind::Create, "b.txt");
  fs::write(w.path("b.txt"), "22").unwrap();
  w.expect(EventKind::Update, "b.txt");
  w.nothing_for("a.txt");
}

/// The event path is absolute: joined to the current directory.
#[test]
fn a_relative_path() {
  let here = env::current_dir().unwrap();
  let mut w = watch_in(tempfile::tempdir_in(&here).unwrap());
  let rel = w.dir.path().strip_prefix(&here).unwrap().to_path_buf();
  assert!(rel.is_relative());
  w.watcher.watch(rel.join("a.txt")).unwrap();
  w.watcher.watch(Path::new(".").join(&rel).join("b.txt")).unwrap();

  fs::write(w.path("a.txt"), "1").unwrap();
  w.expect(EventKind::Create, "a.txt");
  fs::write(w.path("b.txt"), "1").unwrap();
  w.expect(EventKind::Create, "b.txt");

  w.watcher.unwatch(rel.join("a.txt")).unwrap();
  w.forget();
  fs::write(w.path("a.txt"), "22").unwrap();
  fs::write(w.path("b.txt"), "22").unwrap();
  w.expect(EventKind::Update, "b.txt");
  w.nothing_for("a.txt");
}

/// `..` takes away the name before it, by text: `nothing` does not have to exist.
#[test]
fn dot_dot_is_folded() {
  let mut w = watch();
  w.watcher.watch(w.path("nothing/../sub/./deep/..//a.txt/")).unwrap();
  fs::create_dir(w.path("sub")).unwrap();
  fs::write(w.path("sub/a.txt"), "1").unwrap();
  w.expect(EventKind::Create, "sub/a.txt");

  // The folded path is the watched one, under every spelling.
  w.watcher.watch(w.path("sub/a.txt")).unwrap();
  w.watcher.unwatch(w.path("sub/x/../a.txt")).unwrap();
  w.watcher.watch(w.path("b.txt")).unwrap();
  w.forget();
  fs::write(w.path("sub/a.txt"), "22").unwrap();
  fs::write(w.path("b.txt"), "1").unwrap();
  w.expect(EventKind::Create, "b.txt");
  w.nothing_for("sub/a.txt");

  // Above the root there is nothing.
  let above = w.path("..").join("../".repeat(64)).join("a.txt");
  assert_eq!(invalid(w.watcher.watch(&above)), above);
}

#[test]
fn one_bad_path_among_good_ones() {
  let mut w = watch();
  let top = w.dir.path().ancestors().last().unwrap().to_path_buf();
  let paths = [w.path("a.txt"), PathBuf::new(), w.path("nul\0.txt"), top.clone(), w.path("b.txt")];
  let error = w.watcher.watch_all(&paths).unwrap_err();
  assert_eq!(error.kind(), ErrorKind::InvalidPath);
  assert_eq!(error.path(), Some(Path::new("")));
  assert!(error.to_string().starts_with("the path cannot be watched"));
  assert_eq!(invalid(w.watcher.watch(&top)), top);
  assert_eq!(invalid(w.watcher.watch(&paths[2])), paths[2]);

  fs::write(w.path("a.txt"), "1").unwrap();
  fs::write(w.path("b.txt"), "1").unwrap();
  // `expect` fails the test when the handler gets an error.
  w.expect(EventKind::Create, "a.txt");
  w.expect(EventKind::Create, "b.txt");

  assert_eq!(invalid(w.watcher.unwatch_all(&paths)), PathBuf::new());
  w.watcher.watch(w.path("c.txt")).unwrap();
  w.forget();
  fs::write(w.path("a.txt"), "22").unwrap();
  fs::write(w.path("b.txt"), "22").unwrap();
  fs::write(w.path("c.txt"), "1").unwrap();
  w.expect(EventKind::Create, "c.txt");
  w.nothing_for("a.txt");
  w.nothing_for("b.txt");
}

#[test]
fn drop_waits_for_the_thread() {
  let w = watch();
  w.watcher.watch(w.path("a.txt")).unwrap();
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
  w.watcher.watch(w.path("a.txt")).unwrap();
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
  w.watcher.watch(w.path("a.txt")).unwrap();
  w.watcher.close();
  closed(w.watcher.watch(w.path("b.txt")));
  closed(w.watcher.watch_all([w.path("b.txt"), w.path("c.txt")]));
  closed(w.watcher.unwatch(w.path("a.txt")));
  closed(w.watcher.unwatch_all([w.path("a.txt")]));
  // `Closed` comes first: the path is not even looked at.
  closed(w.watcher.watch(""));
  assert_eq!(w.watcher.watch("x").unwrap_err().to_string(), "the watcher is closed");
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
    let _ = sender.send(watcher.watch(&extra));
  });
  let watcher = slot.get().unwrap();
  watcher.watch(dir.path()).unwrap();
  fs::write(dir.path().join("a.txt"), "1").unwrap();

  closed(answers.recv_timeout(PATIENCE).expect("the handler ran"));
  // The thread ends and drops the handler with its sender.
  fs::write(dir.path().join("c.txt"), "1").unwrap();
  assert!(matches!(answers.recv_timeout(PATIENCE), Err(RecvTimeoutError::Disconnected)));
  closed(watcher.watch(dir.path().join("d.txt")));
  // From another thread it waits for the thread, which is gone or going.
  watcher.close();
}

/// They do not wait: they take effect when the handler has returned.
#[test]
fn watch_and_unwatch_inside_the_handler() {
  let dir = tempfile::tempdir().unwrap();
  let path = |name: &str| dir.path().join(name);
  let (sender, results) = mpsc::channel();
  let (a, b) = (path("a.txt"), path("b.txt"));
  let mut first = true;
  let (watched, unwatched) = (b.clone(), a.clone());
  let slot = with_handler(move |watcher, result| {
    if std::mem::take(&mut first) {
      sender.send(Err(watcher.watch(&watched))).unwrap();
      sender.send(Err(watcher.unwatch(&unwatched))).unwrap();
      sender.send(Err(watcher.watch(""))).unwrap();
    }
    let _ = sender.send(Ok(result.expect("no error is expected")));
  });
  let watcher = slot.get().unwrap();
  watcher.watch(&a).unwrap();
  fs::write(&a, "1").unwrap();

  let next = || results.recv_timeout(PATIENCE).expect("the handler ran");
  assert!(matches!(next(), Err(Ok(()))), "watch is accepted");
  assert!(matches!(next(), Err(Ok(()))), "unwatch is accepted");
  assert_eq!(invalid(next().unwrap_err()), PathBuf::new());
  assert_eq!(next().unwrap(), [Event { path: a.clone(), kind: EventKind::Create }]);

  // This call waits for its turn in the queue, behind the two calls of the handler.
  watcher.watch(path("c.txt")).unwrap();
  fs::write(&a, "22").unwrap();
  fs::write(&b, "1").unwrap();
  fs::write(path("c.txt"), "1").unwrap();
  let mut seen = BTreeSet::new();
  while !seen.contains(&b) || !seen.contains(&path("c.txt")) {
    seen.extend(next().unwrap().into_iter().map(|event| event.path));
  }
  assert!(!seen.contains(&a), "the handler unwatched it");
  watcher.close();
}

#[test]
fn watch_fails_when_the_thread_is_gone() {
  let dir = tempfile::tempdir().unwrap();
  let handler = |_result| panic!("the handler of this test panics");
  let watcher = Watcher::with_backend(quick(), handler, PollBackend::new).unwrap();
  watcher.watch(dir.path().join("a.txt")).unwrap();
  fs::write(dir.path().join("a.txt"), "1").unwrap();

  let end = Instant::now() + PATIENCE;
  loop {
    match watcher.watch(dir.path().join("b.txt")) {
      Err(error) => {
        assert_eq!(error.kind(), ErrorKind::Closed);
        break;
      }
      Ok(()) => assert!(Instant::now() < end, "the watcher thread did not stop"),
    }
    thread::yield_now();
  }
  closed(watcher.unwatch(dir.path().join("a.txt")));
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
    watcher.watch(&path).unwrap();
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
        watcher.watch_all([dir.join(&kept), dir.join(&dropped)]).unwrap();
        watcher.unwatch(dir.join(&dropped)).unwrap();
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
  watcher.watch(&path).unwrap();
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
  let backend = Silent { translated: Arc::clone(&translated), entered, gate };
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
  watcher.watch(dir.path()).unwrap();
  // Holds the watcher thread, lets `while_held` run, then lets the thread go.
  let hold = |while_held: &dyn Fn()| {
    sink.raw(Vec::new());
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

  hold(&|| {
    flood(20, MIB);
    sink.failed(io::Error::other("the backend thread stopped"));
  });
  loop {
    match results.recv_timeout(PATIENCE).expect("the error comes") {
      Err(error) if error.kind() == ErrorKind::Io => break,
      _ => {}
    }
  }
  // The backend held the other end of this channel.
  assert_eq!(enter.recv_timeout(PATIENCE), Err(RecvTimeoutError::Disconnected));
  drop(watcher);
}

#[test]
fn a_dropped_signal_wakes_the_thread() {
  let dir = tempfile::tempdir().unwrap();
  let (entered, _enter) = mpsc::channel();
  let (_open, gate) = mpsc::channel::<()>();
  let (sender, results) = mpsc::channel();
  let (sink_out, sink_in) = mpsc::channel::<Sink<Vec<u8>>>();
  let backend = Silent { translated: Arc::default(), entered, gate };
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
  watcher.watch(dir.path()).unwrap();

  // Nothing will ever hint at this file: only the scan after the overflow finds it.
  let path = dir.path().join("a.txt");
  fs::write(&path, "1").unwrap();
  // One notification that is bigger than the whole queue: it is dropped.
  sink.raw(vec![0; (16 << 20) + 1]);
  let batch = results.recv_timeout(PATIENCE).expect("the thread wakes up and scans");
  assert_eq!(batch.unwrap(), [Event { path, kind: EventKind::Create }]);
}

/// The delete delay is a setting: with zero, a file that is removed and written again is a
/// `Delete` and a `Create`, not one `Update`.
#[test]
fn the_delete_delay_can_be_zero() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("a.txt");
  fs::write(&path, "1").unwrap();
  let config = quick_config().with_delete_delay(Duration::ZERO);
  let (sender, results) = mpsc::channel();
  let watcher = Watcher::with_config(config, move |result| {
    let _ = sender.send(result);
  })
  .unwrap();
  watcher.watch(&path).unwrap();

  fs::remove_file(&path).unwrap();
  let batch = results.recv_timeout(PATIENCE).expect("the delete is reported");
  assert_eq!(batch.unwrap(), [Event { path: path.clone(), kind: EventKind::Delete }]);
  fs::write(&path, "2").unwrap();
  let batch = results.recv_timeout(PATIENCE).expect("the file is reported again");
  assert_eq!(batch.unwrap(), [Event { path: path.clone(), kind: EventKind::Create }]);
}

/// `Duration::MAX` is a way to say "never": the thread must not die of a time that no clock
/// has.
#[test]
fn a_time_too_long_for_a_clock() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("a.txt");

  // The poll flushes by itself, whatever the quiet time is.
  let config = quick_config().with_debounce(Duration::MAX);
  let (sender, results) = mpsc::channel();
  let watcher = Watcher::with_config(config, move |result| {
    let _ = sender.send(result);
  })
  .unwrap();
  watcher.watch(&path).unwrap();
  fs::write(&path, "1").unwrap();
  let batch = results.recv_timeout(PATIENCE).expect("the file is reported");
  assert_eq!(batch.unwrap(), [Event { path: path.clone(), kind: EventKind::Create }]);

  // Nothing is ever due here. The thread still takes every call.
  for config in [
    Config::default().with_debounce(Duration::MAX),
    Config::default().with_delete_delay(Duration::MAX),
    Config::default().with_polling(true).with_poll_interval(Duration::MAX),
  ] {
    let watcher = Watcher::with_config(config, |_| {}).unwrap();
    watcher.watch(dir.path()).unwrap();
    fs::write(&path, "22").unwrap();
    watcher.watch(dir.path().join("later")).unwrap();
    watcher.unwatch(dir.path()).unwrap();
  }
}

/// Windows finds no entry under such a name as it is given, so the path would never get an event.
#[cfg(windows)]
#[test]
fn a_name_that_windows_does_not_take_as_it_is() {
  let mut w = watch();
  for name in ["a.", "a ", "a.txt:stream", "*.js", "a?", r"dir.\a.txt"] {
    let path = w.path(name);
    assert_eq!(invalid(w.watcher.watch(&path)), path);
    assert_eq!(invalid(w.watcher.unwatch(&path)), path);
  }
  w.watcher.watch(w.path(".a b.txt")).unwrap();
  fs::write(w.path(".a b.txt"), "1").unwrap();
  w.expect(EventKind::Create, ".a b.txt");
}
