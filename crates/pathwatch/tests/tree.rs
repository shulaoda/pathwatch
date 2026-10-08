//! The tree alone: the arena, the table and the way from a hint to a mark. The paths are Unix
//! paths.

#![cfg(unix)]

use std::{
  ffi::OsStr,
  iter, mem,
  path::{Path, PathBuf},
  time::{Duration, Instant},
};

use pathwatch::{
  backend::{DirKey, Hint, Want},
  fs::DirId,
  tree::{Content, Depth, Dir, DirTimes, Fingerprint, Kind, Mark, Tree, Watch},
};

fn child(tree: &mut Tree, parent: DirKey, name: &str) -> DirKey {
  assert_eq!(tree.child(parent, OsStr::new(name)), None);
  tree.insert_child(parent, OsStr::new(name))
}

/// Puts a directory at the node.
fn on_disk(tree: &mut Tree, key: DirKey, complete: bool) {
  let id = DirId { volume: 1, inode: u64::from(key.index()), birth: 0 };
  tree[key].kind = Kind::Dir(Dir { watch: Watch::Watched, ..Dir::new(id, complete) });
}

/// `/` and `/p` only lead to watched paths, `/p/r` is a watched directory with `sub` in it,
/// `/p/gone/deep/x.js` is a watched path below a chain that is not on disk, and `/p/f.js` is
/// a watched file.
struct Fixture {
  tree: Tree,
  top: DirKey,
  p: DirKey,
  r: DirKey,
  sub: DirKey,
  gone: DirKey,
  deep: DirKey,
  f: DirKey,
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
    assert!(tree.make_path(key));
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
    all.push(format!("{}|{}|{flags}", parent.display(), tree.name(key).display()));
  }
  all.sort();
  all
}

/// Takes every mark away, as a flush does.
fn clear_marks(tree: &mut Tree) {
  for key in mem::take(&mut tree.marked) {
    tree.take_mark(key);
    tree.prune(key);
  }
}

fn changed(dir: DirKey, rel: &str, written: bool) -> Hint<'_> {
  let rel = Path::new(rel);
  if written { Hint::Written { dir, rel } } else { Hint::Changed { dir, rel } }
}

#[test]
fn a_removed_key_never_names_a_node_again() {
  let mut tree = Tree::new();
  let top = child(&mut tree, Tree::ROOT, "/");
  let first = child(&mut tree, top, "a");
  assert_eq!(tree.path(first), Path::new("/a"));
  assert_eq!(tree.level(first), 2);

  // Nothing needs the node.
  tree.prune(first);
  assert!(tree.get(first).is_none());
  assert_eq!(tree.child(top, OsStr::new("a")), None);
  assert_eq!(tree.children(top).count(), 0);

  // The slot is used again, under another generation.
  let second = child(&mut tree, top, "b");
  assert_eq!(second.index(), first.index());
  assert_ne!(second.generation(), first.generation());
  assert!(tree.get(first).is_none());
  assert_eq!(tree.path(second), Path::new("/b"));
  assert_eq!(tree.nodes().map(|(key, _)| key).collect::<Vec<_>>(), [Tree::ROOT, top, second]);
  tree.check_links().unwrap();
}

#[test]
fn paths_are_built_from_the_names() {
  let Fixture { tree, top, sub, deep, .. } = fixture();
  assert_eq!(tree.path(Tree::ROOT), Path::new(""));
  assert_eq!(tree.path(top), Path::new("/"));
  assert_eq!(tree.path(sub), Path::new("/p/r/sub"));
  assert_eq!(tree.path(deep), Path::new("/p/gone/deep"));
  assert_eq!(tree.level(deep), 4);

  // A buffer is used again, whatever it held.
  let mut path = PathBuf::from("/something/else");
  tree.path_into(sub, &mut path);
  assert_eq!(path, Path::new("/p/r/sub"));
  tree.path_into(Tree::ROOT, &mut path);
  assert_eq!(path, Path::new(""));
}

