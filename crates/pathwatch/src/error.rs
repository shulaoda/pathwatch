use std::{
  error, fmt, io,
  path::{Path, PathBuf},
};

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

  pub fn kind(&self) -> ErrorKind {
    self.kind
  }

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

pub(crate) fn context(what: &str, error: &io::Error) -> io::Error {
  io::Error::new(error.kind(), format!("{what}: {error}"))
}

impl error::Error for Error {
  fn source(&self) -> Option<&(dyn error::Error + 'static)> {
    self.source.as_ref().map(|source| source as &(dyn error::Error + 'static))
  }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ErrorKind {
  InvalidPath,
  WatchLimit,
  PermissionDenied,
  Io,
  Closed,
}
