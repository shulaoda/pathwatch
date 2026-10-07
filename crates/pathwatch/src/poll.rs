//! The poll backend (every target).

use std::{convert::Infallible, io, path::Path};

use crate::backend::{Backend, Coverage, DirKey, Hint, Sink, Want};

/// The backend that watches nothing. Every directory is answered `NotWatched`, so the engine
/// polls all of them.
#[derive(Debug)]
pub struct PollBackend;

impl PollBackend {
  /// It has no thread and never uses the sink.
  pub fn new(_sink: Sink<Infallible>) -> io::Result<Self> {
    Ok(Self)
  }
}

impl Backend for PollBackend {
  type Raw = Infallible;

  fn watch(&mut self, _key: DirKey, _dir: &Path, _want: Want) -> Coverage {
    Coverage::NotWatched
  }

  fn unwatch(&mut self, _key: DirKey) {}

  fn commit(&mut self) -> io::Result<()> {
    Ok(())
  }

  fn translate(&mut self, raw: Self::Raw, _hint: &mut dyn FnMut(Hint<'_>)) {
    match raw {}
  }
}
