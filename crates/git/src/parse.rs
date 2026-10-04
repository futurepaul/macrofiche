//! git's plumbing output, parsed: `ls-tree -r -l -z`, `diff-tree -r -z`,
//! `merge-tree --write-tree --name-only -z`, `cat-file --batch-command -Z`
//! headers, raw commit objects, `for-each-ref` and `rev-list` lines. Every
//! parser takes bytes as git wrote them and refuses what it does not
//! expect; none trusts a count it has not checked.

use thiserror::Error;

use crate::oid::Oid;

#[derive(Debug, Error, PartialEq, Eq)]
#[error("unexpected output from git {what}: {detail}")]
pub struct ParseError {
    pub what: &'static str,
    pub detail: String,
}

fn bad(what: &'static str, detail: impl Into<String>) -> ParseError {
    ParseError { what, detail: detail.into() }
}

/// An object's kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Blob,
    Tree,
    Commit,
    Tag,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "blob" => Kind::Blob,
            "tree" => Kind::Tree,
            "commit" => Kind::Commit,
            "tag" => Kind::Tag,
            _ => return None,
        })
    }
}

/// File modes git stores.
pub mod mode {
    pub const FILE: u32 = 0o100644;
    pub const EXECUTABLE: u32 = 0o100755;
    pub const SYMLINK: u32 = 0o120000;
    pub const GITLINK: u32 = 0o160000;
    pub const TREE: u32 = 0o040000;

    /// As git writes it in a tree listing: six octal digits.
    pub fn text(m: u32) -> String {
        format!("{m:06o}")
    }
}

/// One entry of `ls-tree -r -l -z`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: u32,
    pub kind: Kind,
    pub oid: Oid,
    /// A blob's size; none for a submodule.
    pub size: Option<u64>,
    /// As stored; a path that is not UTF-8 is read lossily.
    pub path: String,
}

/// `ls-tree -r -l -z --full-tree`: `<mode> SP <type> SP <oid> SP+ <size>
/// TAB <path> NUL`, at most `max` entries.
pub fn ls_tree(out: &[u8], max: usize) -> Result<Vec<TreeEntry>, ParseError> {
    let mut entries = Vec::new();
    for rec in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        if entries.len() == max {
            return Err(bad("ls-tree", format!("more than {max} entries")));
        }
        let tab = rec.iter().position(|b| *b == b'\t').ok_or_else(|| bad("ls-tree", "an entry without a tab"))?;
        let head = std::str::from_utf8(&rec[..tab]).map_err(|_| bad("ls-tree", "a header that is not text"))?;
        let path = String::from_utf8_lossy(&rec[tab + 1..]).into_owned();
        let mut f = head.split_ascii_whitespace();
        let (Some(m), Some(k), Some(o), Some(s), None) = (f.next(), f.next(), f.next(), f.next(), f.next()) else {
            return Err(bad("ls-tree", format!("header {head:?}")));
        };
        let mode = u32::from_str_radix(m, 8).map_err(|_| bad("ls-tree", format!("mode {m:?}")))?;
        let kind = Kind::parse(k).ok_or_else(|| bad("ls-tree", format!("type {k:?}")))?;
        let oid = Oid::parse(o).ok_or_else(|| bad("ls-tree", format!("id {o:?}")))?;
        let size = match s {
            "-" => None,
            n => Some(n.parse().map_err(|_| bad("ls-tree", format!("size {n:?}")))?),
        };
        if path.is_empty() {
            return Err(bad("ls-tree", "an empty path"));
        }
        entries.push(TreeEntry { mode, kind, oid, size, path });
    }
    Ok(entries)
}

/// One changed path of `diff-tree -r -z --no-renames <a> <b>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffEntry {
    pub old_mode: u32,
    pub new_mode: u32,
    pub old: Oid,
    pub new: Oid,
    /// `A`, `D`, `M`, or `T`.
    pub status: u8,
    pub path: String,
}