#[test]
fn a_child_is_found_by_its_parent_and_its_name() {
  let Fixture { mut tree, top, p, r, sub, gone, f, .. } = fixture();
  assert_eq!(tree.child(Tree::ROOT, OsStr::new("/")), Some(top));
  assert_eq!(tree.child(p, OsStr::new("r")), Some(r));
  assert_eq!(tree.child(r, OsStr::new("sub")), Some(sub));
  // The same name below another parent is another node, or none.
  assert_eq!(tree.child(top, OsStr::new("r")), None);
  assert_eq!(tree.child(p, OsStr::new("sub")), None);
  let twin = child(&mut tree, sub, "sub");
  assert_eq!(tree.child(sub, OsStr::new("sub")), Some(twin));
  assert_eq!(tree.child(r, OsStr::new("sub")), Some(sub));

  assert_eq!(tree.parent(sub), Some(r));
  assert_eq!(tree.parent(Tree::ROOT), None);
  // The newest child comes first.
  assert_eq!(tree.children(p).collect::<Vec<_>>(), [f, gone, r]);
  assert_eq!(tree.children(f).count(), 0);
}

/// Many children of one node, and nodes that go away from the start, the middle and the end
/// of the list: the list and the table stay right while the table grows and the slots are
/// used again.
#[test]
fn children_come_and_go() {
  let mut tree = Tree::new();
  let top = child(&mut tree, Tree::ROOT, "/");
  let mut live: Vec<(String, DirKey)> = Vec::new();
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
    assert_eq!(tree.children(top).count(), live.len());
    // A walk from child to child goes through the same list.
    let walk = iter::successors(tree.children(top).next(), |&at| tree.next_sibling(at));
    assert!(walk.eq(tree.children(top)));
    for (name, key) in &live {
      assert_eq!(tree.child(top, OsStr::new(name)), Some(*key));
      assert_eq!(tree.name(*key), OsStr::new(name));
    }
  }
  // No slot is lost: no key is beyond the most nodes that lived at one time.
  assert!(highest < 2 + 500 + 500 * 2 / 3 + 500 * 4 / 9 + 500 * 8 / 27 + 4);
}

