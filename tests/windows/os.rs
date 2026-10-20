//! The Windows backend alone, on a real Windows file system. A test named `open_point_` settles
//! a point that was only known from the Windows documents before it ran.

use std::{
  ffi::OsString,
  fs, io,
  os::windows::ffi::{OsStrExt, OsStringExt},
  path::{Path, PathBuf},
  process::Command,
  sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicUsize, Ordering},
    mpsc,
  },
  thread,
  time::{Duration, Instant},
};

use pathwatch::{
  backend::{Backend, Coverage, DirKey, Hint, Signal, Sink, Want},
  windows::{MAX_HANDLES, Raw, WindowsBackend},
};
use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Seen {
  Changed { key: DirKey, rel: PathBuf },
  Written { key: DirKey, rel: PathBuf },
  Lost,
}

impl Seen {
  fn key(&self) -> Option<DirKey> {
    match self {
      Self::Changed { key, .. } | Self::Written { key, .. } => Some(*key),
      Self::Lost => None,
    }
  }

  /// A `Changed` or a `Written` for this path.
  fn names(&self, of: DirKey, path: &str) -> bool {
    matches!(
      self,
      Self::Changed { key, rel } | Self::Written { key, rel }
        if *key == of && rel == Path::new(path)
    )
  }

  /// A loss report for the directory of the key itself.
  fn names_the_directory(&self, of: DirKey) -> bool {
    *self == Self::Changed { key: of, rel: PathBuf::new() }
  }
}

/// While it is shut, the worker thread of the backend stops inside the sink.
#[derive(Default)]
struct Gate {
  shut: Mutex<bool>,
  moved: Condvar,
}

/// Opens the gate again when dropped, also when the test fails: a shut gate blocks the drop.
struct Shut<'a>(&'a Gate);

impl Gate {
  fn shut(&self) -> Shut<'_> {
    *self.shut.lock().unwrap() = true;
    Shut(self)
  }

  fn pass(&self) {
    drop(self.moved.wait_while(self.shut.lock().unwrap(), |shut| *shut).unwrap());
  }
}

impl Drop for Shut<'_> {
  fn drop(&mut self) {
    *self.0.shut.lock().unwrap() = false;
    self.0.moved.notify_all();
  }
}

struct Harness {
  backend: WindowsBackend,
  signals: mpsc::Receiver<Signal<Raw>>,
  next_key: u32,
  /// How many reads of the handles came back.
  reads: usize,
}

impl Harness {
  fn new() -> Self {
    Self::behind(&Arc::new(Gate::default()))
  }

  fn behind(gate: &Arc<Gate>) -> Self {
    let (sender, signals) = mpsc::channel();
    let gate = Arc::clone(gate);
    let sink = Sink::new(move |signal| {
      gate.pass();
      let _ = sender.send(signal);
    });
    Self { backend: WindowsBackend::new(sink).unwrap(), signals, next_key: 0, reads: 0 }
  }

  fn watch(&mut self, dir: &Path, want: Want) -> (DirKey, Coverage) {
    let key = DirKey::new(self.next_key, 0);
    self.next_key += 1;
    (key, self.watch_again(key, dir, want))
  }

  fn watch_again(&mut self, key: DirKey, dir: &Path, want: Want) -> Coverage {
    let coverage = self.backend.watch(key, dir, want);
    self.backend.commit();
    // A key that got no handle at the commit is asked again, as the engine does: that answer
    // says why.
    if matches!(coverage, Coverage::WatchedTree) && !self.backend.serves(key) {
      let coverage = self.backend.watch(key, dir, want);
      self.backend.commit();
      return coverage;
    }
    coverage
  }

  /// Opens a handle on `dir`.
  fn handle(&mut self, dir: &Path) -> DirKey {
    let (key, coverage) = self.watch(dir, Want::Contents);
    assert!(matches!(coverage, Coverage::WatchedTree), "{}: {coverage:?}", dir.display());
    key
  }

