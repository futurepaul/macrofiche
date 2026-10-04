//! Names the API takes, and their checks: orgs, repositories, branches,
//! file paths, modes, object ids, and a commit's expected head. A name
//! that fails its check is a 400, never stored.

use crate::limits::{BRANCH_NAME_BYTES_MAX, ORG_NAME_BYTES_MAX, PATH_BYTES_MAX, REPO_NAME_BYTES_MAX};

/// An org: 1 to 64 bytes of `[A-Za-z0-9._-]`, not starting with `.` (it
/// names a directory).
pub fn valid_org(s: &str) -> bool {
    !s.is_empty() && s.len() <= ORG_NAME_BYTES_MAX && !s.starts_with('.') && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A repository: 1 to 128 bytes of `[A-Za-z0-9._-]` in segments joined by
/// `/` (the service's names may hold one), no segment empty, `.` or `..`.
/// fragment's are `<prefix><label>--<username>`, at most 117 bytes of
/// `[a-z0-9-]` (contract section 2).
pub fn valid_repo_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= REPO_NAME_BYTES_MAX
        && s.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != ".." && seg.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')))
}

/// A branch: what `git check-ref-format` takes below `refs/heads/`, kept
/// to printable ASCII: no space or `~^:?*[\`, no control byte, no `..` or
/// `@{`, not `@`, no segment empty or starting with `.` or ending with
/// `.lock`, not ending with `.`, at most 255 bytes.
pub fn valid_branch(s: &str) -> bool {
    if s.is_empty() || s.len() > BRANCH_NAME_BYTES_MAX || s == "@" || s.contains("..") || s.contains("@{") || s.ends_with('.') {
        return false;
    }
    if !s.bytes().all(|b| (0x21..0x7f).contains(&b) && !matches!(b, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')) {
        return false;
    }
    s.split('/').all(|seg| !seg.is_empty() && !seg.starts_with('.') && !seg.ends_with(".lock"))
}

/// Why a file path is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathFault {
    Empty,
    TooLong,
    ControlByte,
    /// A leading or trailing `/`, or an empty segment.
    Slashes,
    /// A `.` or `..` segment.
    Dots,
    /// A segment git refuses: `.git` in any case or with trailing dots or
    /// spaces, or its 8.3 short name `git~1`.
    GitDir,
}

impl PathFault {
    pub fn describe(self) -> &'static str {
        match self {
            PathFault::Empty => "an empty path",
            PathFault::TooLong => "a path longer than 4096 bytes",
            PathFault::ControlByte => "a path holding a control byte",
            PathFault::Slashes => "a path with a leading, trailing, or doubled /",
            PathFault::Dots => "a path with a . or .. segment",
            PathFault::GitDir => "a path with a .git segment",
        }
    }
}

/// A file path git can store and the API can carry: relative, at most
/// 4096 bytes, no control byte, no empty, `.`, `..` or `.git` segment.
/// fragment's cell writes only paths that pass its own stricter check
/// (contract section 4).
pub fn check_path(s: &str) -> Result<(), PathFault> {
    if s.is_empty() {
        return Err(PathFault::Empty);
    }
    if s.len() > PATH_BYTES_MAX {
        return Err(PathFault::TooLong);
    }
    if s.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(PathFault::ControlByte);
    }
    for seg in s.split('/') {
        if seg.is_empty() {
            return Err(PathFault::Slashes);
        }
        if seg == "." || seg == ".." {
            return Err(PathFault::Dots);
        }
        let bare = seg.trim_end_matches(['.', ' ']);
        if bare.eq_ignore_ascii_case(".git") || seg.eq_ignore_ascii_case("git~1") {
            return Err(PathFault::GitDir);
        }
    }
    Ok(())
}

/// A file mode a commit may write: `100644`, or `100755`.
pub fn parse_mode(s: &str) -> Option<u32> {
    match s {
        "100644" => Some(0o100644),
        "100755" => Some(0o100755),
        _ => None,
    }
}

/// 40 lowercase hex characters: a SHA-1 object id (contract section 4).
pub fn is_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub const ZERO_SHA: &str = "0000000000000000000000000000000000000000";

