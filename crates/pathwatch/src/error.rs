//! The errors that a watcher reports.

use std::{
  error, fmt, io,
  path::{Path, PathBuf},
};

/// What [`ErrorKind::WatchLimit`] means on this OS, for the text of the error.
const WATCH_LIMIT: &str = if cfg!(any(target_os = "linux", target_os = "android")) {
  "raise fs.inotify.max_user_watches or fs.inotify.max_user_instances"
} else if cfg!(target_os = "macos") {
  "the limit of FSEvents streams of one process is reached"
} else if cfg!(windows) {
  "the limit of handles of this watcher is reached"
} else {
  "a limit of the OS is reached"
};

/// An error of the watcher.
///
/// Its [`source`](std::error::Error::source) is the error of the OS, when there is one.
#[derive(Debug)]
pub struct Error {
  kind: ErrorKind,
  path: Option<PathBuf>,
  source: Option<io::Error>,
}

impl Error {
  pub(crate) fn new(kind: ErrorKind, path: Option<PathBuf>, source: Option<io::Error>) -> Self {
    Self { kind, path, source }
  }

  /// What went wrong.
  pub fn kind(&self) -> ErrorKind {
    self.kind
  }

  /// One path that shows the problem. More paths can have it: the handler gets one error of
  /// a kind for a batch, not one for each path.
  pub fn path(&self) -> Option<&Path> {
    self.path.as_deref()
  }
}

impl fmt::Display for Error {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self.kind {
      ErrorKind::InvalidPath => f.write_str("the path cannot be watched"),
      ErrorKind::WatchLimit => write!(
        f,
        "the OS has no room for more watches ({WATCH_LIMIT}); the directory is read on a timer \
         until then"
      ),
      ErrorKind::PermissionDenied => f.write_str("permission denied"),
      ErrorKind::Io => f.write_str("input or output error"),
      ErrorKind::Closed => f.write_str("the watcher is closed"),
    }?;
    if let Some(path) = &self.path {
      write!(f, ": {}", path.display())?;
    }
    if let Some(source) = &self.source {
      write!(f, ": {source}")?;
    }
    Ok(())
  }
}

impl error::Error for Error {
  fn source(&self) -> Option<&(dyn error::Error + 'static)> {
    self.source.as_ref().map(|source| source as &(dyn error::Error + 'static))
  }
}

/// What went wrong.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[non_exhaustive]
pub enum ErrorKind {
  /// A path given to `watch` or `unwatch` cannot be used: it is empty, has a NUL byte, goes
  /// above the root with `..`, or is a file system root (`/`, `C:\`, `\\server\share`). On
  /// Windows also a path of a device, and `C:name`.
  InvalidPath,
  /// The OS has no room for more watches. The watcher keeps running: what has no watch is
  /// read on a timer until there is room.
  ///
  /// - Linux: the settings `fs.inotify.max_user_watches` and `fs.inotify.max_user_instances`.
  /// - macOS: the limit of `FSEvents` streams of one process.
  /// - Windows: the limit of handles of this watcher.
  WatchLimit,
  /// A path cannot be read. For events it counts as missing.
  PermissionDenied,
  /// Another error of the OS.
  Io,
  /// The watcher is closed, or its thread is gone because the handler panicked.
  Closed,
}
