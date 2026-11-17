//! An import's fast-import stream, read before git sees it (docs/design.md,
//! Import): every command it gives, and every ref it writes, checked
//! against what an import may do. git accepts names an import must not
//! write (a root ref such as `ORIG_HEAD`, or `shallow`, which hides
//! history from every walk and which no ref listing shows), so the names
//! are checked here, where the stream names them, not after.
//!
//! The reading follows git-fast-import(1): commands are lines; a `data`
//! command's bytes are skipped by their count (`data <n>`) or to their
//! delimiter (`data <<EOF`), so no file's bytes are ever read as a command.

use std::io::{BufRead, Read};

use macrofiche_contract::names;

use crate::repo::{EPHEMERAL_PREFIX, LEGACY_PREFIX};

/// One data block's bytes at most (a file's): the same bound a commit
/// pack's file has.
pub const DATA_BYTES_MAX: u64 = 2 << 30;

/// Why a stream is refused, with where.
#[derive(Debug, PartialEq, Eq)]
pub struct Refused {
    pub line: u64,
    pub why: String,
}

/// A ref a stream may write: a branch (or an ephemeral one), or a legacy
/// commit id.
pub fn allowed_ref(name: &str) -> bool {
    if let Some(b) = name.strip_prefix("refs/heads/").or_else(|| name.strip_prefix(EPHEMERAL_PREFIX)) {
        return names::valid_branch(b);
    }
    name.strip_prefix(LEGACY_PREFIX).is_some_and(names::is_sha)
}

/// Reads `input` to its end and refuses it at the first command an import
/// does not take: a ref outside `allowed_ref` (`commit`, `reset`), a tag, an
/// `option`, a `feature` but `done`, a read back (`ls`, `cat-blob`,
/// `get-mark`), an alias, an unknown command, or a data block that is cut.
pub fn check(input: impl Read) -> Result<(), Refused> {
    let mut r = std::io::BufReader::new(input);
    let mut line = Vec::new();
    let mut n = 0u64;
    let refused = |n: u64, why: String| Refused { line: n, why };
    // bounded by the input: each pass reads a line or a data block
    loop {
        line.clear();
        let got = r.read_until(b'\n', &mut line).map_err(|e| refused(n, format!("reading the stream: {e}")))?;
        if got == 0 {
            return Ok(());
        }
        n += 1;
        let text = String::from_utf8_lossy(line.strip_suffix(b"\n").unwrap_or(&line)).into_owned();
        let (cmd, rest) = text.split_once(' ').unwrap_or((text.as_str(), ""));
        match cmd {
            "" | "blob" | "mark" | "original-oid" | "author" | "committer" | "encoding" | "from" | "merge" | "M" | "D" | "C" | "R" | "N" | "deleteall" | "done" | "checkpoint" | "progress" => {}
            "#" => {}
            _ if cmd.starts_with('#') => {}
            "commit" | "reset" => {
                if !allowed_ref(rest) {
                    return Err(refused(n, format!("{cmd} {rest:?}: an import writes branches and legacy ids alone")));
                }
            }
            "feature" if rest == "done" => {}
            "data" => {
                if let Some(delim) = rest.strip_prefix("<<") {
                    // to the line that is the delimiter alone
                    let mut skipped = 0u64;
                    loop {
                        line.clear();
                        let got = r.read_until(b'\n', &mut line).map_err(|e| refused(n, format!("reading the stream: {e}")))?;
                        if got == 0 {
                            return Err(refused(n, format!("data <<{delim} is never ended")));
                        }
                        n += 1;
                        if line.strip_suffix(b"\n").unwrap_or(&line) == delim.as_bytes() {
                            break;
                        }
                        skipped += got as u64;
                        if skipped > DATA_BYTES_MAX {
                            return Err(refused(n, format!("a data block of more than {DATA_BYTES_MAX} bytes")));
                        }
                    }
                } else {
                    let len: u64 = rest.parse().map_err(|_| refused(n, format!("data {rest:?} names no length")))?;
                    if len > DATA_BYTES_MAX {
                        return Err(refused(n, format!("a data block of {len} bytes, more than {DATA_BYTES_MAX}")));
                    }
                    let copied = std::io::copy(&mut (&mut r).take(len), &mut std::io::sink()).map_err(|e| refused(n, format!("reading the stream: {e}")))?;
                    if copied != len {
                        return Err(refused(n, format!("data {len} is cut at {copied} bytes")));
                    }
                    // the LF git allows after a counted block is a blank line
                }
            }
            other => return Err(refused(n, format!("{other}: not a command an import gives"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = "blob\nmark :1\ndata 5\nalpha\ncommit refs/macrofiche/legacy/1111111111111111111111111111111111111111\nmark :2\nauthor a <a@b> 1 +0000\ncommitter a <a@b> 1 +0000\ndata 3\none\nM 100644 :1 a.txt\n\nreset refs/heads/main\nfrom :2\n\ndone\n";

    // Goal: a stream an import writes (fragment's carry) passes, and one
    // that writes any other name, or asks git for anything else, does not.
    // Method: the carry's shape, then each refused command and name,
    // shallow and a root ref among them.
    #[test]
    fn only_branches_and_legacy_ids_are_written() {
        assert_eq!(check(OK.as_bytes()), Ok(()));
        for bad in [
            "reset shallow\nfrom :2\n\n",
            "reset ORIG_HEAD\nfrom :2\n\n",
            "reset objects/info/alternates\nfrom :2\n\n",
            "reset refs/tags/v1\nfrom :2\n\n",
            "reset refs/notes/x\nfrom :2\n\n",
            "reset refs/macrofiche/legacy/nope\nfrom :2\n\n",
            "reset refs/macrofiche/tmp/x\nfrom :2\n\n",
            "reset refs/heads/a..b\nfrom :2\n\n",
            "commit refs/remotes/origin/main\nmark :9\ncommitter a <a@b> 1 +0000\ndata 0\n\n",
            "tag v1\nfrom :2\ntagger a <a@b> 1 +0000\ndata 0\n",
            "feature export-marks=/x\n",
            "option git quiet\n",
            "cat-blob :1\n",
            "ls :2 a.txt\n",
            "get-mark :1\n",
            "alias\nmark :3\nto :2\n",
            "surprise\n",
        ] {
            let s = OK.replace("done\n", &format!("{bad}done\n"));
            assert!(check(s.as_bytes()).is_err(), "{bad:?}");
        }
    }

    // Goal: a file's bytes are never read as commands, counted or
    // delimited, and a block that is cut is refused. Method: data holding
    // a `reset shallow` line, by count and by delimiter.
    #[test]
    fn data_is_skipped_whole() {
        let counted = OK.replace("data 5\nalpha\n", "data 23\nreset shallow\nfrom :2\n\n");
        assert_eq!(check(counted.as_bytes()), Ok(()), "counted");
        let delimited = OK.replace("data 5\nalpha\n", "data <<EOF\nreset shallow\nfrom :2\nEOF\n");
        assert_eq!(check(delimited.as_bytes()), Ok(()), "delimited");
        assert!(check("blob\ndata 10\nshort".as_bytes()).is_err(), "a counted block cut");
        assert!(check("blob\ndata <<EOF\nnever ended\n".as_bytes()).is_err(), "a delimited block never ended");
        assert!(check("blob\ndata many\n".as_bytes()).is_err(), "no length");
    }
}
