//! The last-commit index (docs/design.md, The last-commit index): for each
//! commit, each path's last commit by git's own definition, `log -1 --
//! <path>` from that commit, which follows the first TREESAME parent
//! through a merge (contract section 7).
//!
//! A commit's record holds only what differs from its first parent's map
//! (a delta), or every path (a snapshot: a root, and every
//! `SNAPSHOT_EVERY`th record along first parents), so a map is rebuilt
//! from at most `SNAPSHOT_EVERY` records. Records are files written like
//! loose objects: a temporary file, synced, renamed into place, before the
//! commit's ref moves. This module is the format and the rule, pure; the
//! store reads and writes the files (`index`).

use std::collections::{BTreeMap, HashMap, HashSet};

use macrofiche_git::Oid;
use thiserror::Error;

/// Deltas between snapshots along first parents.
pub const SNAPSHOT_EVERY: u32 = 32;
const MAGIC: &str = "mfl1";

/// A commit's map: each path to its last commit.
pub type LcMap = HashMap<String, Oid>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    /// Every path at the commit.
    Snapshot(BTreeMap<String, Oid>),
    /// The first parent's map, then these paths set and these removed.
    Delta { set: BTreeMap<String, Oid>, removed: Vec<String> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub commit: Oid,
    /// The record a delta applies to (the first parent); none for a
    /// snapshot.
    pub base: Option<Oid>,
    /// Deltas since the last snapshot along the bases: 0 for one.
    pub depth: u32,
    pub body: Body,
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("a malformed last-commit record: {0}")]
pub struct RecordError(pub &'static str);

impl Record {
    pub fn encode(&self) -> Vec<u8> {
        let base = self.base.map_or_else(|| "-".to_string(), |b| b.to_string());
        let mut out = format!("{MAGIC}\ncommit {}\nbase {base}\ndepth {}\n\n", self.commit, self.depth).into_bytes();
        match &self.body {
            Body::Snapshot(m) => {
                assert!(self.base.is_none() && self.depth == 0, "a snapshot has no base");
                for (p, c) in m {
                    out.extend_from_slice(format!("{c} {p}\0").as_bytes());
                }
            }
            Body::Delta { set, removed } => {
                assert!(self.base.is_some() && self.depth > 0, "a delta has a base");
                for (p, c) in set {
                    out.extend_from_slice(format!("+{c} {p}\0").as_bytes());
                }
                for p in removed {
                    out.extend_from_slice(format!("-{p}\0").as_bytes());
                }
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Record, RecordError> {
        let head_end = bytes.windows(2).position(|w| w == b"\n\n").ok_or(RecordError("no header"))?;
        let head = std::str::from_utf8(&bytes[..head_end]).map_err(|_| RecordError("a header that is not text"))?;
        let mut lines = head.lines();
        if lines.next() != Some(MAGIC) {
            return Err(RecordError("not a record"));
        }
        let field = |l: Option<&str>, k: &str| l.and_then(|l| l.strip_prefix(k)).map(str::to_string).ok_or(RecordError("a missing field"));
        let commit = Oid::parse(&field(lines.next(), "commit ")?).ok_or(RecordError("commit"))?;
        let base = match field(lines.next(), "base ")?.as_str() {
            "-" => None,
            b => Some(Oid::parse(b).ok_or(RecordError("base"))?),
        };
        let depth: u32 = field(lines.next(), "depth ")?.parse().map_err(|_| RecordError("depth"))?;
        if base.is_none() != (depth == 0) || depth >= SNAPSHOT_EVERY {
            return Err(RecordError("depth"));
        }
        let mut set = BTreeMap::new();
        let mut removed = Vec::new();
        for e in bytes[head_end + 2..].split(|b| *b == 0).filter(|e| !e.is_empty()) {
            let e = std::str::from_utf8(e).map_err(|_| RecordError("an entry that is not text"))?;
            match (base.is_some(), e.as_bytes()[0]) {
                (true, b'-') => removed.push(e[1..].to_string()),
                (true, b'+') => {
                    let (c, p) = e[1..].split_once(' ').ok_or(RecordError("an entry"))?;
                    set.insert(p.to_string(), Oid::parse(c).ok_or(RecordError("an entry's commit"))?);
                }
                (false, _) => {
                    let (c, p) = e.split_once(' ').ok_or(RecordError("an entry"))?;
                    set.insert(p.to_string(), Oid::parse(c).ok_or(RecordError("an entry's commit"))?);
                }
                _ => return Err(RecordError("an entry")),
            }
        }
        let body = if base.is_some() { Body::Delta { set, removed } } else { Body::Snapshot(set) };
        Ok(Record { commit, base, depth, body })
    }
}

/// Applies records, a snapshot first, then each delta in order, oldest
/// first: the newest's map.
pub fn rebuild(chain_oldest_first: &[Record]) -> Result<LcMap, RecordError> {
    let (first, rest) = chain_oldest_first.split_first().ok_or(RecordError("an empty chain"))?;
    let Body::Snapshot(m) = &first.body else { return Err(RecordError("a chain starts with a snapshot")) };
    let mut map: LcMap = m.iter().map(|(p, c)| (p.clone(), *c)).collect();
    let mut prev = first.commit;
    for r in rest {
        let Body::Delta { set, removed } = &r.body else { return Err(RecordError("a snapshot inside a chain")) };
        if r.base != Some(prev) {
            return Err(RecordError("a chain's bases do not join"));
        }
        for p in removed {
            map.remove(p);
        }
        for (p, c) in set {
            map.insert(p.clone(), *c);
        }
        prev = r.commit;
    }
    Ok(map)
}

/// One parent's diff against the commit: the paths that differ, and
/// which of them the commit removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParentDiff {
    pub changed: HashSet<String>,
    pub removed: HashSet<String>,
}

/// The delta of `commit` against its first parent, by git's rule. A path
/// that differs from the first parent takes the last commit of the first
/// other parent it is the same as (`lookup(i, path)`, i ≥ 1), else
/// `commit` itself; a path the first parent had and `commit` lacks is
/// removed. `diffs[0]` is against the first parent.
pub fn delta(commit: Oid, diffs: &[ParentDiff], mut lookup: impl FnMut(usize, &str) -> Option<Oid>) -> (BTreeMap<String, Oid>, Vec<String>) {
    assert!(!diffs.is_empty(), "a delta has a first parent");
    let mut set = BTreeMap::new();
    let mut removed: Vec<String> = diffs[0].removed.iter().cloned().collect();
    removed.sort();
    for p in diffs[0].changed.iter().filter(|p| !diffs[0].removed.contains(*p)) {
        let same_as = (1..diffs.len()).find(|i| !diffs[*i].changed.contains(p));
        let last = same_as.and_then(|i| lookup(i, p)).unwrap_or(commit);
        set.insert(p.clone(), last);
    }
    (set, removed)
}

/// A snapshot of `map` with `delta` applied (when a delta would pass
/// `SNAPSHOT_EVERY`).
pub fn snapshot_of(mut map: LcMap, set: &BTreeMap<String, Oid>, removed: &[String]) -> BTreeMap<String, Oid> {
    for p in removed {
        map.remove(p);
    }
    for (p, c) in set {
        map.insert(p.clone(), *c);
    }
    map.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(n: u8) -> Oid {
        Oid::from_bytes([n; 20])
    }

    fn diff(changed: &[&str], removed: &[&str]) -> ParentDiff {
        ParentDiff { changed: changed.iter().map(|s| s.to_string()).collect(), removed: removed.iter().map(|s| s.to_string()).collect() }
    }

    // Goal: records round-trip, snapshots and deltas, paths with spaces
    // and quotes; a malformed one is refused. Method: encode, decode.
    #[test]
    fn records_round_trip() {
        let snap = Record { commit: oid(1), base: None, depth: 0, body: Body::Snapshot(BTreeMap::from([("a b.txt".into(), oid(1)), ("d/\"q\".md".into(), oid(1))])) };
        assert_eq!(Record::decode(&snap.encode()).unwrap(), snap);
        let delta = Record { commit: oid(2), base: Some(oid(1)), depth: 1, body: Body::Delta { set: BTreeMap::from([("a b.txt".into(), oid(2))]), removed: vec!["d/\"q\".md".into()] } };
        assert_eq!(Record::decode(&delta.encode()).unwrap(), delta);
        assert!(Record::decode(b"").is_err());
        assert!(Record::decode(b"mfl1\ncommit x\nbase -\ndepth 0\n\n").is_err());
        assert!(Record::decode(format!("mfl1\ncommit {}\nbase -\ndepth 3\n\n", oid(1)).as_bytes()).is_err(), "a snapshot at depth 3");
        assert!(Record::decode(format!("mfl1\ncommit {}\nbase {}\ndepth {SNAPSHOT_EVERY}\n\n", oid(2), oid(1)).as_bytes()).is_err());
        let mut torn = delta.encode();
        torn.truncate(torn.len() - 3);
        torn.extend_from_slice(b"\0+zz");
        assert!(Record::decode(&torn).is_err(), "a torn entry");
    }

    // Goal: git's rule. A plain commit: what it changed is its own, the
    // rest its parent's. A merge: a path the same as the first parent's
    // keeps that lineage (no delta); one the same as the second parent's
    // takes that lineage; one like neither is the merge's own; one it
    // removed is removed. Method: the delta of each.
    #[test]
    fn deltas_follow_treesame() {
        let (set, removed) = delta(oid(9), &[diff(&["a", "gone"], &["gone"])], |_, _| None);
        assert_eq!(set, BTreeMap::from([("a".into(), oid(9))]));
        assert_eq!(removed, vec!["gone".to_string()]);
        // a merge of P1 (first) and P2: x is P2's version, y differs from
        // both, z is P1's (no entry), w was removed
        let p2_last = |i: usize, p: &str| (i == 1 && p == "x").then(|| oid(2));
        let (set, removed) = delta(oid(9), &[diff(&["x", "y", "w"], &["w"]), diff(&["y", "z"], &[])], p2_last);
        assert_eq!(set, BTreeMap::from([("x".into(), oid(2)), ("y".into(), oid(9))]));
        assert_eq!(removed, vec!["w".to_string()]);
    }

    // Goal: a chain rebuilds the newest map, and a chain whose bases do
    // not join is refused. Method: a snapshot and two deltas.
    #[test]
    fn chains_rebuild() {
        let r0 = Record { commit: oid(1), base: None, depth: 0, body: Body::Snapshot(BTreeMap::from([("a".into(), oid(1)), ("b".into(), oid(1))])) };
        let r1 = Record { commit: oid(2), base: Some(oid(1)), depth: 1, body: Body::Delta { set: BTreeMap::from([("a".into(), oid(2)), ("c".into(), oid(2))]), removed: vec![] } };
        let r2 = Record { commit: oid(3), base: Some(oid(2)), depth: 2, body: Body::Delta { set: BTreeMap::new(), removed: vec!["b".into()] } };
        let m = rebuild(&[r0.clone(), r1.clone(), r2.clone()]).unwrap();
        assert_eq!(m, LcMap::from([("a".into(), oid(2)), ("c".into(), oid(2))]));
        assert!(rebuild(&[r0.clone(), r2]).is_err());
        assert!(rebuild(&[r1]).is_err());
        let snap = snapshot_of(m, &BTreeMap::from([("d".into(), oid(4))]), &["a".into()]);
        assert_eq!(snap, BTreeMap::from([("c".into(), oid(2)), ("d".into(), oid(4))]));
    }
}
