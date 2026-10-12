//! The parts of the Windows backend that make no Windows call: `place`, `Handles`, `Records` and
//! `translate`.

#![cfg(target_os = "windows")]

use std::path::PathBuf;

use pathwatch::backend::DirKey;

/// Where the name starts in a record: after three `u32` fields.
const FILE_NAME: usize = 12;

/// Builds a buffer the way the kernel does: each record but the last is padded to four bytes.
fn encode(records: &[(u32, &str)]) -> Vec<u8> {
  let mut bytes = Vec::new();
  for (at, &(action, name)) in records.iter().enumerate() {
    let name: Vec<u8> = name.encode_utf16().flat_map(u16::to_ne_bytes).collect();
    let last = at + 1 == records.len();
    let next = if last { 0 } else { (FILE_NAME + name.len()).next_multiple_of(4) };
    bytes.extend(u32::try_from(next).unwrap().to_ne_bytes());
    bytes.extend(action.to_ne_bytes());
    bytes.extend(u32::try_from(name.len()).unwrap().to_ne_bytes());
    bytes.extend(name);
    if !last {
      bytes.resize(bytes.len().next_multiple_of(4), 0);
    }
  }
  bytes
}

/// Where the name starts in a record of the longer form.
const EXTENDED_FILE_NAME: usize = 84;
const DIRECTORY: u32 = 0x10;

/// The longer records of `ReadDirectoryChangesExW`: times, sizes, the attributes and two file
/// ids between the action and the name, and each record padded to eight bytes.
fn encode_extended(records: &[(u32, &str, bool)]) -> Vec<u8> {
  let mut bytes = Vec::new();
  for (at, &(action, name, is_dir)) in records.iter().enumerate() {
    let name: Vec<u8> = name.encode_utf16().flat_map(u16::to_ne_bytes).collect();
    let last = at + 1 == records.len();
    let next = if last { 0 } else { (EXTENDED_FILE_NAME + name.len()).next_multiple_of(8) };
    bytes.extend(u32::try_from(next).unwrap().to_ne_bytes());
    bytes.extend(action.to_ne_bytes());
    // Four times and two sizes.
    bytes.extend([0xAB; 48]);
    bytes.extend((if is_dir { DIRECTORY | 0x2000 } else { 0x20 }).to_ne_bytes());
    // The tag of a reparse point, and two file ids.
    bytes.extend([0xCD; 20]);
    bytes.extend(u32::try_from(name.len()).unwrap().to_ne_bytes());
    bytes.extend(name);
    if !last {
      bytes.resize(bytes.len().next_multiple_of(8), 0);
    }
  }
  bytes
}

fn key(index: u32) -> DirKey {
  DirKey::new(index, 0)
}

/// The number that `key` made the key from.
fn index(of: DirKey) -> u32 {
  (0..10_000).find(|&index| key(index) == of).expect("a key that the test did not make")
}

/// An absolute path, from text with `/`.
fn dir(path: &str) -> PathBuf {
  format!("C:{}", path.replace('/', "\\")).into()
}

mod place {
  use std::path::PathBuf;

  use pathwatch::{
    backend::Want::{self, Contents, Names},
    windows::{MAX_HANDLES, place},
  };

  use super::{dir, index, key};

  /// The placement of these directories; the key of each one is its position in the list.
  fn placed(wanted: &[(&str, Want)], max: usize) -> Vec<(PathBuf, Vec<u32>)> {
    placed_with_links(wanted, max, &[])
  }

  /// `placed`, on a disk where these directories are junctions.
  fn placed_with_links(
    wanted: &[(&str, Want)],
    max: usize,
    links: &[&str],
  ) -> Vec<(PathBuf, Vec<u32>)> {
    let dirs: Vec<PathBuf> = wanted.iter().map(|(path, _)| dir(path)).collect();
    let wanted: Vec<_> =
      (0..).zip(wanted).zip(&dirs).map(|((at, (_, want)), dir)| (key(at), &**dir, *want)).collect();
    let links: Vec<PathBuf> = links.iter().map(|link| dir(link)).collect();
    let places = place(&wanted, max, &mut |path| links.iter().any(|link| link == path));
    places
      .into_iter()
      .map(|place| (place.dir, place.keys.into_iter().map(index).collect()))
      .collect()
  }

  /// A handle on `path` that serves these keys.
  fn at(path: &str, keys: &[u32]) -> (PathBuf, Vec<u32>) {
    (dir(path), keys.to_vec())
  }

  #[test]
  fn each_directory_has_its_own_handle_while_there_is_room() {
    assert_eq!(
      placed(&[("/p/b", Contents), ("/p/a", Contents), ("/q", Contents)], 3),
      [at("/p/a", &[1]), at("/p/b", &[0]), at("/q", &[2])]
    );
    assert_eq!(placed(&[], 3), []);
  }

  #[test]
  fn a_handle_serves_every_key_at_or_below_its_directory() {
    let wanted = [("/p/a/b/c", Contents), ("/p/a", Contents), ("/p/a/x", Names), ("/p/ab", Names)];
    // `/p/ab` is not below `/p/a`: paths are compared part by part.
    assert_eq!(placed(&wanted, 9), [at("/p/a", &[1, 0, 2])]);
    // Two keys can name one directory.
    assert_eq!(placed(&[("/p", Contents), ("/p", Names)], 9), [at("/p", &[0, 1])]);
  }

  #[test]
  fn a_directory_that_wants_names_only_gets_no_handle() {
    assert_eq!(placed(&[("/p", Names), ("/p/a", Contents), ("/q", Names)], 9), [at("/p/a", &[1])]);
    assert_eq!(placed(&[("/p", Names)], 9), []);
  }

  #[test]
  fn more_directories_than_handles_share_the_lowest_common_directories() {
    let wanted = [
      ("/p/a/1", Contents),
      ("/p/a/2", Contents),
      ("/p/b/1", Contents),
      ("/p/b/2", Contents),
      ("/q", Contents),
      ("/p", Names),
    ];
    assert_eq!(
      placed(&wanted, 5),
      [
        at("/p/a/1", &[0]),
        at("/p/a/2", &[1]),
        at("/p/b/1", &[2]),
        at("/p/b/2", &[3]),
        at("/q", &[4])
      ]
    );
    // Of two common directories that are equally deep, the first in path order is taken.
    assert_eq!(
      placed(&wanted, 4),
      [at("/p/a", &[0, 1]), at("/p/b/1", &[2]), at("/p/b/2", &[3]), at("/q", &[4])]
    );
    assert_eq!(placed(&wanted, 3), [at("/p/a", &[0, 1]), at("/p/b", &[2, 3]), at("/q", &[4])]);
    // The handle on `/p` also serves the key that only wants the names in `/p`.
    assert_eq!(placed(&wanted, 2), [at("/p", &[5, 0, 1, 2, 3]), at("/q", &[4])]);
    // `/p` and `/q` have only the top directory in common. The last in path order gets nothing.
    assert_eq!(placed(&wanted, 1), [at("/p", &[5, 0, 1, 2, 3])]);
    assert_eq!(placed(&wanted, 0), []);
  }

  #[test]
  fn a_deeper_common_directory_is_taken_before_a_higher_one() {
    let wanted = [("/a", Contents), ("/p/x/1", Contents), ("/p/x/2", Contents), ("/p/y", Contents)];
    assert_eq!(placed(&wanted, 3), [at("/a", &[0]), at("/p/x", &[1, 2]), at("/p/y", &[3])]);
    assert_eq!(placed(&wanted, 2), [at("/a", &[0]), at("/p", &[1, 2, 3])]);
    // Directories that go on together for a while meet in the lowest directory of them all.
    let wanted = [("/p/q/r/1", Contents), ("/p/q/r/2", Contents)];
    assert_eq!(placed(&wanted, 1), [at("/p/q/r", &[0, 1])]);
  }

  #[test]
  fn the_same_directories_give_the_same_placement_in_any_order() {
    let dirs: Vec<PathBuf> = ["/p/a/1", "/p/a/2", "/p/b/1", "/p/b/2", "/q", "/p/a"].map(dir).into();
    let wanted: Vec<_> = (0..).zip(&dirs).map(|(at, dir)| (key(at), &**dir, Contents)).collect();
    let mut turned = wanted.clone();
    turned.reverse();
    turned.swap(1, 4);
    for max in 0..=5 {
      assert_eq!(place(&wanted, max, &mut |_| false), place(&turned, max, &mut |_| false), "{max}");
    }
  }

