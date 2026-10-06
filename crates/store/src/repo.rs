//! One repository: its reads (refs, files, listings, history, merge
//! previews), its writes under one lock (commit packs, branches, merges,
//! restores, each a compare-and-swap through `update-ref`), and its
//! last-commit index's files.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use macrofiche_contract::names::{self, Expected};
use macrofiche_git::fastimport::{Head, Ident};
use macrofiche_git::linux::plumbing::{unknown_object, EMPTY_TREE};
use macrofiche_git::parse::{self, mode, Kind, Signature};
use macrofiche_git::{BlobReader, Change, CatFile, GitError, Merged, ObjInfo, Oid, Pool, RefOp, RefRefusal, Repo, Source};

use crate::error::StoreError;
use crate::lastcommit::{self, Body, LcMap, ParentDiff, Record, SNAPSHOT_EVERY};
use crate::lru::Lru;
use crate::registry::RepoMeta;

/// Bytes of a commit object read whole.
pub const COMMIT_BYTES_MAX: u64 = 16 << 20;
/// Commits one index rebuild may compute before it gives up (each is
/// written as it is made, so the next call goes on from there).
pub const REBUILD_COMMITS_MAX: usize = 100_000;
/// Cached last-commit maps and listings, in entries; cached commits.
pub const MAP_CACHE_ENTRIES: u64 = 500_000;
pub const LISTING_CACHE_ENTRIES: u64 = 500_000;
pub const COMMIT_CACHE_ENTRIES: u64 = 20_000;
/// The namespace of ephemeral refs (the service's).
pub const EPHEMERAL_PREFIX: &str = "refs/namespaces/ephemeral/refs/heads/";

/// The identity a write that names none commits as.
pub fn default_ident() -> Ident {
    Ident::new("macrofiche", "macrofiche@macrofiche.invalid").expect("a valid ident")
}

/// A branch's ref: `refs/heads/<b>`, or in the ephemeral namespace.
pub fn ref_name(branch: &str, ephemeral: bool) -> String {
    assert!(names::valid_branch(branch), "branch names are checked before they reach a ref");
    if ephemeral {
        format!("{EPHEMERAL_PREFIX}{branch}")
    } else {
        format!("refs/heads/{branch}")
    }
}

/// A commit, read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitInfo {
    pub sha: Oid,
    pub tree: Oid,
    pub parents: Vec<Oid>,
    pub author: Signature,
    pub committer: Signature,
    pub message: String,
}

/// A file at a commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileInfo {
    pub blob: Oid,
    pub size: u64,
    pub last_commit: Oid,
}

/// One entry of a listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListEntry {
    pub path: String,
    pub mode: u32,
    /// `blob`, `symlink`, or `submodule`.
    pub kind: &'static str,
    pub size: u64,
    pub blob: Oid,
    pub last_commit: Oid,
}

/// Every file at a commit, sorted by path bytes, with last commits.
pub struct Listing {
    pub commit: Oid,
    pub entries: Vec<ListEntry>,
    /// Each last commit's rank in a topological walk from `commit`
    /// (`FileLastCommit.order`), found once.
    orders: tokio::sync::OnceCell<HashMap<Oid, u32>>,
}

/// The caches every repository shares, bounded in entries.
pub struct Caches {
    maps: Mutex<Lru<(String, Oid), Arc<LcMap>>>,
    listings: Mutex<Lru<(String, Oid), Arc<Listing>>>,
    commits: Mutex<Lru<(String, Oid), Arc<CommitInfo>>>,
}

impl Default for Caches {
    fn default() -> Caches {
        Caches { maps: Mutex::new(Lru::new(MAP_CACHE_ENTRIES)), listings: Mutex::new(Lru::new(LISTING_CACHE_ENTRIES)), commits: Mutex::new(Lru::new(COMMIT_CACHE_ENTRIES)) }
    }
}

/// A branch that moved, for the push webhook.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Moved {
    pub ref_name: String,
    pub before: Option<Oid>,
    pub after: Oid,
}

/// A commit a write made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Committed {
    pub branch: String,
    pub old: Option<Oid>,
    pub new: Oid,
    pub tree: Oid,
    pub blob_count: usize,
}

/// A commit pack's file, its bytes spooled.
pub struct PackFile {
    pub path: String,
    pub change: PackChange,
}

pub enum PackChange {
    Upsert { mode: u32, spool: PathBuf, len: u64 },
    Delete,
}

pub enum CommitOutcome {
    Committed(Committed),
    /// The branch is not where the pack expected (409).
    Moved { current: Option<Oid> },
    /// The pack changes nothing: its tree is its parent's, or empty on a
    /// branch it would create. The service's 412 (`no changes to
    /// commit`): no empty commit is made, and nothing moves.
    Unchanged { tip: Option<Oid> },
}

