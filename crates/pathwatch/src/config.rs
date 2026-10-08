//! The settings of a watcher.

use std::{fmt, path::Path, sync::Arc, time::Duration};

use crate::engine::{Ignored, Options};

/// How often the watcher polls what the OS does not watch for it, when polling is off: the
/// directories above the watched ones, and directories whose watch failed.
const FALLBACK_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// With a pause of zero the watcher thread would never sleep.
const SHORTEST_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// The settings of a [`Watcher`](crate::Watcher). They cannot be changed after the watcher is
/// made.
///
/// ```
/// use std::time::Duration;
///
/// use pathwatch::Config;
///
/// // Everything at or below a directory named `node_modules`.
/// let config = Config::default()
///   .with_ignored(|path, _kind| path.components().any(|part| part.as_os_str() == "node_modules"))
///   .with_debounce(Duration::from_millis(20));
/// ```
#[derive(Clone)]
pub struct Config {
  ignored: Option<Ignored>,
  debounce: Duration,
  polling: bool,
  poll_interval: Duration,
}

impl Default for Config {
  fn default() -> Self {
    Self {
      ignored: None,
      debounce: Duration::from_millis(50),
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
      .field("polling", &self.polling)
      .field("poll_interval", &self.poll_interval)
      .finish()
  }
}

impl Config {
  /// How long the watcher waits for things to be quiet: the disk is checked, and a batch goes
  /// out, when no change was noticed for this long. Default: 50 ms. Zero: no waiting.
  ///
  /// While changes keep coming, a batch still goes out every 500 ms, or every four times
  /// `debounce` if that is longer.
  #[must_use]
  pub fn with_debounce(mut self, debounce: Duration) -> Self {
    self.debounce = debounce;
    self
  }

  /// A path for which `ignored` returns `true` is never reported, and an ignored directory is
  /// never read or watched. Default: nothing is ignored.
  ///
  /// The path is absolute, and the kind is always known: a file or a directory. The filter
  /// runs on the watcher thread. To change the rules, make a new watcher.
  ///
  /// # What the filter must do
  ///
  /// - Give the same answer for the same input. The watcher keeps an answer and may not ask
  ///   again.
  /// - If it ignores a directory, it must ignore everything below it. A filter that only looks
  ///   at the last name of the path (`path.ends_with("node_modules")`) does not, and its
  ///   results depend on which directories exist: see the last point below.
  ///
  /// # What is asked, and when
  ///
  /// - **An entry inside a watched directory:** once when it is found, and again when its kind
  ///   changes. Nothing below an ignored directory is asked. An ignored entry is asked again
  ///   each time its directory is read.
  /// - **A path that you watch:** `watch` asks about it as a file and as a directory, so the
  ///   kind can be one that is not on the disk. If both answers are "ignored", the path is not
  ///   watched at all: no events, no error, no cost. If only the kind that is there now is
  ///   ignored, the path counts as missing until its kind changes.
  /// - **The directory that serves a watched path:** the directory that holds a watched file,
  ///   or, when nothing is at the watched path, the lowest directory that exists above it. It
  ///   is asked, as a directory, before the watcher watches it for the path; the directories
  ///   further up are not asked. If it is ignored, the path is dropped: a file that was
  ///   reported for it gets its `Delete` (it is gone from the disk), then the path is forgotten
  ///   as `unwatch` of it would do it, and only `watch` of the path looks again.
  ///
  ///   A filter that keeps the second rule ignores such a path itself, so this point never
  ///   changes an event. With a filter that only looks at the last name, a watched file
  ///   directly in `node_modules` is dropped, and one in `node_modules/pkg` is dropped when
  ///   `pkg` does not exist at a time the watcher looks.
  #[must_use]
  pub fn with_ignored(
    mut self,
    ignored: impl Fn(&Path, EntryKind) -> bool + Send + Sync + 'static,
  ) -> Self {
    self.ignored = Some(Arc::new(ignored));
    self
  }

  /// Read the disk on a timer and do not use the watch API of the OS. Default: `false`.
  ///
  /// Use it for network drives and other places where the OS reports no changes. On a target
  /// without a watch API the watcher always polls.
  #[must_use]
  pub fn with_polling(mut self, polling: bool) -> Self {
    self.polling = polling;
    self
  }

  /// The shortest pause between two rounds of polling. Default: 100 ms. A pause below 1 ms is
  /// taken as 1 ms.
  ///
  /// The watcher spends about a tenth of a core on polling at most, so on a big set of paths
  /// the pause is longer. It only counts when the watcher polls: with
  /// [`with_polling`](Self::with_polling), on a target without a watch API, or when the watch
  /// API could not be started.
  #[must_use]
  pub fn with_poll_interval(mut self, interval: Duration) -> Self {
    self.poll_interval = interval.max(SHORTEST_POLL_INTERVAL);
    self
  }

  pub(crate) fn polling(&self) -> bool {
    self.polling
  }

  /// The settings of the engine, for a watcher that polls everything (`true`) or one that has
  /// a native backend.
  pub(crate) fn into_options(self, polling: bool) -> Options {
    Options {
      ignored: self.ignored,
      debounce: self.debounce,
      poll_interval: if polling { self.poll_interval } else { FALLBACK_POLL_INTERVAL },
      // The usual disks of macOS and Windows find a name under another case of its letters,
      // and under another Unicode form.
      loose_spelling: cfg!(any(target_vendor = "apple", windows)),
      ..Options::default()
    }
  }
}

/// What the ignore filter is asked about: a file, or a directory.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum EntryKind {
  File,
  Dir,
}