  /// A handle reports nothing behind a junction below it: a directory with a link on the way
  /// down from the common directory keeps a handle of its own, and the others share without
  /// it. The handle of its own can stand below the shared one.
  #[test]
  fn a_directory_behind_a_link_is_not_joined_above_the_link() {
    let wanted = [("/p/a", Contents), ("/p/link", Contents), ("/p/b", Contents)];
    // Without the link, all three share `/p`.
    assert_eq!(placed(&wanted, 1), [at("/p", &[0, 2, 1])]);
    assert_eq!(
      placed_with_links(&wanted, 2, &["/p/link"]),
      [at("/p", &[0, 2]), at("/p/link", &[1])]
    );
    // A link above the directory, not the directory itself.
    let wanted = [("/p/a", Contents), ("/p/link/x", Contents), ("/p/link/y", Contents)];
    assert_eq!(
      placed_with_links(&wanted, 2, &["/p/link"]),
      [at("/p/a", &[0]), at("/p/link", &[1, 2])]
    );
    // The deepest handle at or above a directory serves it, and a key that wants names only
    // goes with it.
    let wanted =
      [("/p/a", Contents), ("/p/link", Contents), ("/p/b", Contents), ("/p/link/n", Names)];
    assert_eq!(
      placed_with_links(&wanted, 2, &["/p/link"]),
      [at("/p", &[0, 2]), at("/p/link", &[1, 3])]
    );
    // Directories that were joined below a link are not taken above it with their neighbours.
    let wanted =
      [("/p/a/b/c1", Contents), ("/p/a/b/c2", Contents), ("/p/a/d", Contents), ("/p/e", Contents)];
    assert_eq!(
      placed_with_links(&wanted, 3, &["/p/a/b"]),
      [at("/p/a/b", &[0, 1]), at("/p/a/d", &[2]), at("/p/e", &[3])]
    );
    assert_eq!(
      placed_with_links(&wanted, 2, &["/p/a/b"]),
      [at("/p", &[2, 3]), at("/p/a/b", &[0, 1])]
    );
    // Two links next to each other never share.
    let wanted = [
      ("/p/src/1", Contents),
      ("/p/src/2", Contents),
      ("/p/nm/a", Contents),
      ("/p/nm/b", Contents),
    ];
    assert_eq!(
      placed_with_links(&wanted, 3, &["/p/nm/a", "/p/nm/b"]),
      [at("/p/nm/a", &[2]), at("/p/nm/b", &[3]), at("/p/src", &[0, 1])]
    );
  }

  #[test]
  fn no_handle_goes_on_a_top_directory_or_a_home_directory() {
    // Nothing is shared there.
    assert_eq!(placed(&[("/a", Contents), ("/b", Contents)], 1), [at("/a", &[0])]);
    assert_eq!(
      placed(&[("/Users/a/x", Contents), ("/Users/b/x", Contents)], 1),
      [at("/Users/a/x", &[0])]
    );
    assert_eq!(
      placed(&[("/Users/me/a", Contents), ("/users/me/b", Contents)], 1),
      [at("/Users/me/a", &[0])]
    );
    // One directory below a home directory it is.
    assert_eq!(
      placed(&[("/Users/me/p/a", Contents), ("/Users/me/p/b", Contents)], 1),
      [at("/Users/me/p", &[0, 1])]
    );
    // A directory there gets no handle of its own either, in any letter case.
    let wanted = [("/", Contents), ("/Users", Contents), ("/users/Me", Contents)];
    assert_eq!(placed(&wanted, 9), []);
    // What is below it does.
    assert_eq!(
      placed(&[("/Users/me", Contents), ("/Users/me/p", Contents)], 9),
      [at("/Users/me/p", &[1])]
    );
    // `Users` is a name like any other further down, and so is a directory next to it.
    assert_eq!(
      placed(&[("/p/Users", Contents), ("/Usersx", Contents)], 9),
      [at("/Usersx", &[1]), at("/p/Users", &[0])]
    );
  }

  #[test]
  fn two_thousand_directories_fit_into_the_limit() {
    let packages = (0..1900).map(|n| format!("/p/node_modules/pkg{n}/lib"));
    let paths: Vec<String> = packages.chain((0..100).map(|n| format!("/p/src/dir{n}"))).collect();
    let wanted: Vec<(&str, Want)> = paths.iter().map(|path| (path.as_str(), Contents)).collect();
    let places = placed(&wanted, MAX_HANDLES);
    let sizes: Vec<(PathBuf, usize)> =
      places.into_iter().map(|(dir, keys)| (dir, keys.len())).collect();
    assert_eq!(sizes, [(dir("/p/node_modules"), 1900), (dir("/p/src"), 100)]);
  }

  #[test]
  fn only_as_many_directories_share_as_the_limit_asks_for() {
    // 62 directories that have nothing in common, and three that have.
    let mut paths: Vec<String> = (0..62).map(|n| format!("/d{n}")).collect();
    paths.extend(["/p/x/1", "/p/x/2", "/p/y"].map(String::from));
    let wanted: Vec<(&str, Want)> = paths.iter().map(|path| (path.as_str(), Contents)).collect();
    let dirs = |wanted: &[(&str, Want)]| -> Vec<PathBuf> {
      placed(wanted, MAX_HANDLES)
        .into_iter()
        .map(|(dir, _)| dir)
        .filter(|dir| dir.starts_with(super::dir("/p")))
        .collect()
    };
    // 65 directories: one pair shares, and 64 handles are left.
    assert_eq!(dirs(&wanted), [dir("/p/x"), dir("/p/y")]);
    // 64 directories fit as they are.
    assert_eq!(dirs(&wanted[1..]), [dir("/p/x/1"), dir("/p/x/2"), dir("/p/y")]);
    // 66 directories: all three share.
    let mut more = wanted.clone();
    more.push(("/d99", Contents));
    assert_eq!(dirs(&more), [dir("/p")]);
  }
}

mod handles {
  use std::{
    io,
    path::{Path, PathBuf},
  };

  use pathwatch::{
    backend::{Coverage, Want, is_watch_limit},
    windows::{Handles, MAX_HANDLES, System, Told, WatchId, place},
  };

  use super::{dir, index, key};

  #[derive(Debug, PartialEq, Eq)]
  enum Answer {
    Tree,
    /// A handle serves the key, which wants names only.
    Names,
    NotWatched,
    Limit,
    Failed,
  }

  /// The system of these tests: it writes down what `Handles` asks of it.
  #[derive(Default)]
  struct Disk {
    /// The handles that are open now, in the order in which they were opened.
    open: Vec<(WatchId, PathBuf)>,
    /// Every id that `open` was called with.
    ids: Vec<WatchId>,
    /// A handle on this directory cannot be opened.
    broken: Option<PathBuf>,
    /// The junctions.
    links: Vec<PathBuf>,
  }

  impl System for Disk {
    fn open(&mut self, id: WatchId, dir: &Path) -> io::Result<()> {
      self.ids.push(id);
      if self.broken.as_deref() == Some(dir) {
        return Err(io::ErrorKind::PermissionDenied.into());
      }
      self.open.push((id, dir.to_owned()));
      Ok(())
    }

    fn close(&mut self, id: WatchId) {
      let at = self.open.iter().position(|(open, _)| *open == id);
      self.open.remove(at.expect("a handle is closed that is not open"));
    }

    fn is_link(&mut self, path: &Path) -> bool {
      self.links.iter().any(|link| link == path)
    }
  }

  #[derive(Default)]
  struct Fake {
    handles: Handles,
    disk: Disk,
    /// The keys that were told that no handle serves them any more.
    lost: Vec<u32>,
    /// The keys that got no handle before and that a new handle reports now.
    covered: Vec<u32>,
  }

  impl Fake {
    fn watch(&mut self, index: u32, path: &str, want: Want) -> Answer {
      let (key, dir) = (key(index), dir(path));
      let (mut coverage, told) = self.handles.watch(key, &dir, want, &mut self.disk);
      self.told(told);
      let mut settled = self.handles.commit(&mut self.disk);
      // A key that got no handle at the commit is asked again, as the engine does: that
      // answer says why.
      let lost = settled.lost.iter().position(|&lost| lost == key);
      if let Some(at) = lost {
        settled.lost.swap_remove(at);
      }
      self.told(settled);
      if lost.is_some() {
        let (answer, told) = self.handles.watch(key, &dir, want, &mut self.disk);
        coverage = answer;
        self.told(told);
      }
      match coverage {
        Coverage::WatchedTree => Answer::Tree,
        Coverage::NotWatched => Answer::NotWatched,
        Coverage::Failed(error) if is_watch_limit(&error) => Answer::Limit,
        Coverage::Failed(_) => Answer::Failed,
        Coverage::Watched => Answer::Names,
      }
    }

    /// Asks for the contents of `path`; a handle is opened on it. Gives the id of the handle.
    fn open(&mut self, index: u32, path: &str) -> WatchId {
      assert_eq!(self.watch(index, path, Want::Contents), Answer::Tree, "{path}");
      self.id(path)
    }

    /// `unwatch`, and the commit that the engine makes after a flush that gave a watch back.
    fn unwatch(&mut self, index: u32) {
      self.unwatch_only(index);
      self.commit();
    }

    /// `unwatch` alone, as the engine calls it for each directory of a flush.
    fn unwatch_only(&mut self, index: u32) {
      let told = self.handles.unwatch(key(index), &mut self.disk);
      self.told(told);
    }

    fn commit(&mut self) {
      let told = self.handles.commit(&mut self.disk);
      self.told(told);
    }

    /// Keeps what a call told, and checks what holds after every call: no handle is below
    /// another, and there are never too many.
    /// Keeps what a call told, and checks what holds after every call: no handle is below
    /// another unless a link lies between them, and there are never too many.
    fn told(&mut self, told: Told) {
      self.lost.extend(told.lost.into_iter().map(index));
      self.covered.extend(told.covered.into_iter().map(index));
      for (_, above) in &self.disk.open {
        for (_, below) in &self.disk.open {
          if below == above || !below.starts_with(above) {
            continue;
          }
          let on_the_way = below.ancestors().take_while(|path| *path != above.as_path());
          let is_link = |path: &Path| self.disk.links.iter().any(|link| link == path);
          let through_a_link = on_the_way.into_iter().any(is_link);
          assert!(
            through_a_link,
            "{} is below {} in {:?}",
            below.display(),
            above.display(),
            self.disk.open
          );
        }
      }
      assert!(self.disk.open.len() <= MAX_HANDLES);
    }