/// `:<old mode> <new mode> <old> <new> <status> NUL <path> NUL`, at most
/// `max` entries.
pub fn diff_tree(out: &[u8], max: usize) -> Result<Vec<DiffEntry>, ParseError> {
    let mut fields = out.split(|b| *b == 0);
    let mut entries = Vec::new();
    // Bounded by the output's fields, two per entry, and by `max`.
    while let Some(head) = fields.next() {
        if head.is_empty() {
            continue;
        }
        if entries.len() == max {
            return Err(bad("diff-tree", format!("more than {max} entries")));
        }
        let head = std::str::from_utf8(head).map_err(|_| bad("diff-tree", "a header that is not text"))?;
        let rest = head.strip_prefix(':').ok_or_else(|| bad("diff-tree", format!("header {head:?}")))?;
        let f: Vec<&str> = rest.split(' ').collect();
        let [om, nm, oo, no, st] = f.as_slice() else { return Err(bad("diff-tree", format!("header {head:?}"))) };
        let path = fields.next().filter(|p| !p.is_empty()).ok_or_else(|| bad("diff-tree", "an entry without a path"))?;
        let parse_mode = |m: &str| u32::from_str_radix(m, 8).map_err(|_| bad("diff-tree", format!("mode {m:?}")));
        let parse_oid = |o: &str| Oid::parse(o).ok_or_else(|| bad("diff-tree", format!("id {o:?}")));
        let status = match st.as_bytes() {
            [s @ (b'A' | b'D' | b'M' | b'T')] => *s,
            _ => return Err(bad("diff-tree", format!("status {st:?}"))),
        };
        entries.push(DiffEntry {
            old_mode: parse_mode(om)?,
            new_mode: parse_mode(nm)?,
            old: parse_oid(oo)?,
            new: parse_oid(no)?,
            status,
            path: String::from_utf8_lossy(path).into_owned(),
        });
    }
    Ok(entries)
}

/// `merge-tree --write-tree --no-messages --name-only -z`: the merged
/// tree, then each conflicted path (once, sorted).
pub fn merge_tree(out: &[u8]) -> Result<(Oid, Vec<String>), ParseError> {
    let mut fields = out.split(|b| *b == 0);
    let tree = fields.next().and_then(|t| std::str::from_utf8(t).ok()).and_then(|t| Oid::parse(t.trim())).ok_or_else(|| bad("merge-tree", "no tree id first"))?;
    let mut paths: Vec<String> = fields.filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).collect();
    paths.sort();
    paths.dedup();
    Ok((tree, paths))
}

/// A `cat-file --batch-command -Z` answer's header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatHeader {
    Found { oid: Oid, kind: Kind, size: u64 },
    Missing,
    Ambiguous,
}

/// `<oid> SP <type> SP <size>`, `<name> SP missing`, or `<name> SP
/// ambiguous` (the NUL already taken off).
pub fn cat_header(line: &[u8]) -> Result<CatHeader, ParseError> {
    let s = String::from_utf8_lossy(line);
    if s.ends_with(" missing") {
        return Ok(CatHeader::Missing);
    }
    if s.ends_with(" ambiguous") {
        return Ok(CatHeader::Ambiguous);
    }
    let mut f = s.split(' ');
    let (Some(o), Some(k), Some(n), None) = (f.next(), f.next(), f.next(), f.next()) else { return Err(bad("cat-file", format!("header {s:?}"))) };
    Ok(CatHeader::Found {
        oid: Oid::parse(o).ok_or_else(|| bad("cat-file", format!("id {o:?}")))?,
        kind: Kind::parse(k).ok_or_else(|| bad("cat-file", format!("type {k:?}")))?,
        size: n.parse().map_err(|_| bad("cat-file", format!("size {n:?}")))?,
    })
}

/// A person and a time, as a commit records them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub email: String,
    /// Unix seconds.
    pub time: i64,
    /// `+0000`, as written.
    pub tz: String,
}

fn signature(line: &str) -> Result<Signature, ParseError> {
    let open = line.rfind(" <").ok_or_else(|| bad("commit", format!("signature {line:?}")))?;
    let close = line.rfind("> ").filter(|c| *c > open).ok_or_else(|| bad("commit", format!("signature {line:?}")))?;
    let mut when = line[close + 2..].split(' ');
    let (Some(t), Some(tz), None) = (when.next(), when.next(), when.next()) else { return Err(bad("commit", format!("signature {line:?}"))) };
    Ok(Signature {
        name: line[..open].to_string(),
        email: line[open + 2..close].to_string(),
        time: t.parse().map_err(|_| bad("commit", format!("time {t:?}")))?,
        tz: tz.to_string(),
    })
}

/// A commit object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub tree: Oid,
    pub parents: Vec<Oid>,
    pub author: Signature,
    pub committer: Signature,
    pub message: String,
}

/// Parents a commit may name before it is refused as malformed.
pub const PARENTS_MAX: usize = 64;

