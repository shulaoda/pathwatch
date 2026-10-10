//! Watch paths and get file events: create, update, delete.
//!
//! ```no_run
//! use std::sync::mpsc;
//!
//! use pathwatch::Watcher;
//!
//! # fn main() -> Result<(), pathwatch::Error> {
//! // The handler runs on the watcher thread: it only passes the batch on.
//! let (sender, batches) = mpsc::channel();
//! let watcher = Watcher::new(move |result| {
//!   let _ = sender.send(result);
//! })?;
//! // A directory, a file, and a path that does not exist yet.
//! watcher.watch_all(["/project/src", "/project/package.json", "/project/not-there-yet.json"])?;
//!
//! for result in batches {
//!   match result {
//!     Ok(events) => {
//!       for event in events {
//!         println!("{:?} {}", event.kind, event.path.display());
//!       }
//!     }
//!     Err(error) => eprintln!("{error}"),
//!   }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! The README of the repository has every promise, and what is reported late or not at all.

mod config;
mod error;
mod event;
mod watcher;

#[doc(hidden)]
pub mod backend;
#[doc(hidden)]
pub mod engine;
#[doc(hidden)]
pub mod fs;
#[doc(hidden)]
pub mod path;
#[doc(hidden)]
pub mod tree;

#[cfg(target_os = "macos")]
#[doc(hidden)]
pub mod fsevents;
#[cfg(any(target_os = "linux", target_os = "android"))]
#[doc(hidden)]
pub mod inotify;
#[doc(hidden)]
pub mod poll;
#[cfg(target_os = "windows")]
#[doc(hidden)]
pub mod windows;

pub use crate::{
  config::{Config, EntryKind},
  error::{Error, ErrorKind},
  event::{Event, EventKind},
  watcher::Watcher,
};