    fn died(&mut self, id: WatchId) -> Vec<u32> {
      self.disk.open.retain(|(open, _)| *open != id);
      let died = io::Error::other("the read failed");
      let mut keys: Vec<u32> = self.handles.closed(id, &died).into_iter().map(index).collect();
      keys.sort_unstable();
      keys
    }

    /// The directories of the open handles.
    fn dirs(&self) -> Vec<PathBuf> {
      let mut dirs: Vec<PathBuf> = self.disk.open.iter().map(|(_, dir)| dir.clone()).collect();
      dirs.sort();
      dirs
    }

    /// The id of the handle on `path`.
    fn id(&self, path: &str) -> WatchId {
      let on_path = self.disk.open.iter().find(|(_, open)| *open == dir(path));
      on_path.unwrap_or_else(|| panic!("no handle is on {path}: {:?}", self.disk.open)).0
    }

    /// The keys that were told since the last call.
    fn lost(&mut self) -> Vec<u32> {
      self.lost.sort_unstable();
      std::mem::take(&mut self.lost)
    }
  }

  /// The engine polls a directory that wants names only, until a handle stands above it.
  #[test]
  fn a_key_without_a_handle_is_told_when_a_handle_comes_to_stand_above_it() {
    let mut fake = Fake::default();
    assert_eq!(fake.watch(200, "/p", Want::Names), Answer::NotWatched);
    assert_eq!(fake.watch(201, "/p/1/deep", Want::Names), Answer::NotWatched);
    assert_eq!(fake.watch(202, "/q", Want::Names), Answer::NotWatched);
    assert_eq!(fake.watch(203, "/p/2/gone", Want::Names), Answer::NotWatched);
    fake.unwatch(203);
    let count = u32::try_from(MAX_HANDLES).unwrap();
    for index in 0..count {
      fake.open(index, &format!("/p/{index}"));
    }
    // A handle of its own on `/p/1` reports the directory below it.
    assert_eq!(std::mem::take(&mut fake.covered), [201]);

    // One more: the handles move up to `/p`.
    assert_eq!(fake.watch(100, "/r", Want::Contents), Answer::Tree);
    assert_eq!(fake.dirs(), dirs(["/p", "/r"]));
    fake.covered.sort_unstable();
    assert_eq!(std::mem::take(&mut fake.covered), [200, 201]);

    // The engine asks again and is served. Nothing is told twice.
    assert_eq!(fake.watch(200, "/p", Want::Names), Answer::Names);
    assert_eq!(fake.watch(201, "/p/1/deep", Want::Names), Answer::Names);
    assert_eq!(fake.watch(101, "/s", Want::Contents), Answer::Tree);
    assert_eq!(fake.covered, []);
  }

  /// A handle reports nothing for what is behind a junction below it.
  #[test]
  fn a_key_behind_a_link_is_not_served() {
    let mut fake = Fake::default();
    fake.disk.links = vec![dir("/p/link")];
    fake.open(0, "/p");
    for behind in ["/p/link", "/p/link/sub"] {
      assert_eq!(fake.watch(1, behind, Want::Contents), Answer::NotWatched);
    }
    assert_eq!(fake.watch(2, "/p/plain/sub", Want::Contents), Answer::Tree);
    assert_eq!(fake.disk.open.len(), 1);
    assert_eq!(fake.lost(), []);

    let mut fake = Fake::default();
    fake.disk.links = vec![dir("/p/link")];
    for (index, path) in [(0, "/p"), (1, "/p/link/sub")] {
      let (coverage, _) =
        fake.handles.watch(key(index), &dir(path), Want::Contents, &mut fake.disk);
      assert!(matches!(coverage, Coverage::WatchedTree), "{coverage:?}");
    }
    let told = fake.handles.commit(&mut fake.disk);
    assert_eq!(told.lost, [key(1)]);
    assert_eq!(fake.watch(1, "/p/link/sub", Want::Contents), Answer::NotWatched);
    assert_eq!(fake.dirs(), dirs(["/p"]));
  }

  /// A key that got no handle is asked again at once. When the handle that comes to serve it
  /// then stands above a link, it is not watched: that is no limit.
  #[test]
  fn a_refused_key_that_comes_to_lie_behind_a_link_is_not_watched() {
    let mut fake = Fake::default();
    fake.disk.links = vec![dir("/p/link")];
    fake.disk.broken = Some(dir("/p/link/sub"));
    let (coverage, _) =
      fake.handles.watch(key(1), &dir("/p/link/sub"), Want::Contents, &mut fake.disk);
    assert!(matches!(coverage, Coverage::WatchedTree), "{coverage:?}");
    assert_eq!(fake.handles.commit(&mut fake.disk).lost, [key(1)]);
    // Before the engine asks again, the directory above is watched in the same flush.
    let (coverage, _) = fake.handles.watch(key(0), &dir("/p"), Want::Contents, &mut fake.disk);
    assert!(matches!(coverage, Coverage::WatchedTree), "{coverage:?}");
    assert_eq!(fake.watch(1, "/p/link/sub", Want::Contents), Answer::NotWatched);
    assert_eq!(fake.dirs(), dirs(["/p"]));
    assert_eq!(fake.lost(), []);
  }

  /// More directories than handles, and one of them is a junction: the others share a handle
  /// above, and the directory behind the link keeps one of its own. Nothing is told, the key
  /// of the directory above is covered, and the commits that follow move nothing: the handles
  /// come to rest.
  #[test]
  fn a_directory_behind_a_link_keeps_a_handle_of_its_own_above_the_limit() {
    let mut fake = Fake::default();
    fake.disk.links = vec![dir("/p/link")];
    assert_eq!(fake.watch(200, "/p", Want::Names), Answer::NotWatched);
    let count = u32::try_from(MAX_HANDLES).unwrap();
    for index in 0..count {
      fake.open(index, &format!("/p/{index}"));
    }
    assert_eq!(fake.watch(100, "/p/link", Want::Contents), Answer::Tree);
    assert_eq!(fake.dirs(), dirs(["/p", "/p/link"]));
    assert_eq!(fake.lost(), []);
    assert_eq!(std::mem::take(&mut fake.covered), [200]);
    let opened = fake.disk.ids.len();
    // The engine asks again for the key that is covered now, and commits at each flush.
    assert_eq!(fake.watch(200, "/p", Want::Names), Answer::Names);
    for _ in 0..3 {
      fake.commit();
    }
    assert_eq!(fake.disk.ids.len(), opened);
    assert_eq!(fake.dirs(), dirs(["/p", "/p/link"]));
    assert_eq!(fake.lost(), []);
    // A key below the directory behind the link is served by the handle of its own.
    assert_eq!(fake.watch(101, "/p/link/sub", Want::Names), Answer::Names);
    assert_eq!(fake.disk.ids.len(), opened);
    // Without it the others fit as they are, and the keys that only the shared handle served
    // are told.
    fake.unwatch(100);
    assert_eq!(fake.disk.open.len(), MAX_HANDLES);
    assert!(!fake.dirs().contains(&dir("/p")) && !fake.dirs().contains(&dir("/p/link")));
    assert_eq!(fake.lost(), [101, 200]);
  }

  /// A key below a directory that has a handle anyway, with a link between: it is not
  /// watched, and the handles stay where they are.
  #[test]
  fn a_key_behind_a_link_below_a_shared_handle_is_not_watched() {
    let mut fake = Fake::default();
    fake.disk.links = vec![dir("/p/5/link")];
    let count = u32::try_from(MAX_HANDLES).unwrap();
    for index in 0..count {
      fake.open(index, &format!("/p/{index}"));
    }
    assert_eq!(fake.watch(100, "/q", Want::Contents), Answer::Tree);
    assert_eq!(fake.dirs(), dirs(["/p", "/q"]));
    let opened = fake.disk.ids.len();
    assert_eq!(fake.watch(101, "/p/5/link/sub", Want::Contents), Answer::NotWatched);
    assert_eq!(fake.watch(101, "/p/5/link/sub", Want::Contents), Answer::NotWatched);
    assert_eq!(fake.disk.ids.len(), opened);
    assert_eq!(fake.dirs(), dirs(["/p", "/q"]));
    assert_eq!(fake.lost(), []);
  }

  /// `unwatch` of the keys of a shared handle moves nothing: the handle goes with its last key,
  /// and where the handles stand is worked out once at the next commit, not for each key.
  #[test]
  fn unwatch_moves_the_handles_once_at_the_commit() {
    let mut fake = Fake::default();
    let count = u32::try_from(MAX_HANDLES).unwrap();
    for index in 0..count {
      fake.open(index, &format!("/p/{index}"));
    }
    assert_eq!(fake.watch(count, "/p/last", Want::Contents), Answer::Tree);
    assert_eq!(fake.dirs(), dirs(["/p"]));
    let opened = fake.disk.ids.len();
    // Two keys go: with the commit, each of the rest gets a handle of its own, opened once.
    for index in [3, 4] {
      fake.unwatch_only(index);
    }
    assert_eq!(fake.dirs(), dirs(["/p"]));
    assert_eq!(fake.disk.ids.len(), opened);
    fake.commit();
    assert_eq!(fake.disk.open.len(), MAX_HANDLES - 1);
    assert_eq!(fake.disk.ids.len(), opened + MAX_HANDLES - 1);
    assert_eq!(fake.lost(), []);
    // All of them go, one by one: nothing is opened, and the last one closes its handle.
    let mut fake = Fake::default();
    for index in 0..count {
      fake.open(index, &format!("/p/{index}"));
    }
    assert_eq!(fake.watch(count, "/p/last", Want::Contents), Answer::Tree);
    let opened = fake.disk.ids.len();
    for index in 0..=count {
      fake.unwatch_only(index);
      assert_eq!(fake.disk.open.len(), usize::from(index < count), "{index}");
    }
    assert_eq!(fake.disk.ids.len(), opened);
    fake.commit();
    assert_eq!(fake.disk.open, []);
  }

