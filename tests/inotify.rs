//! The inotify backend alone, on the real kernel: its hints, its watches and its shutdown.
#![cfg(any(target_os = "linux", target_os = "android"))]

use std::{
  collections::HashSet,
  ffi::OsStr,
  fs::{self, File, Permissions},
  io::{self, Write},
  os::unix::{
    ffi::OsStrExt,
    fs::{PermissionsExt, symlink},
  },
  path::{Path, PathBuf},
  process::Command,
  sync::mpsc::{self, Receiver, TryRecvError},
  thread,
  time::{Duration, Instant},
};

use pathwatch::{
  backend::{Backend, Coverage, DirKey, Hint, Signal, Sink, Want, is_watch_limit},
  inotify::InotifyBackend,
};
use tempfile::TempDir;

/// Only a failing test waits this long.
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Seen {
  Changed(DirKey, PathBuf),
  Written(DirKey, PathBuf),
  Lost,
  Mount(PathBuf),
}

/// `""` is the directory of `dir` itself.
fn changed(dir: DirKey, rel: &str) -> Seen {
  Seen::Changed(dir, PathBuf::from(rel))
}

fn written(dir: DirKey, rel: &str) -> Seen {
  Seen::Written(dir, PathBuf::from(rel))
}

/// MODIFY or CREATE (`Written`) and then CLOSE_WRITE (`Changed`). How many of each is up to the
/// kernel, so they are compared as a set.
fn written_and_closed(dir: DirKey, rel: &str) -> HashSet<Seen> {
  HashSet::from([written(dir, rel), changed(dir, rel)])
}

fn set(hints: Vec<Seen>) -> HashSet<Seen> {
  hints.into_iter().collect()
}

/// One raw event as the kernel lays it out.
fn event(wd: i32, mask: u32, name: &str) -> Vec<u8> {
  let name_len = if name.is_empty() { 0 } else { (name.len() + 1).next_multiple_of(16) };
  let mut raw = Vec::new();
  raw.extend(wd.to_ne_bytes());
  raw.extend(mask.to_ne_bytes());
  raw.extend(0_u32.to_ne_bytes());
  raw.extend(u32::try_from(name_len).unwrap().to_ne_bytes());
  raw.extend(name.bytes());
  raw.resize(16 + name_len, 0);
  raw
}

struct Harness {
  backend: InotifyBackend,
  signals: Receiver<Signal<Vec<u8>>>,
  dir: TempDir,
  /// Watched by the harness itself, for `hints`.
  sentinel_dir: TempDir,
  sentinel_key: DirKey,
  sentinels: u32,
}

impl Harness {
  fn new() -> Self {
    let (sender, signals) = mpsc::channel();
    let sink = Sink::new(move |signal| {
      let _ = sender.send(signal);
    });
    let mut backend = InotifyBackend::new(sink).unwrap();
    let sentinel_dir = tempfile::tempdir().unwrap();
    let sentinel_key = DirKey::new(u32::MAX, u32::MAX);
    assert!(matches!(
      backend.watch(sentinel_key, sentinel_dir.path(), Want::Names),
      Coverage::Watched
    ));
    let dir = tempfile::tempdir().unwrap();
    Self { backend, signals, dir, sentinel_dir, sentinel_key, sentinels: 0 }
  }

  /// `""` is the playground itself.
  fn path(&self, rel: &str) -> PathBuf {
    self.dir.path().join(rel)
  }

  fn watch(&mut self, index: u32, rel: &str, want: Want) -> DirKey {
    let key = DirKey::new(index, 0);
    let coverage = self.backend.watch(key, &self.path(rel), want);
    assert!(matches!(coverage, Coverage::Watched), "{coverage:?}");
    self.backend.commit();
    key
  }

  fn watch_fails(&mut self, index: u32, path: &Path) -> io::Error {
    match self.backend.watch(DirKey::new(index, 0), path, Want::Contents) {
      Coverage::Failed(error) => error,
      coverage => panic!("{coverage:?} for {}", path.display()),
    }
  }

  fn wd(&self, key: DirKey) -> Option<i32> {
    self.backend.wd(key)
  }

