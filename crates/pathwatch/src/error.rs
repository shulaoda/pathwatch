use std::{
  error, fmt, io,
  path::{Path, PathBuf},
};

/// What went wrong. The handler gets one error of a kind for a batch, with one path that shows
/// the problem, and the watcher keeps running after every error.
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
      ErrorKind::Io => "input or output error",
      ErrorKind::Closed => "the watcher is closed",
    })?;
    if let Some(path) = &self.path {
      write!(f, ": {}", path.display())?;
    }
    if let Some(source) = &self.source {
      write!(f, ": {source}")?;
    }
    Ok(())
  }
}

/// `what: error`, with `error` kept as the source: its OS error code stays readable.
pub(crate) fn context(what: &str, error: io::Error) -> io::Error {
  io::Error::new(error.kind(), Context { what: what.to_owned(), error })
}

#[derive(Debug)]
struct Context {
  what: String,
  error: io::Error,
}

impl fmt::Display for Context {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}: {}", self.what, self.error)
  }
}

impl error::Error for Context {
  fn source(&self) -> Option<&(dyn error::Error + 'static)> {
    Some(&self.error)
  }
}

impl error::Error for Error {
  fn source(&self) -> Option<&(dyn error::Error + 'static)> {
    self.source.as_ref().map(|source| source as &(dyn error::Error + 'static))
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
  /// The OS has no room for one more watch. What could not be watched is polled instead.
  WatchLimit,
  /// A path cannot be read or watched for want of permission. For events it counts as missing.
  PermissionDenied,
  /// Any other error of the disk or of the watch API of the OS. Also the word that the watch
  /// API could not start, or stopped, and that every path is polled from now on.
  Io,
  /// The watcher is closed. Only `watch` and `unwatch` return it.
  Closed,
}