  /// A failed settle keeps a handle for the keys that still use it. When the last of them is a
  /// key that wants names only, `unwatch` of it closes the handle: none stays open with no key.
  #[test]
  fn a_handle_kept_by_a_failed_settle_closes_with_its_last_key() {
    let mut fake = Fake::default();
    let id = fake.open(0, "/p");
    assert_eq!(fake.watch(1, "/p/sub", Want::Names), Answer::Names);
    fake.disk.broken = Some(dir("/q"));
    let (coverage, _) = fake.handles.watch(key(2), &dir("/q"), Want::Contents, &mut fake.disk);
    assert!(matches!(coverage, Coverage::WatchedTree), "{coverage:?}");
    // The settle of this `unwatch` fails on `/q`: the handle on `/p` stays for the key that
    // wants names.
    fake.unwatch(0);
    assert_eq!(fake.disk.open, [(id, dir("/p"))]);
    assert_eq!(fake.lost(), [2]);
    fake.unwatch(1);
    assert_eq!(fake.disk.open, []);
  }

  fn dirs<const N: usize>(paths: [&str; N]) -> Vec<PathBuf> {
    paths.map(dir).into()
  }

  #[test]
  fn an_ancestor_that_nothing_covers_is_not_watched() {
    let mut fake = Fake::default();
    assert_eq!(fake.watch(0, "/p", Want::Names), Answer::NotWatched);
    fake.unwatch(0);
    fake.open(1, "/p/a/b");
    assert_eq!(fake.watch(2, "/p/a", Want::Names), Answer::NotWatched);
    assert_eq!(fake.dirs(), dirs(["/p/a/b"]));
    assert_eq!(fake.watch(0, "/p", Want::Contents), Answer::Tree);
    assert_eq!(fake.dirs(), dirs(["/p"]));
  }

  #[test]
  fn a_directory_at_or_below_a_handle_gets_no_handle() {
    let mut fake = Fake::default();
    let id = fake.open(0, "/p/a");
    for (index, want, answer) in
      [(1, Want::Names, Answer::Names), (2, Want::Contents, Answer::Tree)]
    {
      assert_eq!(fake.watch(index, "/p/a/b/c", want), answer);
      fake.unwatch(index);
      assert_eq!(fake.disk.open, [(id, dir("/p/a"))]);
    }
    assert_eq!(fake.disk.ids, [id]);
    // A key for the directory of the handle itself replaces the handle, as every such request does.
    assert_eq!(fake.watch(3, "/p/a", Want::Contents), Answer::Tree);
    assert_eq!(fake.dirs(), dirs(["/p/a"]));
    assert_ne!(fake.id("/p/a"), id);
    assert_eq!(fake.lost(), []);
  }

  #[test]
  fn a_path_is_compared_part_by_part() {
    let mut fake = Fake::default();
    fake.open(0, "/p/a");
    fake.open(1, "/p/ab");
    fake.open(2, "/p/b/a");
    assert_eq!(fake.dirs(), dirs(["/p/a", "/p/ab", "/p/b/a"]));
  }

  #[test]
  fn a_handle_above_replaces_the_handles_below() {
    let mut fake = Fake::default();
    let low = fake.open(0, "/p/a");
    let deep = fake.open(1, "/p/b/c");
    let other = fake.open(2, "/q");

    let high = fake.open(3, "/p");
    assert_eq!(fake.disk.open, [(other, dir("/q")), (high, dir("/p"))]);
    assert!(![low, deep, other].contains(&high));
    // The keys of the closed handles are not told: the new handle reported before the old
    // ones stopped.
    assert_eq!(fake.lost(), []);

    assert_eq!(fake.watch(0, "/p/a", Want::Contents), Answer::Tree);
    assert_eq!(fake.watch(1, "/p/b/c", Want::Contents), Answer::Tree);
    fake.unwatch(0);
    fake.unwatch(1);
    assert_eq!(fake.watch(4, "/q/x", Want::Contents), Answer::Tree);
    assert_eq!(fake.disk.open, [(other, dir("/q")), (high, dir("/p"))]);
    fake.unwatch(2);
    // The key below it is still there, so its handle only moves down.
    assert_eq!(fake.dirs(), dirs(["/p", "/q/x"]));
    assert_eq!(fake.lost(), []);
  }

  #[test]
  fn the_key_of_a_handle_gets_a_new_handle_when_it_asks_again() {
    let mut fake = Fake::default();
    let first = fake.open(0, "/p");
    let second = fake.open(0, "/p");
    assert_ne!(first, second);
    assert_eq!(fake.disk.open, [(second, dir("/p"))]);
    assert_eq!(fake.lost(), []);
    assert_eq!(fake.died(first), []);
    assert_eq!(fake.died(second), [0]);
  }

  #[test]
  fn a_request_below_a_shared_handle_opens_nothing_and_one_for_its_directory_replaces_it() {
    let mut fake = Fake::default();
    fake.open(0, "/p");
    assert_eq!(fake.watch(1, "/p/a", Want::Contents), Answer::Tree);
    assert_eq!(fake.watch(2, "/p/b", Want::Names), Answer::Names);
    let shared = fake.id("/p");
    for _ in 0..3 {
      assert_eq!(fake.watch(1, "/p/a", Want::Contents), Answer::Tree);
      assert_eq!(fake.watch(2, "/p/b", Want::Contents), Answer::Tree);
    }
    assert_eq!(fake.disk.open, [(shared, dir("/p"))]);

    assert_eq!(fake.watch(0, "/p", Want::Contents), Answer::Tree);
    assert_ne!(fake.id("/p"), shared);
    // The other keys of the handle are not told, and they are keys of the new handle.
    assert_eq!(fake.lost(), []);
    assert_eq!(fake.died(shared), []);
    let new = fake.id("/p");
    assert_eq!(fake.died(new), [0, 1, 2]);
  }

  /// Put off to the commit, the engine would take the key for watched until the commit says
  /// no, at every poll.
  #[test]
  fn a_key_that_got_no_handle_is_answered_at_once_the_next_time() {
    let mut fake = Fake::default();
    fake.disk.broken = Some(dir("/p"));
    assert_eq!(fake.watch(0, "/p", Want::Contents), Answer::Failed);
    let (coverage, told) = fake.handles.watch(key(0), &dir("/p"), Want::Contents, &mut fake.disk);
    assert!(matches!(coverage, Coverage::Failed(_)), "{coverage:?}");
    assert_eq!(told.lost, []);

    // When the handle can be opened, the key is like every other one again.
    fake.disk.broken = None;
    assert_eq!(fake.watch(0, "/p", Want::Contents), Answer::Tree);
    fake.unwatch(0);
    let (coverage, _) = fake.handles.watch(key(0), &dir("/p"), Want::Contents, &mut fake.disk);
    assert!(matches!(coverage, Coverage::WatchedTree), "{coverage:?}");
    assert_eq!(fake.disk.open, []);
  }

  #[test]
  fn an_id_is_never_given_out_twice() {
    let mut fake = Fake::default();
    fake.disk.broken = Some(dir("/failed"));
    for round in 0..3 {
      assert_eq!(fake.watch(9, "/failed", Want::Contents), Answer::Failed);
      fake.open(round, "/p");
      fake.open(round, "/p");
      fake.unwatch(round);
    }
    // The open that fails is tried twice the first time: by the commit, and again for the
    // answer. After that it is tried once.
    let count = fake.disk.ids.len();
    assert_eq!(count, 10);
    fake.disk.ids.sort_by_key(|id| id.0);
    fake.disk.ids.dedup();
    assert_eq!(fake.disk.ids.len(), count);
  }

  #[test]
  fn a_handle_that_could_not_be_opened_changes_nothing() {
    let mut fake = Fake::default();
    fake.disk.broken = Some(dir("/p"));
    let low = fake.open(0, "/p/a");
    assert_eq!(fake.watch(1, "/p", Want::Contents), Answer::Failed);
    assert_eq!(fake.disk.open, [(low, dir("/p/a"))]);
    // The key that failed is forgotten.
    fake.unwatch(1);
    assert_eq!(fake.watch(2, "/p/a/b", Want::Contents), Answer::Tree);
    assert_eq!(fake.lost(), []);
    assert_eq!(fake.died(low), [0, 2]);
  }

  #[test]
  fn a_handle_that_could_not_be_replaced_leaves_its_key_unwatched() {
    let mut fake = Fake::default();
    let id = fake.open(0, "/p");
    assert_eq!(fake.watch(1, "/p/a", Want::Contents), Answer::Tree);
    fake.disk.broken = Some(dir("/p"));
    // After `Failed` nothing is watched for the key that asked; the other key gets its own handle.
    assert_eq!(fake.watch(0, "/p", Want::Contents), Answer::Failed);
    assert_eq!(fake.dirs(), dirs(["/p/a"]));
    assert_eq!(fake.lost(), []);
    assert_eq!(fake.died(id), []);
  }