  /// What /proc shows for the inotify descriptor: one line for each kernel watch.
  fn kernel_watches(&self) -> Vec<String> {
    let fd = self.backend.inotify_fd();
    let fdinfo = fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).unwrap();
    fdinfo.lines().filter(|line| line.starts_with("inotify wd:")).map(str::to_owned).collect()
  }

  /// The events that the kernel reports for the watch of `key`.
  fn kernel_events(&self, key: DirKey) -> u32 {
    let watch = format!("inotify wd:{:x} ", self.wd(key).unwrap());
    let watches = self.kernel_watches();
    let line = watches.iter().find(|line| line.starts_with(&watch)).unwrap();
    let mask = line.split_once(" mask:").unwrap().1.split(' ').next().unwrap();
    u32::from_str_radix(mask, 16).unwrap() & libc::IN_ALL_EVENTS
  }

  /// The kernel watches of the test, without the one of the harness. One more in the kernel
  /// than the backend knows is a leak.
  fn watches(&self) -> usize {
    let watches = self.kernel_watches().len();
    assert_eq!(self.backend.watch_count(), watches);
    watches - 1
  }

  fn translate(&mut self, raw: Vec<u8>) -> Vec<Seen> {
    let mut seen = Vec::new();
    self.backend.translate(raw, &mut |hint| {
      seen.push(match hint {
        Hint::Changed { dir, rel } => Seen::Changed(dir, rel.to_owned()),
        Hint::Written { dir, rel } => Seen::Written(dir, rel.to_owned()),
        Hint::Lost => Seen::Lost,
        Hint::Mount { path } => Seen::Mount(path.to_owned()),
      });
    });
    seen
  }

  /// Receives and translates until `last` shows up.
  fn hints_until(&mut self, last: &Seen) -> Vec<Seen> {
    let deadline = Instant::now() + TIMEOUT;
    let mut seen = Vec::new();
    while !seen.contains(last) {
      let left = deadline.saturating_duration_since(Instant::now());
      match self.signals.recv_timeout(left) {
        Ok(Signal::Raw(raw)) => seen.extend(self.translate(raw)),
        Ok(Signal::Failed(error)) => panic!("the backend failed: {error}"),
        Err(error) => panic!("no {last:?} ({error}), only {seen:?}"),
      }
    }
    seen
  }

  /// Every hint for what the test did up to now, in order. inotify queues an event before the
  /// call that made it returns, so all of them are in front of the one for a new sentinel file.
  fn hints(&mut self) -> Vec<Seen> {
    self.sentinels += 1;
    let name = self.sentinels.to_string();
    File::create(self.sentinel_dir.path().join(&name)).unwrap();
    let sentinel = written(self.sentinel_key, &name);
    let mut seen = self.hints_until(&sentinel);
    assert_eq!(seen.pop(), Some(sentinel));
    seen
  }
}

fn chmod(path: &Path, mode: u32) {
  fs::set_permissions(path, Permissions::from_mode(mode)).unwrap();
}

/// A file that is written, made or moved in is "written", under its new name only; everything
/// else, and every directory entry, is "changed".
#[test]
fn entries_give_their_hints() {
  let mut h = Harness::new();
  fs::write(h.path("a"), "1").unwrap();
  fs::write(h.path("b.tmp"), "1").unwrap();
  let key = h.watch(1, "", Want::Contents);

  fs::write(h.path("a"), "2").unwrap();
  assert_eq!(set(h.hints()), written_and_closed(key, "a"));
  File::create(h.path("c")).unwrap();
  assert_eq!(set(h.hints()), written_and_closed(key, "c"));
  fs::rename(h.path("b.tmp"), h.path("b")).unwrap();
  assert_eq!(h.hints(), [changed(key, "b.tmp"), written(key, "b")]);
  fs::remove_file(h.path("a")).unwrap();
  assert_eq!(h.hints(), [changed(key, "a")]);

  fs::create_dir(h.path("sub")).unwrap();
  assert_eq!(h.hints(), [changed(key, "sub")]);
  fs::rename(h.path("sub"), h.path("moved")).unwrap();
  assert_eq!(h.hints(), [changed(key, "sub"), changed(key, "moved")]);
  fs::remove_dir(h.path("moved")).unwrap();
  assert_eq!(h.hints(), [changed(key, "moved")]);
}

#[test]
fn watch_does_not_reach_below_its_directory() {
  let mut h = Harness::new();
  fs::create_dir(h.path("sub")).unwrap();
  let key = h.watch(1, "", Want::Contents);

  fs::write(h.path("sub/a"), "1").unwrap();
  assert_eq!(h.hints(), []);

  fs::write(h.path("a"), "1").unwrap();
  assert_eq!(set(h.hints()), written_and_closed(key, "a"));
}

