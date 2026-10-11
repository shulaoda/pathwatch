//! Cleans a path given to `watch`, by text only: `.` is dropped, `..` takes the name before it
//! away, and no link is resolved. The disk is never asked.

use std::{
  borrow::Cow,
  ffi::OsStr,
  path::{Path, PathBuf},
};

/// The registered form of `path`: absolute, no `.` or `..`, no repeated or trailing separator.
///
/// `None` when the path is relative, has a NUL, names a top directory (`/`, `C:\`,
/// `\\server\share\`), goes above it with `..`, or has a name longer than `MAX_NAME`. A path
/// that is clean already is given back as it is: no copy.
pub fn normalize(path: &Path) -> Option<Cow<'_, Path>> {
  #[cfg(unix)]
  {
    use std::{
      ffi::OsString,
      os::unix::ffi::{OsStrExt, OsStringExt},
    };
    let bytes = path.as_os_str().as_bytes();
    if unix_is_clean(bytes) {
      return Some(Cow::Borrowed(path));
    }
    unix(bytes).map(|bytes| Cow::Owned(PathBuf::from(OsString::from_vec(bytes))))
  }
  #[cfg(windows)]
  {
    use std::{
      ffi::OsString,
      os::windows::ffi::{OsStrExt, OsStringExt},
    };
    // The bytes of the path, not its UTF-16 units: every character that the rules name is one
    // ASCII byte there, and a name has at least as many bytes as units.
    if windows_rules::windows_is_clean(path.as_os_str().as_encoded_bytes()) {
      return Some(Cow::Borrowed(path));
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    windows_rules::windows(&wide).map(|wide| Cow::Owned(PathBuf::from(OsString::from_wide(&wide))))
  }
  #[cfg(not(any(unix, windows)))]
  {
    let bytes = unix(path.to_str()?.as_bytes())?;
    String::from_utf8(bytes).ok().map(|clean| Cow::Owned(PathBuf::from(clean)))
  }
}

/// The longest name that can be watched, in units. No disk takes more than 255, and the tree
/// keeps the length of a name in 16 bits of bytes: 3 bytes a unit, two names for a share.
pub(crate) const MAX_NAME: usize = 10_000;

/// Splits a clean path at its top directory: `/a/b` gives `/` and `a/b`.
pub(crate) fn split_top(path: &Path) -> Option<(&OsStr, &Path)> {
  // On Unix the top directory is always `/`: it is cut off at the front.
  #[cfg(unix)]
  let top = Path::new("/");
  // Anywhere else it can be `C:\` or `\\server\share\`. It is found from the end of the path.
  #[cfg(not(unix))]
  let top = path.ancestors().last()?;
  let rest = path.strip_prefix(top).ok()?;
  Some((top.as_os_str(), rest))
}

/// A byte on Unix, a UTF-16 unit on Windows. The rules are generic, so that the Windows rules
/// are tested on every system.
pub(crate) trait Unit: Copy + Into<u32> + From<u8> {}

impl<U: Copy + Into<u32> + From<u8>> Unit for U {}

/// Whether [`unix`] would give `text` back as it is.
#[cfg(any(unix, test))]
pub(crate) fn unix_is_clean<U: Unit>(text: &[U]) -> bool {
  let is_slash = |unit: U| is(unit, b'/');
  text
    .split_first()
    .is_some_and(|(first, rest)| is_slash(*first) && names_are_clean(rest, is_slash))
}

/// The Unix rules: the path starts with `/`, and only `/` separates.
#[cfg(any(not(windows), test))]
pub(crate) fn unix<U: Unit>(text: &[U]) -> Option<Vec<U>> {
  let (first, rest) = text.split_first()?;
  if !is(*first, b'/') {
    return None;
  }
  // The clean form is never longer than the text: one allocation is enough.
  let mut clean = Vec::with_capacity(text.len());
  clean.push(U::from(b'/'));
  push_names(&mut clean, rest, |unit| is(unit, b'/'), b'/')?;
  Some(clean)
}

/// The Windows rules. They are compiled for the tests of every system.
#[cfg(any(windows, test))]
pub(crate) mod windows_rules {
  use super::{MAX_NAME, Unit, has_nul, is, names_are_clean, push_names};