  #[test]
  fn no_handle_is_opened_above_the_limit() {
    let mut fake = Fake::default();
    // Directories that have only the top directory in common: no handle can serve two of them.
    for index in 0..u32::try_from(MAX_HANDLES).unwrap() {
      fake.open(index, &format!("/d{index}"));
    }
    let before = fake.disk.open.clone();
    assert_eq!(fake.watch(100, "/q", Want::Contents), Answer::Limit);
    fake.unwatch(100);
    assert_eq!(fake.disk.open, before);
    assert_eq!(fake.lost(), []);
    // Nothing covers an ancestor, with or without room.
    assert_eq!(fake.watch(100, "/q", Want::Names), Answer::NotWatched);

    // A handle that takes the place of another still fits.
    fake.open(0, "/d0");
    assert_eq!(fake.disk.open.len(), MAX_HANDLES);
    assert_eq!(fake.watch(100, "/q", Want::Contents), Answer::Limit);

    // Who gets none is a matter of path order, not of who came first: a directory that is
    // before the others takes the handle of the last one, and the key of that one is told.
    assert_eq!(fake.watch(101, "/a", Want::Contents), Answer::Tree);
    assert_eq!(fake.disk.open.len(), MAX_HANDLES);
    assert_eq!(fake.lost(), [9]);
    assert_eq!(fake.watch(9, "/d9", Want::Contents), Answer::Limit);
    assert_eq!(fake.lost(), []);
  }

  #[test]
  fn there_is_room_again_when_a_handle_goes() {
    let mut fake = Fake::default();
    for index in 0..u32::try_from(MAX_HANDLES).unwrap() {
      fake.open(index, &format!("/d{index}"));
    }
    assert_eq!(fake.watch(100, "/q", Want::Contents), Answer::Limit);
    fake.unwatch(7);
    assert_eq!(fake.watch(100, "/q", Want::Contents), Answer::Tree);
    assert_eq!(fake.disk.open.len(), MAX_HANDLES);
  }

  #[test]
  fn more_directories_than_handles_share_a_handle() {
    let mut fake = Fake::default();
    let count = u32::try_from(MAX_HANDLES).unwrap();
    for index in 0..count {
      fake.open(index, &format!("/p/{index}"));
    }
    // One more: all of them have `/p` in common. The directory elsewhere keeps its handle.
    assert_eq!(fake.watch(100, "/q", Want::Contents), Answer::Tree);
    assert_eq!(fake.dirs(), dirs(["/p", "/q"]));
    assert_eq!(fake.lost(), []);
    // More of them open nothing.
    assert_eq!(fake.watch(101, "/p/new", Want::Contents), Answer::Tree);
    assert_eq!(fake.dirs(), dirs(["/p", "/q"]));
    assert_eq!(fake.lost(), []);

    // With room for all, each one has its own handle again.
    fake.unwatch(101);
    assert_eq!(fake.dirs(), dirs(["/p", "/q"]));
    fake.unwatch(100);
    assert_eq!(fake.disk.open.len(), MAX_HANDLES);
    assert!(fake.dirs().contains(&dir("/p/5")));
    assert_eq!(fake.lost(), []);
  }

  #[test]
  fn only_the_directories_that_have_to_share_move() {
    let mut fake = Fake::default();
    for index in 0..62 {
      fake.open(index, &format!("/d{index}"));
    }
    let first = fake.open(70, "/p/x/1");
    let other = fake.open(71, "/p/y");
    // The 65th directory: it shares with the one next to it, and nothing else moves.
    assert_eq!(fake.watch(72, "/p/x/2", Want::Contents), Answer::Tree);
    assert_eq!(fake.disk.open.len(), MAX_HANDLES);
    let shared = fake.id("/p/x");
    assert_eq!(fake.id("/p/y"), other);
    assert_eq!(fake.lost(), []);
    assert_eq!(fake.died(first), []);

    // The 66th: now all three share.
    fake.open(63, "/d63");
    assert_eq!(fake.dirs().last(), Some(&dir("/p")));
    assert_eq!(fake.disk.open.len(), MAX_HANDLES);
    assert_eq!(fake.lost(), []);
    assert_eq!(fake.died(shared), []);
  }

  #[test]
  fn a_shared_handle_that_cannot_be_opened_leaves_all_as_it_was() {
    let mut fake = Fake::default();
    fake.disk.broken = Some(dir("/p"));
    for index in 0..u32::try_from(MAX_HANDLES).unwrap() {
      fake.open(index, &format!("/p/{index}"));
    }
    let before = fake.disk.open.clone();
    // The handle of `/q` was opened for this, and is closed again.
    assert_eq!(fake.watch(100, "/q", Want::Contents), Answer::Failed);
    assert_eq!(fake.disk.open, before);
    assert_eq!(fake.lost(), []);

    fake.disk.broken = None;
    assert_eq!(fake.watch(100, "/q", Want::Contents), Answer::Tree);
    assert_eq!(fake.dirs(), dirs(["/p", "/q"]));
  }

  #[test]
  fn a_key_that_wants_names_is_served_while_a_handle_is_above_it() {
    let mut fake = Fake::default();
    fake.open(0, "/p/a");
    fake.open(1, "/p");
    assert_eq!(fake.lost(), []);
    assert_eq!(fake.watch(2, "/p/b", Want::Names), Answer::Names);
    // The handle moves down to the one directory that wants the contents: the key that wanted
    // names only has no handle above it any more, and is told.
    fake.unwatch(1);
    assert_eq!(fake.dirs(), dirs(["/p/a"]));
    assert_eq!(fake.lost(), [2]);
    assert_eq!(fake.watch(2, "/p/b", Want::Names), Answer::NotWatched);
    // A key that wants names only never moves a handle.
    assert_eq!(fake.watch(3, "/p/a/x", Want::Names), Answer::Names);
    fake.unwatch(3);
    assert_eq!(fake.lost(), []);
  }

  #[test]
  fn a_key_that_wants_more_later_keeps_its_handle() {
    let mut fake = Fake::default();
    let id = fake.open(0, "/p");
    assert_eq!(fake.watch(1, "/p/a", Want::Names), Answer::Names);
    assert_eq!(fake.watch(1, "/p/a", Want::Contents), Answer::Tree);
    assert_eq!(fake.disk.open, [(id, dir("/p"))]);
    // It wants the contents now, also when it is asked for less (rule 5): the handle moves to
    // it when the key above goes.
    assert_eq!(fake.watch(1, "/p/a", Want::Names), Answer::Tree);
    fake.unwatch(0);
    assert_eq!(fake.dirs(), dirs(["/p/a"]));
  }

  #[test]
  fn no_handle_for_a_home_directory() {
    let mut fake = Fake::default();
    for path in ["/", "/Users", "/Users/me"] {
      assert_eq!(fake.watch(0, path, Want::Contents), Answer::NotWatched, "{path}");
    }
    assert_eq!(fake.disk.ids, []);
    fake.open(1, "/Users/me/p");
  }

  #[test]
  fn unwatch_closes_a_handle_with_its_last_key() {
    let mut fake = Fake::default();
    let id = fake.open(0, "/p");
    assert_eq!(fake.watch(1, "/p/a", Want::Contents), Answer::Tree);

    fake.unwatch(1);
    assert_eq!(fake.disk.open, [(id, dir("/p"))]);
    assert_eq!(fake.watch(1, "/p/a", Want::Contents), Answer::Tree);
    // The key below is still watched: it gets a handle on its own directory.
    fake.unwatch(0);
    assert_eq!(fake.dirs(), dirs(["/p/a"]));
    assert_eq!(fake.lost(), []);
    fake.unwatch(0);
    fake.unwatch(1);
    assert_eq!(fake.dirs(), dirs([]));
    assert_eq!(fake.died(id), []);
  }

  #[test]
  fn the_keys_below_a_handle_that_went_away_get_handles_of_their_own() {
    let mut fake = Fake::default();
    fake.open(0, "/p/a");
    fake.open(1, "/p");
    fake.open(2, "/q");
    assert_eq!(fake.watch(3, "/p/b", Want::Names), Answer::Names);
    assert_eq!(fake.watch(4, "/q/c", Want::Contents), Answer::Tree);
    assert_eq!(fake.lost(), []);

    fake.unwatch(1);
    assert_eq!(fake.dirs(), dirs(["/p/a", "/q"]));
    // The engine asks again for each key that is told.
    assert_eq!(fake.lost(), [3]);
    assert_eq!(fake.watch(0, "/p/a", Want::Contents), Answer::Tree);
    assert_eq!(fake.watch(3, "/p/b", Want::Names), Answer::NotWatched);
    assert_eq!(fake.watch(4, "/q/c", Want::Contents), Answer::Tree);
    assert_eq!(fake.dirs(), dirs(["/p/a", "/q"]));
  }

  #[test]
  fn a_dead_handle_is_forgotten_once_with_its_keys() {
    let mut fake = Fake::default();
    let id = fake.open(0, "/p");
    assert_eq!(fake.watch(1, "/p/a", Want::Contents), Answer::Tree);

    assert_eq!(fake.died(id), [0, 1]);
    assert_eq!(fake.died(id), []);
    fake.unwatch(0);
    assert_eq!(fake.dirs(), dirs([]));
    fake.open(1, "/p/a");
  }