#[test]
fn attrib_on_a_directory_is_dropped() {
  let mut h = Harness::new();
  fs::create_dir(h.path("sub")).unwrap();
  File::create(h.path("file")).unwrap();
  let key = h.watch(1, "", Want::Contents);

  // The first comes with the name "sub", the second with no name from the directory itself.
  chmod(&h.path("sub"), 0o750);
  chmod(&h.path(""), 0o750);
  assert_eq!(h.hints(), []);

  chmod(&h.path("file"), 0o600);
  assert_eq!(h.hints(), [changed(key, "file")]);
}

#[test]
fn each_want_asks_the_kernel_for_its_events_only() {
  let mut h = Harness::new();
  let entries = libc::IN_CREATE | libc::IN_DELETE | libc::IN_MOVED_FROM | libc::IN_MOVED_TO;
  let content = libc::IN_MODIFY | libc::IN_ATTRIB | libc::IN_CLOSE_WRITE;
  let itself = libc::IN_MOVE_SELF | libc::IN_DELETE_SELF;
  for name in ["a", "b"] {
    fs::create_dir(h.path(name)).unwrap();
  }
  fs::write(h.path("a/x"), "1").unwrap();

  let ancestor = h.watch(1, "a", Want::Names);
  let dir = h.watch(2, "b", Want::Contents);
  assert_eq!(h.kernel_events(ancestor), entries | itself);
  assert_eq!(h.kernel_events(dir), entries | itself | content);

  // So an ancestor hears the entries, but no writes.
  fs::write(h.path("a/x"), "2").unwrap();
  chmod(&h.path("a/x"), 0o600);
  assert_eq!(h.hints(), []);
  File::create(h.path("a/y")).unwrap();
  assert_eq!(h.hints(), [written(ancestor, "y")]);
  fs::rename(h.path("a/y"), h.path("a/z")).unwrap();
  assert_eq!(h.hints(), [changed(ancestor, "y"), written(ancestor, "z")]);
  fs::remove_file(h.path("a/z")).unwrap();
  assert_eq!(h.hints(), [changed(ancestor, "z")]);
}

#[test]
fn reads_make_no_hints() {
  let mut h = Harness::new();
  fs::write(h.path("a"), "1").unwrap();
  h.watch(1, "", Want::Contents);

  assert_eq!(fs::read_dir(h.path("")).unwrap().count(), 1);
  assert_eq!(fs::read(h.path("a")).unwrap(), b"1");
  assert_eq!(h.hints(), []);
}

#[test]
fn write_to_a_replaced_file_is_no_hint_for_the_new_file() {
  let mut h = Harness::new();
  let mut replaced = File::create(h.path("a")).unwrap();
  fs::write(h.path("a.tmp"), "1").unwrap();
  let key = h.watch(1, "", Want::Contents);

  fs::rename(h.path("a.tmp"), h.path("a")).unwrap();
  assert_eq!(h.hints(), [changed(key, "a.tmp"), written(key, "a")]);

  // Without EXCL_UNLINK the kernel reports both under the name "a".
  replaced.write_all(b"late").unwrap();
  drop(replaced);
  assert_eq!(h.hints(), []);
}

#[test]
fn want_of_a_key_goes_up_and_stays_up() {
  let mut h = Harness::new();
  fs::write(h.path("a"), "1").unwrap();
  let key = h.watch(1, "", Want::Names);
  fs::write(h.path("a"), "2").unwrap();
  assert_eq!(h.hints(), []);

  h.watch(1, "", Want::Contents);
  fs::write(h.path("a"), "3").unwrap();
  assert_eq!(set(h.hints()), written_and_closed(key, "a"));

  h.watch(1, "", Want::Names);
  fs::write(h.path("a"), "4").unwrap();
  assert_eq!(set(h.hints()), written_and_closed(key, "a"));
  assert_eq!(h.watches(), 1);
  assert_eq!(h.backend.keys(h.wd(key).unwrap()), [key]);
}