  /// Whether [`windows`] would give `text` back as it is. Only the usual form is looked at,
  /// `C:\a\b`: a clean path on a share is cleaned again, which gives the same path.
  pub(crate) fn windows_is_clean<U: Unit>(text: &[U]) -> bool {
    let [letter, colon, slash, rest @ ..] = text else { return false };
    let is_letter =
      char::from_u32((*letter).into()).is_some_and(|letter| letter.is_ascii_alphabetic());
    // `/` separates too, and the clean form has none.
    let has_slash = rest.iter().any(|&unit| is(unit, b'/'));
    let back = |unit: U| is(unit, b'\\');
    is_letter && is(*colon, b':') && back(*slash) && !has_slash && names_are_clean(rest, back)
  }

  pub(crate) fn windows<U: Unit>(text: &[U]) -> Option<Vec<U>> {
    let either = |unit: U| is(unit, b'\\') || is(unit, b'/');
    let back = |unit: U| is(unit, b'\\');
    let mut clean = Vec::with_capacity(text.len());

    if let Some(verbatim) = strip(text, br"\\?\") {
      // In a verbatim path only `\` separates, and `/` is a plain character that the normal form
      // cannot hold.
      if verbatim.iter().any(|&unit| is(unit, b'/')) {
        return None;
      }
      let rest = match strip_no_case(verbatim, br"UNC\") {
        Some(share) => push_share(&mut clean, share, back)?,
        None => push_drive(&mut clean, verbatim, back)?,
      };
      push_names(&mut clean, rest, back, b'\\')?;
    } else if text.len() >= 2 && either(text[0]) && either(text[1]) {
      let rest = push_share(&mut clean, &text[2..], either)?;
      push_names(&mut clean, rest, either, b'\\')?;
    } else {
      let rest = push_drive(&mut clean, text, either)?;
      push_names(&mut clean, rest, either, b'\\')?;
    }
    Some(clean)
  }

  /// Appends `C:\` and returns what comes after it. `C:name` and `\name` are not absolute.
  fn push_drive<'a, U: Unit>(
    clean: &mut Vec<U>,
    text: &'a [U],
    separator: impl Fn(U) -> bool,
  ) -> Option<&'a [U]> {
    let [letter, colon, slash, rest @ ..] = text else { return None };
    let is_letter =
      char::from_u32((*letter).into()).is_some_and(|letter| letter.is_ascii_alphabetic());
    if !is_letter || !is(*colon, b':') || !separator(*slash) {
      return None;
    }
    clean.extend([*letter, *colon, U::from(b'\\')]);
    Some(rest)
  }

  /// Appends `\\server\share\` and returns what comes after it. `text` starts at the server name.
  fn push_share<'a, U: Unit>(
    clean: &mut Vec<U>,
    text: &'a [U],
    separator: impl Fn(U) -> bool,
  ) -> Option<&'a [U]> {
    let mut rest = text;
    clean.extend([U::from(b'\\'), U::from(b'\\')]);
    for _ in 0..2 {
      let start = rest.iter().position(|&unit| !separator(unit))?;
      let name = &rest[start..];
      let end = name.iter().position(|&unit| separator(unit)).unwrap_or(name.len());
      let (name, after) = name.split_at(end);
      // `\\.\` and `\\?\` name devices, not shares.
      let device = matches!(name, [only] if is(*only, b'.') || is(*only, b'?'));
      if device || has_nul(name) || name.len() > MAX_NAME {
        return None;
      }
      clean.extend_from_slice(name);
      clean.push(U::from(b'\\'));
      rest = after;
    }
    Some(rest)
  }

  fn strip<'a, U: Unit>(text: &'a [U], prefix: &[u8]) -> Option<&'a [U]> {
    let (head, rest) = text.split_at_checked(prefix.len())?;
    head.iter().zip(prefix).all(|(&unit, &byte)| is(unit, byte)).then_some(rest)
  }

  fn strip_no_case<'a, U: Unit>(text: &'a [U], prefix: &[u8]) -> Option<&'a [U]> {
    let (head, rest) = text.split_at_checked(prefix.len())?;
    let same = |(&unit, &byte): (&U, &u8)| {
      char::from_u32(unit.into()).is_some_and(|unit| unit.eq_ignore_ascii_case(&char::from(byte)))
    };
    head.iter().zip(prefix).all(same).then_some(rest)
  }
}