  #[test]
  fn a_key_is_covered_when_a_new_handle_takes_the_place_of_a_dead_one() {
    let mut fake = Fake::default();
    let id = fake.open(0, "/p");
    assert_eq!(fake.watch(1, "/p/a", Want::Contents), Answer::Tree);

    assert_eq!(fake.died(id), [0, 1]);
    fake.open(0, "/p");
    assert_eq!(fake.watch(1, "/p/a", Want::Contents), Answer::Tree);
    assert_eq!(fake.dirs(), dirs(["/p"]));
  }

  #[test]
  fn a_key_that_is_no_longer_covered_gets_a_handle_when_it_asks_again() {
    let mut fake = Fake::default();
    let id = fake.open(0, "/p");
    assert_eq!(fake.watch(1, "/p/a", Want::Contents), Answer::Tree);
    assert_eq!(fake.watch(2, "/p/b", Want::Names), Answer::Names);

    assert_eq!(fake.died(id), [0, 1, 2]);
    fake.open(1, "/p/a");
    assert_eq!(fake.watch(2, "/p/b", Want::Names), Answer::NotWatched);
    assert_eq!(fake.lost(), []);
  }

  #[test]
  fn the_handles_are_always_where_the_placement_puts_them() {
    // 216 directories, three levels deep.
    let level = |a: u32, b: u32, c: u32| {
      if c == 0 { format!("/p/{a}/{b}") } else { format!("/p/{a}/{b}/{c}") }
    };
    let paths: Vec<String> =
      (0..4).flat_map(|a| (0..6).flat_map(move |b| (0..9).map(move |c| level(a, b, c)))).collect();
    let mut watched = vec![false; paths.len()];
    let mut fake = Fake::default();
    let mut seed = 1_u64;
    for step in 0..4000 {
      seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
      let at = usize::try_from(seed >> 40).unwrap() % paths.len();
      let index = u32::try_from(at).unwrap();
      // Two of three steps watch a directory, so most of the time there are more than fit.
      watched[at] = !(seed >> 20).is_multiple_of(3);
      if watched[at] {
        assert_eq!(fake.watch(index, &paths[at], Want::Contents), Answer::Tree, "step {step}");
      } else {
        fake.unwatch(index);
      }

      let dirs: Vec<PathBuf> = paths.iter().map(|path| dir(path)).collect();
      let wanted: Vec<_> = (0..)
        .zip(&dirs)
        .zip(&watched)
        .filter(|(_, watched)| **watched)
        .map(|((at, dir), _)| (key(at), &**dir, Want::Contents))
        .collect();
      let placed: Vec<PathBuf> =
        place(&wanted, MAX_HANDLES, &mut |_| false).into_iter().map(|place| place.dir).collect();
      assert_eq!(fake.dirs(), placed, "step {step}");
    }
  }
}

mod records {
  use pathwatch::windows::{Malformed, Records};

  use super::{FILE_NAME, encode};

  const ADDED: u32 = 1;
  const REMOVED: u32 = 2;
  const MODIFIED: u32 = 3;

  /// One record with every field given by hand.
  fn raw(next: u32, action: u32, name_len: u32, name: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend(next.to_ne_bytes());
    bytes.extend(action.to_ne_bytes());
    bytes.extend(name_len.to_ne_bytes());
    bytes.extend(name);
    bytes
  }

  fn parse(bytes: &[u8]) -> Vec<Result<(u32, String), Malformed>> {
    Records::new(bytes)
      .map(|record| {
        record.map(|record| {
          (record.action, String::from_utf16(&record.name().collect::<Vec<_>>()).unwrap())
        })
      })
      .collect()
  }

  fn named(action: u32, name: &str) -> (u32, String) {
    (action, name.to_owned())
  }

  #[test]
  fn the_longer_records_tell_what_is_a_directory() {
    let bytes = super::encode_extended(&[
      (ADDED, "new.txt", false),
      (MODIFIED, r"sub\deep", true),
      (MODIFIED, "é and 日本.txt", false),
    ]);
    let seen: Vec<_> = Records::extended(&bytes)
      .map(|record| {
        let record = record.unwrap();
        (
          record.action,
          String::from_utf16(&record.name().collect::<Vec<_>>()).unwrap(),
          record.is_dir,
        )
      })
      .collect();
    let expected = [
      (ADDED, "new.txt".to_owned(), Some(false)),
      (MODIFIED, r"sub\deep".to_owned(), Some(true)),
      (MODIFIED, "é and 日本.txt".to_owned(), Some(false)),
    ];
    assert_eq!(seen, expected);
    // The short records tell nothing about it.
    assert!(Records::new(&encode(&[(ADDED, "a")])).all(|record| record.unwrap().is_dir.is_none()));
    // A record that is cut off, in the fixed part and in the name.
    for len in [40, super::EXTENDED_FILE_NAME + 3] {
      assert_eq!(Records::extended(&bytes[..len]).next(), Some(Err(Malformed)));
    }
  }

  #[test]
  fn no_bytes_hold_no_record() {
    assert_eq!(parse(&[]), []);
  }

  #[test]
  fn several_records() {
    let bytes = encode(&[(ADDED, "a"), (MODIFIED, r"sub\b.txt"), (REMOVED, "ünï")]);
    assert_eq!(
      parse(&bytes),
      [Ok(named(ADDED, "a")), Ok(named(MODIFIED, r"sub\b.txt")), Ok(named(REMOVED, "ünï"))]
    );
  }

  #[test]
  fn a_name_of_no_length() {
    assert_eq!(
      parse(&encode(&[(ADDED, ""), (REMOVED, "b")])),
      [Ok(named(ADDED, "")), Ok(named(REMOVED, "b"))]
    );
  }

  #[test]
  fn the_last_record_ends_the_walk() {
    let mut bytes = encode(&[(ADDED, "a")]);
    bytes.extend(encode(&[(REMOVED, "not reached")]));
    assert_eq!(parse(&bytes), [Ok(named(ADDED, "a"))]);
  }

  #[test]
  fn the_bytes_need_no_alignment() {
    let records = encode(&[(ADDED, "a"), (MODIFIED, "bc")]);
    for shift in 0..4 {
      let mut bytes = vec![0; shift];
      bytes.extend(&records);
      assert_eq!(parse(&bytes[shift..]), [Ok(named(ADDED, "a")), Ok(named(MODIFIED, "bc"))]);
    }
  }

  #[test]
  fn records_packed_without_padding() {
    let name: Vec<u8> = "abc".encode_utf16().flat_map(u16::to_ne_bytes).collect();
    let mut bytes = raw(18, ADDED, 6, &name);
    bytes.extend(raw(0, REMOVED, 6, &name));
    assert_eq!(parse(&bytes), [Ok(named(ADDED, "abc")), Ok(named(REMOVED, "abc"))]);
  }

  #[test]
  fn a_header_that_runs_past_the_end() {
    let bytes = encode(&[(ADDED, "a")]);
    for len in 1..FILE_NAME {
      assert_eq!(parse(&bytes[..len]), [Err(Malformed)]);
    }
  }

  #[test]
  fn a_name_that_runs_past_the_end() {
    let bytes = encode(&[(ADDED, "abcd")]);
    assert_eq!(parse(&bytes[..bytes.len() - 1]), [Err(Malformed)]);
    assert_eq!(parse(&raw(0, ADDED, u32::MAX - 1, &[0; 8])), [Err(Malformed)]);
  }

  #[test]
  fn a_name_with_an_odd_length() {
    assert_eq!(parse(&raw(0, ADDED, 3, &[b'a', 0, b'b', 0])), [Err(Malformed)]);
  }

  #[test]
  fn a_next_record_past_the_end() {
    let good = encode(&[(ADDED, "a"), (REMOVED, "b")]);
    let cut_before_second = &good[..16];
    assert_eq!(parse(cut_before_second), [Ok(named(ADDED, "a")), Err(Malformed)]);
    assert_eq!(parse(&raw(4096, ADDED, 2, &[b'a', 0, 0, 0])), [Err(Malformed)]);
    assert_eq!(parse(&raw(u32::MAX, ADDED, 2, &[b'a', 0, 0, 0])), [Err(Malformed)]);
  }

  #[test]
  fn a_next_record_inside_this_one() {
    let name: Vec<u8> = "abcd".encode_utf16().flat_map(u16::to_ne_bytes).collect();
    for next in [1, 4, 12, 19] {
      assert_eq!(parse(&raw(next, ADDED, 8, &name)), [Err(Malformed)]);
    }
  }

  #[test]
  fn nothing_comes_after_a_malformed_record() {
    let mut records = Records::new(&[0; 5]);
    assert_eq!(records.next(), Some(Err(Malformed)));
    assert_eq!(records.next(), None);
  }
}

mod translate {
  use std::{
    io,
    path::{MAIN_SEPARATOR_STR, Path, PathBuf},
  };

  use pathwatch::{
    backend::{Coverage, Hint, Want},
    windows::{Handles, Raw, System, WatchId, translate},
  };

  use super::{dir, encode, index, key};

  const ADDED: u32 = 1;
  const REMOVED: u32 = 2;
  const MODIFIED: u32 = 3;
  const RENAMED_OLD_NAME: u32 = 4;
  const RENAMED_NEW_NAME: u32 = 5;

  #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
  enum Seen {
    Changed(u32, PathBuf),
    Written(u32, PathBuf),
    Lost,
  }

  fn changed(index: u32, rel: &str) -> Seen {
    Seen::Changed(index, rel.into())
  }

  fn written(index: u32, rel: &str) -> Seen {
    Seen::Written(index, rel.into())
  }