  /// Takes hints until `done` says yes to one. Gives every hint taken on the way.
  fn hints_until(&mut self, done: impl Fn(&Seen) -> bool) -> Vec<Seen> {
    let deadline = Instant::now() + TIMEOUT;
    let mut seen = Vec::new();
    loop {
      let left = deadline.saturating_duration_since(Instant::now());
      let signal = match self.signals.recv_timeout(left) {
        Ok(Signal::Raw(raw)) => raw,
        Ok(Signal::Failed(error)) => panic!("the backend failed: {error}"),
        Err(error) => panic!("the hint did not come: {error}. Hints so far: {seen:?}"),
      };
      self.reads += 1;
      let before = seen.len();
      seen.extend(self.hints_of(signal));
      if seen[before..].iter().any(&done) {
        return seen;
      }
    }
  }

  /// The hints of one signal.
  fn hints_of(&mut self, raw: Raw) -> Vec<Seen> {
    let mut seen = Vec::new();
    self.backend.translate(raw, &mut |hint| {
      seen.push(match hint {
        Hint::Changed { dir, rel } => Seen::Changed { key: dir, rel: rel.to_owned() },
        Hint::Written { dir, rel, .. } => Seen::Written { key: dir, rel: rel.to_owned() },
        Hint::Lost => Seen::Lost,
        Hint::Mount { .. } => panic!("only inotify tells of a mount by its path"),
      });
    });
    seen
  }
}

/// A new directory in the temp directory, spelled as the system spells it: often with a short
/// name in it (`RUNNER~1`), and not in `\\?\` form.
fn scratch() -> tempfile::TempDir {
  tempfile::tempdir().unwrap()
}

fn make_dir(path: PathBuf) -> PathBuf {
  fs::create_dir_all(&path).unwrap();
  path
}

/// Tries `action` until it works: Windows allows some things only a moment after the last
/// handle on a directory is closed.
fn until_ok(action: impl Fn() -> io::Result<()>) {
  let deadline = Instant::now() + TIMEOUT;
  while let Err(error) = action() {
    assert!(Instant::now() < deadline, "still failing after {TIMEOUT:?}: {error}");
    thread::sleep(Duration::from_millis(10));
  }
}

fn junction(link: &Path, target: &Path) {
  let made = Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link).arg(target).output();
  assert!(made.is_ok_and(|output| output.status.success()), "mklink /J failed");
}

/// The short (8.3) name of the file. `None` if the volume makes no short names.
fn short_name(path: &Path) -> Option<OsString> {
  let long: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
  let mut short = vec![0; 32 * 1024];
  let room = u32::try_from(short.len()).unwrap();
  // SAFETY: `long` ends with a NUL, and `short` has room for `room` units.
  let len = unsafe { GetShortPathNameW(long.as_ptr(), short.as_mut_ptr(), room) };
  assert!(len > 0 && len < room, "GetShortPathNameW: {}", io::Error::last_os_error());
  let short = PathBuf::from(OsString::from_wide(&short[..len as usize]));
  let name = short.file_name()?;
  (Some(name) != path.file_name()).then(|| name.to_owned())
}

#[test]
fn a_change_of_a_file_becomes_a_hint() {
  let temp = scratch();
  for name in ["written.txt", "old.txt", "removed.txt"] {
    fs::write(temp.path().join(name), "1").unwrap();
  }
  let deep = make_dir(temp.path().join("a").join("b"));
  let mut harness = Harness::new();
  let key = harness.handle(temp.path());

  fs::write(temp.path().join("created.txt"), "1").unwrap();
  harness.hints_until(|hint| hint.names(key, "created.txt"));
  // A file two levels down is reported from the same handle, with the way down to it.
  fs::write(deep.join("f.txt"), "1").unwrap();
  harness.hints_until(|hint| hint.names(key, r"a\b\f.txt"));

  fs::write(temp.path().join("written.txt"), "12").unwrap();
  let written = Seen::Written { key, rel: "written.txt".into() };
  harness.hints_until(|hint| *hint == written);

  fs::rename(temp.path().join("old.txt"), temp.path().join("new.txt")).unwrap();
  let seen = harness.hints_until(|hint| hint.names(key, "new.txt"));
  assert!(seen.iter().any(|hint| hint.names(key, "old.txt")), "{seen:?}");

  fs::remove_file(temp.path().join("removed.txt")).unwrap();
  harness.hints_until(|hint| hint.names(key, "removed.txt"));
}

