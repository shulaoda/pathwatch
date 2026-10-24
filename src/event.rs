use std::path::PathBuf;

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Event {
  pub path: PathBuf,
  pub kind: EventKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum EventKind {
  Create,
  Update,
  Delete,
}
