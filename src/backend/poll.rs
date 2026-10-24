use std::{convert::Infallible, path::Path};

use crate::backend::{Backend, Coverage, Hint, NodeKey, Want};

#[derive(Debug)]
pub struct PollBackend;

impl Backend for PollBackend {
  type Raw = Infallible;

  fn watch(&mut self, _key: NodeKey, _dir: &Path, _want: Want) -> Coverage {
    Coverage::NotWatched
  }

  fn unwatch(&mut self, _key: NodeKey) {}

  fn translate(&mut self, _raw: Infallible, _hint: &mut dyn FnMut(Hint<'_>)) {}
}