/// The kernel completes a read as soon as one record is in it. The backend rests a millisecond
/// before the next read, so the records of a flood come in batches, not one a read.
#[test]
fn a_flood_of_records_comes_in_batches() {
  let temp = scratch();
  let mut harness = Harness::new();
  let key = harness.handle(temp.path());

  // 300 files as fast as the disk takes them: at least two records each.
  for n in 0..300 {
    fs::write(temp.path().join(format!("f{n}.txt")), "1").unwrap();
  }
  fs::write(temp.path().join("last.txt"), "1").unwrap();
  let seen = harness.hints_until(|hint| hint.names(key, "last.txt"));
  let (reads, records) = (harness.reads, seen.len());
  // Without the rest the reads are as many as the records (measured: one record a read).
  assert!(reads * 2 <= records, "{reads} reads for {records} records");
}

#[test]
fn a_change_of_a_directory_becomes_a_hint() {
  let temp = scratch();
  let mut harness = Harness::new();
  let key = harness.handle(temp.path());

  fs::create_dir(temp.path().join("made")).unwrap();
  harness.hints_until(|hint| hint.names(key, "made"));

  fs::rename(temp.path().join("made"), temp.path().join("moved")).unwrap();
  harness.hints_until(|hint| hint.names(key, "moved"));

  fs::remove_dir(temp.path().join("moved")).unwrap();
  let seen = harness.hints_until(|hint| *hint == Seen::Changed { key, rel: "moved".into() });
  // A directory that goes below the handle is no loss report for the handle itself.
  assert!(
    seen.iter().all(|hint| !hint.names_the_directory(key) && *hint != Seen::Lost),
    "{seen:?}"
  );
}

#[test]
fn a_watch_is_live_when_watch_returns() {
  let temp = scratch();
  let mut harness = Harness::new();
  for round in 0..50 {
    let dir = make_dir(temp.path().join(round.to_string()));
    let key = harness.handle(&dir);
    fs::write(dir.join("f.txt"), "1").unwrap();
    harness.hints_until(|hint| hint.names(key, "f.txt"));
    harness.backend.unwatch(key);
  }
}

/// Windows also sends a record for the directory when an entry in it changes.
#[test]
fn a_change_in_a_directory_below_makes_no_hint_for_that_directory() {
  let temp = scratch();
  let sub = make_dir(temp.path().join("sub"));
  let mut harness = Harness::new();
  let key = harness.handle(temp.path());

  for name in ["one.txt", "two.txt", "last.txt"] {
    fs::write(sub.join(name), "1").unwrap();
  }
  fs::remove_file(sub.join("one.txt")).unwrap();
  fs::write(temp.path().join("end.txt"), "1").unwrap();
  let seen = harness.hints_until(|hint| hint.names(key, "end.txt"));
  assert!(seen.iter().any(|hint| hint.names(key, r"sub\last.txt")), "{seen:?}");
  assert!(seen.iter().all(|hint| !hint.names(key, "sub")), "{seen:?}");
}

#[test]
fn a_directory_below_a_handle_gets_no_handle_of_its_own() {
  let temp = scratch();
  let top = make_dir(temp.path().join("top"));
  let sub = make_dir(top.join("sub"));
  let mut harness = Harness::new();
  let key = harness.handle(&top);
  let (sub_key, coverage) = harness.watch(&sub, Want::Contents);
  assert!(matches!(coverage, Coverage::WatchedTree), "{coverage:?}");

  fs::write(sub.join("f.txt"), "1").unwrap();
  fs::write(top.join("marker"), "1").unwrap();
  let seen = harness.hints_until(|hint| hint.names(key, "marker"));
  // The key above gets the change too: the tree reaches `sub` from it (rule 7).
  assert!(seen.iter().any(|hint| hint.names(sub_key, "f.txt")), "{seen:?}");
  assert!(seen.iter().any(|hint| hint.names(key, r"sub\f.txt")), "{seen:?}");
  // With a handle on `sub` Windows would refuse this rename.
  fs::rename(&top, temp.path().join("top 2")).unwrap();
}