  fn failed() -> io::Result<Vec<u8>> {
    Err(io::ErrorKind::PermissionDenied.into())
  }

  /// Watches `path`. Gives the handle that was opened for it, if one was, and the handles closed.
  fn watch(
    handles: &mut Handles,
    index: u32,
    path: &str,
    want: Want,
  ) -> (Option<WatchId>, Vec<WatchId>) {
    let mut calls = Calls::default();
    let (coverage, _) = handles.watch(key(index), &dir(path), want, &mut calls);
    handles.commit(&mut calls);
    let served = matches!(coverage, Coverage::WatchedTree | Coverage::Watched);
    assert!(served && handles.serves(key(index)), "{path}: {coverage:?}");
    (calls.opened, calls.closed)
  }

  /// What `Handles` asked of the system.
  #[derive(Default)]
  struct Calls {
    opened: Option<WatchId>,
    closed: Vec<WatchId>,
  }

  impl System for Calls {
    fn open(&mut self, id: WatchId, _dir: &Path) -> io::Result<()> {
      self.opened = Some(id);
      Ok(())
    }

    fn close(&mut self, id: WatchId) {
      self.closed.push(id);
    }

    fn is_link(&mut self, _path: &Path) -> bool {
      false
    }
  }

  /// Asks for the contents of `path`, and a handle is opened for it.
  fn open(handles: &mut Handles, index: u32, path: &str) -> WatchId {
    watch(handles, index, path, Want::Contents).0.expect("no handle was opened")
  }

  /// What a read on the handle `watch` on `path` gave.
  fn read(watch: WatchId, path: &str, result: io::Result<Vec<u8>>) -> Raw {
    Raw::Read { watch, dir: dir(path), extended: false, result }
  }

  /// Records whose names have `/` between their parts: Windows puts `\` there.
  fn records(records: &[(u32, &str)]) -> Vec<u8> {
    let names: Vec<String> =
      records.iter().map(|(_, name)| name.replace('/', MAIN_SEPARATOR_STR)).collect();
    let records: Vec<(u32, &str)> =
      records.iter().zip(&names).map(|(&(action, _), name)| (action, &**name)).collect();
    encode(&records)
  }

  fn hints(handles: &mut Handles, raw: Raw) -> Vec<Seen> {
    let mut seen = Vec::new();
    translate(handles, raw, &mut |hint| {
      seen.push(match hint {
        Hint::Changed { dir, rel } => Seen::Changed(index(dir), rel.to_owned()),
        Hint::Written { dir, rel } => Seen::Written(index(dir), rel.to_owned()),
        Hint::Lost => Seen::Lost,
      });
    });
    seen
  }

  /// For the hints of one loss: they come in no order.
  fn sorted(mut seen: Vec<Seen>) -> Vec<Seen> {
    seen.sort();
    seen
  }

  /// Windows says "modified" for a directory whose entry changed: the entry has its own record.
  #[test]
  fn a_modified_record_of_a_directory_is_dropped() {
    let mut handles = Handles::default();
    let id = open(&mut handles, 7, "/p");
    let bytes = super::encode_extended(&[
      (MODIFIED, "sub", true),
      (ADDED, r"sub\new.txt", false),
      (MODIFIED, r"sub\new.txt", false),
      (ADDED, "made", true),
      (REMOVED, "gone", true),
    ]);
    let raw = Raw::Read { watch: id, dir: dir("/p"), extended: true, result: Ok(bytes) };
    assert_eq!(
      hints(&mut handles, raw),
      [
        changed(7, r"sub\new.txt"),
        written(7, r"sub\new.txt"),
        changed(7, "made"),
        changed(7, "gone"),
      ]
    );
  }

  #[test]
  fn a_modified_record_is_written_and_the_others_are_changed() {
    let mut handles = Handles::default();
    let id = open(&mut handles, 7, "/p");
    let bytes = encode(&[
      (ADDED, "new.txt"),
      (MODIFIED, r"sub\deep\a.js"),
      (REMOVED, "old.txt"),
      (RENAMED_OLD_NAME, "from"),
      (RENAMED_NEW_NAME, "to"),
      (0, "no action"),
      (99, "unknown action"),
    ]);
    assert_eq!(
      hints(&mut handles, read(id, "/p", Ok(bytes))),
      [
        changed(7, "new.txt"),
        written(7, r"sub\deep\a.js"),
        changed(7, "old.txt"),
        changed(7, "from"),
        changed(7, "to"),
        changed(7, "no action"),
        changed(7, "unknown action"),
      ]
    );
  }

  #[test]
  fn no_bytes_are_a_loss_for_each_key_of_the_handle() {
    let mut handles = Handles::default();
    let id = open(&mut handles, 0, "/p");
    watch(&mut handles, 1, "/p/sub", Want::Names);
    watch(&mut handles, 2, "/p/sub/deep", Want::Contents);
    let other = open(&mut handles, 3, "/q");
    let lost = hints(&mut handles, read(id, "/p", Ok(Vec::new())));
    assert_eq!(sorted(lost), [changed(0, ""), changed(1, ""), changed(2, "")]);
    assert_eq!(hints(&mut handles, read(other, "/q", Ok(Vec::new()))), [changed(3, "")]);
    // The handle reads on.
    let bytes = encode(&[(ADDED, "a")]);
    assert_eq!(hints(&mut handles, read(id, "/p", Ok(bytes))), [changed(0, "a")]);
  }

  #[test]
  fn an_error_forgets_the_handle_and_names_the_directory_of_each_key() {
    let mut handles = Handles::default();
    let id = open(&mut handles, 0, "/p");
    watch(&mut handles, 1, "/p/sub", Want::Contents);
    let dead = hints(&mut handles, read(id, "/p", failed()));
    assert_eq!(sorted(dead), [changed(0, ""), changed(1, "")]);
    assert_eq!(hints(&mut handles, read(id, "/p", failed())), []);
    // The keys are forgotten too: a later read gives no hint, and a key that asks gets a new one.
    assert_eq!(hints(&mut handles, read(id, "/p", Ok(records(&[(ADDED, "sub/a")])))), []);
    open(&mut handles, 0, "/p");
  }

  /// A handle that dies as soon as it is opened, twice in a row: the key is told once, the
  /// engine polls the directory, and a later `watch` tries a handle again. A handle that
  /// delivered records was no such death.
  #[test]
  fn a_handle_that_dies_twice_is_a_failure() {
    let mut handles = Handles::default();
    let mut calls = Calls::default();
    let first = open(&mut handles, 0, "/p");
    assert_eq!(hints(&mut handles, read(first, "/p", failed())), [changed(0, "")]);
    // The engine asks again: a directory that was removed and made again gets its handle at
    // once.
    let second = open(&mut handles, 0, "/p");
    assert_ne!(first, second);
    assert_eq!(hints(&mut handles, read(second, "/p", failed())), [changed(0, "")]);
    let (coverage, _) = handles.watch(key(0), &dir("/p"), Want::Contents, &mut calls);
    let failed_with = match coverage {
      Coverage::Failed(error) => error.kind(),
      other => panic!("{other:?}"),
    };
    assert_eq!(failed_with, io::ErrorKind::PermissionDenied);
    assert_eq!(calls.opened, None);

    // The ask at the next poll tries again, and a handle that delivered starts afresh.
    let third = open(&mut handles, 0, "/p");
    let bytes = records(&[(ADDED, "a")]);
    assert_eq!(hints(&mut handles, read(third, "/p", Ok(bytes))), [changed(0, "a")]);
    assert_eq!(hints(&mut handles, read(third, "/p", failed())), [changed(0, "")]);
    open(&mut handles, 0, "/p");
  }

  #[test]
  fn the_end_of_a_handle_that_was_unwatched_gives_no_hint() {
    let mut handles = Handles::default();
    let id = open(&mut handles, 0, "/p");
    let mut calls = Calls::default();
    handles.unwatch(key(0), &mut calls);
    assert_eq!((calls.opened, calls.closed), (None, vec![id]));
    assert_eq!(hints(&mut handles, read(id, "/p", failed())), []);
    // What the handle read before it was closed gives no hint either: no key holds it.
    assert_eq!(hints(&mut handles, read(id, "/p", Ok(encode(&[(ADDED, "a")])))), []);
  }

  #[test]
  fn a_read_of_a_replaced_handle_still_gives_hints_with_its_key() {
    let mut handles = Handles::default();
    let low = open(&mut handles, 0, "/p/a");
    let (high, closed) = watch(&mut handles, 1, "/p", Want::Contents);
    let high = high.unwrap();
    assert_eq!(closed, [low]);
    let bytes = encode(&[(MODIFIED, "f.js")]);
    assert_eq!(
      hints(&mut handles, read(low, "/p/a", Ok(bytes))),
      [written(0, "f.js"), written(1, "a/f.js")]
    );
    // Its key still stands at its directory, so the loss has a place.
    assert_eq!(hints(&mut handles, read(low, "/p/a", Ok(Vec::new()))), [changed(0, "")]);

    // The end of the replaced handle does not touch the handle that took its place.
    assert_eq!(hints(&mut handles, read(low, "/p/a", failed())), []);
    let bytes = records(&[(ADDED, "a/g.js"), (ADDED, "h.js")]);
    assert_eq!(
      hints(&mut handles, read(high, "/p", Ok(bytes))),
      [changed(0, "g.js"), changed(1, "a/g.js"), changed(1, "h.js")]
    );
  }

