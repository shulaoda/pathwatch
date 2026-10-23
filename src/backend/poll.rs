//! The backend that watches nothing: the engine polls every directory.

use std::{convert::Infallible, path::Path};

use crate::backend::{Backend, Coverage, DirKey, Hint, Want};

/// Watches nothing.
#[derive(Debug)]
pub struct PollBackend;

impl Backend for PollBackend {
  type Raw = Infallible;

  fn watch(&mut self, _key: DirKey, _dir: &Path, _want: Want) -> Coverage {
    Coverage::NotWatched
  }

  fn unwatch(&mut self, _key: DirKey) {}

  fn translate(&mut self, _raw: Infallible, _hint: &mut dyn FnMut(Hint<'_>)) {}
}
