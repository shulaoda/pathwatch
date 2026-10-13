//! The engine after its backend stopped: it polls as often as a watcher that only polls.

use std::{
  fs, io,
  time::{Duration, Instant},
};

use pathwatch::{
  ErrorKind, EventKind,
  backend::{Signal, Sink},
  engine::{Engine, Options, Output},
  fs::StdFs,
  poll::PollBackend,
};

#[test]
fn a_stopped_backend_is_polled_at_the_interval_of_the_settings() {
  let dir = tempfile::tempdir().unwrap();
  let hour = Duration::from_secs(3600);
  let options = Options {
    poll_interval: hour,
    poll_interval_alone: Some(Duration::from_millis(100)),
    loose_spelling: cfg!(any(target_vendor = "apple", windows)),
    ..Options::default()
  };
  let backend = PollBackend::new(Sink::new(|_| {})).unwrap();
  let mut engine = Engine::new(backend, StdFs, options);
  let (start, mut out) = (Instant::now(), Output::default());
  engine.watch(vec![dir.path().to_owned()], start, &mut out);
  let first_poll = engine.deadline().expect("only the poll is due, in an hour");
  assert!(first_poll >= start + hour && first_poll < start + hour + Duration::from_secs(1));

  engine.signal(Signal::Failed(io::Error::other("gone")), start);
  fs::write(dir.path().join("a.txt"), "1").unwrap();
  // No backend tells of the file: only a poll finds it.
  while out.events.is_empty() {
    let now = engine.deadline().expect("the poll is always due");
    assert!(now < start + Duration::from_secs(1), "no poll came in a second");
    engine.tick(now, &mut out);
  }
  assert_eq!(out.errors.len(), 1);
  assert_eq!(out.errors[0].kind(), ErrorKind::Io);
  assert_eq!(out.events.len(), 1);
  assert_eq!(
    (out.events[0].kind, &out.events[0].path),
    (EventKind::Create, &dir.path().join("a.txt"))
  );
}
