use std::{convert::Infallible, io, path::Path};

use crate::backend::{Backend, Coverage, DirKey, Hint, Sink, Want};

#[derive(Debug)]
pub struct PollBackend;

impl PollBackend {
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

  fn commit(&mut self) -> io::Result<Vec<DirKey>> {
    Ok(Vec::new())
  }

  fn translate(&mut self, raw: Self::Raw, _hint: &mut dyn FnMut(Hint<'_>)) {
    match raw {}
  }
}
