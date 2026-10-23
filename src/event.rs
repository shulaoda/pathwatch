//! The events that the handler gets.

use std::path::PathBuf;

/// One change of one file. A batch has at most one event for a path.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Event {
  /// The watched path as given to `watch`, or a path below it. It is absolute.
  pub path: PathBuf,
  /// What happened to the file.
  pub kind: EventKind,
}

/// What happened to a file. Only files are reported, never a directory.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum EventKind {
  /// A file is there that the handler was not told about.
  Create,
  /// The file may have other content: its modification time, its size, or the file itself is
  /// another one.
  Update,
  /// The file that the handler was told about is gone.
  Delete,
}