/// What a write says its target branch must be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expected {
    /// No expectation: the write lands on whatever the tip is.
    Any,
    /// The branch must not exist (an empty or all-zero sha).
    Absent,
    /// The branch's tip must be this sha.
    At(String),
}

/// `expected_target_sha` as sent: absent, empty or zeros, or a sha.
pub fn parse_expected(v: Option<&str>) -> Option<Expected> {
    match v {
        None => Some(Expected::Any),
        Some("") => Some(Expected::Absent),
        Some(s) if s == ZERO_SHA => Some(Expected::Absent),
        Some(s) if is_sha(s) => Some(Expected::At(s.to_string())),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Goal: fragment's names pass and the service's slashes too; what
    // would escape a directory or confuse a ref does not. Method: valid
    // and invalid names of each kind.
    #[test]
    fn names() {
        assert!(valid_org("fragment-dev") && valid_org("finite") && !valid_org("") && !valid_org(".x") && !valid_org("a/b") && !valid_org(&"o".repeat(65)));
        for ok in ["todo--paul", "main--todo--paul", "acme/web", "a.b_c-d"] {
            assert!(valid_repo_name(ok), "{ok}");
        }
        for bad in ["", "/a", "a/", "a//b", "../x", "a/./b", "a b", "a%2Fb", &"r".repeat(129)] {
            assert!(!valid_repo_name(bad), "{bad}");
        }
        for ok in ["main", "live", "preview/0123456789ab", "feature/x-1", "a.b"] {
            assert!(valid_branch(ok), "{ok}");
        }
        for bad in ["", "@", "a..b", "a@{1}", "a.", ".a", "a/.b", "a.lock", "a/b.lock/c", "a b", "a~1", "a^", "a:b", "a?", "a*", "a[", "a\\b", "/a", "a/", "a//b", "a\x7fb", "café", &"b".repeat(256)] {
            assert!(!valid_branch(bad), "{bad:?}");
        }
    }

    // Goal: paths git stores pass (spaces, quotes, non-ASCII, dots inside
    // names), and each fault is named. Method: one path per fault.
    #[test]
    fn paths() {
        for ok in ["notes/a.md", "a b.txt", "d/\"q\".md", "café/ü.md", ".gitignore", "x/.github/w.yml", "a..b", "...", "back\\slash"] {
            assert_eq!(check_path(ok), Ok(()), "{ok}");
        }
        assert_eq!(check_path(""), Err(PathFault::Empty));
        assert_eq!(check_path(&"p".repeat(4097)), Err(PathFault::TooLong));
        assert_eq!(check_path("a\nb"), Err(PathFault::ControlByte));
        assert_eq!(check_path("a\0b"), Err(PathFault::ControlByte));
        assert_eq!(check_path("/a"), Err(PathFault::Slashes));
        assert_eq!(check_path("a/"), Err(PathFault::Slashes));
        assert_eq!(check_path("a//b"), Err(PathFault::Slashes));
        assert_eq!(check_path("./a"), Err(PathFault::Dots));
        assert_eq!(check_path("a/../b"), Err(PathFault::Dots));
        for git in [".git", ".git/config", "x/.GIT/y", ".git.", ".git ", "GIT~1/x"] {
            assert_eq!(check_path(git), Err(PathFault::GitDir), "{git}");
        }
    }

    // Goal: modes, shas, and expected heads, each form.
    #[test]
    fn modes_shas_expected() {
        assert_eq!(parse_mode("100644"), Some(0o100644));
        assert_eq!(parse_mode("100755"), Some(0o100755));
        assert_eq!(parse_mode("120000"), None);
        let sha = "3033acb8fd0d39124facfc02ff13d974de0e6568";
        assert!(is_sha(sha) && !is_sha(&sha.to_uppercase()) && !is_sha(&sha[1..]));
        assert_eq!(parse_expected(None), Some(Expected::Any));
        assert_eq!(parse_expected(Some("")), Some(Expected::Absent));
        assert_eq!(parse_expected(Some(ZERO_SHA)), Some(Expected::Absent));
        assert_eq!(parse_expected(Some(sha)), Some(Expected::At(sha.into())));
        assert_eq!(parse_expected(Some("main")), None);
    }
}