/// Appends the names of `text` to `clean`, which holds the top directory and ends with a
/// separator. At least one name must be left, or the path is a top directory.
fn push_names<U: Unit>(
  clean: &mut Vec<U>,
  text: &[U],
  separator: impl Fn(U) -> bool,
  join: u8,
) -> Option<()> {
  let top = clean.len();
  for name in text.split(|&unit| separator(unit)) {
    match part_of(name) {
      Part::Nothing => {}
      Part::Unusable => return None,
      // Above the top directory there is nothing to take away.
      Part::Parent => {
        if clean.len() == top {
          return None;
        }
        let last = clean[top..].iter().rposition(|&unit| is(unit, join)).unwrap_or(0);
        clean.truncate(top + last);
      }
      Part::Name => {
        if clean.len() > top {
          clean.push(U::from(join));
        }
        clean.extend_from_slice(name);
      }
    }
  }
  (clean.len() > top).then_some(())
}

/// Whether what comes after a top directory is clean: names with one separator between them.
#[cfg(any(unix, windows, test))]
fn names_are_clean<U: Unit>(text: &[U], separator: impl Fn(U) -> bool) -> bool {
  text.split(|&unit| separator(unit)).all(|name| matches!(part_of(name), Part::Name))
}

/// What the text between two separators is.
enum Part {
  /// Nothing, or `.`: the clean form leaves it out.
  Nothing,
  /// `..`: it takes the name before it away.
  Parent,
  /// A name with a NUL, or one that is too long: the path cannot be used.
  Unusable,
  Name,
}

fn part_of<U: Unit>(name: &[U]) -> Part {
  match name {
    [] => Part::Nothing,
    [dot] if is(*dot, b'.') => Part::Nothing,
    [dot, second] if is(*dot, b'.') && is(*second, b'.') => Part::Parent,
    _ if has_nul(name) || name.len() > MAX_NAME => Part::Unusable,
    _ => Part::Name,
  }
}

fn has_nul<U: Unit>(name: &[U]) -> bool {
  name.iter().any(|&unit| is(unit, 0))
}

fn is<U: Unit>(unit: U, byte: u8) -> bool {
  unit.into() == u32::from(byte)
}

#[cfg(test)]
mod tests {
  use super::{
    MAX_NAME, normalize, unix, unix_is_clean,
    windows_rules::{windows, windows_is_clean},
  };

  fn clean_unix(text: &str) -> Option<String> {
    unix(text.as_bytes()).map(|bytes| String::from_utf8(bytes).unwrap())
  }

  fn clean_windows(text: &str) -> Option<String> {
    let wide: Vec<u16> = text.encode_utf16().collect();
    windows(&wide).map(|wide| String::from_utf16(&wide).unwrap())
  }

  #[test]
  fn unix_paths() {
    let table = [
      ("/a/b", Some("/a/b")),
      ("/a/b/", Some("/a/b")),
      ("/a//b///c", Some("/a/b/c")),
      ("//a/b", Some("/a/b")),
      ("/./a/./b/.", Some("/a/b")),
      ("/a/.hidden/..b/c..", Some("/a/.hidden/..b/c..")),
      ("/a b/c\u{e9}", Some("/a b/c\u{e9}")),
      // Not absolute.
      ("", None),
      ("a/b", None),
      ("./a", None),
      // `..` takes the name before it away, by text. Above the root there is nothing.
      ("/a/../b", Some("/b")),
      ("/a/b/../../c/d/..", Some("/c")),
      ("/a/b/..", Some("/a")),
      ("/a/..", None),
      ("/..", None),
      ("/a/../..", None),
      ("/a\0b", None),
      // A top directory has no parent to watch it from.
      ("/", None),
      ("//", None),
      ("/.", None),
      ("/./", None),
    ];
    for (given, expected) in table {
      assert_eq!(clean_unix(given).as_deref(), expected, "{given:?}");
      // A path is taken as it is when it is its own clean form, and only then.
      assert_eq!(unix_is_clean(given.as_bytes()), expected == Some(given), "{given:?}");
    }
  }