#[test]
fn renamed_directory_serves_the_old_and_the_new_key() {
  let mut h = Harness::new();
  fs::create_dir(h.path("old")).unwrap();
  fs::write(h.path("old/a"), "1").unwrap();
  let old = h.watch(1, "old", Want::Contents);

  fs::rename(h.path("old"), h.path("new")).unwrap();
  assert_eq!(h.hints(), [changed(old, "")]);
  let new = h.watch(2, "new", Want::Contents);
  assert_eq!(h.wd(old), h.wd(new));
  assert_eq!(h.watches(), 1);

  fs::write(h.path("new/a"), "2").unwrap();
  let both: HashSet<Seen> =
    written_and_closed(old, "a").union(&written_and_closed(new, "a")).cloned().collect();
  assert_eq!(set(h.hints()), both);

  h.backend.unwatch(old);
  assert_eq!(h.wd(old), None);
  assert_eq!(h.watches(), 1);
  fs::write(h.path("new/a"), "3").unwrap();
  assert_eq!(set(h.hints()), written_and_closed(new, "a"));
}

#[test]
fn second_key_with_a_lower_want_takes_no_events_away() {
  let mut h = Harness::new();
  fs::create_dir(h.path("real")).unwrap();
  fs::write(h.path("real/a"), "1").unwrap();
  symlink(h.path("real"), h.path("link")).unwrap();
  let dir = h.watch(1, "real", Want::Contents);
  let ancestor = h.watch(2, "link", Want::Names);
  assert_eq!(h.wd(dir), h.wd(ancestor));

  // The two keys share one kernel watch, so the second hears the writes too: more hints than
  // asked for are fine.
  fs::write(h.path("real/a"), "2").unwrap();
  let both: HashSet<Seen> =
    written_and_closed(dir, "a").union(&written_and_closed(ancestor, "a")).cloned().collect();
  assert_eq!(set(h.hints()), both);

  fs::remove_file(h.path("real/a")).unwrap();
  assert_eq!(h.hints(), [changed(dir, "a"), changed(ancestor, "a")]);
}

#[test]
fn key_moves_to_the_directory_that_is_at_its_path_now() {
  let mut h = Harness::new();
  fs::create_dir(h.path("sub")).unwrap();
  fs::create_dir(h.path("next")).unwrap();
  let key = h.watch(1, "sub", Want::Contents);
  let first_wd = h.wd(key);

  fs::rename(h.path("sub"), h.path("away")).unwrap();
  fs::rename(h.path("next"), h.path("sub")).unwrap();
  assert_eq!(h.hints(), [changed(key, "")]);

  // Made before the second `watch` and read after it: no hint.
  File::create(h.path("away/early")).unwrap();
  h.watch(1, "sub", Want::Contents);
  assert_ne!(h.wd(key), first_wd);
  assert_eq!(h.watches(), 1);

  File::create(h.path("away/late")).unwrap();
  File::create(h.path("sub/fresh")).unwrap();
  assert_eq!(set(h.hints()), written_and_closed(key, "fresh"));
}

#[test]
fn late_ignored_of_a_replaced_directory_is_dropped() {
  let mut h = Harness::new();
  fs::create_dir(h.path("sub")).unwrap();
  let key = h.watch(1, "sub", Want::Contents);

  // An open descriptor keeps the removed directory alive, so the kernel holds back the IGNORED
  // event of its watch until the path has a new directory and the key a new watch.
  let held = File::open(h.path("sub")).unwrap();
  fs::remove_dir(h.path("sub")).unwrap();
  fs::create_dir(h.path("sub")).unwrap();
  h.watch(1, "sub", Want::Contents);
  drop(held);

  File::create(h.path("sub/a")).unwrap();
  assert_eq!(set(h.hints()), written_and_closed(key, "a"));
  assert_eq!(h.watches(), 1);
}

#[test]
fn unwatch_stops_hints() {
  let mut h = Harness::new();
  let key = h.watch(1, "", Want::Contents);

  // Events still in the queue when the key goes are dropped, and so is the IGNORED event.
  File::create(h.path("before")).unwrap();
  h.backend.unwatch(key);
  File::create(h.path("after")).unwrap();
  assert_eq!(h.hints(), []);
  assert_eq!(h.watches(), 0);

  h.backend.unwatch(key);
}

