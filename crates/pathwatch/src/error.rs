use std::{
  error, fmt, io,
  path::{Path, PathBuf},
};

const WATCH_LIMIT: &str = if cfg!(any(target_os = "linux", target_os = "android")) {
  "raise fs.inotify.max_user_watches or fs.inotify.max_user_instances"
} else if cfg!(target_os = "macos") {
  "the limit of FSEvents streams of one process is reached"
} else if cfg!(windows) {
  "the limit of handles of this watcher is reached"
} else {
  "a limit of the OS is reached"
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

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ErrorKind {
  InvalidPath,
  WatchLimit,
  PermissionDenied,
  Io,
  Closed,
}