  #[test]
  fn windows_paths() {
    let table = [
      (r"C:\a\b", Some(r"C:\a\b")),
      (r"C:\a\b\", Some(r"C:\a\b")),
      (r"C:/a/b", Some(r"C:\a\b")),
      (r"C:\a\\b\.\c", Some(r"C:\a\b\c")),
      (r"c:\a", Some(r"c:\a")),
      (r"C:\a b\..c", Some(r"C:\a b\..c")),
      // A share.
      (r"\\server\share\a\b", Some(r"\\server\share\a\b")),
      (r"//server/share/a", Some(r"\\server\share\a")),
      (r"\\server\share\\a\.\b\", Some(r"\\server\share\a\b")),
      // A verbatim path becomes the plain form.
      (r"\\?\C:\a\b", Some(r"C:\a\b")),
      (r"\\?\C:\a\\b\", Some(r"C:\a\b")),
      (r"\\?\UNC\server\share\a", Some(r"\\server\share\a")),
      (r"\\?\unc\server\share\a", Some(r"\\server\share\a")),
      // Not absolute.
      ("", None),
      (r"a\b", None),
      (r"\a\b", None),
      (r"C:a\b", None),
      ("C:", None),
      ("/a/b", None),
      // `..`, NUL.
      (r"C:\a\..\b", Some(r"C:\b")),
      (r"C:\a\..", None),
      (r"\\server\share\a\..\b", Some(r"\\server\share\b")),
      (r"\\server\share\..", None),
      ("C:\\a\0b", None),
      // A top directory.
      (r"C:\", None),
      (r"C:\.", None),
      (r"\\server\share", None),
      (r"\\server\share\", None),
      (r"\\?\C:\", None),
      (r"\\?\UNC\server\share\", None),
      // No share, a device, a volume name, a `/` that a verbatim path would keep as a character.
      (r"\\server", None),
      (r"\\.\pipe\x", None),
      (r"\\?\Volume{1b3b1146-4076-11e1-84aa-806e6f6e6963}\a", None),
      (r"\\?\C:\a/b", None),
    ];
    for (given, expected) in table {
      assert_eq!(clean_windows(given).as_deref(), expected, "{given:?}");
      // A clean path with a drive letter is taken as it is. One on a share is cleaned again.
      let taken = expected == Some(given) && !given.starts_with('\\');
      assert_eq!(windows_is_clean(given.as_bytes()), taken, "{given:?}");
      let wide: Vec<u16> = given.encode_utf16().collect();
      assert_eq!(windows_is_clean(&wide), taken, "{given:?}");
    }
  }

  /// The length of a name fits in 16 bits as bytes, also for a top directory of two UTF-16 names.
  #[test]
  fn a_name_has_a_longest_length() {
    assert!(u16::try_from(2 * 3 * MAX_NAME + 4).is_ok());

    let (longest, too_long) = ("x".repeat(MAX_NAME), "x".repeat(MAX_NAME + 1));
    assert!(clean_unix(&format!("/a/{longest}/b")).is_some());
    assert_eq!(clean_unix(&format!("/a/{too_long}/b")), None);
    assert_eq!(clean_unix(&format!("/{too_long}")), None);

    assert!(clean_windows(&format!(r"C:\a\{longest}")).is_some());
    assert_eq!(clean_windows(&format!(r"C:\a\{too_long}")), None);
    assert!(clean_windows(&format!(r"\\{longest}\{longest}\a")).is_some());
    assert_eq!(clean_windows(&format!(r"\\{too_long}\share\a")), None);
    assert_eq!(clean_windows(&format!(r"\\server\{too_long}\a")), None);
    assert_eq!(clean_windows(&format!(r"\\?\UNC\server\{too_long}\a")), None);
  }

  #[cfg(unix)]
  #[test]
  fn keeps_bytes_that_are_not_utf8() {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt, path::Path};

    let given = Path::new(OsStr::from_bytes(b"/a/\xff\xfe//b/"));
    let expected = Path::new(OsStr::from_bytes(b"/a/\xff\xfe/b"));
    assert_eq!(normalize(given).as_deref(), Some(expected));
  }

  #[cfg(windows)]
  #[test]
  fn the_host_rules_are_the_windows_rules() {
    use std::path::Path;

    assert_eq!(normalize(Path::new(r"\\?\C:\a\.\b\")).as_deref(), Some(Path::new(r"C:\a\b")));
    assert_eq!(normalize(Path::new(r"C:\")), None);
  }
}
