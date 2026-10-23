//! The error that the handler and the calls of the watcher get, and its kinds.

use std::{
  error, fmt, io,
  path::{Path, PathBuf},
};

/// What went wrong. The handler gets one error of a kind for a batch, with one path that shows
/// the problem, and the watcher keeps running after every error.
///
/// Its text is the kind, what happened to the watcher when the error is about it, the path and
/// the source in a row, each once. [`source`] gives the source once more, for code that looks at
/// the error of the OS: a report that prints the text and then every source would say the last
/// part twice.
///
/// [`source`]: std::error::Error::source
#[derive(Debug)]
pub struct Error {
  kind: ErrorKind,
  what: Option<&'static str>,
  path: Option<PathBuf>,
  source: Option<io::Error>,
}

impl Error {
  pub(crate) fn new(kind: ErrorKind, path: Option<PathBuf>, source: Option<io::Error>) -> Self {
    Self { kind, what: None, path, source }
  }

  /// An `Io` error about the watcher itself: `what` says what happened to it.
  pub(crate) fn of_watcher(what: &'static str, source: io::Error) -> Self {
    Self { kind: ErrorKind::Io, what: Some(what), path: None, source: Some(source) }
  }

  /// Which kind of error it is.
  pub fn kind(&self) -> ErrorKind {
    self.kind
  }

  /// The path that shows the problem, if the error is about one.
  pub fn path(&self) -> Option<&Path> {
    self.path.as_deref()
  }
}

impl fmt::Display for Error {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(match self.kind {
      ErrorKind::InvalidPath => "the path cannot be watched",
      ErrorKind::WatchLimit => "the OS has no room for one more watch, polling instead",
      ErrorKind::PermissionDenied => "permission denied",
      ErrorKind::RemoteDisk => "the OS does not report what other machines change here",
      ErrorKind::Io => "input or output error",
      ErrorKind::Closed => "the watcher is closed",
    })?;
    if let Some(what) = self.what {
      write!(f, ": {what}")?;
    }
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
    Some(self.source.as_ref()?)
  }
}

/// The kinds of [`Error`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ErrorKind {
  /// The path cannot be used: it is empty, has a NUL in it, names the root of a file system, or
  /// has a name that is too long. On Windows also: a name in it ends with a dot or a space, or
  /// has a `:`, a control character or one of `< > " | ? *`. Only `watch` and `unwatch` return
  /// it.
  InvalidPath,
  /// The OS has no room for one more watch. What could not be watched is polled instead. A
  /// watcher says it once.
  WatchLimit,
  /// A path cannot be read or watched for want of permission. For events it counts as missing.
  PermissionDenied,
  /// The path is on a disk that other machines can change too: NFS, SMB, a folder of the host
  /// in a virtual machine or a container. The OS reports only what changes on this machine, and
  /// nothing more is polled: `Config::with_polling(true)` finds the rest. A watcher says it once
  /// for each disk.
  RemoteDisk,
  /// Any other error of the disk or of the watch API of the OS. Also the word that the watch
  /// API could not start, or stopped, and that every path is polled from now on.
  Io,
  /// The watcher is closed. Only `watch` and `unwatch` return it.
  Closed,
}
