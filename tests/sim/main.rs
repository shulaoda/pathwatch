//! The engine on the simulated disk of `fixtures/`: what the real disk cannot play on demand.
//! A notification that comes late, twice or not at all, a disk that changes in the middle of a
//! call, a clock that only the test moves, and what the engine asks of its backend. A story that
//! needs none of that is in `tests/stories`, and runs here and on the real disk.
//!
//! The paths are Unix paths, so nothing here runs on another host.

#![cfg(unix)]

mod counting;
mod fixtures;

mod allocations;
mod baseline;
mod check_table;
mod cost;
mod delete_delay;
mod failures;
mod filter;
mod fuzz;
mod links;
mod moved_directories;
mod regressions;
mod sim;
mod spelling;
mod timers;
mod unwatch;
mod want;
mod young;