#[test]
fn a_handle_above_takes_over_from_the_handle_below() {
  let temp = scratch();
  let low = make_dir(temp.path().join("mid").join("low"));
  let mut harness = Harness::new();
  let low_key = harness.handle(&low);
  let key = harness.handle(temp.path());

  fs::write(low.join("f.txt"), "1").unwrap();
  harness.hints_until(|hint| hint.names(low_key, "f.txt"));

  // The handle on `low` is closed when `watch` returns: Windows refuses this while it is open.
  fs::rename(temp.path().join("mid"), temp.path().join("moved")).unwrap();
  fs::write(temp.path().join("moved").join("low").join("g.txt"), "1").unwrap();
  harness.hints_until(|hint| hint.names(key, r"moved\low\g.txt"));
}

#[test]
fn what_is_no_directory_fails() {
  let temp = scratch();
  let file = temp.path().join("file.txt");
  fs::write(&file, "1").unwrap();
  let mut harness = Harness::new();

  let (_, coverage) = harness.watch(&temp.path().join("missing"), Want::Contents);
  assert!(
    matches!(&coverage, Coverage::Failed(error) if error.kind() == io::ErrorKind::NotFound),
    "{coverage:?}"
  );
  let (_, coverage) = harness.watch(&file, Want::Contents);
  assert!(
    matches!(&coverage, Coverage::Failed(error) if error.kind() == io::ErrorKind::NotADirectory),
    "{coverage:?}"
  );
}

#[test]
fn unwatch_ends_the_hints() {
  let temp = scratch();
  let gone = make_dir(temp.path().join("above").join("gone"));
  let kept = make_dir(temp.path().join("kept"));
  let mut harness = Harness::new();
  let gone_key = harness.handle(&gone);
  let kept_key = harness.handle(&kept);

  harness.backend.unwatch(gone_key);
  // Windows allows this rename only when the handle is closed: `unwatch` waited for it.
  let moved = temp.path().join("moved");
  fs::rename(temp.path().join("above"), &moved).unwrap();
  fs::write(moved.join("gone").join("f.txt"), "1").unwrap();
  fs::write(kept.join("marker"), "1").unwrap();
  let seen = harness.hints_until(|hint| hint.names(kept_key, "marker"));
  assert!(seen.iter().all(|hint| hint.key() != Some(gone_key)), "{seen:?}");
}

#[test]
fn watch_again_takes_the_directory_that_is_at_the_path_now() {
  let temp = scratch();
  let dir = make_dir(temp.path().join("dir"));
  let mut harness = Harness::new();
  let key = harness.handle(&dir);

  fs::rename(&dir, temp.path().join("away")).unwrap();
  fs::create_dir(&dir).unwrap();
  let coverage = harness.watch_again(key, &dir, Want::Contents);
  assert!(matches!(coverage, Coverage::WatchedTree), "{coverage:?}");

  fs::write(dir.join("f.txt"), "1").unwrap();
  harness.hints_until(|hint| hint.names(key, "f.txt"));
}

#[test]
fn more_directories_than_handles_share_a_handle() {
  let temp = scratch();
  let mut harness = Harness::new();
  let keys: Vec<DirKey> =
    (0..MAX_HANDLES).map(|n| harness.handle(&make_dir(temp.path().join(n.to_string())))).collect();
  let one_more = make_dir(temp.path().join("one more"));
  let more_key = harness.handle(&one_more);

  fs::write(one_more.join("f.txt"), "1").unwrap();
  harness.hints_until(|hint| hint.names(more_key, "f.txt"));
  fs::write(temp.path().join("7").join("f.txt"), "1").unwrap();
  harness.hints_until(|hint| hint.names(keys[7], "f.txt"));

  // The shared handle also reports what is next to the watched directories: no key, no hint.
  let nobody = make_dir(temp.path().join("nobody").join("deep"));
  fs::write(nobody.join("f.txt"), "1").unwrap();
  fs::write(one_more.join("marker"), "1").unwrap();
  let seen = harness.hints_until(|hint| hint.names(more_key, "marker"));
  assert!(seen.iter().all(|hint| !format!("{hint:?}").contains("nobody")), "{seen:?}");
  assert!(!seen.contains(&Seen::Lost), "{seen:?}");

  // With room for all, each directory has its own handle again.
  harness.backend.unwatch(more_key);
  fs::write(temp.path().join("7").join("g.txt"), "1").unwrap();
  harness.hints_until(|hint| hint.names(keys[7], "g.txt"));
}