/// A raw commit object (`cat-file` `contents`): headers, a blank line,
/// the message. Headers it does not use (`gpgsig`, `encoding`, `mergetag`
/// and their continuation lines) are skipped.
pub fn commit(raw: &[u8]) -> Result<Commit, ParseError> {
    let text = String::from_utf8_lossy(raw);
    let (head, message) = text.split_once("\n\n").unwrap_or((&text, ""));
    let (mut tree, mut parents, mut author, mut committer) = (None, Vec::new(), None, None);
    for line in head.lines() {
        if line.starts_with(' ') {
            continue;
        }
        let (k, v) = line.split_once(' ').unwrap_or((line, ""));
        match k {
            "tree" => tree = Some(Oid::parse(v).ok_or_else(|| bad("commit", format!("tree {v:?}")))?),
            "parent" => {
                if parents.len() == PARENTS_MAX {
                    return Err(bad("commit", "too many parents"));
                }
                parents.push(Oid::parse(v).ok_or_else(|| bad("commit", format!("parent {v:?}")))?)
            }
            "author" => author = Some(signature(v)?),
            "committer" => committer = Some(signature(v)?),
            _ => {}
        }
    }
    Ok(Commit {
        tree: tree.ok_or_else(|| bad("commit", "no tree"))?,
        parents,
        author: author.ok_or_else(|| bad("commit", "no author"))?,
        committer: committer.ok_or_else(|| bad("commit", "no committer"))?,
        message: message.to_string(),
    })
}

/// `rev-list` or `merge-base` output: one id a line, at most `max`.
pub fn oids(out: &[u8], max: usize) -> Result<Vec<Oid>, ParseError> {
    let text = std::str::from_utf8(out).map_err(|_| bad("rev-list", "output that is not text"))?;
    let mut v = Vec::new();
    for line in text.lines().filter(|l| !l.is_empty()) {
        if v.len() == max {
            return Err(bad("rev-list", format!("more than {max} ids")));
        }
        v.push(Oid::parse(line).ok_or_else(|| bad("rev-list", format!("id {line:?}")))?);
    }
    Ok(v)
}