/// A node is kept while it is registered, while something is at its path, and while a mark
/// waits on it.
#[test]
fn a_node_is_freed_when_nothing_needs_it() {
  let Fixture { mut tree, r, sub, gone, deep, f, .. } = fixture();
  // Registered: a watched path with nothing at it, and the chain to a watched path.
  for key in [f, gone, deep] {
    tree.prune(key);
    assert!(tree.get(key).is_some());
  }
  // A directory that was found: it goes when it is gone from the disk.
  tree.prune(sub);
  assert!(tree.get(sub).is_some());
  tree[sub].kind = Kind::Missing;
  tree.prune(sub);
  assert!(tree.get(sub).is_none());

  // A file that was found, with a mark.
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
  // A watched path, and every node above one.
  for key in [Tree::ROOT, top, p, gone, deep] {
    assert!(tree[key].is_ancestor() && tree[key].is_registered());
  }
  for key in [r, f] {
    assert!(tree[key].is_path() && tree[key].is_registered() && !tree[key].is_ancestor());
  }
  assert!(!tree[sub].is_registered());

  // A path is made a watched path once.
  assert!(!tree.make_path(r));
  // A watched path below a watched path: both.
  let below = child(&mut tree, sub, "x.js");
  assert!(tree.make_path(below));
  assert!(tree[r].is_path() && tree[r].is_ancestor());
  assert!(tree[sub].is_ancestor() && !tree[sub].is_path());
  tree.check_links().unwrap();

  // The way back. A node above another watched path stays registered.
  assert!(tree.drop_path(r));
  assert!(!tree.drop_path(r));
  assert!(!tree[r].is_path() && tree[r].is_registered());
  tree.check_links().unwrap();
  assert!(tree.drop_path(below));
  for key in [below, sub, r] {
    assert!(!tree[key].is_registered());
  }
  // `/p` still leads to `f.js` and to the chain that is not on disk.
  assert!(tree[p].is_ancestor());
  tree.check_links().unwrap();
  // With the last watched path, nothing is registered any more, up to the root.
  let x = tree.child(deep, OsStr::new("x.js")).unwrap();
  assert!(tree.drop_path(f) && tree.drop_path(x));
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
  // A watched directory, and a directory inside one.
  assert_eq!(tree.want(r), Some(Want::Contents));
  assert_eq!(tree.want(sub), Some(Want::Contents));

  // `/p` holds the watched directory `r`, and the registered names `f.js` and `gone`, where
  // the tree has nothing: the disk has to say what is there.
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

#[test]
fn the_link_of_a_node() {
  let Fixture { mut tree, r, f, .. } = fixture();
  assert_eq!(tree.link(f), None);
  tree.set_link(f, Some(7));
  tree.set_link(r, Some(0));
  assert_eq!((tree.link(f), tree.link(r)), (Some(7), Some(0)));
  let mut links: Vec<DirKey> = tree.links().collect();
  links.sort_by_key(|key| key.index());
  assert_eq!(links, [r, f]);
  tree.set_link(f, None);
  assert_eq!(tree.links().collect::<Vec<_>>(), [r]);
  tree.check_links().unwrap();
}

#[test]
fn the_content_of_a_node() {
  let Fixture { mut tree, r, .. } = fixture();
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

/// A held node is no directory on disk, and everything below it is frozen. What the tree has
/// at the node and below it stays.
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
  assert_eq!(tree.holds().collect::<Vec<_>>(), [(r, until)]);
  // The node keeps what it had, but no directory is on disk there.
  assert!(matches!(tree[r].kind, Kind::Dir(_)));
  assert!(!tree[r].is_dir() && tree[r].dir().is_none() && !tree[r].is_complete());
  assert_eq!(tree[r].watch(), Watch::None);
  // Everything below is frozen, however deep. The held node and its neighbours are not.
  for key in [sub, deeper, file] {
    assert!(tree.frozen(key));
  }
  for key in [r, f, p, top, Tree::ROOT] {
    assert!(!tree.frozen(key));
  }
  // No scan is queued for a held node.
  tree.queue_scan(r, Depth::Deep);
  assert_eq!(tree.to_scan, []);
  tree.check_links().unwrap();

  // A hint with the key of the held node, and a hint from above for a path below it, both
  // end at the held node: its parent checks whether it is back.
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
  assert!(tree.get(file).is_none() && tree.holds().next().is_none());
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
  ];
  for case in table {
    let mut fixture = fixture();
    let nodes = fixture.tree.nodes().count();
    let hint = (case.hint)(&fixture);
    // A hint says whether it left work.
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
  assert_eq!(f.tree.marked, [f.tree.child(f.r, OsStr::new("a.js")).unwrap()]);
}

/// A hint with no path below the directory is a loss report: the backend does not know
/// what happened at or below it. Its mark says so, until a hint that names the path comes.
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
  assert_eq!(f.tree.marked, [f.sub]);

  // A hint for a path below the directory is none, and neither is one that goes to the
  // directory above because the node has no directory.
  assert!(f.tree.hint(changed(f.r, "a.js", false)));
  assert!(f.tree.hint(changed(f.gone, "", false)));
  for key in [f.tree.child(f.r, OsStr::new("a.js")).unwrap(), f.gone] {
    assert!(!f.tree.take_mark(key).unwrap().lost);
  }
  // Nothing checks the root.
  assert!(!f.tree.hint(changed(Tree::ROOT, "", false)));
  assert!(f.tree.hint(Hint::Lost) && f.tree.lost);
}