#[test]
fn open_point_a_directory_below_a_handle_that_is_renamed_is_told_itself() {
  let temp = scratch();
  let sub = make_dir(temp.path().join("sub"));
  let mut harness = Harness::new();
  let key = harness.handle(temp.path());
  let (sub_key, coverage) = harness.watch(&sub, Want::Contents);
  assert!(matches!(coverage, Coverage::WatchedTree), "{coverage:?}");

  // A new entry in `sub` makes Windows say "modified" for `sub` too: that is no hint for it.
  fs::write(sub.join("f.txt"), "1").unwrap();
  fs::write(temp.path().join("marker"), "1").unwrap();
  let seen = harness.hints_until(|hint| hint.names(key, "marker"));
  assert!(seen.iter().any(|hint| hint.names(sub_key, "f.txt")), "{seen:?}");
  assert!(seen.iter().all(|hint| !hint.names_the_directory(sub_key)), "{seen:?}");

  fs::rename(&sub, temp.path().join("sub 2")).unwrap();
  harness.hints_until(|hint| hint.names_the_directory(sub_key));
}

#[test]
fn drop_ends_the_thread() {
  let temp = scratch();
  let signals = Arc::new(AtomicUsize::new(0));
  let sink = Sink::new({
    let signals = Arc::clone(&signals);
    move |_: Signal<Raw>| {
      signals.fetch_add(1, Ordering::Relaxed);
    }
  });
  let mut backend = WindowsBackend::new(sink).unwrap();
  for n in 0..4 {
    let dir = make_dir(temp.path().join(n.to_string()));
    let coverage = backend.watch(DirKey::new(n, 0), &dir, Want::Contents);
    assert!(matches!(coverage, Coverage::WatchedTree), "{coverage:?}");
  }
  // The handles open at the commit.
  backend.commit();
  for n in 0..4 {
    assert!(backend.serves(DirKey::new(n, 0)), "{n}");
  }

  drop(backend);
  // The thread owned the only other copy of the sink.
  assert_eq!(Arc::strong_count(&signals), 1);
}

