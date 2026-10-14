use std::{fmt, path::Path, sync::Arc, time::Duration};

use crate::engine::{Ignored, Options};

/// With a native backend: how often the watcher polls what the OS does not watch.
const FALLBACK_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// With a pause of zero the watcher thread would never sleep.
const SHORTEST_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// The settings of a [`Watcher`](crate::Watcher). They cannot be changed after it is made.
#[derive(Clone)]
pub struct Config {
  ignored: Option<Ignored>,
  debounce: Duration,
  delete_delay: Duration,
  polling: bool,
  poll_interval: Duration,
}

/// What the ignore filter is asked about.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum EntryKind {
  /// Anything that is not a directory: a symbolic link too.
  File,
  /// A directory.
  Dir,
}

impl Default for Config {
  fn default() -> Self {
    Self {
      ignored: None,
      debounce: Duration::from_millis(20),
      delete_delay: Duration::from_millis(100),
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
  /// The quiet time: a batch goes out when no change was noticed for this long. Default: 20 ms:
  /// long enough for the steps of one save (a temporary file, a rename) to be one batch, also
  /// on a busy machine, and short enough not to be felt.
  ///
  /// While changes keep coming, a batch still goes out every 500 ms, or every four quiet times
  /// if that is longer. The most is a year.
  #[must_use]
  pub fn with_debounce(mut self, debounce: Duration) -> Self {
    self.debounce = debounce;
    self
  }

  /// How long a `Delete` waits for the path to come back. Default: 100 ms.
  ///
  /// A file that is removed and written again within this time is one `Update`, as the tools
  /// that replace a file that way expect. A real delete is reported this long after the OS
  /// said that the path is gone. Zero: a `Delete` comes at the first look that finds the path
  /// gone, and such a file is a `Delete` and a `Create`. The most is a year.
  #[must_use]
  pub fn with_delete_delay(mut self, delay: Duration) -> Self {
    self.delete_delay = delay;
    self
  }

  /// A path for which `ignored` returns `true` is never reported, and an ignored directory is
  /// never read or watched. Default: nothing is ignored.
  ///
  /// The path is absolute. The filter must give the same answer for the same path and kind,
  /// and when it ignores a directory, it should ignore everything below it. It decides about
  /// the paths given to `watch` too: one below a directory that it ignores, or ignored as a
  /// file and as a directory, is not watched at all, and one ignored for the kind that is
  /// there counts as missing (README, promise 10).
  #[must_use]
  pub fn with_ignored(
    mut self,
    ignored: impl Fn(&Path, EntryKind) -> bool + Send + Sync + 'static,
  ) -> Self {
    self.ignored = Some(Arc::new(ignored));
    self
  }

  /// Reads the disk on a timer and does not use the watch API of the OS. Default: `false`.
  ///
  /// For network drives and other places where the OS reports no changes.
  #[must_use]
  pub fn with_polling(mut self, polling: bool) -> Self {
    self.polling = polling;
    self
  }

  /// The pause between two rounds of polling, when the watcher polls everything. Default:
  /// 100 ms. It is never below 1 ms and never above a year.
  ///
  /// A round that takes longer than the pause is followed by a pause as long as itself: polling
  /// takes half of a core at most. A round looks at every watched file once (20,000 files: about
  /// 50 ms on APFS), so with many files a longer pause saves the CPU.
  #[must_use]
  pub fn with_poll_interval(mut self, interval: Duration) -> Self {
    self.poll_interval = interval.max(SHORTEST_POLL_INTERVAL);
    self
  }

  pub(crate) fn polling(&self) -> bool {
    self.polling
  }

  /// The settings of the engine. `polling`: the watcher polls everything.
  pub(crate) fn into_options(self, polling: bool) -> Options {
    Options {
      ignored: self.ignored,
      debounce: self.debounce,
      delete_delay: self.delete_delay,
      poll_interval: if polling { self.poll_interval } else { FALLBACK_POLL_INTERVAL },
      poll_interval_alone: Some(self.poll_interval),
      // macOS and Windows: a disk takes two spellings of a name for one.
      loose_spelling: cfg!(any(target_vendor = "apple", windows)),
      // The poll lists a directory again by itself.
      two_step_links: cfg!(windows) && !polling,
    }
  }
}
