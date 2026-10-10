use std::{error, fmt, io, path::Path, sync::Arc};

pub trait Backend: Send + 'static {
  type Raw: Send + 'static;

  fn watch(&mut self, key: DirKey, dir: &Path, want: Want) -> Coverage;

  fn unwatch(&mut self, key: DirKey);

  fn commit(&mut self) -> io::Result<Vec<DirKey>> {
    Ok(Vec::new())
  }

  fn translate(&mut self, raw: Self::Raw, hint: &mut dyn FnMut(Hint<'_>));

  fn raw_bytes(_raw: &Self::Raw) -> usize {
    0
  }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DirKey {
  index: u32,
  generation: u32,
}

impl DirKey {
  pub const fn new(index: u32, generation: u32) -> Self {
    Self { index, generation }
  }

  pub fn index(self) -> u32 {
    self.index
  }

  pub fn generation(self) -> u32 {
    self.generation
  }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Want {
  Names,
  Contents,
}

#[derive(Debug)]
pub enum Coverage {
  Watched,
  WatchedTree,
  NotWatched,
  Failed(io::Error),
}

#[derive(Clone, Copy, Debug)]
pub enum Hint<'a> {
  Changed { dir: DirKey, rel: &'a Path },
  Written { dir: DirKey, rel: &'a Path },
  Lost,
}

pub struct Sink<R> {
  inner: Arc<dyn Fn(Signal<R>) + Send + Sync>,
}

impl<R> Sink<R> {
  pub fn new(deliver: impl Fn(Signal<R>) + Send + Sync + 'static) -> Self {
    Self { inner: Arc::new(deliver) }
  }

  pub fn raw(&self, raw: R) {
    (self.inner)(Signal::Raw(raw));
  }

  pub fn failed(&self, error: io::Error) {
    (self.inner)(Signal::Failed(error));
  }
}

impl<R> Clone for Sink<R> {
  fn clone(&self) -> Self {
    Self { inner: Arc::clone(&self.inner) }
  }
}

impl<R> fmt::Debug for Sink<R> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Sink").finish_non_exhaustive()
  }
}

#[derive(Debug)]
pub enum Signal<R> {
  Raw(R),
  Failed(io::Error),
}

#[derive(Debug)]
struct Limit(&'static str);

impl fmt::Display for Limit {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.0)
  }
}

impl error::Error for Limit {}

pub fn watch_limit(message: &'static str) -> io::Error {
  io::Error::other(Limit(message))
}

pub fn is_watch_limit(error: &io::Error) -> bool {
  limit(error).is_some()
}

pub(crate) fn same(error: &io::Error) -> io::Error {
  match (limit(error), error.raw_os_error()) {
    (Some(message), _) => watch_limit(message),
    (None, Some(code)) => io::Error::from_raw_os_error(code),
    (None, None) => io::Error::new(error.kind(), error.to_string()),
  }
}

fn limit(error: &io::Error) -> Option<&'static str> {
  Some(error.get_ref()?.downcast_ref::<Limit>()?.0)
}
