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
//! # Events
//!
//! - Only files are reported. A directory that appears is one `Create` for each file below it,
//!   and one that goes away is one `Delete` for each file that was known below it. A rename is
//!   a `Delete` for the old path and a `Create` for the new one.
//! - An event is the difference between what the handler was told and the disk. So the last
//!   event for a path tells its state: after `Create` or `Update` the file is there, after
//!   `Delete` it is not. There can be more `Update` events than writes.
//! - Changes are collected until the disk is quiet for a short time
//!   ([`Config::with_debounce`]), and then handed over as one batch. A batch is never empty and
//!   has at most one event for a path. Its order is not promised.
//! - A file that is removed and written again within 100 ms is one `Update`, the way editors
//!   save. So a `Delete` comes 100 ms late.
//! - Event paths are absolute, spelled as the watched path was given. No link is resolved.
//! - An error goes to the handler and the watcher keeps running.
//!
//! # A watched path
//!
//! - It can be a file, a directory (every file below it, at any depth), or a path that does
//!   not exist yet.
//! - The watch belongs to the path: it keeps working when the file is replaced, when a missing
//!   path appears, when a file becomes a directory, and when a directory above it is renamed
//!   away and comes back.
//! - [`Watcher::watch`] returns when the path is watched. What exists then is not reported;
//!   every later change is. [`Watcher::unwatch`] takes the watch back and reports nothing.
//! - A watched path that is a symbolic link is followed; a link inside a watched directory is
//!   a file.
//! - The ignore filter ([`Config::with_ignored`]) cuts parts out of a watched directory.
//!
//! # Platforms
//!
//! - Linux and Android: inotify, with one kernel watch for each directory
//!   (`fs.inotify.max_user_watches`). **Android was never built.**
//! - macOS: `FSEvents`. A file that its writer keeps open is reported when it is closed.
//! - Windows: `ReadDirectoryChangesW`. **This backend has not run on Windows yet.**
//! - Every other target: polling.
//!
//! The watcher polls the directories that the OS cannot watch for it, and everything when the
//! watch API cannot start. [`Config::with_polling`] turns polling on for everything.
//!
//! # Left out on purpose
//!
//! Events for directories, rename pairs, a report of what exists at the time of `watch`,
//! globs, a depth option, an option to follow symbolic links inside a watched directory, a
//! `since` time for `watch`, and an option for the 100 ms that a `Delete` waits.
//!
//! The README of the repository has the rest: every promise, what is not reported or reported
//! late ("Holes", "Timing"), and a table for users of notify, chokidar and @parcel/watcher.
//!
//! The modules that these docs do not show (the engine and the backends) are public only for
//! the tests of this repository, and they can change in any release.

// The API.
mod config;
mod error;
mod event;
mod watcher;

// The engine and what it stands on.
#[doc(hidden)]
pub mod backend;
#[doc(hidden)]
pub mod engine;
#[doc(hidden)]
pub mod fs;
#[doc(hidden)]
pub mod path;
mod tree;

// The backends, one file each. `windows` is compiled on every target: only the part of it that
// calls Windows is behind `cfg(windows)`, and the rest is tested on the other systems.
#[cfg(target_os = "macos")]
#[doc(hidden)]
pub mod fsevents;
#[cfg(any(target_os = "linux", target_os = "android"))]
#[doc(hidden)]
pub mod inotify;
#[doc(hidden)]
pub mod placement;
#[doc(hidden)]
pub mod poll;
#[doc(hidden)]
pub mod windows;

pub use crate::{
  config::{Config, EntryKind},
  error::{Error, ErrorKind},
  event::{Event, EventKind},
  watcher::Watcher,
};
