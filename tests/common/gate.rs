//! A gate in the sink of a backend, for the tests of the inotify and the Windows backend.

use std::{
  sync::{
    Condvar, Mutex,
    atomic::{AtomicUsize, Ordering},
  },
  thread,
  time::{Duration, Instant},
};

/// Only a failing test waits this long.
const TIMEOUT: Duration = Duration::from_secs(10);

/// While it is shut, the thread of the backend that sends stops inside the sink.
#[derive(Default)]
pub struct Gate {
  shut: Mutex<bool>,
  moved: Condvar,
  /// How many sends stop at the gate now.
  waiting: AtomicUsize,
}

/// Opens the gate again when dropped, also when the test fails: a shut gate blocks the drop.
pub struct Shut<'a>(&'a Gate);

impl Gate {
  pub fn shut(&self) -> Shut<'_> {
    *self.shut.lock().unwrap() = true;
    Shut(self)
  }

  pub fn pass(&self) {
    let shut = self.shut.lock().unwrap();
    self.waiting.fetch_add(1, Ordering::SeqCst);
    drop(self.moved.wait_while(shut, |shut| *shut).unwrap());
    self.waiting.fetch_sub(1, Ordering::SeqCst);
  }

  /// Returns once a send stops at the shut gate.
  pub fn until_one_waits(&self) {
    let deadline = Instant::now() + TIMEOUT;
    while self.waiting.load(Ordering::SeqCst) == 0 {
      assert!(Instant::now() < deadline, "nothing came to the gate");
      thread::sleep(Duration::from_millis(1));
    }
  }
}

impl Drop for Shut<'_> {
  fn drop(&mut self) {
    *self.0.shut.lock().unwrap() = false;
    self.0.moved.notify_all();
  }
}