#[test]
fn removed_directory_is_changed_itself_and_its_watch_is_forgotten() {
  let mut h = Harness::new();
  fs::create_dir(h.path("sub")).unwrap();
  fs::write(h.path("sub/a"), "1").unwrap();
  symlink(h.path("sub"), h.path("link")).unwrap();
  let sub = h.watch(1, "sub", Want::Contents);
  let link = h.watch(2, "link", Want::Contents);

  // DELETE_SELF and IGNORED: one hint for the two, with no watch on the parent.
  fs::remove_dir_all(h.path("sub")).unwrap();
  let expected = [changed(sub, "a"), changed(link, "a"), changed(sub, ""), changed(link, "")];
  assert_eq!(h.hints_until(&changed(link, "")), expected);
  assert_eq!((h.wd(sub), h.wd(link)), (None, None));
  assert_eq!(h.watches(), 0);

  // The kernel watch is gone already. This must not disturb anything.
  h.backend.unwatch(sub);
  assert_eq!(h.hints(), []);

  fs::create_dir(h.path("sub")).unwrap();
  h.watch(1, "sub", Want::Contents);
  File::create(h.path("sub/a")).unwrap();
  assert_eq!(set(h.hints()), written_and_closed(sub, "a"));
}

#[test]
fn moved_directory_is_changed_itself_with_no_watch_on_its_parent() {
  let mut h = Harness::new();
  fs::create_dir(h.path("sub")).unwrap();
  let key = h.watch(1, "sub", Want::Names);

  fs::rename(h.path("sub"), h.path("away")).unwrap();
  assert_eq!(h.hints(), [changed(key, "")]);

  // The watch follows the directory. Until `watch` is called again, the key still hears it.
  assert_eq!(h.watches(), 1);
  File::create(h.path("away/a")).unwrap();
  assert_eq!(h.hints(), [written(key, "a")]);

  // Still in the queue when the key goes: it is dropped.
  File::create(h.path("away/b")).unwrap();
  assert_eq!(h.watch_fails(1, &h.path("sub")).kind(), io::ErrorKind::NotFound);
  assert_eq!(h.wd(key), None);
  assert_eq!(h.watches(), 0);
  File::create(h.path("away/c")).unwrap();
  assert_eq!(h.hints(), []);
}

#[test]
fn moved_directory_is_told_by_its_parent_and_by_itself() {
  let mut h = Harness::new();
  fs::create_dir(h.path("sub")).unwrap();
  let parent = h.watch(1, "", Want::Names);
  let sub = h.watch(2, "sub", Want::Contents);

  fs::rename(h.path("sub"), h.path("away")).unwrap();
  assert_eq!(h.hints(), [changed(parent, "sub"), changed(parent, "away"), changed(sub, "")]);
}

#[test]
fn directory_that_another_one_is_moved_over_is_changed_itself() {
  let mut h = Harness::new();
  fs::create_dir(h.path("sub")).unwrap();
  fs::create_dir(h.path("next")).unwrap();
  let key = h.watch(1, "sub", Want::Contents);

  // The old directory at "sub" is deleted by the rename.
  fs::rename(h.path("next"), h.path("sub")).unwrap();
  assert_eq!(h.hints(), [changed(key, "")]);
  assert_eq!(h.wd(key), None);
  assert_eq!(h.watches(), 0);

  h.watch(1, "sub", Want::Contents);
  File::create(h.path("sub/a")).unwrap();
  assert_eq!(set(h.hints()), written_and_closed(key, "a"));
}

#[test]
fn moved_directory_with_two_keys_gives_one_hint_each() {
  let mut h = Harness::new();
  fs::create_dir(h.path("real")).unwrap();
  symlink(h.path("real"), h.path("link")).unwrap();
  let real = h.watch(1, "real", Want::Contents);
  let link = h.watch(2, "link", Want::Contents);
  let first_wd = h.wd(real);

  fs::rename(h.path("real"), h.path("away")).unwrap();
  assert_eq!(h.hints(), [changed(real, ""), changed(link, "")]);

  // The old watch stays as long as one key is on it.
  fs::create_dir(h.path("real")).unwrap();
  h.watch(1, "real", Want::Contents);
  assert_ne!(h.wd(real), first_wd);
  assert_eq!(h.wd(link), first_wd);
  assert_eq!(h.watches(), 2);
  File::create(h.path("away/a")).unwrap();
  assert_eq!(set(h.hints()), written_and_closed(link, "a"));

  h.watch(2, "link", Want::Contents);
  assert_eq!(h.wd(link), h.wd(real));
  assert_eq!(h.watches(), 1);
  File::create(h.path("away/b")).unwrap();
  File::create(h.path("link/c")).unwrap();
  let both: HashSet<Seen> =
    written_and_closed(real, "c").union(&written_and_closed(link, "c")).cloned().collect();
  assert_eq!(set(h.hints()), both);
}