/// `for-each-ref --format='%(objectname) %(refname)'`: (ref, id) pairs.
pub fn refs(out: &[u8], max: usize) -> Result<Vec<(String, Oid)>, ParseError> {
    let text = std::str::from_utf8(out).map_err(|_| bad("for-each-ref", "output that is not text"))?;
    let mut v = Vec::new();
    for line in text.lines().filter(|l| !l.is_empty()) {
        if v.len() == max {
            return Err(bad("for-each-ref", format!("more than {max} refs")));
        }
        let (o, r) = line.split_once(' ').ok_or_else(|| bad("for-each-ref", format!("line {line:?}")))?;
        v.push((r.to_string(), Oid::parse(o).ok_or_else(|| bad("for-each-ref", format!("id {o:?}")))?));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    const C1: &str = "3033acb8fd0d39124facfc02ff13d974de0e6568";
    const B1: &str = "32f95c0d1244a78b2be1bab8de17906fabb2c4a8";
    const E: &str = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";

    // Goal: a tree listing as git 2.55.0 wrote it (captured from the
    // engine) parses, sizes padded and paths with spaces and quotes kept;
    // a malformed or overlong one is refused. Method: the bytes, then
    // broken variants.
    #[test]
    fn ls_tree_parses_and_refuses() {
        let out = format!("100644 blob {B1}       2\ta b.txt\x00100644 blob {E}       0\td/\"q\".md\x00160000 commit {C1}       -\tsub\x00");
        let e = ls_tree(out.as_bytes(), 10).unwrap();
        assert_eq!(e.len(), 3);
        assert_eq!((e[0].mode, e[0].kind, e[0].size, e[0].path.as_str()), (mode::FILE, Kind::Blob, Some(2), "a b.txt"));
        assert_eq!(e[1].path, "d/\"q\".md");
        assert_eq!((e[2].kind, e[2].size), (Kind::Commit, None));
        assert!(ls_tree(out.as_bytes(), 2).is_err(), "more than max");
        assert!(ls_tree(format!("100644 blob {B1} 2 a\x00").as_bytes(), 10).is_err(), "no tab");
        assert!(ls_tree(format!("100644 blub {B1} 2\ta\x00").as_bytes(), 10).is_err(), "unknown type");
        assert!(ls_tree(format!("1006x4 blob {B1} 2\ta\x00").as_bytes(), 10).is_err(), "bad mode");
        assert!(ls_tree(b"", 10).unwrap().is_empty());
        assert_eq!(mode::text(mode::FILE), "100644");
    }

    // Goal: a raw diff as git wrote it parses; a broken one is refused.
    #[test]
    fn diff_tree_parses_and_refuses() {
        let z = "0".repeat(40);
        let out = format!(":000000 100644 {z} {B1} A\x00f.txt\x00:100644 000000 {E} {z} D\x00old\x00");
        let d = diff_tree(out.as_bytes(), 10).unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!((d[0].status, d[0].path.as_str(), d[0].new.to_string()), (b'A', "f.txt", B1.to_string()));
        assert_eq!((d[1].status, d[1].old_mode), (b'D', mode::FILE));
        assert!(diff_tree(format!(":000000 100644 {z} {B1} R100\x00a\x00b\x00").as_bytes(), 10).is_err(), "renames are off");
        assert!(diff_tree(format!(":000000 100644 {z} {B1} A\x00").as_bytes(), 10).is_err(), "no path");
        assert!(diff_tree(out.as_bytes(), 1).is_err(), "more than max");
    }

    // Goal: merge-tree's tree and conflicted paths, deduplicated (a path
    // conflicts at two stages). Method: git 2.55.0's bytes.
    #[test]
    fn merge_tree_parses() {
        let (t, p) = merge_tree(b"0b12aee89c795b421d0ff90b307831bb593a4a15\x00f.txt\x00f.txt\x00a\x00").unwrap();
        assert_eq!(t.to_string(), "0b12aee89c795b421d0ff90b307831bb593a4a15");
        assert_eq!(p, vec!["a", "f.txt"]);
        let (_, clean) = merge_tree(b"1032b79fee65384a640a8f48841c5f49b81e8947\x00").unwrap();
        assert!(clean.is_empty());
        assert!(merge_tree(b"fatal: refusing to merge unrelated histories\n").is_err());
    }

    // Goal: cat-file's three header forms. Method: git 2.55.0's bytes.
    #[test]
    fn cat_headers() {
        assert_eq!(cat_header(format!("{C1} commit 117").as_bytes()).unwrap(), CatHeader::Found { oid: Oid::parse(C1).unwrap(), kind: Kind::Commit, size: 117 });
        assert_eq!(cat_header(b"refs/heads/nope missing").unwrap(), CatHeader::Missing);
        assert_eq!(cat_header(format!("{C1}:a b missing").as_bytes()).unwrap(), CatHeader::Missing);
        assert_eq!(cat_header(b"abc ambiguous").unwrap(), CatHeader::Ambiguous);
        assert!(cat_header(b"what").is_err());
        assert!(cat_header(format!("{C1} commit x").as_bytes()).is_err());
    }

    // Goal: a commit's headers and message, a signed one's signature
    // skipped. Method: raw objects as git writes them.
    #[test]
    fn commits_parse() {
        let raw = format!("tree {B1}\nparent {C1}\nparent {E}\nauthor A B <a@x> 1790000000 +0000\ncommitter C <c@x> 1790000001 +0000\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n abc\n -----END PGP SIGNATURE-----\n\nhello\n\nworld\n");
        let c = commit(raw.as_bytes()).unwrap();
        assert_eq!(c.tree.to_string(), B1);
        assert_eq!(c.parents.len(), 2);
        assert_eq!((c.author.name.as_str(), c.author.email.as_str(), c.author.time), ("A B", "a@x", 1_790_000_000));
        assert_eq!(c.committer.tz, "+0000");
        assert_eq!(c.message, "hello\n\nworld\n");
        assert!(commit(b"parent x\n\nm").is_err());
        assert!(commit(format!("tree {B1}\nauthor nobody\ncommitter C <c@x> 1 +0000\n\nm").as_bytes()).is_err());
    }

    // Goal: id lists and refs, bounded.
    #[test]
    fn lists() {
        assert_eq!(oids(format!("{C1}\n{E}\n").as_bytes(), 5).unwrap().len(), 2);
        assert!(oids(format!("{C1}\n{E}\n").as_bytes(), 1).is_err());
        assert!(oids(b"nope\n", 5).is_err());
        let r = refs(format!("{C1} refs/heads/main\n{E} refs/heads/preview/a1\n").as_bytes(), 5).unwrap();
        assert_eq!(r[1].0, "refs/heads/preview/a1");
    }
}
