use std::{fmt, path::Path, sync::Arc, time::Duration};

use crate::engine::{Ignored, Options};

const LONGEST_SETTING: Duration = Duration::from_hours(365 * 24);
const FALLBACK_POLL_INTERVAL: Duration = Duration::from_secs(1);
const SHORTEST_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// What the ignore filter is asked about.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum EntryKind {
  /// Anything that is not a directory: a symbolic link too.
  File,
  /// A directory.
  Dir,
}

/// The settings of a [`Watcher`](crate::Watcher). They cannot be changed after it is made.
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
  /// The quiet time: a batch goes out when no change was noticed for this long. While changes
  /// keep coming, it goes out four quiet times after the first of them, and 500 ms after it at
  /// most, unless the quiet time is longer. Default: none on macOS, 10 ms elsewhere. The most is
  /// a year.
  #[must_use]
  pub fn with_debounce(mut self, debounce: Duration) -> Self {
    self.debounce = debounce.min(LONGEST_SETTING);
    self
  }

  /// How long a `Delete` waits for the path to come back: a file that is removed and written
  /// again within this time is one `Update`, and a real delete is reported this long after the
  /// path went, or after the quiet time if that is longer. Default: 10 ms; none when the watcher
  /// polls everything. Zero: no waiting. The most is a year.
  #[must_use]
  pub fn with_delete_delay(mut self, delay: Duration) -> Self {
    self.delete_delay = delay.min(LONGEST_SETTING);
    self
  }

  /// A path for which `ignored` returns `true` is never reported, and an ignored directory is
  /// never read or watched. Default: nothing is ignored.
  ///
  /// The path is absolute. The filter must give the same answer for the same path and kind, and
  /// should ignore everything below a directory that it ignores. It decides about the paths
  /// given to `watch` too (README, promise 10).
  #[must_use]
  pub fn with_ignored(
    mut self,
    ignored: impl Fn(&Path, EntryKind) -> bool + Send + Sync + 'static,
  ) -> Self {
    self.ignored = Some(Arc::new(ignored));
    self
  }

  /// Reads the disk on a timer in place of the watch API of the OS, for network drives and other
  /// places where the OS reports no changes. Default: `false`.
  #[must_use]
  pub fn with_polling(mut self, polling: bool) -> Self {
    self.polling = polling;
    self
  }

  /// Whether the watcher polls everything ([`with_polling`](Self::with_polling)).
  pub fn polling(&self) -> bool {
    self.polling
  }

  /// The pause between two rounds of polling when the watcher polls everything, and at least as
  /// long as the round took: polling takes half of a core at most. Default: 100 ms; at least
  /// 1 ms, at most a year.
  #[must_use]
  pub fn with_poll_interval(mut self, interval: Duration) -> Self {
    self.poll_interval = interval.clamp(SHORTEST_POLL_INTERVAL, LONGEST_SETTING);
    self
  }

  /// The settings of the engine. `polling`: the watcher polls everything.
  pub(crate) fn into_options(self, polling: bool) -> Options {
    Options {
      ignored: self.ignored,
      debounce: self.debounce,
      // The poll looks only at its interval: it gives no hint for the quiet time to wait for,
      // and no second look holds a `Delete` (`with_delete_delay`).
      delete_delay: if polling { Duration::ZERO } else { self.delete_delay },
      poll_interval: if polling { self.poll_interval } else { FALLBACK_POLL_INTERVAL },
      // macOS and Windows: a disk takes two spellings of a name for one.
      loose_spelling: cfg!(any(target_vendor = "apple", windows)),
      // The poll lists a directory again by itself.
      two_step_links: cfg!(windows) && !polling,
      // inotify and ReadDirectoryChangesW report every write; FSEvents and the poll do not.
      written_hints: cfg!(any(target_os = "linux", target_os = "android", windows)) && !polling,
    }
  }
}