#[test]
fn missing_path_and_file_fail() {
  let mut h = Harness::new();
  File::create(h.path("file")).unwrap();

  assert_eq!(h.watch_fails(1, &h.path("missing")).kind(), io::ErrorKind::NotFound);
  assert_eq!(h.watch_fails(1, &h.path("file")).kind(), io::ErrorKind::NotADirectory);
  assert_eq!(h.watch_fails(1, &h.path("file/below")).kind(), io::ErrorKind::NotADirectory);
  let with_nul = Path::new(OsStr::from_bytes(b"/tmp/a\0b"));
  assert_eq!(h.watch_fails(1, with_nul).kind(), io::ErrorKind::InvalidInput);

  assert_eq!(h.wd(DirKey::new(1, 0)), None);
  assert_eq!(h.watches(), 0);
}

/// A volume that is mounted over a watched directory, or unmounted, is told by its mount point:
/// inotify says nothing of it. A user namespace with its own mounts can mount without root:
///
/// ```sh
/// unshare -Urm cargo test -p pathwatch --test inotify -- --ignored mount
/// ```
#[test]
#[ignore = "mounts a volume, see the comment"]
fn a_mount_over_a_watched_directory_is_told() {
  let mut h = Harness::new();
  // The kernel writes a space in a mount point as `\040`.
  fs::create_dir_all(h.path("a b/sub")).unwrap();
  h.watch(1, "a b", Want::Contents);
  h.watch(2, "a b/sub", Want::Contents);
  let point = fs::canonicalize(h.path("a b")).unwrap();
  let run = |command: &mut Command| assert!(command.status().unwrap().success(), "{command:?}");
  run(Command::new("mount").args(["-t", "tmpfs", "tmpfs"]).arg(&point));
  assert_eq!(h.hints_until(&Seen::Mount(point.clone())), [Seen::Mount(point.clone())]);
  run(Command::new("umount").arg(&point));
  h.hints_until(&Seen::Mount(point));
}

/// A user namespace can lower the limit without root:
///
/// ```sh
/// unshare -Urm sh -c 'echo 8 > /proc/sys/user/max_inotify_watches &&
///   cargo test -p pathwatch --test inotify -- --ignored'
/// ```
#[test]
#[ignore = "needs a lowered watch limit, see the comment"]
fn watch_limit_is_reported() {
  let mut h = Harness::new();
  for index in 1..=64 {
    let path = h.path(&index.to_string());
    fs::create_dir(&path).unwrap();
    let key = DirKey::new(index, 0);
    if let Coverage::Failed(error) = h.backend.watch(key, &path, Want::Contents) {
      assert!(is_watch_limit(&error));
      assert!(error.to_string().contains("fs.inotify.max_user_watches"), "{error}");
      assert_eq!(h.wd(key), None);
      // A directory that has a watch can still be asked for again.
      h.watch(1, "1", Want::Contents);
      return;
    }
  }
  panic!("64 watches were added: the limit is not lowered");
}

