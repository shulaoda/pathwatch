use std::{fmt, path::Path, sync::Arc, time::Duration};

use crate::engine::{Ignored, Options};

const LONGEST_SETTING: Duration = Duration::from_hours(365 * 24);
const FALLBACK_POLL_INTERVAL: Duration = Duration::from_secs(1);
const SHORTEST_POLL_INTERVAL: Duration = Duration::from_millis(1);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum EntryKind {
  File,
  Dir,
}

#[derive(Clone)]
pub struct Config {
  ignored: Option<Ignored>,
  debounce: Duration,
  delete_delay: Duration,
  polling: bool,
  poll_interval: Duration,
}

impl Default for Config {
  fn default() -> Self {
    Self {
      ignored: None,
      debounce: Duration::from_millis(if cfg!(target_os = "macos") { 0 } else { 10 }),
      delete_delay: Duration::from_millis(10),
      polling: false,
      poll_interval: Duration::from_millis(100),
    }
  }
}

impl fmt::Debug for Config {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Config")
      .field("ignored", &self.ignored.is_some())
      .field("debounce", &self.debounce)
      .field("delete_delay", &self.delete_delay)
      .field("polling", &self.polling)
      .field("poll_interval", &self.poll_interval)
      .finish()
  }
}

impl Config {
  #[must_use]
  pub fn with_debounce(mut self, debounce: Duration) -> Self {
    self.debounce = debounce.min(LONGEST_SETTING);
    self
  }

  #[must_use]
  pub fn with_delete_delay(mut self, delay: Duration) -> Self {
    self.delete_delay = delay.min(LONGEST_SETTING);
    self
  }

  #[must_use]
  pub fn with_ignored(
    mut self,
    ignored: impl Fn(&Path, EntryKind) -> bool + Send + Sync + 'static,
  ) -> Self {
    self.ignored = Some(Arc::new(ignored));
    self
  }

  #[must_use]
  pub fn with_polling(mut self, polling: bool) -> Self {
    self.polling = polling;
    self
  }

  pub fn polling(&self) -> bool {
    self.polling
  }

  #[must_use]
  pub fn with_poll_interval(mut self, interval: Duration) -> Self {
    self.poll_interval = interval.clamp(SHORTEST_POLL_INTERVAL, LONGEST_SETTING);
    self
  }

  pub(crate) fn into_options(self, polling: bool) -> Options {
    Options {
      ignored: self.ignored,
      debounce: self.debounce,
      delete_delay: if polling { Duration::ZERO } else { self.delete_delay },
      poll_interval: if polling { self.poll_interval } else { FALLBACK_POLL_INTERVAL },
      loose_spelling: cfg!(any(target_vendor = "apple", windows)),
      two_step_links: cfg!(windows) && !polling,
      written_hints: cfg!(any(target_os = "linux", target_os = "android", windows)) && !polling,
    }
  }
}