/// `strategy`: the service's three.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// A merge commit whenever the source is not already in the target.
    Merge,
    FfOnly,
    FfPrefer,
}

impl Strategy {
    pub fn parse(s: &str) -> Option<Strategy> {
        Some(match s {
            "merge" => Strategy::Merge,
            "ff_only" => Strategy::FfOnly,
            "ff_prefer" => Strategy::FfPrefer,
            _ => return None,
        })
    }
}

pub struct MergeRequest {
    pub target: String,
    pub target_ephemeral: bool,
    pub source: String,
    pub source_ephemeral: bool,
    pub strategy: Strategy,
    pub expected: Option<Oid>,
    pub message: String,
    pub author: Option<Ident>,
    pub committer: Option<Ident>,
    pub squash: bool,
    pub allow_unrelated: bool,
}

/// What a merge does: the service's `result`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeKind {
    NoOp,
    FastForward,
    MergeCommit,
    Squash,
}

impl MergeKind {
    pub fn name(self) -> &'static str {
        match self {
            MergeKind::NoOp => "no_op",
            MergeKind::FastForward => "fast_forward",
            MergeKind::MergeCommit => "merge_commit",
            MergeKind::Squash => "squash",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeDone {
    pub kind: MergeKind,
    pub old: Oid,
    pub new: Oid,
    pub tree: Oid,
    pub base: Option<Oid>,
    pub source: Oid,
    pub promoted: u64,
}

pub enum MergeOutcome {
    Done(MergeDone),
    /// `expected_target_sha` is not the target's tip (409 `target_moved`).
    TargetMoved { current: Oid, base: Option<Oid> },
    /// The sides conflict (409 `merge_conflict`).
    Conflict { base: Option<Oid>, paths: Vec<String> },
    /// `ff_only`, and the target has diverged.
    NotFastForward { base: Option<Oid> },
    /// A squash whose tree is the target's: a one-parent commit that
    /// changes nothing, refused as a commit pack's is (412), nothing
    /// moved. A merge commit is never refused so: its second parent
    /// records the merge even where the tree stays the same, as git's.
    Unchanged { current: Oid },
}

/// A merge preview (contract 15.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    pub source_tip: Oid,
    pub target_tip: Oid,
    pub kind: MergeKind,
    pub base: Option<Oid>,
    pub conflicts: Vec<String>,
}

pub enum RestoreOutcome {
    Done(Committed),
    /// `expected_target_sha` is not the tip (409).
    Moved { current: Oid },
    /// The service's 412: the tip itself, not an ancestor, or no change.
    Refused { tip: Oid, why: &'static str },
}

pub enum CreateOutcome {
    Created(Oid),
    Exists,
}

/// Repositories written since their last maintenance, by `org/id`, with
/// when they were last written: the store's, shared by every handle.
pub type Dirty = Arc<Mutex<HashMap<String, (RepoMeta, Instant)>>>;

pub struct RepoHandle {
    pub meta: RepoMeta,
    pub dir: PathBuf,
    pub git: Repo,
    pub pool: Arc<Pool>,
    pub(crate) write_lock: tokio::sync::Mutex<()>,
    caches: Arc<Caches>,
    dirty: Dirty,
}

static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn tmp_ref() -> String {
    format!("refs/macrofiche/tmp/{}-{}", std::process::id(), TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

fn idents(author: Option<Ident>, committer: Option<Ident>) -> (Ident, Ident) {
    let a = author.unwrap_or_else(default_ident);
    let c = committer.unwrap_or_else(|| a.clone());
    (a, c)
}

impl RepoHandle {
    pub(crate) fn new(meta: RepoMeta, dir: PathBuf, git: Repo, caches: Arc<Caches>, dirty: Dirty) -> RepoHandle {
        let pool = Pool::new(git.clone());
        RepoHandle { meta, dir, git, pool, write_lock: tokio::sync::Mutex::new(()), caches, dirty }
    }

    /// The lock every write of this repository takes (and maintenance,
    /// and macrofiche's webhook registrations).
    pub fn write_lock(&self) -> &tokio::sync::Mutex<()> {
        &self.write_lock
    }

    fn key(&self, c: Oid) -> (String, Oid) {
        (self.meta.repo_id.clone(), c)
    }

    /// Marks the repository written, for idle maintenance.
    pub fn touched(&self) {
        let k = format!("{}/{}", self.meta.org, self.meta.repo_id);
        self.dirty.lock().expect("the dirty lock").insert(k, (self.meta.clone(), Instant::now()));
    }

    // ---- reads ----

    async fn cat(&self) -> Result<CatFile, StoreError> {
        Ok(self.pool.get().await?)
    }

    /// An object's header by any name git resolves.
    pub async fn info(&self, name: &str) -> Result<Option<ObjInfo>, StoreError> {
        let mut cat = self.cat().await?;
        let r = cat.info(name).await;
        self.pool.put(cat);
        Ok(r?)
    }

    /// A ref's commit, or `None`.
    pub async fn ref_tip(&self, name: &str) -> Result<Option<Oid>, StoreError> {
        match self.info(name).await? {
            Some(i) if i.kind == Kind::Commit => Ok(Some(i.oid)),
            Some(i) => Err(StoreError::Internal(format!("{name} names a {:?}", i.kind))),
            None => Ok(None),
        }
    }

    pub async fn branch(&self, branch: &str, ephemeral: bool) -> Result<Option<Oid>, StoreError> {
        self.ref_tip(&ref_name(branch, ephemeral)).await
    }

    /// A ref as the API takes one: a commit's sha, a branch (in the
    /// ephemeral namespace when asked), or a tag; `None` when it names no
    /// commit.
    pub async fn resolve(&self, rev: &str, ephemeral: bool) -> Result<Option<Oid>, StoreError> {
        if names::is_sha(rev) {
            return match self.info(&format!("{rev}^{{commit}}")).await? {
                Some(i) => Ok(Some(i.oid)),
                None => Ok(None),
            };
        }
        if !names::valid_branch(rev) {
            return Ok(None);
        }
        if let Some(c) = self.branch(rev, ephemeral).await? {
            return Ok(Some(c));
        }
        if ephemeral {
            return Ok(None);
        }
        Ok(self.info(&format!("refs/tags/{rev}^{{commit}}")).await?.map(|i| i.oid))
    }

    /// A commit, read and cached.
    pub async fn commit_info(&self, c: Oid) -> Result<Arc<CommitInfo>, StoreError> {
        if let Some(ci) = self.caches.commits.lock().expect("the cache lock").get(&self.key(c)) {
            return Ok(ci);
        }
        let mut cat = self.cat().await?;
        let r = cat.read_all(c, COMMIT_BYTES_MAX).await;
        self.pool.put(cat);
        let Some((info, raw)) = r? else { return Err(StoreError::not_found("commit_not_found", format!("no commit {c}"))) };
        if info.kind != Kind::Commit {
            return Err(StoreError::not_found("commit_not_found", format!("{c} is no commit")));
        }
        let p = parse::commit(&raw).map_err(|e| StoreError::Internal(e.to_string()))?;
        let ci = Arc::new(CommitInfo { sha: c, tree: p.tree, parents: p.parents, author: p.author, committer: p.committer, message: p.message });
        self.caches.commits.lock().expect("the cache lock").insert(self.key(c), ci.clone(), 1);
        Ok(ci)
    }

    /// A file at commit `c`: its blob, size and last commit; `None` when
    /// the path holds no file there.
    pub async fn file(&self, c: Oid, path: &str) -> Result<Option<FileInfo>, StoreError> {
        let Some(i) = self.info(&format!("{c}:{path}")).await? else { return Ok(None) };
        if i.kind != Kind::Blob {
            return Ok(None);
        }
        let map = self.lc_map(c).await?;
        let last = *map.get(path).ok_or_else(|| StoreError::Internal(format!("the index has no last commit for {path} at {c}")))?;
        Ok(Some(FileInfo { blob: i.oid, size: i.size, last_commit: last }))
    }

    /// A blob's bytes, streamed from a pooled reader.
    pub async fn open_blob(&self, blob: Oid) -> Result<BlobReader, StoreError> {
        BlobReader::open(self.pool.clone(), blob).await?.ok_or_else(|| StoreError::Internal(format!("blob {blob} vanished")))
    }

    /// Every file at commit `c`, cached (a commit is immutable).
    pub async fn listing(&self, c: Oid) -> Result<Arc<Listing>, StoreError> {
        if let Some(l) = self.caches.listings.lock().expect("the cache lock").get(&self.key(c)) {
            return Ok(l);
        }
        let tree = self.git.ls_tree(c).await?;
        let map = self.lc_map(c).await?;
        let mut entries = Vec::with_capacity(tree.len());
        for t in tree {
            let kind = match (t.kind, t.mode) {
                (Kind::Blob, mode::SYMLINK) => "symlink",
                (Kind::Blob, _) => "blob",
                (Kind::Commit, _) => "submodule",
                _ => continue,
            };
            let last_commit = *map.get(&t.path).ok_or_else(|| StoreError::Internal(format!("the index has no last commit for {} at {c}", t.path)))?;
            entries.push(ListEntry { size: t.size.unwrap_or(0), path: t.path, mode: t.mode, kind, blob: t.oid, last_commit });
        }
        entries.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        let l = Arc::new(Listing { commit: c, entries, orders: tokio::sync::OnceCell::new() });
        let w = l.entries.len().max(1) as u64;
        self.caches.listings.lock().expect("the cache lock").insert(self.key(c), l.clone(), w);
        Ok(l)
    }

    /// Each of the listing's last commits ranked by a topological walk
    /// from its commit, 0 the nearest (`FileLastCommit.order`): one walk
    /// per listing, ended once all are found.
    pub async fn orders(&self, l: &Listing) -> Result<HashMap<Oid, u32>, StoreError> {
        let got = l
            .orders
            .get_or_try_init(|| async {
                let mut want: HashSet<Oid> = l.entries.iter().map(|e| e.last_commit).collect();
                let mut found = HashMap::new();
                let mut walk = self.git.rev_walk(&["--topo-order", &l.commit.to_string()]).await?;
                let mut n = 0u32;
                // Bounded by the walk (REVS_MAX): it ends, or every one is found.
                while !want.is_empty() {
                    let Some(c) = walk.next().await? else { break };
                    if want.remove(&c) {
                        found.insert(c, n);
                    }
                    n = n.saturating_add(1);
                }
                Ok::<_, StoreError>(found)
            })
            .await?;
        Ok(got.clone())
    }

    /// History from `tip` in commit-date order (the service's), `skip`
    /// shown already, at most `limit`; and whether more follow.
    pub async fn history(&self, tip: Oid, skip: usize, limit: usize) -> Result<(Vec<Arc<CommitInfo>>, bool), StoreError> {
        let skip_arg = format!("--skip={skip}");
        let max = format!("--max-count={}", limit + 1);
        let ids = self.git.rev_list(&["--date-order", &skip_arg, &max, &tip.to_string()], limit + 1).await?;
        let more = ids.len() > limit;
        let mut out = Vec::with_capacity(ids.len().min(limit));
        for id in ids.into_iter().take(limit) {
            out.push(self.commit_info(id).await?);
        }
        Ok((out, more))
    }

    /// When a branch was made, if macrofiche saw it made.
    pub fn created_at(&self, ref_name: &str) -> Option<i64> {
        std::fs::read_to_string(self.created_path(ref_name)).ok().and_then(|s| s.lines().next()?.parse().ok())
    }

    fn created_path(&self, ref_name: &str) -> PathBuf {
        let d = ring::digest::digest(&ring::digest::SHA256, ref_name.as_bytes());
        let hex: String = d.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        self.dir.join("macrofiche/created").join(hex)
    }

    async fn mark_created(&self, ref_name: &str, now: i64) -> Result<(), StoreError> {
        let path = self.created_path(ref_name);
        let body = format!("{now}\n{ref_name}\n");
        tokio::task::spawn_blocking(move || write_durably(&path, body.as_bytes())).await.map_err(|e| StoreError::Internal(e.to_string()))??;
        Ok(())
    }

    // ---- the last-commit index ----

    fn record_path(&self, c: Oid) -> PathBuf {
        let h = c.to_string();
        self.dir.join("macrofiche/lastcommit").join(&h[..2]).join(&h[2..])
    }

    fn read_record(&self, c: Oid) -> Option<Record> {
        let bytes = std::fs::read(self.record_path(c)).ok()?;
        Record::decode(&bytes).ok().filter(|r| r.commit == c)
    }

    fn has_record(&self, c: Oid) -> bool {
        self.record_path(c).is_file()
    }

    async fn write_record(&self, r: &Record) -> Result<(), StoreError> {
        let path = self.record_path(r.commit);
        let bytes = r.encode();
        tokio::task::spawn_blocking(move || write_durably(&path, &bytes)).await.map_err(|e| StoreError::Internal(e.to_string()))??;
        Ok(())
    }

    /// A commit's diff against one parent, as the index's rule reads it.
    async fn parent_diff(&self, parent: Oid, c: Oid) -> Result<ParentDiff, StoreError> {
        let mut d = ParentDiff::default();
        for e in self.git.diff_tree(Some(parent), c).await? {
            if e.status == b'D' {
                d.removed.insert(e.path.clone());
            }
            d.changed.insert(e.path);
        }
        Ok(d)
    }

    /// The record of `c`, whose parents have records.
    async fn compute_record(&self, c: Oid, parents: &[Oid]) -> Result<Record, StoreError> {
        let Some((&p1, rest)) = parents.split_first() else {
            let all: BTreeMap<String, Oid> = self.git.ls_tree(c).await?.into_iter().map(|t| (t.path, c)).collect();
            return Ok(Record { commit: c, base: None, depth: 0, body: Body::Snapshot(all) });
        };
        let mut diffs = vec![self.parent_diff(p1, c).await?];
        let mut others = Vec::with_capacity(rest.len());
        for p in rest {
            diffs.push(self.parent_diff(*p, c).await?);
            // Boxed: a map may need its own records computed first.
            others.push(Box::pin(self.lc_map(*p)).await?);
        }
        let (set, removed) = lastcommit::delta(c, &diffs, |i, path| others[i - 1].get(path).copied());
        let base_depth = self.read_record(p1).ok_or_else(|| StoreError::Unavailable(format!("the index lost {p1}'s record")))?.depth;
        if base_depth + 1 >= SNAPSHOT_EVERY {
            let pmap = Box::pin(self.lc_map(p1)).await?;
            return Ok(Record { commit: c, base: None, depth: 0, body: Body::Snapshot(lastcommit::snapshot_of((*pmap).clone(), &set, &removed)) });
        }
        Ok(Record { commit: c, base: Some(p1), depth: base_depth + 1, body: Body::Delta { set, removed } })
    }

    /// Makes sure `c` has a record: computed for it and every ancestor
    /// without one, parents first, each written as it is made (a commit
    /// macrofiche did not write, or an index lost).
    pub async fn ensure_record(&self, c: Oid) -> Result<(), StoreError> {
        let mut stack = vec![c];
        let mut computed = 0usize;
        // Bounded by REBUILD_COMMITS_MAX: each pass pops a commit or pushes
        // its parents, older than it, and history is a DAG.
        while let Some(&x) = stack.last() {
            if self.has_record(x) {
                stack.pop();
                continue;
            }
            let info = self.commit_info(x).await?;
            let missing: Vec<Oid> = info.parents.iter().copied().filter(|p| !self.has_record(*p)).collect();
            if missing.is_empty() {
                computed += 1;
                if computed > REBUILD_COMMITS_MAX {
                    return Err(StoreError::Unavailable(format!("the index rebuild passed {REBUILD_COMMITS_MAX} commits; it goes on with the next read")));
                }
                let r = self.compute_record(x, &info.parents).await?;
                self.write_record(&r).await?;
                stack.pop();
            } else {
                stack.extend(missing);
            }
        }
        Ok(())
    }

    /// `c`'s map: each path to its last commit, cached.
    pub async fn lc_map(&self, c: Oid) -> Result<Arc<LcMap>, StoreError> {
        if let Some(m) = self.caches.maps.lock().expect("the cache lock").get(&self.key(c)) {
            return Ok(m);
        }
        self.ensure_record(c).await?;
        let mut chain = Vec::new();
        let mut cur = c;
        // Bounded by SNAPSHOT_EVERY: a record's depth falls by one a base.
        for _ in 0..=SNAPSHOT_EVERY {
            let r = self.read_record(cur).ok_or_else(|| StoreError::Unavailable(format!("the index lost {cur}'s record")))?;
            let base = r.base;
            chain.push(r);
            match base {
                None => break,
                Some(b) => cur = b,
            }
        }
        chain.reverse();
        let map = Arc::new(lastcommit::rebuild(&chain).map_err(|e| StoreError::Internal(e.to_string()))?);
        let w = map.len().max(1) as u64;
        self.caches.maps.lock().expect("the cache lock").insert(self.key(c), map.clone(), w);
        Ok(map)
    }

    // ---- writes, each under the write lock ----

    /// Moves `ref_name` from `old` (none: creates it) to `new`, deleting
    /// `also` in the same transaction.
    async fn move_ref(&self, ref_name: &str, old: Option<Oid>, new: Oid, also: Option<&str>) -> Result<Result<(), RefRefusal>, StoreError> {
        let mut ops = vec![match old {
            Some(old) => RefOp::Update { name: ref_name.into(), new, old },
            None => RefOp::Create { name: ref_name.into(), new },
        }];
        if let Some(t) = also {
            ops.push(RefOp::Delete { name: t.into(), old: new });
        }
        let r = self.git.update_refs(&ops).await?;
        if r.is_err() {
            if let Some(t) = also {
                let _ = self.git.update_refs(&[RefOp::Delete { name: t.into(), old: new }]).await;
            }
        }
        Ok(r)
    }

    /// Deletes a temporary ref at `at` that no branch will move to.
    async fn drop_tmp_ref(&self, tmp: &str, at: Oid) -> Result<(), StoreError> {
        match self.git.update_refs(&[RefOp::Delete { name: tmp.into(), old: at }]).await? {
            Ok(()) => Ok(()),
            Err(r) => Err(self.refusal(r, tmp)),
        }
    }

    fn refusal(&self, r: RefRefusal, ref_name: &str) -> StoreError {
        match r {
            RefRefusal::Locked => StoreError::Unavailable(format!("{ref_name} is locked")),
            RefRefusal::NameConflict => StoreError::conflict("branch_name_conflict", format!("{ref_name} conflicts with an existing branch's name")),
            other => StoreError::Internal(format!("{ref_name}: {other:?}")),
        }
    }

    /// One commit from a pack (contract 5.8): the compare-and-swap first,
    /// then fast-import from the spools, the index record, and the ref
    /// moved from the tip it was checked at.
    pub async fn commit_pack(&self, meta: &macrofiche_pack::Metadata, files: &[PackFile], author: Option<Ident>, committer: Option<Ident>, now: i64) -> Result<(CommitOutcome, Option<Moved>), StoreError> {
        let (author, committer) = idents(author, committer);
        let _w = self.write_lock.lock().await;
        let refname = ref_name(&meta.target_branch, meta.ephemeral);
        let tip = self.ref_tip(&refname).await?;
        let matches = match &meta.expected {
            Expected::Any => true,
            Expected::Absent => tip.is_none(),
            Expected::At(x) => tip.is_some_and(|t| t.to_string() == *x),
        };
        if !matches {
            return Ok((CommitOutcome::Moved { current: tip }, None));
        }
        let parent = match (tip, &meta.base_branch) {
            (Some(t), _) => Some(t),
            (None, Some(b)) => Some(self.branch(b, meta.base_is_ephemeral).await?.ok_or_else(|| StoreError::not_found("branch_not_found", format!("base_branch not found: {b}")))?),
            (None, None) => None,
        };
        if let Some(p) = parent {
            self.ensure_record(p).await?;
        }
        let changes: Vec<Change<'_>> = files
            .iter()
            .map(|f| match &f.change {
                PackChange::Upsert { mode, spool, len } => Change::Upsert { path: &f.path, mode: *mode, source: Source::File { path: spool, len: *len } },
                PackChange::Delete => Change::Delete { path: &f.path },
            })
            .collect();
        let tmp = tmp_ref();
        let head = Head { tmp_ref: &tmp, author: &author, committer: &committer, time: now, message: meta.message.as_bytes(), from: parent };
        let new = self.git.fast_import(&head, &changes).await.map_err(|e| match &e {
            GitError::Failed { stderr, .. } if stderr.contains("invalid path") => StoreError::invalid("invalid_path", stderr.trim().trim_start_matches("fatal: ").to_string()),
            _ => e.into(),
        })?;
        // The service makes no empty commit (contract 5.8): a pack whose
        // tree is its parent's, or empty where it would make the branch,
        // is refused after the compare-and-swap, as the fake orders the
        // two. fast-import's commit goes with its temporary ref, an object
        // nothing reaches, which maintenance prunes.
        let tree = self.commit_info(new).await?.tree;
        let before = match parent {
            Some(p) => self.commit_info(p).await?.tree,
            None => Oid::parse(EMPTY_TREE).expect("the empty tree's id is an id"),
        };
        if tree == before {
            self.drop_tmp_ref(&tmp, new).await?;
            self.touched();
            return Ok((CommitOutcome::Unchanged { tip }, None));
        }
        let record = self.compute_record(new, parent.as_slice()).await?;
        self.write_record(&record).await?;
        match self.move_ref(&refname, tip, new, Some(&tmp)).await? {
            Ok(()) => {}
            Err(RefRefusal::Moved { current }) => return Ok((CommitOutcome::Moved { current: Some(current) }, None)),
            Err(RefRefusal::Exists) => return Ok((CommitOutcome::Moved { current: self.ref_tip(&refname).await? }, None)),
            Err(RefRefusal::Missing) => return Ok((CommitOutcome::Moved { current: None }, None)),
            Err(r) => return Err(self.refusal(r, &refname)),
        }
        if tip.is_none() {
            self.mark_created(&refname, now).await?;
        }
        self.touched();
        let blob_count = files.iter().filter(|f| matches!(f.change, PackChange::Upsert { .. })).count();
        let moved = (!meta.ephemeral).then_some(Moved { ref_name: refname, before: tip, after: new });
        Ok((CommitOutcome::Committed(Committed { branch: meta.target_branch.clone(), old: tip, new, tree, blob_count }), moved))
    }

    /// A branch at `base` (contract 5.9): one creator wins.
    pub async fn create_branch(&self, target: &str, ephemeral: bool, base: &str, base_ephemeral: bool, now: i64) -> Result<(CreateOutcome, Option<Moved>), StoreError> {
        let _w = self.write_lock.lock().await;
        let at = self.resolve(base, base_ephemeral).await?.ok_or_else(|| StoreError::not_found("ref_not_found", format!("base_ref not found: {base}")))?;
        let refname = ref_name(target, ephemeral);
        match self.move_ref(&refname, None, at, None).await? {
            Ok(()) => {}
            Err(RefRefusal::Exists) | Err(RefRefusal::NameConflict) => return Ok((CreateOutcome::Exists, None)),
            Err(r) => return Err(self.refusal(r, &refname)),
        }
        self.mark_created(&refname, now).await?;
        self.touched();
        let moved = (!ephemeral).then_some(Moved { ref_name: refname, before: None, after: at });
        Ok((CreateOutcome::Created(at), moved))
    }

    /// What merging `source` into `target` does: nothing when the target
    /// holds the source, a fast-forward when it can, else three ways from
    /// the merge base (contract 15.1, git's semantics).
    async fn decide(&self, target: Oid, source: Oid, allow_unrelated: bool) -> Result<(Option<Oid>, Decision), StoreError> {
        let base = self.git.merge_base(target, source).await?;
        if base.is_none() && !allow_unrelated {
            return Err(StoreError::invalid("unrelated_histories", "refusing to merge unrelated histories"));
        }
        if base == Some(source) {
            return Ok((base, Decision::NoOp));
        }
        if base == Some(target) {
            return Ok((base, Decision::FastForward));
        }
        Ok((base, Decision::ThreeWay))
    }

    /// The merge preview (contract 15.1): the merge's decision, writing no
    /// ref (merge-tree writes the merged tree's objects, referenced by
    /// nothing, which maintenance prunes).
    pub async fn preview(&self, source: &str, source_ephemeral: bool, target: &str, target_ephemeral: bool) -> Result<Preview, StoreError> {
        let s = self.branch(source, source_ephemeral).await?.ok_or_else(|| StoreError::not_found("branch_not_found", format!("branch not found: {source}")))?;
        let t = self.branch(target, target_ephemeral).await?.ok_or_else(|| StoreError::not_found("branch_not_found", format!("branch not found: {target}")))?;
        let (base, d) = self.decide(t, s, false).await?;
        let (kind, conflicts) = match d {
            Decision::NoOp => (MergeKind::NoOp, vec![]),
            Decision::FastForward => (MergeKind::FastForward, vec![]),
            Decision::ThreeWay => match self.git.merge_tree(t, s, false).await? {
                Merged::Clean { .. } => (MergeKind::MergeCommit, vec![]),
                Merged::Conflict { paths, .. } => (MergeKind::MergeCommit, paths),
                Merged::Unrelated => return Err(StoreError::invalid("unrelated_histories", "refusing to merge unrelated histories")),
            },
        };
        Ok(Preview { source_tip: s, target_tip: t, kind, base, conflicts })
    }

    /// A merge (contract 5.10, with the service's semantics).
    pub async fn merge(&self, req: MergeRequest, now: i64) -> Result<(MergeOutcome, Option<Moved>), StoreError> {
        let (author, committer) = idents(req.author.clone(), req.committer.clone());
        let _w = self.write_lock.lock().await;
        let refname = ref_name(&req.target, req.target_ephemeral);
        let old = self.ref_tip(&refname).await?.ok_or_else(|| StoreError::not_found("branch_not_found", format!("branch not found: {}", req.target)))?;
        let source = self.resolve(&req.source, req.source_ephemeral).await?.ok_or_else(|| StoreError::not_found("ref_not_found", format!("source_ref not found: {}", req.source)))?;
        if req.expected.is_some_and(|e| e != old) {
            return Ok((MergeOutcome::TargetMoved { current: old, base: None }, None));
        }
        let (base, d) = self.decide(old, source, req.allow_unrelated).await?;
        let done = |kind, new, tree, promoted| MergeOutcome::Done(MergeDone { kind, old, new, tree, base, source, promoted });
        if d == Decision::NoOp {
            let tree = self.commit_info(old).await?.tree;
            return Ok((done(MergeKind::NoOp, old, tree, 0), None));
        }
        if d == Decision::FastForward && !req.squash && req.strategy != Strategy::Merge {
            let promoted = self.git.rev_count(&format!("{old}..{source}")).await?;
            return match self.move_ref(&refname, Some(old), source, None).await? {
                Ok(()) => {
                    self.touched();
                    let tree = self.commit_info(source).await?.tree;
                    let moved = (!req.target_ephemeral).then(|| Moved { ref_name: refname.clone(), before: Some(old), after: source });
                    Ok((done(MergeKind::FastForward, source, tree, promoted), moved))
                }
                Err(RefRefusal::Moved { current }) => Ok((MergeOutcome::TargetMoved { current, base }, None)),
                Err(r) => Err(self.refusal(r, &refname)),
            };
        }
        if d == Decision::ThreeWay && req.strategy == Strategy::FfOnly {
            return Ok((MergeOutcome::NotFastForward { base }, None));
        }
        let tree = match self.git.merge_tree(old, source, req.allow_unrelated).await? {
            Merged::Clean { tree } => tree,
            Merged::Conflict { paths, .. } => return Ok((MergeOutcome::Conflict { base, paths }, None)),
            Merged::Unrelated => return Err(StoreError::invalid("unrelated_histories", "refusing to merge unrelated histories")),
        };
        // A squash is one commit on the target's tip: one that would change
        // nothing is refused, as a commit pack's is (merge-tree's tree is
        // left unreferenced, as a preview's, for maintenance to prune).
        if req.squash && tree == self.commit_info(old).await?.tree {
            return Ok((MergeOutcome::Unchanged { current: old }, None));
        }
        let parents = if req.squash { vec![old] } else { vec![old, source] };
        for p in &parents {
            self.ensure_record(*p).await?;
        }
        let new = self.git.commit_tree(tree, &parents, req.message.as_bytes(), &author, &committer, now).await?;
        let record = self.compute_record(new, &parents).await?;
        self.write_record(&record).await?;
        let promoted = self.git.rev_count(&format!("{old}..{source}")).await?;
        match self.move_ref(&refname, Some(old), new, None).await? {
            Ok(()) => {}
            Err(RefRefusal::Moved { current }) => return Ok((MergeOutcome::TargetMoved { current, base }, None)),
            Err(r) => return Err(self.refusal(r, &refname)),
        }
        self.touched();
        let kind = if req.squash { MergeKind::Squash } else { MergeKind::MergeCommit };
        let moved = (!req.target_ephemeral).then_some(Moved { ref_name: refname, before: Some(old), after: new });
        Ok((done(kind, new, tree, promoted), moved))
    }

    /// A restore commit (contract 5.11, the service's refusals).
    #[allow(clippy::too_many_arguments)]
    pub async fn restore(&self, target: &str, ephemeral: bool, base_ref: &str, expected: &Expected, message: &str, author: Option<Ident>, committer: Option<Ident>, now: i64) -> Result<(RestoreOutcome, Option<Moved>), StoreError> {
        let (author, committer) = idents(author, committer);
        let _w = self.write_lock.lock().await;
        let refname = ref_name(target, ephemeral);
        let tip = self.ref_tip(&refname).await?.ok_or_else(|| StoreError::not_found("branch_not_found", format!("branch not found: {target}")))?;
        let matches = match expected {
            Expected::Any => true,
            Expected::Absent => false,
            Expected::At(x) => tip.to_string() == *x,
        };
        if !matches {
            return Ok((RestoreOutcome::Moved { current: tip }, None));
        }
        let refused = |why| Ok((RestoreOutcome::Refused { tip, why }, None));
        let base = match self.resolve(base_ref, false).await {
            Ok(Some(b)) => b,
            Ok(None) => return refused("base_ref is not an ancestor of the target's tip"),
            Err(e) => return Err(e),
        };
        if base == tip {
            return refused("the branch already points at base_ref");
        }
        match self.git.is_ancestor(base, tip).await {
            Ok(true) => {}
            Ok(false) => return refused("base_ref is not an ancestor of the target's tip"),
            Err(e) if unknown_object(&e) => return refused("base_ref is not an ancestor of the target's tip"),
            Err(e) => return Err(e.into()),
        }
        let base_tree = self.commit_info(base).await?.tree;
        if base_tree == self.commit_info(tip).await?.tree {
            return refused("the restore would produce no change");
        }
        self.ensure_record(tip).await?;
        let new = self.git.commit_tree(base_tree, &[tip], message.as_bytes(), &author, &committer, now).await?;
        let record = self.compute_record(new, &[tip]).await?;
        self.write_record(&record).await?;
        match self.move_ref(&refname, Some(tip), new, None).await? {
            Ok(()) => {}
            Err(RefRefusal::Moved { current }) => return Ok((RestoreOutcome::Moved { current }, None)),
            Err(r) => return Err(self.refusal(r, &refname)),
        }
        self.touched();
        let moved = (!ephemeral).then_some(Moved { ref_name: refname, before: Some(tip), after: new });
        Ok((RestoreOutcome::Done(Committed { branch: target.into(), old: Some(tip), new, tree: base_tree, blob_count: 0 }), moved))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
    NoOp,
    FastForward,
    ThreeWay,
}

/// Writes `bytes` at `path` as git writes a loose object: a temporary
/// file beside it, synced, renamed into place, its directory synced.
pub fn write_durably(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().expect("a file has a directory");
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    crate::registry::sync_dir(dir)
}
