use std::{
  ffi::{OsStr, OsString},
  path::{MAIN_SEPARATOR_STR, Path, PathBuf, is_separator},
};

/// The longest name that can be watched, in bytes. No disk takes a name of more than 765 bytes,
/// and the tree keeps the length of a name in 16 bits, where a share is two names.
const MAX_NAME: usize = 10_000;

/// The registered form of `path`: absolute, with no `.`, `..`, repeated or trailing separator.
///
/// `None` if the path is relative, has a NUL, is a top directory (`/`, `C:\`, `\\server\share\`)
/// or goes above one, has a name longer than `MAX_NAME`, or on Windows a name that Windows does
/// not take as it is.
pub fn normalize(path: &Path) -> Option<PathBuf> {
  // Every character that the rules look at is one ASCII byte, on Windows too.
  let text = path.as_os_str().as_encoded_bytes();
  // Elsewhere a path must be UTF-8.
  #[cfg(not(any(unix, windows)))]
  path.to_str()?;
  #[cfg(windows)]
  let clean = windows_rules::windows(text)?;
  #[cfg(not(windows))]
  let clean = unix(text)?;
  // SAFETY: `clean` is pieces of `text` cut next to ASCII bytes, joined by ASCII bytes.
  Some(PathBuf::from(unsafe { OsString::from_encoded_bytes_unchecked(clean) }))
}

/// The Unix rules: the path starts with `/`, and only `/` separates.
#[cfg(any(not(windows), test))]
fn unix(text: &[u8]) -> Option<Vec<u8>> {
  let rest = text.strip_prefix(b"/")?;
  // The clean form is never longer than the text.
  let mut clean = Vec::with_capacity(text.len());
  clean.push(b'/');
  push_names(&mut clean, rest, |byte| byte == b'/', b'/', |_| true)?;
  Some(clean)
}

/// The Windows rules, compiled for the tests on every system.
#[cfg(any(windows, test))]
mod windows_rules {
  use super::{MAX_NAME, push_names};

  pub(super) fn windows(text: &[u8]) -> Option<Vec<u8>> {
    let mut clean = Vec::with_capacity(text.len());
    let rest = if let Some(verbatim) = strip(text, br"\\?\") {
      // In a verbatim path `/` is a plain character, which the plain form cannot hold.
      if verbatim.contains(&b'/') {
        return None;
      }
      match strip(verbatim, br"UNC\") {
        Some(share) => push_share(&mut clean, share)?,
        None => push_drive(&mut clean, verbatim)?,
      }
    } else if let [b'\\' | b'/', b'\\' | b'/', share @ ..] = text {
      push_share(&mut clean, share)?
    } else {
      push_drive(&mut clean, text)?
    };
    push_names(&mut clean, rest, is_separator, b'\\', is_name)?;
    Some(clean)
  }

  /// `text` after `prefix`, in any case of its ASCII letters.
  fn strip<'a>(text: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    let (head, rest) = text.split_at_checked(prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then_some(rest)
  }

  /// Appends `\\server\share\` and returns the rest. `text` starts at the server.
  fn push_share<'a>(clean: &mut Vec<u8>, text: &'a [u8]) -> Option<&'a [u8]> {
    let mut rest = text;
    clean.extend_from_slice(br"\\");
    for is_server in [true, false] {
      let start = rest.iter().position(|&byte| !is_separator(byte))?;
      // Windows refuses a separator too many before the server, and takes one before the share.
      if is_server && start != 0 {
        return None;
      }
      let name = &rest[start..];
      let end = name.iter().position(|&byte| is_separator(byte)).unwrap_or(name.len());
      let (name, after) = name.split_at(end);
      // `\\.\` and `\\?\` name devices, not shares.
      if matches!(name, [b'.' | b'?']) || name.contains(&0) || name.len() > MAX_NAME {
        return None;
      }
      clean.extend_from_slice(name);
      clean.push(b'\\');
      rest = after;
    }
    Some(rest)
  }

  /// Appends `C:\` and returns the rest. `C:name` and `\name` are not absolute.
  fn push_drive<'a>(clean: &mut Vec<u8>, text: &'a [u8]) -> Option<&'a [u8]> {
    let [letter, b':', b'\\' | b'/', rest @ ..] = text else { return None };
    if !letter.is_ascii_alphabetic() {
      return None;
    }
    clean.extend([*letter, b':', b'\\']);
    Some(rest)
  }

  fn is_separator(byte: u8) -> bool {
    matches!(byte, b'\\' | b'/')
  }

  /// Whether Windows finds an entry under this very name. It drops a dot or a space at the end,
  /// reads `:` as a stream, and has no name with a control character or one of `< > " | ? *`:
  /// such a path would never get an event.
  fn is_name(name: &[u8]) -> bool {
    let is_kept = |byte: u8| byte >= 0x20 && !br#"<>:"|?*"#.contains(&byte);
    !matches!(name.last(), Some(b'.' | b' ')) && name.iter().all(|&byte| is_kept(byte))
  }
}

