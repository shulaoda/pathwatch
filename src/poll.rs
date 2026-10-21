//! The backend that watches nothing: the engine polls every directory.

use std::{convert::Infallible, fmt, io, marker::PhantomData, path::Path};

use crate::backend::{Backend, Coverage, DirKey, Hint, Sink, Want};

/// Watches nothing. `R` is the raw type of the queue of the watcher, which is made before the
/// backend: `Infallible`, or that of a native backend that did not start.
pub struct PollBackend<R = Infallible>(PhantomData<fn(R)>);

impl PollBackend {
  pub fn new(_sink: Sink<Infallible>) -> io::Result<Self> {
    Ok(Self::default())
  }
}

impl Default for PollBackend {
  fn default() -> Self {
    Self(PhantomData)
  }
}

impl<R> PollBackend<R> {
  /// For a queue that was made for the raw notifications of a backend that did not start.
  pub(crate) fn for_queue() -> Self {
    Self(PhantomData)
  }
}

impl<R> fmt::Debug for PollBackend<R> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str("PollBackend")
  }
}

impl<R: Send + 'static> Backend for PollBackend<R> {
  type Raw = R;

  fn watch(&mut self, _key: DirKey, _dir: &Path, _want: Want) -> Coverage {
    Coverage::NotWatched
  }

  fn unwatch(&mut self, _key: DirKey) {}

  fn translate(&mut self, _raw: R, _hint: &mut dyn FnMut(Hint<'_>)) {}
}
