//! Where the OS watches for whole trees go. One rule for the two backends that have such
//! watches: the handles of Windows and the stream paths of FSEvents. It is plain code, and
//! `tests/windows_parts.rs` runs it on every system.

use std::path::{Path, PathBuf};

use crate::backend::{DirKey, Want};

/// One OS watch of a placement: its directory, and the keys that it serves.
#[derive(Debug, PartialEq, Eq)]
pub struct Place {
  pub dir: PathBuf,
  pub keys: Vec<DirKey>,
}

/// Decides where the OS watches go for the directories that the engine asked for. No place is
/// below another. The same directories give the same placement, in whatever order they come.
///
/// 1. Each directory that wants the contents, with no other such directory above it, gets a
///    place. It serves every key at or below its directory, also a key that wants names only.
/// 2. If that makes more than `max` places, places go up to a directory that they have in
///    common and become one: the deepest common directory first, and of two that are equally
///    deep the first in path order. So the places stay as low as the limit allows.
/// 3. No place goes up to a directory for which `too_high` says yes. So more than `max` places
///    can be left: the caller decides what to do with them.
pub fn place(
  wanted: &[(DirKey, &Path, Want)],
  max: usize,
  too_high: impl Fn(&Path) -> bool,
) -> Vec<Place> {
  // In this order a directory comes before everything below it, and nothing else is between.
  let mut wanted = wanted.to_vec();
  wanted.sort_by(|a, b| a.1.cmp(b.1));

  let mut tops: Vec<&Path> = Vec::new();
  for &(_, dir, want) in &wanted {
    let below_a_top = tops.last().is_some_and(|top| dir.starts_with(top));
    if want == Want::Contents && !below_a_top {
      tops.push(dir);
    }
  }

  // `shared[i]`: how many parts the tops `i` and `i + 1` have in common at their start. The
  // directory made of these parts is the lowest one that holds both. `joined[i]`: one place
  // serves both.
  let shared: Vec<usize> = tops.windows(2).map(|pair| common_parts(pair[0], pair[1])).collect();
  let mut joined = vec![false; shared.len()];
  let mut count = tops.len();
  let mut depths = shared.clone();
  depths.sort_unstable_by(|a, b| b.cmp(a));
  depths.dedup();
  'deepest_first: for depth in depths {
    let mut at = 0;
    while at < shared.len() {
      // The tops `at..=end` are all the tops below one directory of `depth` parts.
      let end = at + shared[at..].iter().take_while(|&&parts| parts >= depth).count();
      if end == at {
        at += 1;
        continue;
      }
      if count <= max {
        break 'deepest_first;
      }
      // Two of them part right below that directory: it is the lowest one that holds them
      // all. If they all go on together, a directory below it holds them, and its turn was
      // before.
      if shared[at..end].contains(&depth) && !too_high(&first_parts(tops[at], depth)) {
        for joined in &mut joined[at..end] {
          count -= usize::from(!*joined);
          *joined = true;
        }
      }
      at = end;
    }
  }

  // One place for each run of joined tops, on the directory that the whole run has in common.
  let mut places = Vec::with_capacity(count);
  let mut first = 0;
  while first < tops.len() {
    let pairs = joined[first..].iter().take_while(|&&joined| joined).count();
    let dir = match shared[first..first + pairs].iter().min() {
      Some(&parts) => first_parts(tops[first], parts),
      None => tops[first].to_owned(),
    };
    places.push(Place { dir, keys: Vec::new() });
    first += pairs + 1;
  }

  // Each key goes to the place at or above its directory. Both lists are in path order.
  let mut at = 0;
  for &(key, dir, _) in &wanted {
    // A place that is before this directory and does not hold it is before all that follow.
    while places
      .get(at)
      .is_some_and(|place| place.dir.as_path() < dir && !dir.starts_with(&place.dir))
    {
      at += 1;
    }
    if let Some(place) = places.get_mut(at)
      && dir.starts_with(&place.dir)
    {
      place.keys.push(key);
    }
  }
  places
}

/// How many parts the two paths have in common at their start.
fn common_parts(a: &Path, b: &Path) -> usize {
  a.components().zip(b.components()).take_while(|(a, b)| a == b).count()
}

fn first_parts(path: &Path, count: usize) -> PathBuf {
  path.components().take(count).collect()
}