/// Appends the names of `text` to `clean`, which holds the top directory and ends with a
/// separator. `None` if no name is left: the path is a top directory.
fn push_names(
  clean: &mut Vec<u8>,
  text: &[u8],
  separator: impl Fn(u8) -> bool,
  join: u8,
  is_name: fn(&[u8]) -> bool,
) -> Option<()> {
  let top = clean.len();
  for name in text.split(|&byte| separator(byte)) {
    match name {
      [] | [b'.'] => {}
      // `..` takes the name before it away. Above the top directory there is none.
      [b'.', b'.'] => {
        if clean.len() == top {
          return None;
        }
        let last = clean[top..].iter().rposition(|&byte| byte == join).unwrap_or(0);
        clean.truncate(top + last);
      }
      _ if name.contains(&0) || name.len() > MAX_NAME || !is_name(name) => return None,
      _ => {
        if clean.len() > top {
          clean.push(join);
        }
        clean.extend_from_slice(name);
      }
    }
  }
  (clean.len() > top).then_some(())
}

/// Splits a clean path at its top directory: `/a/b` gives `/` and `a/b`.
pub(crate) fn split_top(path: &Path) -> Option<(&OsStr, &Path)> {
  #[cfg(unix)]
  let top = Path::new("/");
  // `C:\` or `\\server\share\`.
  #[cfg(not(unix))]
  let top = path.ancestors().last()?;
  let rest = path.strip_prefix(top).ok()?;
  Some((top.as_os_str(), rest))
}

/// Appends `name` to `path` by its text. `PathBuf::push` would start the path anew for a name
/// that reads as a drive or a root (`C:`, `\\a`), which Windows can list on a disk that another
/// system filled.
pub(crate) fn push_name(path: &mut PathBuf, name: &OsStr) {
  let text = path.as_mut_os_string();
  if text.as_encoded_bytes().last().is_some_and(|&byte| !is_separator(char::from(byte))) {
    text.push(MAIN_SEPARATOR_STR);
  }
  text.push(name);
}

#[cfg(test)]
mod tests {
  #[cfg(any(unix, windows))]
  use super::normalize;
  use super::{MAX_NAME, unix, windows_rules::windows};

  fn clean_unix(text: &str) -> Option<String> {
    unix(text.as_bytes()).map(|bytes| String::from_utf8(bytes).unwrap())
  }

