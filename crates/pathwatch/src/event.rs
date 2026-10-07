//! The events that a watcher reports.

use std::path::PathBuf;

/// One change of one file. A directory is never reported.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Event {
  /// The absolute path of the file: the watched path in its clean form (see
  /// [`Watcher::watch`](crate::Watcher::watch)). For a file in a watched directory: that path
  /// and the rest below it. No link is resolved.
  pub path: PathBuf,
  pub kind: EventKind,
}

/// What happened to a file. After `Create` or `Update` the file exists. After `Delete` it
/// does not.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum EventKind {
  /// A file is there that was not there before.
  Create,
  /// The file may have new content. A file that was removed and written again in a short time
  /// (100 ms) is one `Update` too, the way editors save.
  Update,
  /// The file is gone.
  Delete,
}
