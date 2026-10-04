//! The `fast-import` stream for one commit, as bytes: the command, its
//! people and time, its message, its parent, and each file's change, with
//! paths C-quoted. The blobs' bytes are not here: the caller streams each
//! one after its `data <len>` line (`linux::repo::fast_import`).

use std::io::Write;

use thiserror::Error;

use crate::oid::Oid;

/// A person, as git records one: a name and an email, neither holding
/// `<`, `>`, a newline, or NUL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ident {
    pub name: String,
    pub email: String,
}

/// Bytes of an ident's name or email.
pub const IDENT_BYTES_MAX: usize = 256;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IdentError {
    #[error("an author's or committer's {0} is empty, longer than {IDENT_BYTES_MAX} bytes, or holds <, >, a newline, or NUL")]
    Bad(&'static str),
}

impl Ident {
    pub fn new(name: &str, email: &str) -> Result<Ident, IdentError> {
        let ok = |s: &str| !s.is_empty() && s.len() <= IDENT_BYTES_MAX && !s.bytes().any(|b| matches!(b, b'<' | b'>' | b'\n' | b'\r' | 0)) && s.trim() == s;
        if !ok(name) {
            return Err(IdentError::Bad("name"));
        }
        if !ok(email) {
            return Err(IdentError::Bad("email"));
        }
        Ok(Ident { name: name.into(), email: email.into() })
    }

    /// `Name <email> <unix s> +0000`.
    pub fn line(&self, time: i64) -> String {
        format!("{} <{}> {time} +0000", self.name, self.email)
    }
}

/// A path as fast-import reads one: in double quotes, with `"` and `\`
/// escaped. The caller has refused control bytes (contract `path`), so
/// nothing else needs escaping; bytes above 0x7f pass through.
pub fn quote(path: &str) -> String {
    assert!(!path.bytes().any(|b| b < 0x20 || b == 0x7f), "paths reach fast-import without control bytes");
    let mut q = String::with_capacity(path.len() + 2);
    q.push('"');
    for c in path.chars() {
        if c == '"' || c == '\\' {
            q.push('\\');
        }
        q.push(c);
    }
    q.push('"');
    q
}

/// One commit's head: everything before its file changes.
pub struct Head<'a> {
    /// The ref fast-import writes (a temporary one: the branch moves by
    /// `update-ref`, under compare-and-swap).
    pub tmp_ref: &'a str,
    pub author: &'a Ident,
    pub committer: &'a Ident,
    pub time: i64,
    pub message: &'a [u8],
    /// The parent; none for a branch's first commit.
    pub from: Option<Oid>,
}

/// The mark the commit gets, so `get-mark` can name it.
pub const COMMIT_MARK: &str = ":1";

pub fn head(h: &Head<'_>) -> Vec<u8> {
    assert!(h.tmp_ref.starts_with("refs/macrofiche/tmp/") && !h.tmp_ref.contains(char::is_whitespace));
    let mut s = Vec::with_capacity(256 + h.message.len());
    write!(s, "commit {}\nmark {COMMIT_MARK}\nauthor {}\ncommitter {}\ndata {}\n", h.tmp_ref, h.author.line(h.time), h.committer.line(h.time), h.message.len()).expect("a Vec takes writes");
    s.extend_from_slice(h.message);
    s.push(b'\n');
    if let Some(p) = h.from {
        writeln!(s, "from {p}").expect("a Vec takes writes");
    }
    s
}

/// `M <mode> inline <path>` and the `data` line; the blob's bytes follow,
/// then one newline.
pub fn upsert(mode: u32, path: &str, len: u64) -> Vec<u8> {
    assert!(mode == crate::parse::mode::FILE || mode == crate::parse::mode::EXECUTABLE);
    format!("M {mode:06o} inline {}\ndata {len}\n", quote(path)).into_bytes()
}

pub fn delete(path: &str) -> Vec<u8> {
    format!("D {}\n", quote(path)).into_bytes()
}

/// After the last change: the commit ends, its id is asked for, the
/// stream is done.
pub const TAIL: &[u8] = b"\nget-mark :1\ndone\n";

#[cfg(test)]
mod tests {
    use super::*;

    // Goal: paths with quotes, backslashes, spaces and non-ASCII are
    // quoted as fast-import unquotes them. Method: literal expectations.
    #[test]
    fn paths_are_quoted() {
        assert_eq!(quote("a b.txt"), "\"a b.txt\"");
        assert_eq!(quote("d/\"q\".md"), "\"d/\\\"q\\\".md\"");
        assert_eq!(quote("back\\slash"), "\"back\\\\slash\"");
        assert_eq!(quote("café/ü.md"), "\"café/ü.md\"");
    }

    // Goal: idents git would mangle or refuse are refused. Method: each
    // forbidden byte, and empties.
    #[test]
    fn idents_are_checked() {
        assert!(Ident::new("fragment/1a2b3c4d", "1a2b3c4d@users.fragment").is_ok());
        assert!(Ident::new("npub1abc", "npub1abc@fragment.invalid").is_ok());
        for (n, e) in [("", "a@x"), ("a", ""), ("a<b", "a@x"), ("a", "a>x"), ("a\nb", "a@x"), (" a", "a@x"), ("a", "a@x\0")] {
            assert!(Ident::new(n, e).is_err(), "{n:?} {e:?}");
        }
        assert!(Ident::new(&"n".repeat(IDENT_BYTES_MAX + 1), "a@x").is_err());
    }

    // Goal: a commit's head is the stream fast-import 2.55.0 takes (the
    // same bytes the engine test feeds it). Method: the bytes.
    #[test]
    fn a_head_is_written() {
        let a = Ident::new("A", "a@x").unwrap();
        let from = Oid::parse("3033acb8fd0d39124facfc02ff13d974de0e6568").unwrap();
        let h = head(&Head { tmp_ref: "refs/macrofiche/tmp/7", author: &a, committer: &a, time: 1_790_000_000, message: b"msg", from: Some(from) });
        assert_eq!(
            String::from_utf8(h).unwrap(),
            "commit refs/macrofiche/tmp/7\nmark :1\nauthor A <a@x> 1790000000 +0000\ncommitter A <a@x> 1790000000 +0000\ndata 3\nmsg\nfrom 3033acb8fd0d39124facfc02ff13d974de0e6568\n"
        );
        assert_eq!(String::from_utf8(upsert(0o100644, "a b", 2)).unwrap(), "M 100644 inline \"a b\"\ndata 2\n");
        assert_eq!(String::from_utf8(delete("old.md")).unwrap(), "D \"old.md\"\n");
    }
}