  /// Every key whose directory holds the path gets the record, the deepest first (rule 7).
  #[test]
  fn a_record_goes_to_every_key_that_holds_its_path() {
    let mut handles = Handles::default();
    let id = open(&mut handles, 0, "/p");
    watch(&mut handles, 1, "/p/a", Want::Contents);
    watch(&mut handles, 2, "/p/a/b", Want::Names);
    // Two keys for one directory: each can see the change.
    watch(&mut handles, 3, "/p/c", Want::Names);
    watch(&mut handles, 4, "/p/c", Want::Names);
    let bytes = records(&[
      (ADDED, "x.js"),
      (MODIFIED, "a/x.js"),
      (REMOVED, "a/b/x.js"),
      (MODIFIED, "a/b/deep/x.js"),
      (ADDED, "ab/x.js"),
    ]);
    assert_eq!(
      hints(&mut handles, read(id, "/p", Ok(bytes))),
      [
        changed(0, "x.js"),
        written(1, "x.js"),
        written(0, "a/x.js"),
        changed(2, "x.js"),
        changed(1, "b/x.js"),
        changed(0, "a/b/x.js"),
        written(2, "deep/x.js"),
        written(1, "b/deep/x.js"),
        written(0, "a/b/deep/x.js"),
        changed(0, "ab/x.js"),
      ]
    );
    let two = hints(&mut handles, read(id, "/p", Ok(records(&[(MODIFIED, "c/x.js")]))));
    assert_eq!(sorted(two), [written(0, "c/x.js"), written(3, "x.js"), written(4, "x.js")]);
  }

  #[test]
  fn a_record_for_the_directory_of_a_key_itself() {
    let mut handles = Handles::default();
    let id = open(&mut handles, 0, "/p");
    watch(&mut handles, 1, "/p/a", Want::Contents);
    // Windows also says "modified" for a directory when an entry in it changes: that is dropped
    // for every key, in any letter case of the name.
    let bytes = records(&[
      (REMOVED, "a"),
      (MODIFIED, "a"),
      (MODIFIED, "A"),
      (RENAMED_NEW_NAME, "a"),
      (MODIFIED, "a/sub"),
    ]);
    assert_eq!(
      hints(&mut handles, read(id, "/p", Ok(bytes))),
      [
        changed(1, ""),
        changed(0, "a"),
        changed(1, ""),
        changed(0, "a"),
        written(1, "sub"),
        written(0, "a/sub"),
      ]
    );
  }

  /// The two letter cases of a drive are one disk, and two top nodes of the tree: the key
  /// above reaches the path of the other case only when it is told too.
  #[test]
  fn a_key_with_the_drive_letter_in_the_other_case_gets_its_records_with_the_key_above() {
    let mut handles = Handles::default();
    let id = open(&mut handles, 0, "/p");
    let mut calls = Calls::default();
    let lower = PathBuf::from(r"c:\p\src");
    let (coverage, _) = handles.watch(key(1), &lower, Want::Contents, &mut calls);
    assert!(matches!(coverage, Coverage::WatchedTree), "{coverage:?}");
    assert_eq!(calls.opened, None);
    let bytes = records(&[(ADDED, "src/a.js"), (MODIFIED, "src/a.js")]);
    assert_eq!(
      hints(&mut handles, read(id, "/p", Ok(bytes))),
      [changed(1, "a.js"), changed(0, "src/a.js"), written(1, "a.js"), written(0, "src/a.js")]
    );
  }

  /// NTFS folds more than ASCII letters: a key in the other case of such a letter shares the
  /// handle and gets the records.
  #[test]
  fn a_key_in_the_other_case_of_a_letter_outside_ascii_gets_its_records() {
    let mut handles = Handles::default();
    let id = open(&mut handles, 0, "/p");
    watch(&mut handles, 1, "/p/café", Want::Contents);
    watch(&mut handles, 2, "/p/CAFÉ", Want::Contents);
    let told = hints(&mut handles, read(id, "/p", Ok(records(&[(ADDED, "café/x.js")]))));
    assert_eq!(sorted(told), [changed(0, "café/x.js"), changed(1, "x.js"), changed(2, "x.js")]);
    let lost = hints(&mut handles, read(id, "/p", Ok(Vec::new())));
    assert_eq!(sorted(lost), [changed(0, ""), changed(1, ""), changed(2, "")]);
  }

  #[test]
  fn a_record_that_no_key_holds_is_dropped() {
    let mut handles = Handles::default();
    // What a handle on `/p` reads when it is shared by keys below it, with no key on `/p`.
    let id = open(&mut handles, 0, "/p/a");
    open(&mut handles, 1, "/p/b");
    let bytes = records(&[
      (ADDED, "x.js"),
      (MODIFIED, "a/x.js"),
      (MODIFIED, "c/x.js"),
      (REMOVED, "b/sub/x.js"),
      (REMOVED, "b"),
      (ADDED, "c"),
    ]);
    assert_eq!(
      hints(&mut handles, read(id, "/p", Ok(bytes))),
      [written(0, "x.js"), changed(1, "sub/x.js"), changed(1, "")]
    );
  }

  #[test]
  fn a_key_that_is_spelled_in_another_letter_case_gets_its_records() {
    let mut handles = Handles::default();
    let id = open(&mut handles, 0, "/p/SRC/Lib");
    open(&mut handles, 1, "/p/other");
    // The record has the names of the disk.
    let bytes = records(&[(MODIFIED, "src/lib/A.js"), (ADDED, "src/lib"), (ADDED, "srç/lib/a.js")]);
    assert_eq!(
      hints(&mut handles, read(id, "/p", Ok(bytes))),
      [written(0, "A.js"), changed(0, "")]
    );
    let lost = hints(&mut handles, read(id, "/P", Ok(Vec::new())));
    assert_eq!(sorted(lost), [changed(0, ""), changed(1, "")]);

    // Two spellings of one directory are two watched paths, and each has its events, however
    // the record spells the way.
    watch(&mut handles, 2, "/p/src", Want::Contents);
    open(&mut handles, 3, "/p/SRC");
    let told = |handles: &mut Handles, record: &str| {
      let bytes = records(&[(ADDED, record)]);
      sorted(hints(handles, read(id, "/p", Ok(bytes))))
    };
    let deep = || changed(0, "a.js");
    assert_eq!(
      told(&mut handles, "SRC/lib/a.js"),
      [deep(), changed(2, "lib/a.js"), changed(3, "lib/a.js")]
    );
    assert_eq!(
      told(&mut handles, "SRC/Lib/a.js"),
      [deep(), changed(2, "Lib/a.js"), changed(3, "Lib/a.js")]
    );
    assert_eq!(
      told(&mut handles, "src/lib/a.js"),
      [deep(), changed(2, "lib/a.js"), changed(3, "lib/a.js")]
    );
    assert_eq!(told(&mut handles, "src/b.js"), [changed(2, "b.js"), changed(3, "b.js")]);
  }

  #[test]
  fn a_malformed_record_is_lost_notifications() {
    let mut handles = Handles::default();
    let id = open(&mut handles, 0, "/p");
    let mut bytes = encode(&[(ADDED, "a"), (REMOVED, "b"), (ADDED, "c")]);
    let in_the_second_record = 24;
    bytes.truncate(in_the_second_record);
    assert_eq!(hints(&mut handles, read(id, "/p", Ok(bytes))), [changed(0, "a"), Seen::Lost]);
  }

  #[test]
  fn a_record_without_a_name_names_the_directory_itself() {
    let mut handles = Handles::default();
    let id = open(&mut handles, 0, "/p");
    let bytes = encode(&[(REMOVED, ""), (MODIFIED, "a")]);
    assert_eq!(hints(&mut handles, read(id, "/p", Ok(bytes))), [changed(0, ""), written(0, "a")]);
  }

  #[test]
  fn a_short_name_names_the_directory_that_holds_it() {
    let mut handles = Handles::default();
    let id = open(&mut handles, 0, "/p");
    let bytes = encode(&[
      (MODIFIED, "LONGFI~1.TXT"),
      (MODIFIED, r"sub\deep\LONGFI~1.TXT"),
      (ADDED, r"sub\PROGRA~1\deep\a.js"),
      (MODIFIED, r"sub\not~short.txt"),
    ]);
    assert_eq!(
      hints(&mut handles, read(id, "/p", Ok(bytes))),
      [
        changed(0, ""),
        changed(0, r"sub\deep"),
        changed(0, "sub"),
        written(0, r"sub\not~short.txt"),
      ]
    );
  }

  #[test]
  fn a_short_name_that_no_key_holds_is_a_loss_for_the_keys_below_it() {
    let mut handles = Handles::default();
    // As above: what a shared handle on `/p` reads, with no key on `/p`.
    let id = open(&mut handles, 0, "/p/long name/a");
    open(&mut handles, 1, "/p/other");
    open(&mut handles, 2, "/q");
    // `LONGNA~1` can be the directory on the way to either key.
    let bytes = encode(&[(MODIFIED, r"LONGNA~1\a\x.js")]);
    let lost = hints(&mut handles, read(id, "/p", Ok(bytes)));
    assert_eq!(sorted(lost), [changed(0, ""), changed(1, "")]);
  }

  #[test]
  fn the_keys_that_lost_their_handle_are_told() {
    let mut handles = Handles::default();
    assert_eq!(
      hints(&mut handles, Raw::Lost(vec![key(3), key(5)])),
      [changed(3, ""), changed(5, "")]
    );
  }
}