#[test]
fn masks_map_to_hints() {
  let mut h = Harness::new();
  let key = h.watch(1, "", Want::Contents);
  let wd = h.wd(key).unwrap();

  for mask in [libc::IN_MODIFY, libc::IN_CREATE, libc::IN_MOVED_TO] {
    assert_eq!(h.translate(event(wd, mask, "n")), [written(key, "n")], "{mask:#x}");
  }
  for mask in [libc::IN_DELETE, libc::IN_MOVED_FROM, libc::IN_ATTRIB, libc::IN_CLOSE_WRITE] {
    assert_eq!(h.translate(event(wd, mask, "n")), [changed(key, "n")], "{mask:#x}");
  }
  for mask in [libc::IN_CREATE, libc::IN_DELETE, libc::IN_MOVED_FROM, libc::IN_MOVED_TO] {
    let raw = event(wd, mask | libc::IN_ISDIR, "n");
    assert_eq!(h.translate(raw), [changed(key, "n")], "{mask:#x}");
  }
  for mask in [libc::IN_ATTRIB, libc::IN_MODIFY] {
    assert_eq!(h.translate(event(wd, mask | libc::IN_ISDIR, "n")), [], "{mask:#x}");
    assert_eq!(h.translate(event(wd, mask | libc::IN_ISDIR, "")), [], "{mask:#x}");
  }

  // The watched directory itself. A move leaves the watch; a delete or an unmount ends it, and
  // the IGNORED that the kernel sends with each of them gives no second hint.
  for mask in [libc::IN_MOVE_SELF, libc::IN_MOVE_SELF | libc::IN_ISDIR] {
    assert_eq!(h.translate(event(wd, mask, "")), [changed(key, "")], "{mask:#x}");
  }
  // A mount point that the reader found: a record of its own, under no watch.
  let mount = event(-2, 0, "/mnt/a b");
  assert_eq!(h.translate(mount), [Seen::Mount(PathBuf::from("/mnt/a b"))]);
  assert_eq!(h.wd(key), Some(wd));
  for (index, mask) in [(1, libc::IN_DELETE_SELF), (2, libc::IN_UNMOUNT)] {
    // The kernel watch is still there: the second key gets it, after the first forgot it.
    let key = h.watch(index, "", Want::Contents);
    let wd = h.wd(key).unwrap();
    let mut raw = event(wd, mask | libc::IN_ISDIR, "");
    raw.extend(event(wd, libc::IN_IGNORED, ""));
    assert_eq!(h.translate(raw), [changed(key, "")], "{mask:#x}");
    assert_eq!(h.wd(key), None);
  }

  // The queue of the kernel was full: it cannot say what is missing.
  assert_eq!(h.translate(event(-1, libc::IN_Q_OVERFLOW, "")), [Seen::Lost]);
}

#[test]
fn packed_events_unknown_watch_and_cut_off_buffer() {
  let mut h = Harness::new();
  let key = h.watch(1, "", Want::Contents);
  let wd = h.wd(key).unwrap();
  let long_name = "a-name-that-needs-more-than-one-block.txt";

  let events = [
    event(wd, libc::IN_CREATE, "a"),
    event(wd + 1000, libc::IN_MODIFY, "ghost"),
    event(wd, libc::IN_MODIFY, long_name),
    event(wd, libc::IN_DELETE | libc::IN_ISDIR, "sub"),
  ];
  let raw = events.concat();
  let all = h.translate(raw.clone());
  assert_eq!(all, [written(key, "a"), written(key, long_name), changed(key, "sub")]);

  // The kernel never cuts an event. If a buffer is cut all the same, the whole events before
  // the cut are used, and the rest is lost.
  let mut ends = vec![0];
  for event in &events {
    ends.push(ends[ends.len() - 1] + event.len());
  }
  let mut hints_at_cut = Vec::new();
  for cut in 0..raw.len() {
    let mut part = h.translate(raw[..cut].to_vec());
    if !ends.contains(&cut) {
      assert_eq!(part.pop(), Some(Seen::Lost), "cut at {cut}");
    }
    assert!(all.starts_with(&part), "cut at {cut}: {part:?}");
    hints_at_cut.push(part.len());
  }
  assert!(hints_at_cut.is_sorted());
  assert_eq!(hints_at_cut.last(), Some(&2));
}

#[test]
fn flood_arrives_whole_over_several_reads() {
  let mut h = Harness::new();
  let key = h.watch(1, "", Want::Contents);

  // 6000 events of 32 bytes do not fit in two read buffers, and stay below the default queue
  // limit of 16384 events even if the reader thread gets no time.
  let names: Vec<String> = (0..3000).map(|index| format!("file-{index}")).collect();
  for name in &names {
    File::create(h.path(name)).unwrap();
  }
  let seen = set(h.hints());
  assert_eq!(seen.len(), 2 * names.len());
  assert!(names.iter().all(|name| written_and_closed(key, name).is_subset(&seen)));
}

#[test]
fn drop_joins_the_reader() {
  let mut h = Harness::new();
  let key = h.watch(1, "", Want::Contents);
  File::create(h.path("a")).unwrap();
  assert_eq!(set(h.hints()), written_and_closed(key, "a"));

  let Harness { backend, signals, dir, .. } = h;
  let (done, joined) = mpsc::channel();
  thread::spawn(move || {
    drop(backend);
    let _ = done.send(());
  });
  joined.recv_timeout(TIMEOUT).expect("drop does not return");

  File::create(dir.path().join("b")).unwrap();
  // The reader thread owned the only sender: the channel is closed only when that thread ended.
  assert!(matches!(signals.try_recv(), Err(TryRecvError::Disconnected)));
}
