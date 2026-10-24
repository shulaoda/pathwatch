//! The tree alone: the arena, the table and the way from a hint to a mark, with Unix paths
//! (on every OS: the tree joins names by their text, and a path is compared part by part).

use std::{
  ffi::OsStr,
  iter, mem,
  path::{Path, PathBuf},
  time::{Duration, Instant},
};

use pathwatch::{
  backend::{Hint, NodeKey, Want},
  fs::DirId,
  tree::{Content, Depth, Dir, Fingerprint, Kind, Mark, Tree, Watch},
};

fn child(tree: &mut Tree, parent: NodeKey, name: &str) -> NodeKey {
  assert_eq!(tree.child(parent, OsStr::new(name)), None);
  tree.insert_child(parent, OsStr::new(name))
}

/// Puts a directory at the node.
fn on_disk(tree: &mut Tree, key: NodeKey, complete: bool) {
  let id = DirId { volume: 1, inode: u64::from(key.index()), birth: 0 };
  tree[key].kind = Kind::Dir(Dir { watch: Watch::Watched, ..Dir::new(id, complete) });
}

/// `/p/r` is a watched directory with `sub` in it, `/p/f.js` a watched file, and
/// `/p/gone/deep/x.js` a watched path below a chain that is not on disk.
struct Fixture {
  tree: Tree,
  top: NodeKey,
  p: NodeKey,
  r: NodeKey,
  sub: NodeKey,
  gone: NodeKey,
  deep: NodeKey,
  f: NodeKey,
}

fn fixture() -> Fixture {
  let mut tree = Tree::new();
  let top = child(&mut tree, Tree::ROOT, "/");
  let p = child(&mut tree, top, "p");
  let r = child(&mut tree, p, "r");
  let sub = child(&mut tree, r, "sub");
  let gone = child(&mut tree, p, "gone");
  let deep = child(&mut tree, gone, "deep");
  let x = child(&mut tree, deep, "x.js");
  let f = child(&mut tree, p, "f.js");
  for key in [top, p] {
    on_disk(&mut tree, key, false);
  }
  for key in [r, sub] {
    on_disk(&mut tree, key, true);
  }
  for key in [r, f, x] {
    assert!(tree.set_path(key, true));
  }
  tree.check_links().unwrap();
  Fixture { tree, top, p, r, sub, gone, deep, f }
}

/// Every mark as `path of the directory|name|d` (deep) or `w` (written), sorted.
fn marks(tree: &Tree) -> Vec<String> {
  let mut all = Vec::new();
  for (key, node) in tree.nodes() {
    let Some(mark) = node.mark() else { continue };
    let flags =
      format!("{}{}", if mark.deep { "d" } else { "" }, if mark.written { "w" } else { "" });
    let parent = tree.parent(key).map(|parent| tree.path(parent)).unwrap_or_default();
    all.push(format!("{}|{}|{flags}", shown(&parent), tree.name(key).display()));
  }
  all.sort();
  all
}

/// The marks of a flush (`Tree::take_marked`): level, directory, node and time of each.
fn taken(tree: &mut Tree) -> Vec<(usize, NodeKey, NodeKey, Option<Instant>)> {
  let mut marks = Vec::new();
  tree.take_marked(&mut marks);
  marks.into_iter().map(|mark| (mark.level, mark.parent, mark.key, mark.since)).collect()
}

/// The path as text, with `/` between the names on every OS.
fn shown(path: &Path) -> String {
  path.display().to_string().replace('\\', "/")
}

/// Takes every mark away, as a flush does.
fn clear_marks(tree: &mut Tree) {
  for (key, _) in mem::take(&mut tree.marked) {
    tree.take_mark(key);
    tree.prune(key);
  }
}

fn changed(dir: NodeKey, rel: &str, written: bool) -> Hint<'_> {
  let rel = Path::new(rel);
  if written { Hint::Written { dir, rel, new: false } } else { Hint::Changed { dir, rel } }
}

#[test]
fn paths_are_built_from_the_names() {
  let Fixture { tree, top, sub, deep, .. } = fixture();
  assert_eq!(tree.path(Tree::ROOT), Path::new(""));
  assert_eq!(tree.path(top), Path::new("/"));
  assert_eq!(tree.path(sub), Path::new("/p/r/sub"));
  assert_eq!(tree.path(deep), Path::new("/p/gone/deep"));

  // A buffer is used again, whatever it held.
  let mut path = PathBuf::from("/something/else");
  tree.path_into(sub, &mut path);
  assert_eq!(path, Path::new("/p/r/sub"));
  tree.path_into(Tree::ROOT, &mut path);
  assert_eq!(path, Path::new(""));
}

