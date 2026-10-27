use std::{
  error, fmt, io,
  path::{Path, PathBuf},
};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ErrorKind {
  /// The path cannot be watched, for example an empty path or a file system root.
  InvalidPath,
  /// No room is left for one more watch, by a limit of the OS or one that pathwatch keeps. What
  /// got no watch is polled.
  WatchLimit,
  /// No permission to read or watch a path. Events treat the path as missing.
  PermissionDenied,
  /// The disk is shared with other machines (NFS, SMB, a VM or container folder), and the OS does
  /// not report their changes.
  RemoteDisk,
  /// Any other error of the disk or the OS.
  Io,
  /// The watcher is closed.
  Closed,
}

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

  pub(crate) fn of_watcher(what: &'static str, source: io::Error) -> Self {
    Self { kind: ErrorKind::Io, what: Some(what), path: None, source: Some(source) }
  }

  /// The kind of the error.
  pub fn kind(&self) -> ErrorKind {
    self.kind
  }

  /// The path the error is about, if any.
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