#[test]
fn both_spellings_of_the_temp_directory_work() {
  let temp = scratch();
  let verbatim = fs::canonicalize(temp.path()).unwrap();
  assert!(verbatim.as_os_str().to_string_lossy().starts_with(r"\\?\"));

  for (round, spelling) in [temp.path(), verbatim.as_path()].into_iter().enumerate() {
    let mut harness = Harness::new();
    let key = harness.handle(spelling);
    let name = format!("{round}.txt");
    fs::write(spelling.join(&name), "1").unwrap();
    harness.hints_until(|hint| hint.names(key, &name));
  }
}

#[test]
fn a_path_longer_than_260_units() {
  let temp = scratch();
  let mut dir = temp.path().to_owned();
  dir.extend(["a".repeat(100), "b".repeat(100), "c".repeat(100)]);
  fs::create_dir_all(&dir).unwrap();
  assert!(dir.as_os_str().encode_wide().count() > 260);
  let mut harness = Harness::new();
  let key = harness.handle(&dir);

  fs::write(dir.join("f.txt"), "1").unwrap();
  harness.hints_until(|hint| hint.names(key, "f.txt"));
}

#[test]
fn open_point_a_directory_name_that_ends_with_a_dot() {
  let temp = scratch();
  // Only a path in `\\?\` form lets std make and reach such a name.
  let verbatim = fs::canonicalize(temp.path()).unwrap().join("dir.");
  fs::create_dir(&verbatim).unwrap();
  let mut harness = Harness::new();
  // The backend opens the directory of this very name, not `dir`, which does not exist.
  let key = harness.handle(&temp.path().join("dir."));

  fs::write(verbatim.join("f.txt"), "1").unwrap();
  harness.hints_until(|hint| hint.names(key, "f.txt"));
}

#[test]
fn open_point_lost_records_become_a_hint_for_each_key_of_the_handle() {
  let temp = scratch();
  let sub = make_dir(temp.path().join("sub"));
  let gate = Arc::new(Gate::default());
  let mut harness = Harness::behind(&gate);
  let key = harness.handle(temp.path());
  let (sub_key, coverage) = harness.watch(&sub, Want::Names);
  assert!(matches!(coverage, Coverage::Watched), "{coverage:?}");

  {
    // The worker stops in the sink and reads no more. The kernel keeps what follows in a
    // buffer of 64 KiB; these names need about 500 KiB.
    let _shut = gate.shut();
    for n in 0..2000 {
      fs::write(temp.path().join(format!("{n:0>120}")), "").unwrap();
    }
  }
  // The loss has a place, the directory of each key of the handle: nothing says "everywhere".
  let seen = harness.hints_until(|hint| hint.names_the_directory(key));
  assert!(seen.iter().any(|hint| hint.names_the_directory(sub_key)), "{seen:?}");
  assert!(!seen.contains(&Seen::Lost), "{seen:?}");

  // The handle reads on.
  fs::write(temp.path().join("after.txt"), "1").unwrap();
  harness.hints_until(|hint| hint.names(key, "after.txt"));
}

#[test]
fn open_point_a_directory_that_is_deleted_and_made_again() {
  let temp = scratch();
  let dir = make_dir(temp.path().join("dir"));
  let mut harness = Harness::new();
  let key = harness.handle(&dir);

  fs::remove_dir(&dir).unwrap();
  // On a file system without POSIX delete the name stays until our handle is closed.
  let made_at_once = fs::create_dir(&dir).is_ok();
  // The deletion is a hint for the directory itself.
  harness.hints_until(|hint| hint.names_the_directory(key));
  if !made_at_once {
    until_ok(|| fs::create_dir(&dir));
  }

  let coverage = harness.watch_again(key, &dir, Want::Contents);
  assert!(matches!(coverage, Coverage::WatchedTree), "{coverage:?}");
  fs::write(dir.join("f.txt"), "1").unwrap();
  harness.hints_until(|hint| hint.names(key, "f.txt"));
}

#[test]
fn open_point_a_renamed_directory_reports_under_its_key() {
  let temp = scratch();
  let dir = make_dir(temp.path().join("dir"));
  let mut harness = Harness::new();
  let key = harness.handle(&dir);

  let moved = temp.path().join("moved");
  fs::rename(&dir, &moved).unwrap();
  fs::write(moved.join("f.txt"), "1").unwrap();
  harness.hints_until(|hint| hint.names(key, "f.txt"));
}

#[test]
fn open_point_only_the_directories_above_a_handle_cannot_be_renamed() {
  let temp = scratch();
  let watched = make_dir(temp.path().join("top").join("watched"));
  make_dir(watched.join("below"));
  let mut harness = Harness::new();
  let key = harness.handle(&watched);

  fs::rename(watched.join("below"), watched.join("below 2")).unwrap();
  let watched_2 = temp.path().join("top").join("watched 2");
  fs::rename(&watched, &watched_2).unwrap();
  let top_2 = temp.path().join("top 2");
  assert!(fs::rename(temp.path().join("top"), &top_2).is_err());

  harness.backend.unwatch(key);
  fs::rename(temp.path().join("top"), &top_2).unwrap();
}

/// A handle that another one replaces is read until the kernel holds nothing more for it. A
/// write in the moment between the last read that the worker took and the close of the handle
/// is not lost, from whichever handle it comes.
#[test]
fn a_change_while_a_handle_is_replaced_is_not_lost() {
  let temp = scratch();
  let low = make_dir(temp.path().join("mid").join("low"));
  let mut harness = Harness::new();
  for round in 0..20 {
    let low_key = harness.handle(&low);
    let stop = Arc::new(AtomicUsize::new(0));
    let writer = thread::spawn({
      let (low, stop) = (low.clone(), Arc::clone(&stop));
      move || {
        let mut written = 0;
        while stop.load(Ordering::Acquire) == 0 && written < 300 {
          fs::write(low.join(format!("r{round}-{written}")), "1").unwrap();
          written += 1;
        }
        written
      }
    });
    // The handle moves up to the top: the one on `low` is replaced while the writer writes.
    let key = harness.handle(temp.path());
    stop.store(1, Ordering::Release);
    let written = writer.join().unwrap();
    fs::write(low.join(format!("r{round}-marker")), "1").unwrap();
    let mut seen =
      harness.hints_until(|hint| hint.names(key, &format!(r"mid\low\r{round}-marker")));
    while let Ok(Signal::Raw(raw)) = harness.signals.recv_timeout(Duration::from_millis(200)) {
      seen.extend(harness.hints_of(raw));
    }
    for n in 0..written {
      let (name, below) = (format!("r{round}-{n}"), format!(r"mid\low\r{round}-{n}"));
      assert!(
        seen.iter().any(|hint| hint.names(low_key, &name) || hint.names(key, &below)),
        "round {round}: no hint for {name}: {seen:?}"
      );
    }
    harness.backend.unwatch(key);
    harness.backend.unwatch(low_key);
  }
}

#[test]
fn open_point_a_handle_opened_through_a_junction_watches_the_target() {
  let temp = scratch();
  let target = make_dir(temp.path().join("target"));
  let link = temp.path().join("link");
  junction(&link, &target);
  let mut harness = Harness::new();
  let key = harness.handle(&link);

  fs::write(target.join("f.txt"), "1").unwrap();
  harness.hints_until(|hint| hint.names(key, "f.txt"));
}

#[test]
fn open_point_a_change_behind_a_junction_is_reported_under_the_real_path() {
  let temp = scratch();
  let target = make_dir(temp.path().join("target"));
  let link = temp.path().join("link");
  junction(&link, &target);
  let mut harness = Harness::new();
  let key = harness.handle(temp.path());

  fs::write(link.join("f.txt"), "1").unwrap();
  let seen = harness.hints_until(|hint| hint.names(key, r"target\f.txt"));
  assert!(seen.iter().all(|hint| !hint.names(key, r"link\f.txt")), "{seen:?}");
}

/// A handle reports nothing for what is behind a junction below it.
#[test]
fn a_directory_behind_a_junction_below_a_handle_is_not_covered() {
  let temp = scratch();
  let top = make_dir(temp.path().join("top"));
  let plain = make_dir(top.join("plain").join("sub"));
  make_dir(temp.path().join("target").join("sub"));
  let link = top.join("link");
  junction(&link, &temp.path().join("target"));
  let behind = link.join("sub");
  let mut harness = Harness::new();
  harness.handle(&top);

  for dir in [&link, &behind] {
    let (_, coverage) = harness.watch(dir, Want::Contents);
    assert!(matches!(coverage, Coverage::NotWatched), "{}: {coverage:?}", dir.display());
  }
  let (key, coverage) = harness.watch(&plain, Want::Contents);
  assert!(matches!(coverage, Coverage::WatchedTree), "{coverage:?}");
  fs::write(plain.join("f.txt"), "1").unwrap();
  harness.hints_until(|hint| hint.names(key, "f.txt"));

  // The directory behind the junction and the one above it in one flush: the handle is opened
  // by the commit, and only then is the junction below it known. The key behind it is told,
  // and the ask that follows says why.
  let mut harness = Harness::new();
  let (top_key, key) = (DirKey::new(100, 0), DirKey::new(101, 0));
  for (key, dir) in [(top_key, &top), (key, &behind)] {
    let coverage = harness.backend.watch(key, dir, Want::Contents);
    assert!(matches!(coverage, Coverage::WatchedTree), "{coverage:?}");
  }
  harness.backend.commit();
  harness.hints_until(|hint| hint.names_the_directory(key));
  let coverage = harness.watch_again(key, &behind, Want::Contents);
  assert!(matches!(coverage, Coverage::NotWatched), "{coverage:?}");
  fs::write(top.join("f.txt"), "1").unwrap();
  harness.hints_until(|hint| hint.names(top_key, "f.txt"));
}

#[test]
fn a_directory_that_becomes_a_junction_below_a_handle_is_given_up() {
  let temp = scratch();
  let top = make_dir(temp.path().join("top"));
  let dir = make_dir(top.join("dir"));
  let target = make_dir(temp.path().join("target"));
  let mut harness = Harness::new();
  let top_key = harness.handle(&top);
  let (key, coverage) = harness.watch(&dir, Want::Contents);
  assert!(matches!(coverage, Coverage::WatchedTree), "{coverage:?}");

  fs::remove_dir(&dir).unwrap();
  junction(&dir, &target);
  let coverage = harness.watch_again(key, &dir, Want::Contents);
  assert!(matches!(coverage, Coverage::NotWatched), "{coverage:?}");

  // No hint carries the key that was given up.
  fs::write(top.join("f.txt"), "1").unwrap();
  let seen = harness.hints_until(|hint| hint.names(top_key, "f.txt"));
  assert!(seen.iter().all(|hint| hint.key() != Some(key)), "{seen:?}");
}

#[test]
fn open_point_a_change_made_through_a_short_name() {
  let temp = scratch();
  let long = temp.path().join("a long file name.txt");
  fs::write(&long, "1").unwrap();
  let Some(short) = short_name(&long) else { return };
  let mut harness = Harness::new();
  let key = harness.handle(temp.path());

  fs::write(temp.path().join(&short), "12").unwrap();
  // Windows may report the long name or the short one: the engine asks the disk how a name
  // it does not know is spelled, and finds the entry under its long name.
  let short = short.to_string_lossy();
  harness.hints_until(|hint| hint.names(key, "a long file name.txt") || hint.names(key, &short));
}

/// More directories than handles, and one of them is a junction: the others share a handle
/// above them, the directory behind the link keeps a handle of its own, and nothing is told
/// again and again. A change behind the link reaches its key, and one next to it reaches the
/// key of its directory.
#[test]
fn directories_above_the_limit_and_a_junction_come_to_rest() {
  let temp = scratch();
  let project = make_dir(temp.path().join("project"));
  let target = make_dir(temp.path().join("target"));
  let mut harness = Harness::new();
  let keys: Vec<DirKey> =
    (0..MAX_HANDLES).map(|n| harness.handle(&make_dir(project.join(format!("d{n}"))))).collect();
  let link = project.join("link");
  junction(&link, &target);
  let link_key = harness.handle(&link);
  // The engine would ask again for every key that is told. None is.
  let mut seen = Vec::new();
  while let Ok(Signal::Raw(raw)) = harness.signals.recv_timeout(Duration::from_millis(500)) {
    seen.extend(harness.hints_of(raw));
  }
  let is_a_loss_report = |hint: &Seen| match hint {
    Seen::Changed { rel, .. } => rel.as_os_str().is_empty(),
    Seen::Written { .. } => false,
    Seen::Lost => true,
  };
  assert!(!seen.iter().any(is_a_loss_report), "{seen:?}");
  for key in keys.iter().chain([&link_key]) {
    assert!(harness.backend.serves(*key));
  }

  fs::write(target.join("behind.txt"), "1").unwrap();
  harness.hints_until(|hint| hint.names(link_key, "behind.txt"));
  fs::write(project.join("d5").join("next.txt"), "1").unwrap();
  harness.hints_until(|hint| hint.names(keys[5], "next.txt"));
  harness.backend.unwatch(link_key);
  harness.backend.commit();
  fs::remove_dir(&link).unwrap();
}