/// On a disk that takes two spellings of a name for one, a hint for a name that is not
/// registered in a partial node marks the registered names that can be that name in another
/// spelling, and nothing else.
#[test]
fn a_name_that_can_be_another_spelling() {
  let mut f = fixture();
  // A strict disk: no watched path can see the name.
  assert!(!f.tree.hint(changed(f.p, "F.JS", true)));
  assert!(!f.tree.has_work());

  f.tree.loose_spelling = true;
  // No registered name of `/p` can be this one: the hint is dropped.
  assert!(!f.tree.hint(changed(f.p, "other.js", false)));
  assert!(!f.tree.hint(changed(f.p, "f.jsx", true)));
  assert!(!f.tree.hint(changed(f.p, "\u{e9}.js", true)));
  assert!(!f.tree.has_work());
  // One level down: `Q` is no registered name of the partial node `/`.
  assert!(!f.tree.hint(changed(f.top, "Q/f.js", false)));
  assert!(!f.tree.has_work());

  assert!(f.tree.hint(changed(f.p, "F.JS", true)));
  assert!(f.tree.hint(changed(f.p, "R/sub/a.js", false)));
  // The rest of the path can be spelled in another way too, so the check goes deep.
  assert_eq!(marks(&f.tree), ["/p|f.js|d", "/p|r|d"]);
  assert_eq!(f.tree.to_scan, []);
  // No node is made for the name.
  assert_eq!(f.tree.child(f.p, OsStr::new("F.JS")), None);
  clear_marks(&mut f.tree);

  // The other spelling is below a name that matches: `/p/gone` is no directory, so the name
  // itself is marked, as always.
  assert!(f.tree.hint(changed(f.p, "gone/DEEP/x.js", false)));
  assert_eq!(marks(&f.tree), ["/p|gone|d"]);
  clear_marks(&mut f.tree);

  // A registered name is marked as always, and so is any name in a complete node.
  assert!(f.tree.hint(changed(f.p, "f.js", true)));
  assert!(f.tree.hint(changed(f.r, "New.js", false)));
  assert_eq!(marks(&f.tree), ["/p/r|New.js|d", "/p|f.js|w"]);
  assert_eq!(f.tree.to_scan, []);
  clear_marks(&mut f.tree);

  // A complete node does not know the name: the OS names an entry that is gone as the caller
  // spelled it, so the nodes that differ in the case of their letters are checked too.
  assert!(!f.tree.has_other_spelling(f.sub));
  assert!(f.tree.hint(changed(f.r, "SUB", false)));
  assert_eq!(marks(&f.tree), ["/p/r|SUB|d", "/p/r|sub|d"]);
  // While the tree has both, the disk has to say which of the two it has.
  let other = f.tree.child(f.r, OsStr::new("SUB")).unwrap();
  assert!(f.tree.has_other_spelling(f.sub) && f.tree.has_other_spelling(other));
  clear_marks(&mut f.tree);
  assert!(!f.tree.has_other_spelling(f.sub));
  // The table does not find the other spellings of a name with a byte outside ASCII: such a
  // name marks no other node in a complete node, and its own spelling is never sure.
  assert!(f.tree.hint(changed(f.r, "s\u{dc}b", false)));
  assert_eq!(marks(&f.tree), ["/p/r|s\u{dc}b|d"]);
  assert!(f.tree.has_other_spelling(f.tree.child(f.r, OsStr::new("s\u{dc}b")).unwrap()));
}

#[test]
fn the_times_of_a_directory() {
  let Fixture { mut tree, r, sub, .. } = fixture();
  let at = |seconds: i64| seconds * 1_000_000_000;
  let (old, new) =
    (DirTimes { changed: at(1), modified: at(1) }, DirTimes { changed: at(2), modified: at(1) });
  assert_eq!(tree.set_times(r, None), None);
  assert_eq!(tree.set_times(r, Some(old)), None);
  assert_eq!(tree.set_times(sub, Some(old)), None);
  assert_eq!(tree.set_times(r, Some(new)), Some(old));
  assert_eq!(tree.set_times(r, None), Some(new));
  assert_eq!(tree.set_times(r, None), None);
  tree.check_links().unwrap();
  // They go with the node.
  tree[sub].kind = Kind::Missing;
  assert!(tree.check_links().is_err());
  tree.prune(sub);
  assert!(tree.get(sub).is_none());
  tree.check_links().unwrap();
}

#[test]
fn a_hint_for_a_dead_key_is_dropped() {
  let mut f = fixture();
  f.tree[f.sub].kind = Kind::Missing;
  f.tree.prune(f.sub);
  f.tree.hint(changed(f.sub, "a.js", true));
  assert!(!f.tree.has_work());
}

/// A backend can still hold the key of a directory that is gone. When the slot has a new
/// node, the old key does not reach it: not for a read, and not for a hint.
#[test]
fn an_old_key_does_not_reach_the_node_that_reuses_its_slot() {
  let mut f = fixture();
  let old = f.sub;
  f.tree[old].kind = Kind::Missing;
  f.tree.prune(old);
  // The next new node takes the free slot. It is a directory that tracks every entry.
  let new = child(&mut f.tree, f.r, "other");
  on_disk(&mut f.tree, new, true);
  assert_eq!(new.index(), old.index());
  assert_ne!(new.generation(), old.generation());

  assert!(f.tree.get(old).is_none() && f.tree.get_mut(old).is_none());
  assert_eq!(f.tree.parent(old), None);
  assert_eq!(f.tree.child(old, OsStr::new("a.js")), None);
  assert_eq!(f.tree.children(old).count(), 0);
  assert_eq!(f.tree.path(old), Path::new(""));
  assert_eq!(f.tree.level(old), 0);
  f.tree.prune(old);
  assert_eq!(f.tree.path(new), Path::new("/p/r/other"));

  f.tree.hint(changed(old, "a.js", true));
  f.tree.hint(changed(old, "", false));
  assert!(!f.tree.has_work());
  assert!(f.tree.take_mark(old).is_none());
  // The same hints under the new key are marks.
  f.tree.hint(changed(new, "a.js", true));
  f.tree.hint(changed(new, "", false));
  assert_eq!(marks(&f.tree), ["/p/r/other|a.js|w", "/p/r|other|d"]);
  f.tree.check_links().unwrap();
}