  fn clean_windows(text: &str) -> Option<String> {
    windows(text.as_bytes()).map(|bytes| String::from_utf8(bytes).unwrap())
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
      // NUL.
      ("/a\0b", None),
      // A top directory has no parent to watch it from.
      ("/", None),
      ("//", None),
      ("/.", None),
      ("/./", None),
    ];
    for (given, expected) in table {
      assert_eq!(clean_unix(given).as_deref(), expected, "{given:?}");
    }
  }

  #[test]
  fn windows_paths() {
    let table = [
      (r"C:\a\b", Some(r"C:\a\b")),
      (r"C:\a\b\", Some(r"C:\a\b")),
      ("C:/a/b", Some(r"C:\a\b")),
      (r"C:\a\\b\.\c", Some(r"C:\a\b\c")),
      (r"c:\a", Some(r"c:\a")),
      (r"C:\a b\..c", Some(r"C:\a b\..c")),
      // A share.
      (r"\\server\share\a\b", Some(r"\\server\share\a\b")),
      ("//server/share/a", Some(r"\\server\share\a")),
      (r"\\server\share\\a\.\b\", Some(r"\\server\share\a\b")),
      // A verbatim path becomes the plain form.
      (r"\\?\C:\a\b", Some(r"C:\a\b")),
      (r"\\?\C:\a\\b\", Some(r"C:\a\b")),
      (r"\\?\UNC\server\share\a", Some(r"\\server\share\a")),
      (r"\\?\unc\server\share\a", Some(r"\\server\share\a")),
      // Names outside ASCII are bytes like the others.
      ("C:\\\u{e9}\\\u{4e2d}", Some("C:\\\u{e9}\\\u{4e2d}")),
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
      // A separator too many before the share, not before the server.
      (r"\\server\\share\f", Some(r"\\server\share\f")),
      ("//server//share/f", Some(r"\\server\share\f")),
      (r"\\\server\share\f", None),
      ("///server/share/f", None),
      // No share, a device, a volume name, a `/` that a verbatim path would keep as a character.
      (r"\\server", None),
      (r"\\.\pipe\x", None),
      ("//?/C:/a", None),
      (r"\\?\Volume{1b3b1146-4076-11e1-84aa-806e6f6e6963}\a", None),
      (r"\\?\C:\a/b", None),
      // A name that Windows does not take as it is: a dot or a space at its end, a stream, a
      // wildcard, a control character. Also in the verbatim form, which is not kept.
      (r"C:\a\b.", None),
      (r"C:\a\b ", None),
      (r"C:\a.\b", None),
      (r"C:\a\...", None),
      (r"C:\a\b.txt:stream", None),
      (r"C:\a\*.js", None),
      (r"C:\a\b?", None),
      (r"C:\a\b<c", None),
      (r"C:\a\b>c", None),
      (r#"C:\a\"b""#, None),
      (r"C:\a\b|c", None),
      ("C:\\a\\b\tc", None),
      (r"\\server\share\a.", None),
      (r"\\?\C:\a\b.", None),
    ];
    for (given, expected) in table {
      assert_eq!(clean_windows(given).as_deref(), expected, "{given:?}");
    }
  }

  /// The length of a name fits in 16 bits, also for a top directory of two names.
  #[test]
  fn a_name_has_a_longest_length() {
    u16::try_from(2 * MAX_NAME + 4).unwrap();

    let (longest, too_long) = ("x".repeat(MAX_NAME), "x".repeat(MAX_NAME + 1));
    assert!(clean_unix(&format!("/a/{longest}/b")).is_some());
    assert_eq!(clean_unix(&format!("/a/{too_long}/b")), None);

    assert!(clean_windows(&format!(r"C:\a\{longest}")).is_some());
    assert_eq!(clean_windows(&format!(r"C:\a\{too_long}")), None);
    assert!(clean_windows(&format!(r"\\{longest}\{longest}\a")).is_some());
    assert_eq!(clean_windows(&format!(r"\\{too_long}\share\a")), None);
    assert_eq!(clean_windows(&format!(r"\\server\{too_long}\a")), None);
    // The length is in bytes on Windows too: `é` is two.
    let (longest, too_long) = ("\u{e9}".repeat(MAX_NAME / 2), "\u{e9}".repeat(MAX_NAME / 2 + 1));
    assert!(clean_windows(&format!(r"C:\a\{longest}")).is_some());
    assert_eq!(clean_windows(&format!(r"C:\a\{too_long}")), None);
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
    use std::{
      ffi::OsString,
      os::windows::ffi::OsStringExt,
      path::{Path, PathBuf},
    };

    assert_eq!(normalize(Path::new(r"\\?\C:\a\.\b\")).as_deref(), Some(Path::new(r"C:\a\b")));
    assert_eq!(normalize(Path::new(r"C:\")), None);

    // A name that is no UTF-16 is kept as it is, and the two halves of a character in two names
    // stay apart.
    let wide = |text: &str| text.encode_utf16().collect::<Vec<u16>>();
    let given = [wide(r"C:\a\"), vec![0xD83D], wide(r"\x\..\"), vec![0xDE00], wide(r"\.\")];
    let expected = [wide(r"C:\a\"), vec![0xD83D], wide(r"\"), vec![0xDE00]];
    let path = |units: Vec<u16>| PathBuf::from(OsString::from_wide(&units));
    assert_eq!(normalize(&path(given.concat())), Some(path(expected.concat())));
  }
}