/// Windows only: the names are joined with the separator of the OS, and `C:\` ends with one
/// there alone.
#[cfg(windows)]
#[test]
fn a_listed_name_that_reads_as_a_drive_stays_in_its_directory() {
  let mut tree = Tree::new();
  let top = tree.insert_child(Tree::ROOT, OsStr::new(r"C:\"));
  let dir = tree.insert_child(top, OsStr::new("dir"));
  for name in ["C:", "Q:evil", "a:b", "plain.txt"] {
    let key = tree.insert_child(dir, OsStr::new(name));
    let path = tree.path(key);
    assert_eq!(path.as_os_str(), OsStr::new(&format!(r"C:\dir\{name}")), "{name}");
    assert!(path.starts_with(r"C:\dir"), "{name}: {}", path.display());
    let mut popped = path.clone();
    assert!(popped.pop());
    assert_eq!(popped, Path::new(r"C:\dir"), "{name}");
  }
  assert_eq!(tree.path(top), Path::new(r"C:\"));
}

/// A child is found by its parent and its name. The list and the table stay right while the
/// table grows and the slots are used again.
#[test]
fn children_come_and_go() {
  let mut tree = Tree::new();
  let top = child(&mut tree, Tree::ROOT, "/");
  // The same name under another parent is another node.
  let other = child(&mut tree, Tree::ROOT, "/other");
  let twin = child(&mut tree, other, "0-1");
  assert_eq!(tree.child(Tree::ROOT, OsStr::new("/")), Some(top));
  assert_eq!(tree.parent(Tree::ROOT), None);
  let mut live: Vec<(String, NodeKey)> = Vec::new();
  let mut highest = top.index();
  for round in 0..4 {
    for number in 0..500 {
      let name = format!("{round}-{number}");
      let key = child(&mut tree, top, &name);
      highest = highest.max(key.index());
      live.push((name, key));
    }
    // Every third node goes, also the newest and the oldest.
    let mut kept = Vec::new();
    for (number, (name, key)) in live.drain(..).enumerate() {
      if number % 3 == 0 {
        tree.prune(key);
        assert_eq!(tree.child(top, OsStr::new(&name)), None);
      } else {
        kept.push((name, key));
      }
    }
    live = kept;
    tree.check_links().unwrap();
    // The newest child comes first.
    let newest_first: Vec<NodeKey> = live.iter().rev().map(|(_, key)| *key).collect();
    assert_eq!(tree.children(top).collect::<Vec<_>>(), newest_first);
    let walk = iter::successors(tree.children(top).next(), |&at| tree.next_sibling(at));
    assert!(walk.eq(tree.children(top)));
    for (name, key) in &live {
      assert_eq!(tree.child(top, OsStr::new(name)), Some(*key));
      assert_eq!(tree.child(other, OsStr::new(name)), (name == "0-1").then_some(twin));
      assert_eq!(tree.name(*key), OsStr::new(name));
      assert_eq!(tree.parent(*key), Some(top));
      assert_eq!(tree.children(*key).count(), 0);
    }
  }
  // No slot is lost: no key is beyond the most nodes that lived at one time.
  assert!(highest < 4 + 500 + 500 * 2 / 3 + 500 * 4 / 9 + 500 * 8 / 27 + 4);
}

/// Needed: registered, something at its path, a mark, or a `Delete` still to tell.
#[test]
fn a_node_is_freed_when_nothing_needs_it() {
  let Fixture { mut tree, r, sub, gone, deep, f, .. } = fixture();
  // Registered: a watched path with nothing at it, and the chain to a watched path.
  for key in [f, gone, deep] {
    tree.prune(key);
    assert!(tree.get(key).is_some());
  }
  // A directory goes when it is gone from the disk.
  tree.prune(sub);
  assert!(tree.get(sub).is_some());
  tree[sub].kind = Kind::Missing;
  tree.prune(sub);
  assert!(tree.get(sub).is_none());

  let file = child(&mut tree, r, "a.js");
  let fingerprint = Fingerprint { modified: 0, size: 1, id: 1 };
  tree[file].kind = Kind::File(fingerprint);
  tree.mark(file, Mark::default());
  tree[file].kind = Kind::Missing;
  tree.prune(file);
  assert!(tree.get(file).is_some(), "the mark waits");
  assert!(tree.take_mark(file).is_some());
  tree.prune(file);
  assert!(tree.get(file).is_none());
  assert_eq!(tree.take_mark(file).map(|mark| mark.deep), None);

  // A file that is gone, while the handler still believes in it: its `Delete` is due.
  let told = child(&mut tree, r, "b.js");
  tree[told].set_told(true);
  tree.prune(told);
  assert!(tree.get(told).is_some(), "the handler has to hear of it");
  tree[told].set_told(false);
  tree.prune(told);
  assert!(tree.get(told).is_none());
  tree.check_links().unwrap();
}

#[test]
fn what_is_registered() {
  let Fixture { mut tree, top, p, r, sub, gone, deep, f } = fixture();
  for key in [Tree::ROOT, top, p, gone, deep] {
    assert!(tree[key].is_ancestor() && tree[key].is_registered());
  }
  for key in [r, f] {
    assert!(tree[key].is_path() && tree[key].is_registered() && !tree[key].is_ancestor());
  }
  assert!(!tree[sub].is_registered());

  assert!(!tree.set_path(r, true));
  let below = child(&mut tree, sub, "x.js");
  assert!(tree.set_path(below, true));
  assert!(tree[r].is_path() && tree[r].is_ancestor());
  assert!(tree[sub].is_ancestor() && !tree[sub].is_path());
  tree.check_links().unwrap();

  // A node above another watched path stays registered.
  assert!(tree.set_path(r, false));
  assert!(!tree.set_path(r, false));
  assert!(!tree[r].is_path() && tree[r].is_registered());
  tree.check_links().unwrap();
  assert!(tree.set_path(below, false));
  for key in [below, sub, r] {
    assert!(!tree[key].is_registered());
  }
  // `/p` still leads to `f.js` and to the chain that is not on disk.
  assert!(tree[p].is_ancestor());
  tree.check_links().unwrap();
  let x = tree.child(deep, OsStr::new("x.js")).unwrap();
  assert!(tree.set_path(f, false) && tree.set_path(x, false));
  for key in [f, x, deep, gone, p, top, Tree::ROOT] {
    assert!(!tree[key].is_registered());
  }
  tree.check_links().unwrap();
}

#[test]
fn what_a_node_wants() {
  let Fixture { mut tree, top, p, r, sub, gone, f, .. } = fixture();
  // Its one registered child is a directory.
  assert_eq!(tree.want(top), Some(Want::Names));
  // The tree has nothing at `deep`, a name on the way to a watched path.
  assert_eq!(tree.want(gone), None);
  assert_eq!(tree.want(r), Some(Want::Contents));
  assert_eq!(tree.want(sub), Some(Want::Contents));

  // `f.js` and `gone` are registered with nothing at them: the disk has to say.
  assert_eq!(tree.want(p), None);
  let fingerprint = Fingerprint { modified: 0, size: 1, id: 1 };
  tree[f].kind = Kind::File(fingerprint);
  assert_eq!(tree.want(p), Some(Want::Contents));
  // Still nothing at `gone`.
  on_disk(&mut tree, f, true);
  assert_eq!(tree.want(p), None);
  // Directories alone ask nothing of the directory above them.
  on_disk(&mut tree, gone, false);
  assert_eq!(tree.want(p), Some(Want::Names));
  // Until a look does not find one of them any more: a watched path, or a name on the way.
  tree.hold(gone, Instant::now());
  assert_eq!(tree.want(p), Some(Want::Contents));
  tree.unhold(gone);
  tree.hold(f, Instant::now());
  assert_eq!(tree.want(p), Some(Want::Contents));
  // A file at a name on the way.
  tree.unhold(f);
  tree[gone].kind = Kind::File(fingerprint);
  assert_eq!(tree.want(p), Some(Want::Contents));
}

/// The link and the content of a node are kept beside the arena.
#[test]
fn the_side_tables_of_a_node() {
  let Fixture { mut tree, r, f, .. } = fixture();
  assert_eq!(tree.link(f), None);
  tree.set_link(f, Some(7));
  tree.set_link(r, Some(0));
  assert_eq!((tree.link(f), tree.link(r)), (Some(7), Some(0)));
  let mut links: Vec<NodeKey> = tree.links().collect();
  links.sort_by_key(|key| key.index());
  assert_eq!(links, [r, f]);
  tree.set_link(f, None);
  assert_eq!(tree.links().collect::<Vec<_>>(), [r]);
  tree.check_links().unwrap();

  let file = child(&mut tree, r, "a.js");
  let fingerprint = Fingerprint { modified: 0, size: 1, id: 1 };
  tree[file].kind = Kind::File(fingerprint);
  assert_eq!(tree.content(file), None);
  tree.set_content(file, Some(Content::Hash(7)));
  assert_eq!(tree.content(file), Some(Content::Hash(7)));
  tree.set_content(file, Some(Content::Unknown));
  assert_eq!(tree.content(file), Some(Content::Unknown));
  tree.check_links().unwrap();
  // It goes with the node.
  tree[file].kind = Kind::Missing;
  tree.prune(file);
  assert!(tree.get(file).is_none());
  assert_eq!(tree.content(file), None);
  tree.check_links().unwrap();
}

/// No directory is on disk at it, everything below is frozen, and what the tree has stays.
#[test]
fn a_held_node() {
  let Fixture { mut tree, top, p, r, sub, f, .. } = fixture();
  let deeper = child(&mut tree, sub, "deeper");
  on_disk(&mut tree, deeper, true);
  let file = child(&mut tree, deeper, "a.js");
  assert!(!tree.frozen(file) && tree.held_until(r).is_none());

  let until = Instant::now() + Duration::from_millis(100);
  tree.hold(r, until);
  assert!(tree[r].is_held() && tree.held_until(r) == Some(until));
  assert_eq!(tree.hold_count(), 1);
  assert!(matches!(tree[r].kind, Kind::Dir(_)));
  assert!(!tree[r].is_dir() && tree[r].dir().is_none() && !tree[r].is_complete());
  assert_eq!(tree[r].watch(), Watch::None);
  // The held node and its neighbours are not frozen.
  for key in [sub, deeper, file] {
    assert!(tree.frozen(key));
  }
  for key in [r, f, p, top, Tree::ROOT] {
    assert!(!tree.frozen(key));
  }
  tree.queue_scan(r, Depth::Deep);
  assert_eq!(tree.to_scan, []);
  tree.check_links().unwrap();

  // Both hints end at the held node: its parent checks whether it is back.
  tree.hint(changed(r, "sub/a.js", true));
  assert_eq!(marks(&tree), ["/p|r|d"]);
  clear_marks(&mut tree);
  tree.hint(changed(p, "r/sub/deeper/a.js", true));
  assert_eq!(marks(&tree), ["/p|r|d"]);
  clear_marks(&mut tree);

  tree.unhold(r);
  assert!(tree[r].is_dir() && !tree[r].is_held() && tree.held_until(r).is_none());
  assert!(!tree.frozen(file));
  tree.unhold(r);
  tree.check_links().unwrap();

  // A hold goes with its node.
  tree.hold(file, until);
  tree[file].kind = Kind::Missing;
  tree.unhold(file);
  tree.prune(file);
  assert!(tree.get(file).is_none() && tree.hold_count() == 0);
  tree.check_links().unwrap();
}

#[test]
fn hint_to_mark() {
  struct Case {
    name: &'static str,
    hint: fn(&Fixture) -> Hint<'static>,
    marks: &'static [&'static str],
  }
  let table = [
    Case {
      name: "the last part is marked",
      hint: |f| changed(f.r, "a.js", false),
      marks: &["/p/r|a.js|d"],
    },
    Case {
      name: "a write is marked as written",
      hint: |f| changed(f.r, "a.js", true),
      marks: &["/p/r|a.js|w"],
    },
    Case {
      name: "the walk goes into a known directory",
      hint: |f| changed(f.r, "sub/a.js", true),
      marks: &["/p/r/sub|a.js|w"],
    },
    Case {
      name: "it ends at the highest name the tree does not know",
      hint: |f| changed(f.r, "new/deeper/a.js", true),
      marks: &["/p/r|new|d"],
    },
    Case {
      name: "a name that is not registered is dropped",
      hint: |f| changed(f.p, "other.js", true),
      marks: &[],
    },
    Case {
      name: "a path through a name that is not registered is dropped",
      hint: |f| changed(f.p, "other/a.js", false),
      marks: &[],
    },
    Case {
      name: "a registered name is marked",
      hint: |f| changed(f.p, "f.js", true),
      marks: &["/p|f.js|w"],
    },
    Case {
      name: "from above, the walk goes through partial and complete nodes",
      hint: |f| changed(f.top, "p/r/sub/a.js", false),
      marks: &["/p/r/sub|a.js|d"],
    },
    Case {
      name: "a registered name that is not on disk is marked itself",
      hint: |f| changed(f.p, "gone/deep/x.js", true),
      marks: &["/p|gone|d"],
    },
    Case {
      name: "a node that is not on disk sends the mark to the nearest one that is",
      hint: |f| changed(f.deep, "x.js", true),
      marks: &["/p|gone|d"],
    },
    Case {
      name: "the same for the node right below",
      hint: |f| changed(f.gone, "", false),
      marks: &["/p|gone|d"],
    },
    Case {
      name: "the watched directory itself is marked in its parent",
      hint: |f| changed(f.sub, "", true),
      marks: &["/p/r|sub|d"],
    },
    Case {
      name: "a directory is marked like a file",
      hint: |f| changed(f.r, "sub", false),
      marks: &["/p/r|sub|d"],
    },
    Case {
      name: "a path that climbs is dropped",
      hint: |f| changed(f.r, "../f.js", false),
      marks: &[],
    },
    Case {
      name: "a top directory is marked in the root",
      hint: |f| changed(f.top, "", false),
      marks: &["|/|d"],
    },
  ];
  for case in table {
    let mut fixture = fixture();
    let nodes = fixture.tree.nodes().count();
    let hint = (case.hint)(&fixture);
    assert_eq!(fixture.tree.hint(hint), !case.marks.is_empty(), "{}", case.name);
    assert_eq!(marks(&fixture.tree), case.marks, "{}", case.name);
    assert_eq!(fixture.tree.marked.len(), case.marks.len(), "{}", case.name);
    assert!(fixture.tree.to_scan.is_empty(), "{}", case.name);
    fixture.tree.check_links().unwrap();
    // A hint that is dropped leaves no node behind. A mark does not either, once it is taken.
    clear_marks(&mut fixture.tree);
    assert_eq!(fixture.tree.nodes().count(), nodes, "{}", case.name);
  }
}

#[test]
fn marks_for_one_name_merge() {
  let mut f = fixture();
  f.tree.hint(changed(f.r, "a.js", true));
  f.tree.hint(changed(f.r, "a.js", false));
  f.tree.hint(changed(f.r, "a.js", true));
  assert_eq!(marks(&f.tree), ["/p/r|a.js|dw"]);
  // A mark that only a loss report asked for joins it: the check does not wait.
  let file = f.tree.child(f.r, OsStr::new("a.js")).unwrap();
  f.tree.mark(file, Mark::LOST);
  assert_eq!(marks(&f.tree), ["/p/r|a.js|dw"]);
  assert!(!f.tree[file].mark().unwrap().lost);
  assert_eq!(f.tree.marked, [(f.tree.child(f.r, OsStr::new("a.js")).unwrap(), None)]);
}

/// A node whose mark a scan took, then marked again, is listed twice: it is taken once, with the
/// time of the mark it has. A flush drops the keys of the nodes that have no mark.
#[test]
fn a_node_marked_again_is_taken_once_with_its_new_time() {
  let mut f = fixture();
  let (old, new) = (Instant::now(), Instant::now() + Duration::from_secs(1));
  let a = child(&mut f.tree, f.r, "a.js");
  let c = child(&mut f.tree, f.r, "c.js");
  f.tree.mark(a, Mark { since: Some(old), ..Mark::default() });
  f.tree.mark(c, Mark::default());
  f.tree.take_mark(a);
  f.tree.take_mark(c);
  f.tree.mark(a, Mark { since: Some(new), ..Mark::default() });
  assert_eq!(f.tree.marked, [(a, Some(old)), (c, None), (a, Some(new))]);
  f.tree.tidy_marked();
  assert_eq!(f.tree.marked, [(a, Some(old)), (a, Some(new))]);
  assert!(f.tree.has_work());
  assert_eq!(taken(&mut f.tree), [(4, f.r, a, Some(new))]);

  // Marked again without a time: the old time is not taken.
  f.tree.take_mark(a);
  f.tree.mark(a, Mark::default());
  f.tree.marked.insert(0, (a, Some(old)));
  assert_eq!(taken(&mut f.tree), [(4, f.r, a, None)]);
  f.tree.take_mark(a);
  f.tree.tidy_marked();
  assert!(!f.tree.has_work());
}

/// Its mark says `lost` until a hint that names the path joins it.
#[test]
fn a_hint_for_the_directory_itself_is_a_loss_report() {
  let mut f = fixture();
  assert!(f.tree.hint(changed(f.sub, "", false)));
  assert!(f.tree.hint(changed(f.sub, "", false)));
  let mark = f.tree.take_mark(f.sub).unwrap();
  assert!(mark.lost && mark.deep && !mark.written);
  f.tree.marked.clear();

  // The directory above names it too: that check does not wait with the loss reports.
  assert!(f.tree.hint(changed(f.sub, "", false)));
  assert!(f.tree.hint(changed(f.r, "sub", false)));
  assert!(f.tree.hint(changed(f.sub, "", false)));
  let mark = f.tree.take_mark(f.sub).unwrap();
  assert!(!mark.lost && mark.deep);
  assert_eq!(f.tree.marked, [(f.sub, None)]);

  // Neither a path below it, nor a node with no directory, is a loss report.
  assert!(f.tree.hint(changed(f.r, "a.js", false)));
  assert!(f.tree.hint(changed(f.gone, "", false)));
  for key in [f.tree.child(f.r, OsStr::new("a.js")).unwrap(), f.gone] {
    assert!(!f.tree.take_mark(key).unwrap().lost);
  }
  // Nothing checks the root.
  assert!(!f.tree.hint(changed(Tree::ROOT, "", false)));
  assert!(f.tree.hint(Hint::Lost) && f.tree.lost);
}

/// A write reported under another spelling of a registered name is a write to that name when
/// every disk is loose: the mark says "written", so a rewrite that leaves the fingerprint as it
/// was is an `Update` for every spelling. A guess (a name that is not plain) keeps the look
/// deciding.
#[test]
fn a_write_under_another_spelling_is_a_write_to_the_registered_name() {
  let mut f = fixture();
  f.tree.loose_spelling = true;
  assert!(f.tree.hint(changed(f.p, "F.JS", true)));
  assert_eq!(marks(&f.tree), ["/p|f.js|w"]);
  clear_marks(&mut f.tree);
  // Through a directory in another spelling on the way. The complete directory `/p/r` gets a
  // node for the name it does not know, as for any hint.
  assert!(f.tree.hint(changed(f.p, "R/SUB/a.js", true)));
  assert_eq!(marks(&f.tree), ["/p/r/sub|a.js|w", "/p/r|SUB|d"]);
  clear_marks(&mut f.tree);
  // A name that is not plain can be every registered name that is not plain: a guess.
  let accent = child(&mut f.tree, f.p, "caf\u{e9}.js");
  assert!(f.tree.set_path(accent, true));
  assert!(f.tree.hint(changed(f.p, "CAF\u{c9}.js", true)));
  assert_eq!(marks(&f.tree), ["/p|caf\u{e9}.js|d"]);
  clear_marks(&mut f.tree);
  assert!(f.tree.set_path(accent, false));
  f.tree.prune(accent);

  // inotify says "written" for a file that was moved in too: the twin is the same file after a
  // rename that changes only the case, so its look decides.
  let moved_in = |rel| Hint::Written { dir: f.p, rel: Path::new(rel), new: true };
  assert!(f.tree.hint(moved_in("F.JS")));
  assert!(f.tree.hint(moved_in("R/SUB/a.js")));
  assert_eq!(marks(&f.tree), ["/p/r/sub|a.js|d", "/p/r|SUB|d", "/p|f.js|d"]);
  clear_marks(&mut f.tree);
  f.tree.check_links().unwrap();
}

/// The name of a hint means each registered name that the OS can take for it, and nothing else.
#[test]
fn a_name_that_can_be_another_spelling() {
  let mut f = fixture();
  // The look decides, so the mark never says "written".
  assert!(f.tree.hint(changed(f.p, "F.JS", true)));
  assert_eq!(marks(&f.tree), ["/p|f.js|d"]);
  assert_eq!(f.tree.child(f.p, OsStr::new("F.JS")), None);
  clear_marks(&mut f.tree);
  let upper = child(&mut f.tree, f.p, "F.JS");
  assert!(f.tree.set_path(upper, true));
  assert!(f.tree.hint(changed(f.p, "f.js", true)));
  assert_eq!(marks(&f.tree), ["/p|F.JS|d", "/p|f.js|w"]);
  clear_marks(&mut f.tree);
  assert!(f.tree.set_path(upper, false));
  f.tree.prune(upper);
  // The walk goes on below a registered directory in another spelling.
  assert!(f.tree.hint(changed(f.p, "R/sub/a.js", true)));
  assert_eq!(marks(&f.tree), ["/p/r/sub|a.js|d"]);
  clear_marks(&mut f.tree);
  // On a strict disk, an accent is no other spelling of anything.
  let accent = child(&mut f.tree, f.p, "\u{e9}.js");
  assert!(f.tree.set_path(accent, true));
  assert!(!f.tree.hint(changed(f.p, "\u{c9}.js", true)));
  assert!(!f.tree.has_work());

  f.tree.loose_spelling = true;
  // Not plain: it can be every registered name that is not plain. Ideographs are plain.
  assert!(f.tree.hint(changed(f.p, "\u{c9}.js", true)));
  assert!(f.tree.hint(changed(f.p, "\u{444}", false)));
  assert!(!f.tree.hint(changed(f.p, "\u{4e2d}", false)));
  assert_eq!(marks(&f.tree), ["/p|\u{e9}.js|d"]);
  clear_marks(&mut f.tree);
  assert!(f.tree.set_path(accent, false));
  f.tree.prune(accent);
  f.tree.check_links().unwrap();

  assert!(!f.tree.hint(changed(f.p, "other.js", false)));
  assert!(!f.tree.hint(changed(f.p, "f.jsx", true)));
  assert!(!f.tree.hint(changed(f.p, "\u{e9}.js", true)));
  assert!(!f.tree.has_work());
  // One level down: `Q` is no registered name of the partial node `/`.
  assert!(!f.tree.hint(changed(f.top, "Q/f.js", false)));
  assert!(!f.tree.has_work());

  assert!(f.tree.hint(changed(f.p, "F.JS", true)));
  assert!(f.tree.hint(changed(f.p, "R/sub", false)));
  assert_eq!(marks(&f.tree), ["/p/r|sub|d", "/p|f.js|w"]);
  assert_eq!(f.tree.to_scan, []);
  clear_marks(&mut f.tree);

  // `/p/gone` is no directory, so the name itself is marked, as always.
  assert!(f.tree.hint(changed(f.p, "gone/DEEP/x.js", false)));
  assert_eq!(marks(&f.tree), ["/p|gone|d"]);
  clear_marks(&mut f.tree);

  assert!(f.tree.hint(changed(f.p, "f.js", true)));
  assert!(f.tree.hint(changed(f.r, "New.js", false)));
  assert_eq!(marks(&f.tree), ["/p/r|New.js|d", "/p|f.js|w"]);
  assert_eq!(f.tree.to_scan, []);
  clear_marks(&mut f.tree);

  // The OS names an entry that is gone as the caller spelled it: the other case is checked too.
  assert!(!f.tree.has_other_spelling(f.sub));
  assert!(f.tree.hint(changed(f.r, "SUB", false)));
  assert_eq!(marks(&f.tree), ["/p/r|SUB|d", "/p/r|sub|d"]);
  // While the tree has both, a hint for one is a hint for the other.
  let other = f.tree.child(f.r, OsStr::new("SUB")).unwrap();
  assert!(f.tree.has_other_spelling(f.sub) && f.tree.has_other_spelling(other));
  f.tree.take_mark(f.sub);
  assert!(f.tree.hint(changed(f.r, "SUB", false)));
  assert!(f.tree[f.sub].is_marked());
  clear_marks(&mut f.tree);
  assert!(!f.tree.has_other_spelling(f.sub));
  // A name that is not plain is never sure: a listing of the names is asked while it is no entry.
  let mut scans = Vec::new();
  assert!(f.tree.hint(changed(f.r, "s\u{dc}b", false)));
  assert_eq!(marks(&f.tree), ["/p/r|s\u{dc}b|d"]);
  let accent = f.tree.child(f.r, OsStr::new("s\u{dc}b")).unwrap();
  assert!(f.tree.has_other_spelling(accent));
  f.tree.take_scans(&mut scans);
  assert_eq!(scans, [(f.r, Depth::Names, false)]);
  on_disk(&mut f.tree, accent, true);
  assert!(f.tree.hint(changed(f.r, "s\u{dc}b", false)));
  assert!(f.tree.hint(changed(f.r, "\u{4e2d}.js", false)));
  f.tree.take_scans(&mut scans);
  assert_eq!(scans, []);
  assert!(!f.tree.has_other_spelling(f.tree.child(f.r, OsStr::new("\u{4e2d}.js")).unwrap()));
}

/// A removed key names no node again: not while its slot is free, and not the node that reuses it.
#[test]
fn an_old_key_does_not_reach_the_node_that_reuses_its_slot() {
  let mut f = fixture();
  let old = f.sub;
  f.tree[old].kind = Kind::Missing;
  f.tree.prune(old);
  assert!(f.tree.get(old).is_none());
  assert_eq!(f.tree.child(f.r, OsStr::new("sub")), None);
  assert_eq!(f.tree.children(f.r).count(), 0);
  assert!(!f.tree.hint(changed(old, "a.js", true)));

  let new = child(&mut f.tree, f.r, "other");
  on_disk(&mut f.tree, new, true);
  assert_eq!(new.index(), old.index());
  assert_ne!(new.generation(), old.generation());
  let keys: Vec<NodeKey> = f.tree.nodes().map(|(key, _)| key).collect();
  assert!(keys.contains(&new) && !keys.contains(&old));

  assert!(f.tree.get(old).is_none() && f.tree.get_mut(old).is_none());
  assert_eq!(f.tree.parent(old), None);
  assert_eq!(f.tree.child(old, OsStr::new("a.js")), None);
  assert_eq!(f.tree.children(old).count(), 0);
  assert_eq!(f.tree.path(old), Path::new(""));
  assert_eq!(f.tree.ancestors(old).count(), 0);
  f.tree.prune(old);
  assert_eq!(f.tree.path(new), Path::new("/p/r/other"));

  f.tree.hint(changed(old, "a.js", true));
  f.tree.hint(changed(old, "", false));
  assert!(!f.tree.has_work());
  assert!(f.tree.take_mark(old).is_none());
  f.tree.hint(changed(new, "a.js", true));
  f.tree.hint(changed(new, "", false));
  assert_eq!(marks(&f.tree), ["/p/r/other|a.js|w", "/p/r|other|d"]);
  f.tree.check_links().unwrap();
}

#[test]
fn a_scan_is_queued_once_and_never_gets_shallower() {
  let mut f = fixture();
  f.tree.queue_scan(f.top, Depth::Deep);
  f.tree.queue_scan(f.top, Depth::Shallow);
  assert_eq!(f.tree[f.top].dir().unwrap().scan, Some(Depth::Deep));
  assert_eq!(f.tree.to_scan, [f.top]);
  let mut scans = Vec::new();
  f.tree.take_scans(&mut scans);
  assert_eq!(scans, [(f.top, Depth::Deep, false)]);
  assert!(!f.tree.has_work());
}

/// Every top directory is a child of the root, so one deep scan of the root reaches them all.
#[test]
fn lost_notifications_scan_everything() {
  let mut f = fixture();
  let other = child(&mut f.tree, Tree::ROOT, "/other-top");
  f.tree.hint(Hint::Lost);
  // The engine says when: the scans after loss reports are paced.
  assert!(f.tree.lost && !f.tree.has_work());
  f.tree.queue_scan(Tree::ROOT, Depth::Deep);
  assert_eq!(f.tree[Tree::ROOT].dir().unwrap().scan, Some(Depth::Deep));
  assert_eq!(f.tree.to_scan, [Tree::ROOT]);
  assert_eq!(f.tree.parent(f.top), Some(Tree::ROOT));
  assert_eq!(f.tree.parent(other), Some(Tree::ROOT));
}

/// `tidy_polled` leaves each directory to look at once, in the order of the arena; the root always.
#[test]
fn the_list_of_the_timer() {
  let mut f = fixture();
  assert_eq!(f.tree.polled, [Tree::ROOT]);
  let set = |tree: &mut Tree, key: NodeKey, watch: Watch, has_error: bool| {
    let dir = tree[key].dir_mut().unwrap();
    (dir.watch, dir.has_error) = (watch, has_error);
    tree.list_if_polled(key);
  };

  // A directory with an OS watch is not listed. One without, or with an error, is.
  set(&mut f.tree, f.r, Watch::Watched, false);
  set(&mut f.tree, f.sub, Watch::Failed, false);
  set(&mut f.tree, f.p, Watch::Polled, false);
  set(&mut f.tree, f.top, Watch::WatchedTree, true);
  set(&mut f.tree, f.p, Watch::Polled, false);
  // A file is never listed.
  f.tree.list_if_polled(f.f);
  assert_eq!(f.tree.polled, [Tree::ROOT, f.sub, f.p, f.top, f.p]);
  f.tree.tidy_polled();
  assert_eq!(f.tree.polled, [Tree::ROOT, f.top, f.p, f.sub]);

  // A directory that got an OS watch, and one that is gone, leave the list at the next tidy.
  set(&mut f.tree, f.p, Watch::Watched, false);
  f.tree[f.sub].kind = Kind::Missing;
  f.tree.prune(f.sub);
  f.tree.tidy_polled();
  assert_eq!(f.tree.polled, [Tree::ROOT, f.top]);

  // A held directory stays on it: the hold can end without a look.
  set(&mut f.tree, f.p, Watch::Polled, false);
  f.tree.hold(f.p, Instant::now());
  f.tree.tidy_polled();
  assert_eq!(f.tree.polled, [Tree::ROOT, f.top, f.p]);

  f.tree.unhold(f.p);
  f.tree.poll_all();
  f.tree.tidy_polled();
  assert_eq!(f.tree.polled, [Tree::ROOT, f.top, f.p, f.r]);
  assert!(f.tree.polled.iter().all(|&key| f.tree[key].watch() == Watch::Polled));
}

/// The marks of a flush come from the top down, and the marks of one directory together.
#[test]
fn the_marks_in_the_order_of_their_checks() {
  let mut f = fixture();
  let a = child(&mut f.tree, f.r, "a.js");
  let b = child(&mut f.tree, f.sub, "b.js");
  let c = child(&mut f.tree, f.r, "c.js");
  for key in [b, c, f.f, a, f.p] {
    f.tree.mark(key, Mark::default());
  }
  // A key that names no node any more is left out.
  let gone = child(&mut f.tree, f.r, "gone.js");
  f.tree.mark(gone, Mark::default());
  f.tree.take_mark(gone);
  f.tree.prune(gone);
  f.tree.marked.push((gone, None));

  assert_eq!(
    taken(&mut f.tree),
    [
      (2, f.top, f.p, None),
      (3, f.p, f.f, None),
      (4, f.r, a, None),
      (4, f.r, c, None),
      (5, f.sub, b, None)
    ]
  );
  // The marks stay on the nodes. Only the list is empty.
  assert!(f.tree.marked.is_empty() && f.tree[a].is_marked());
}