/// A top directory has a parent like every other directory: the root.
#[test]
fn a_top_directory_is_marked_in_the_root() {
  let mut f = fixture();
  f.tree.hint(changed(f.top, "", false));
  assert_eq!(marks(&f.tree), ["|/|d"]);
  assert_eq!(f.tree.marked, [f.top]);
  assert_eq!(f.tree.to_scan, []);
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
  assert_eq!(scans, [(f.top, Depth::Deep)]);
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
  f.tree.scan_all();
  assert_eq!(f.tree[Tree::ROOT].dir().unwrap().scan, Some(Depth::Deep));
  assert_eq!(f.tree.to_scan, [Tree::ROOT]);
  assert_eq!(f.tree.parent(f.top), Some(Tree::ROOT));
  assert_eq!(f.tree.parent(other), Some(Tree::ROOT));
}

/// Every mark as `path of the directory`, `name`, `deep` or `written`, sorted.
fn mark_list(tree: &Tree) -> Vec<String> {
  let mut all = Vec::new();
  for (key, node) in tree.nodes() {
    let Some(mark) = node.mark() else { continue };
    let kind = if mark.deep { "deep" } else { "written" };
    let parent = tree.parent(key).map(|parent| tree.path(parent)).unwrap_or_default();
    all.push(format!("{} {} {kind}", parent.display(), tree.name(key).display()));
  }
  all.sort();
  all
}

/// The way from a hint to a mark, in the corners.
#[test]
fn from_hint_to_mark() {
  let mut tree = Tree::new();
  let top = child(&mut tree, Tree::ROOT, "/");
  let p = child(&mut tree, top, "p");
  let a = child(&mut tree, p, "a");
  let b = child(&mut tree, a, "b");
  let c = child(&mut tree, b, "c.js");
  on_disk(&mut tree, top, false);
  on_disk(&mut tree, p, true);
  assert!(tree.make_path(c));

  // 2. `/p/a` and `/p/a/b` are not on disk. A hint for the deep one goes up to the first node
  // that is, and marks the name that leads to it there.
  tree.hint(changed(b, "c.js", true));
  assert_eq!(mark_list(&tree), ["/p a deep"]);
  clear_marks(&mut tree);
  tree.hint(changed(b, "", false));
  assert_eq!(mark_list(&tree), ["/p a deep"]);
  clear_marks(&mut tree);

  // 3. "Written" for a file below a directory the tree does not know: the mark is on the
  // unknown name, and it says "deep", because the written file is not that name.
  tree.hint(changed(p, "new/f.js", true));
  assert_eq!(mark_list(&tree), ["/p new deep"]);
  clear_marks(&mut tree);
  tree.hint(changed(p, "f.js", true));
  assert_eq!(mark_list(&tree), ["/p f.js written"]);

  // A top directory itself: the root checks it, like any parent.
  clear_marks(&mut tree);
  tree.hint(changed(top, "", false));
  assert_eq!(mark_list(&tree), [" / deep"]);
}

/// The list of the timer: a directory goes on it when its watch or its error is written, and
/// `tidy_polled` leaves each directory that the timer has to look at once, in the order of the
/// arena. The root is always one of them.
#[test]
fn the_list_of_the_timer() {
  let mut f = fixture();
  assert_eq!(f.tree.polled, [Tree::ROOT]);
  let set = |tree: &mut Tree, key: DirKey, watch: Watch, has_error: bool| {
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

  // When the backend is gone, every directory is polled.
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
  f.tree.marked.push(gone);

  let mut marks = Vec::new();
  f.tree.take_marked(&mut marks);
  assert_eq!(marks, [(2, f.top, f.p), (3, f.p, f.f), (4, f.r, a), (4, f.r, c), (5, f.sub, b)]);
  // The marks stay on the nodes. Only the list is empty.
  assert!(f.tree.marked.is_empty() && f.tree[a].is_marked());
}
