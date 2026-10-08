use std::{fmt, io, path::Path, sync::Arc};

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

pub fn watch_limit(message: &'static str) -> io::Error {
  io::Error::new(io::ErrorKind::StorageFull, message)
}

pub fn is_watch_limit(error: &io::Error) -> bool {
  error.kind() == io::ErrorKind::StorageFull
}
