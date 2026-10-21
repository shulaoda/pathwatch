//! Fixtures for the tests of the `pathwatch` engine: a file system and a backend in memory.
//! This crate is not published. The tests that use it are in `tests/`.
//!
//! - [`SimFs`] is the disk. A test changes it with `write`, `mkdir`, `rename`, `remove` and so on,
//!   and moves its clock by hand. Inode numbers are used again as on ext4, modification times
//!   can be made coarse, links and permission errors exist, and the disk can ignore the case of
//!   letters ([`SimFs::set_ignore_case`]).
//! - [`SimBackend`] plays the OS watch API on top of it, in one of three [`Mode`]s. Whether a
//!   change of the disk makes a raw notification is decided at the moment of the change, from the
//!   watches that are live then. The notification goes into a queue inside the `SimFs`.
//! - Nothing delivers it. The test takes it out with [`SimFs::take_raw`] and hands it to
//!   `Engine::signal` when it wants: now, later, twice, in another order. Or it drops it, and
//!   calls `Engine::overflow` if the loss is to be known.
//! - [`Driver`] is the small harness for that. It also plays the handler and checks every batch.
//!   Each time it lets everything settle, it checks that the engine has caught up with the disk
//!   (`caught_up.rs`).
//! - What the engine asked from the backend is in [`SimFs::watch_calls`], and where an OS watch
//!   is now in [`SimFs::os_watches`].
//! - The disk can change in the middle of an engine call: [`SimFs::at_call`].
//! - Things that go wrong: [`SimFs::lose_raw`] (the OS drops notifications and says so),
//!   [`SimFs::set_offline`] (the disk is unplugged, or answers with errors),
//!   [`SimFs::set_denied`], [`SimFs::set_watch_limit`].
//!
//! # How to write a test
//!
//! ```text
//! let mut d = Driver::sim(Mode::Inode, Options::default());   // or Driver::poll(options)
//! d.fs.mkdir_all("/p/r")?;
//! d.watch(&["/p/r"]);               // Engine::watch at the time `d.now`
//! d.fs.write("/p/r/a", "1")?;       // one raw notification waits in the queue
//! d.deliver(usize::MAX);            // Engine::signal for each: marks, and a flush deadline
//! d.step();                         // move both clocks to Engine::deadline, then Engine::tick
//! d.take_events();                  // [Create /p/r/a]
//! d.settle();                       // deliver all and run every deadline until nothing is left
//! caught_up(&mut d);                // the end of the story: settle, and nothing is left to do
//! ```
//!
//! Every story ends with [`caught_up()`]. A story that stops on purpose where the engine cannot
//! know the state of the disk ends with [`not_caught_up`] and says why. A driver that is
//! dropped without either fails the test.
//!
//! The helpers at the end of `driver.rs` make this shorter: [`settled`] and [`taken`] give the
//! events as text (`"C /p/r/a"`), [`on_all`] runs one test body on every backend.
//!
//! A new test goes into the file in `tests/` that fits it:
//!
//! - `check_table`: one test for each row of the table of `check`.
//! - `baseline`, `young`, `delete_delay`, `unwatch`: one rule of the engine each.
//! - `want`: what the backend is asked for a directory, and where an OS watch is put.
//! - `filter`: the ignore filter and the watched paths, and what the filter costs.
//! - `spelling`: a watched path in another spelling than the disk has.
//! - `links`, `moved_directories`: links, and directories that are renamed or replaced.
//! - `failures`: lost notifications, no room for a watch, a disk that does not answer.
//! - `timers`: the quiet time, the poll, and the rest after a slow scan.
//! - `regressions`: the cases of the design review that fit no other file.
//! - `fuzz`: random runs against a second model of the rules. `cost`: the cost program.
//! - `allocations`: how often the engine asks for memory, as budgets that a test checks.
//! - `sim`: the fixtures themselves.
//!
//! Paths are Unix paths (`/a/b`), so this crate and its tests are empty on a host that is not
//! Unix.

mod backend;
mod caught_up;
mod driver;
mod fs;

pub use self::{
  backend::{Mode, RECURSIVE, SimBackend, SimRaw, WatchCall},
  caught_up::{can_be_watched, check_fingerprints, followed, reportable},
  driver::{
    Deliver, Driver, NONE, Sim, asked, at, caught_up, flush, ignore_node_modules, known,
    not_caught_up, on_a_small_stack, on_all, on_modes, os_watches, settled, show, sorted, taken,
    window, window_then_settled,
  },
  fs::SimFs,
};
