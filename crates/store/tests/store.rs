//! The store against the pinned engine: creates, commit packs, the
//! compare-and-swap, last commits checked against git's own `log -1`,
//! merges, previews, restores, history, recovery, maintenance.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use macrofiche_contract::names::Expected;
use macrofiche_git::jail::Settings;
use macrofiche_git::{Oid, RunOpts};
use macrofiche_pack::{FileOp, Metadata, Op};
use macrofiche_store::repo::{CommitOutcome, CreateOutcome, MergeKind, MergeOutcome, MergeRequest, PackChange, PackFile, RestoreOutcome, Strategy};
use macrofiche_store::{Deleted, RepoHandle, RepoMeta, Store};

async fn store() -> (Arc<Store>, PathBuf) {
    let root = macrofiche_testkit::scratch("store");
    let (s, _) = Store::open(root.join("state"), Settings::linux(macrofiche_testkit::git_prefix())).await.unwrap();
    (s, root)
}

/// One upsert's bytes spooled (`Exe`'s as `100755`), or a delete.
enum F<'a> {
    Up(&'a str, &'a [u8]),
    Exe(&'a str, &'a [u8]),
    Del(&'a str),
}

static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

async fn pack(r: &RepoHandle, root: &Path, branch: &str, expected: Expected, files: &[F<'_>], t: i64) -> CommitOutcome {
    pack_on(r, root, branch, None, expected, files, t).await
}

/// A pack whose branch, when absent, is made from `base` (`base_branch`).
async fn pack_on(r: &RepoHandle, root: &Path, branch: &str, base: Option<&str>, expected: Expected, files: &[F<'_>], t: i64) -> CommitOutcome {
    let spool = root.join(format!("spool-{}", N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    std::fs::create_dir_all(&spool).unwrap();
    let mut ops = Vec::new();
    let mut pf = Vec::new();
    for (i, f) in files.iter().enumerate() {
        match f {
            F::Up(p, b) | F::Exe(p, b) => {
                let mode = if matches!(f, F::Exe(..)) { 0o100755 } else { 0o100644 };
                let s = spool.join(i.to_string());
                std::fs::write(&s, b).unwrap();
                ops.push(FileOp { path: p.to_string(), op: Op::Upsert { content_id: format!("c{i}"), mode } });
                pf.push(PackFile { path: p.to_string(), change: PackChange::Upsert { mode, spool: s, len: b.len() as u64 } });
            }
            F::Del(p) => {
                ops.push(FileOp { path: p.to_string(), op: Op::Delete });
                pf.push(PackFile { path: p.to_string(), change: PackChange::Delete });
            }
        }
    }
    let base_branch = base.map(str::to_string);
    let meta = Metadata { target_branch: branch.into(), expected, message: format!("commit at {t}"), author: None, committer: None, files: ops, base_branch, ephemeral: false, base_is_ephemeral: false };
    r.commit_pack(&meta, &pf, None, None, t).await.unwrap().0
}

async fn commit(r: &RepoHandle, root: &Path, branch: &str, files: &[F<'_>], t: i64) -> Oid {
    let tip = r.branch(branch, false).await.unwrap();
    let exp = tip.map_or(Expected::Absent, |t| Expected::At(t.to_string()));
    match pack(r, root, branch, exp, files, t).await {
        CommitOutcome::Committed(c) => c.new,
        CommitOutcome::Moved { .. } => panic!("moved"),
        CommitOutcome::Unchanged { .. } => panic!("changed nothing"),
    }
}

/// git's own last commit of `path` from `c`: `log -1 --format=%H c -- path`.
async fn gits_last(r: &RepoHandle, c: Oid, path: &str) -> Oid {
    let out = r.git.run("log", &["log", "-1", "--format=%H", &c.to_string(), "--", path], RunOpts::read()).await.unwrap();
    Oid::parse(String::from_utf8_lossy(&out.stdout).trim()).unwrap_or_else(|| panic!("git log for {path}: {out:?}"))
}

/// Every path's last commit at `c` equals git's.
async fn agrees_with_git(r: &RepoHandle, c: Oid) {
    let l = r.listing(c).await.unwrap();
    for e in &l.entries {
        assert_eq!(e.last_commit, gits_last(r, c, &e.path).await, "{} at {c}", e.path);
    }
}

// Goal: a repository is made once; its name and id find it; a second
// create of the name is refused; one that fails leaves nothing. Method:
// creates, then the registry read back after a reopen (restart).
#[tokio::test(flavor = "multi_thread")]
async fn repositories_are_made_once_and_survive_a_restart() {
    let (s, root) = store().await;
    let m = s.create_repo("fragment-dev", "todo--paul", "main", 1_790_000_000_000).await.unwrap();
    assert_eq!(m.repo_id.len(), 32);
    let again = s.create_repo("fragment-dev", "todo--paul", "main", 1).await.unwrap_err();
    assert_eq!(again.problem().status, 409);
    assert!(s.create_repo("fragment-dev", "a/../b", "main", 1).await.is_err());
    s.create_repo("other", "todo--paul", "main", 2).await.unwrap();
    assert_eq!(s.repo_by_id("fragment-dev", &m.repo_id).unwrap().repo_name, "todo--paul");
    let r = s.repo("fragment-dev", "todo--paul").await.unwrap();
    assert_eq!(r.branch("main", false).await.unwrap(), None, "a new repo has no branches");
    drop((r, s));
    let (s2, rec) = Store::open(root.join("state"), Settings::linux(macrofiche_testkit::git_prefix())).await.unwrap();
    assert_eq!((rec.repos, rec.strays), (2, 0));
    assert_eq!(s2.repo_meta("fragment-dev", "todo--paul").unwrap(), m);
    assert!(std::fs::read_dir(root.join("state/tmp")).unwrap().next().is_none());
}

/// Every file under `dir`, relative to it, sorted.
fn files_under(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            match e.file_type().unwrap().is_dir() {
                true => stack.push(e.path()),
                false => out.push(e.path().strip_prefix(dir).unwrap().to_string_lossy().into_owned()),
            }
        }
    }
    out.sort();
    out
}

// Goal: a delete (contract 5.13) leaves nothing a reader or a writer can
// reach but its marked identity, keeps its name taken, answers a delete
// again as done, and holds across a restart; a delete a crash cut short
// after its mark is finished at boot. Method: a repository with a commit
// deleted while a handle to it is held, then asked again, made again and
// reopened; another marked by hand with its data left, as such a crash
// leaves it, then a reopen.
#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_repository_is_gone_for_good() {
    let (s, root) = store().await;
    let m = s.create_repo("o", "gone", "main", 1).await.unwrap();
    let kept = s.create_repo("o", "kept", "main", 2).await.unwrap();
    let r = s.repo("o", "gone").await.unwrap();
    commit(&r, &root, "main", &[F::Up("a", b"a")], 1_790_000_000).await;
    let dir = r.dir.clone();
    assert!(files_under(&dir).len() > 1);
    let d = s.delete_repo("o", "gone", 5).await.unwrap();
    assert_eq!(d, Deleted::Now(RepoMeta { deleted_at_ms: Some(5), ..m.clone() }));
    assert_eq!(files_under(&dir), ["macrofiche/repo.json"], "nothing but its marked identity");
    // a handle held across it writes nothing
    assert_eq!(r.write().await.unwrap_err().problem().status, 404);
    assert_eq!(s.repo("o", "gone").await.err().map(|e| e.problem().status), Some(404));
    assert!(s.repo_by_id("o", &m.repo_id).is_none() && s.repo_meta("o", "gone").is_none());
    assert_eq!(s.repos_page("o", None, 100, None).0, std::slice::from_ref(&kept), "it leaves the list");
    // replayed: done; its name is never made again; an unknown one is 404
    assert_eq!(s.delete_repo("o", "gone", 6).await.unwrap(), Deleted::Already);
    let again = s.create_repo("o", "gone", "main", 7).await.unwrap_err().problem();
    assert_eq!((again.status, again.code), (409, "repository_deleted"));
    assert_eq!(s.delete_repo("o", "never", 8).await.unwrap_err().problem().status, 404);
    drop((r, s));
    let open = || Store::open(root.join("state"), Settings::linux(macrofiche_testkit::git_prefix()));
    let (s, rec) = open().await.unwrap();
    assert_eq!((rec.repos, rec.deleted, rec.strays), (1, 1, 0));
    assert!(s.repo_meta("o", "gone").is_none());
    assert_eq!(s.create_repo("o", "gone", "main", 9).await.unwrap_err().problem().code, "repository_deleted", "after a restart too");
    // a delete cut short after its mark: its data is still there
    let r = s.repo("o", "kept").await.unwrap();
    commit(&r, &root, "main", &[F::Up("k", b"k")], 1_790_000_002).await;
    let kdir = r.dir.clone();
    let marked = RepoMeta { deleted_at_ms: Some(10), ..kept };
    std::fs::write(kdir.join("macrofiche/repo.json"), serde_json::to_vec(&marked).unwrap()).unwrap();
    drop((r, s));
    let (s, rec) = open().await.unwrap();
    assert_eq!((rec.repos, rec.deleted), (0, 2));
    assert!(s.repo_meta("o", "kept").is_none());
    assert_eq!(files_under(&kdir), ["macrofiche/repo.json"], "finished at boot");
    assert!(s.repos_page("o", None, 100, None).0.is_empty());
}

// Goal: commit packs land as git commits on the compare-and-swap's terms
// (absent, at the tip, stale, any), deletes of absent paths do nothing,
// and every last commit is git's. Method: a run of packs, each checked.
#[tokio::test(flavor = "multi_thread")]
async fn commit_packs_and_the_cas() {
    let (s, root) = store().await;
    s.create_repo("o", "r", "main", 1).await.unwrap();
    let r = s.repo("o", "r").await.unwrap();
    let c1 = commit(&r, &root, "main", &[F::Up("a.txt", b"alpha"), F::Up("dir/b.txt", b"beta"), F::Up("dir/sub/c.txt", b"")], 1_790_000_000).await;
    // a stale expectation, and "must not exist" on an existing branch: 409, nothing lands
    assert!(matches!(pack(&r, &root, "main", Expected::At("0123456789abcdef0123456789abcdef01234567".into()), &[F::Up("x", b"x")], 1).await, CommitOutcome::Moved { current: Some(c) } if c == c1));
    assert!(matches!(pack(&r, &root, "main", Expected::Absent, &[F::Up("x", b"x")], 1).await, CommitOutcome::Moved { current: Some(c) } if c == c1));
    assert_eq!(r.branch("main", false).await.unwrap(), Some(c1));
    // an update, a delete, and a delete of nothing
    let c2 = commit(&r, &root, "main", &[F::Up("a.txt", b"alpha 2"), F::Del("dir/b.txt"), F::Del("never.txt")], 1_790_000_001).await;
    let f = r.file(c2, "a.txt").await.unwrap().unwrap();
    assert_eq!((f.size, f.last_commit), (7, c2));
    assert_eq!(r.file(c2, "dir/sub/c.txt").await.unwrap().unwrap().last_commit, c1);
    assert!(r.file(c2, "dir/b.txt").await.unwrap().is_none());
    assert!(r.file(c2, "dir").await.unwrap().is_none(), "a tree is no file");
    // with no expectation, it lands on whatever the tip is
    let c3 = match pack(&r, &root, "main", Expected::Any, &[F::Up("d.txt", b"d")], 1_790_000_002).await {
        CommitOutcome::Committed(c) => {
            assert_eq!((c.old, c.blob_count), (Some(c2), 1));
            c.new
        }
        _ => panic!(),
    };
    for c in [c1, c2, c3] {
        agrees_with_git(&r, c).await;
    }
    let info = r.commit_info(c3).await.unwrap();
    assert_eq!((info.parents.clone(), info.author.name.as_str(), info.message.as_str(), info.committer.time), (vec![c2], "macrofiche", "commit at 1790000002", 1_790_000_002));
    // history: newest first, paged
    let (h, more) = r.history(c3, 0, 2).await.unwrap();
    assert_eq!((h.iter().map(|c| c.sha).collect::<Vec<_>>(), more), (vec![c3, c2], true));
    let (h, more) = r.history(c3, 2, 2).await.unwrap();
    assert_eq!((h.iter().map(|c| c.sha).collect::<Vec<_>>(), more), (vec![c1], false));
    // resolution: a sha, a branch, nothing
    assert_eq!(r.resolve(&c1.to_string(), false).await.unwrap(), Some(c1));
    assert_eq!(r.resolve("main", false).await.unwrap(), Some(c3));
    assert_eq!(r.resolve("0123456789abcdef0123456789abcdef01234567", false).await.unwrap(), None);
    assert_eq!(r.resolve("a..b", false).await.unwrap(), None);
    let fsck = r.git.fsck().await.unwrap();
    assert!(fsck.ok, "{}", fsck.stderr);
}

// Goal: a pack that changes nothing is refused (the service's 412, `no
// changes to commit`), never an empty commit, after the compare-and-swap
// as the fake orders the two; what changes anything commits, a mode
// included (git's tree: the fake compares bytes); a squash that adds
// nothing is refused the same way, a merge commit is not (git's); and
// the refusals leave no temporary ref and a whole repository, across a
// reopen. Method: packs on a seeded main (its bytes, a delete of nothing,
// no files, the whole tree again, each stale too), packs that would make
// a branch (deletes only, a base's own bytes), merges of a branch holding
// main's change, then a reopen, the replay, a write, fsck.
#[tokio::test(flavor = "multi_thread")]
async fn a_pack_that_changes_nothing_is_refused() {
    let (s, root) = store().await;
    s.create_repo("o", "r", "main", 1).await.unwrap();
    let r = s.repo("o", "r").await.unwrap();
    let t0 = 1_790_000_000;
    let c1 = commit(&r, &root, "main", &[F::Up("a", b"1"), F::Up("dir/b", b"2")], t0).await;
    let refused = |o: CommitOutcome, tip: Option<Oid>| matches!(o, CommitOutcome::Unchanged { tip: t } if t == tip);
    let at = |c: Oid| Expected::At(c.to_string());
    assert!(refused(pack(&r, &root, "main", at(c1), &[F::Up("a", b"1")], t0 + 1).await, Some(c1)), "the bytes main holds");
    assert!(refused(pack(&r, &root, "main", at(c1), &[F::Del("nope")], t0 + 1).await, Some(c1)), "a delete of what is not there");
    assert!(refused(pack(&r, &root, "main", at(c1), &[], t0 + 1).await, Some(c1)), "no files");
    assert!(refused(pack(&r, &root, "main", Expected::Any, &[F::Up("dir/b", b"2"), F::Up("a", b"1"), F::Del("dir/c")], t0 + 1).await, Some(c1)), "the whole tree again, expecting nothing");
    // the compare-and-swap first: a stale pack is 409 however little it changes
    let stale = Expected::At("0123456789abcdef0123456789abcdef01234567".into());
    assert!(matches!(pack(&r, &root, "main", stale, &[F::Up("a", b"1")], t0 + 1).await, CommitOutcome::Moved { current: Some(c) } if c == c1));
    assert!(matches!(pack(&r, &root, "main", Expected::Absent, &[F::Up("a", b"1")], t0 + 1).await, CommitOutcome::Moved { current: Some(c) } if c == c1));
    assert_eq!(r.branch("main", false).await.unwrap(), Some(c1), "nothing moved");
    assert_eq!(r.history(c1, 0, 10).await.unwrap().0.len(), 1, "no empty commit");
    // on a branch it would make: deletes alone leave the empty tree, and a
    // base's own bytes leave the base's
    assert!(refused(pack(&r, &root, "fresh", Expected::Absent, &[F::Del("x")], t0 + 2).await, None), "deletes only, on no branch");
    assert!(refused(pack_on(&r, &root, "topic", Some("main"), Expected::Absent, &[F::Up("a", b"1")], t0 + 2).await, None), "a base's own bytes");
    assert_eq!((r.branch("fresh", false).await.unwrap(), r.branch("topic", false).await.unwrap()), (None, None), "no branch made");
    // what changes anything commits: a mode, then an empty file
    let CommitOutcome::Committed(c2) = pack(&r, &root, "main", at(c1), &[F::Exe("a", b"1")], t0 + 3).await else { panic!("a mode is a change") };
    let mode = r.listing(c2.new).await.unwrap().entries.iter().find(|e| e.path == "a").map(|e| e.mode);
    assert_eq!(mode, Some(0o100755));
    assert!(refused(pack(&r, &root, "main", at(c2.new), &[F::Exe("a", b"1")], t0 + 4).await, Some(c2.new)), "the same mode again");
    let c3 = commit(&r, &root, "main", &[F::Up("empty", b"")], t0 + 5).await;
    agrees_with_git(&r, c3).await;
    // a branch from c1 that adds main's own empty file: squashing it adds
    // nothing (refused), merging it is a merge commit with main's tree
    r.create_branch("side", false, &c1.to_string(), false, t0).await.unwrap();
    let side = commit(&r, &root, "side", &[F::Up("empty", b"")], t0 + 6).await;
    let merge = |squash: bool, expected: Oid| MergeRequest {
        target: "main".into(),
        target_ephemeral: false,
        source: "side".into(),
        source_ephemeral: false,
        strategy: Strategy::FfPrefer,
        expected: Some(expected),
        message: "merge side".into(),
        author: None,
        committer: None,
        squash,
        allow_unrelated: false,
    };
    assert!(matches!(r.merge(merge(true, c3), t0 + 7).await.unwrap(), (MergeOutcome::Unchanged { current }, None) if current == c3), "a squash that adds nothing");
    assert_eq!(r.branch("main", false).await.unwrap(), Some(c3), "and moves nothing");
    let MergeOutcome::Done(m) = r.merge(merge(false, c3), t0 + 8).await.unwrap().0 else { panic!("a merge commit") };
    let c3_tree = r.commit_info(c3).await.unwrap().tree;
    assert_eq!((m.kind, r.commit_info(m.new).await.unwrap().parents.clone(), m.tree), (MergeKind::MergeCommit, vec![c3, side], c3_tree));
    commit(&r, &root, "side", &[F::Up("g", b"g")], t0 + 9).await;
    let MergeOutcome::Done(q) = r.merge(merge(true, m.new), t0 + 10).await.unwrap().0 else { panic!("a squash that adds g") };
    assert_eq!((q.kind, r.commit_info(q.new).await.unwrap().parents.clone()), (MergeKind::Squash, vec![m.new]));
    agrees_with_git(&r, q.new).await;
    // no refusal left a temporary ref; a reopen clears nothing, and the
    // replay is refused again before a change lands
    let refs = r.git.run("for-each-ref", &["for-each-ref", "refs/macrofiche/"], RunOpts::read()).await.unwrap();
    assert!(refs.stdout.is_empty(), "temporary refs left: {}", String::from_utf8_lossy(&refs.stdout));
    assert!(r.git.fsck().await.unwrap().ok);
    drop((r, s));
    let (s, rec) = Store::open(root.join("state"), Settings::linux(macrofiche_testkit::git_prefix())).await.unwrap();
    assert_eq!((rec.temp_refs, rec.locks, rec.temp_files), (0, 0, 0), "a refusal leaves nothing to clear");
    let r = s.repo("o", "r").await.unwrap();
    assert_eq!(r.branch("main", false).await.unwrap(), Some(q.new));
    assert!(refused(pack(&r, &root, "main", at(q.new), &[F::Up("g", b"g")], t0 + 11).await, Some(q.new)), "the replay, after a reopen");
    let c4 = commit(&r, &root, "main", &[F::Up("g", b"g2")], t0 + 12).await;
    assert_eq!(r.commit_info(c4).await.unwrap().parents, vec![q.new]);
    assert!(r.git.fsck().await.unwrap().ok);
}

// Goal: last commits are git's through merges, restores and fast-forwards,
// and across a snapshot boundary (more than SNAPSHOT_EVERY commits).
// Method: a history of 40 commits on main, a live branch with a rollback
// and three-way merges, every listing compared with `git log -1`.
#[tokio::test(flavor = "multi_thread")]
async fn last_commits_are_gits_through_merges_and_snapshots() {
    let (s, root) = store().await;
    s.create_repo("o", "r", "main", 1).await.unwrap();
    let r = s.repo("o", "r").await.unwrap();
    let t0 = 1_790_000_000;
    let mut c = commit(&r, &root, "main", &[F::Up("index.html", b"v1"), F::Up("a", b"a"), F::Up("b", b"b")], t0).await;
    let v1 = c;
    assert!(matches!(r.create_branch("live", false, "main", false, t0).await.unwrap().0, CreateOutcome::Created(x) if x == v1));
    assert!(matches!(r.create_branch("live", false, "main", false, t0).await.unwrap().0, CreateOutcome::Exists));
    for i in 0..40 {
        let body = format!("{i}");
        let name = format!("f{}", i % 7);
        c = commit(&r, &root, "main", &[F::Up(&name, body.as_bytes())], t0 + 1 + i).await;
    }
    agrees_with_git(&r, c).await;
    // deploy: live fast-forwards
    let req = |expected, message: &str, strategy| MergeRequest { target: "live".into(), target_ephemeral: false, source: "main".into(), source_ephemeral: false, strategy, expected, message: message.into(), author: None, committer: None, squash: false, allow_unrelated: false };
    let (o, moved) = r.merge(req(Some(v1), "deploy", Strategy::FfPrefer), t0 + 100).await.unwrap();
    let MergeOutcome::Done(d) = o else { panic!() };
    assert_eq!((d.kind, d.new, d.promoted, moved.unwrap().after), (MergeKind::FastForward, c, 40, c));
    // a stale expectation
    assert!(matches!(r.merge(req(Some(v1), "again", Strategy::FfPrefer), t0 + 101).await.unwrap().0, MergeOutcome::TargetMoved { current, .. } if current == c));
    // the target holds the source: no_op
    assert!(matches!(r.merge(req(Some(c), "again", Strategy::FfPrefer), t0 + 101).await.unwrap().0, MergeOutcome::Done(MergeDone { kind: MergeKind::NoOp, .. })));
    // rollback to v1 (the service's 412s first)
    let refused = |o: RestoreOutcome| matches!(o, RestoreOutcome::Refused { .. });
    assert!(refused(r.restore("live", false, &c.to_string(), &Expected::At(c.to_string()), "to the tip", None, None, t0).await.unwrap().0), "the tip itself");
    assert!(refused(r.restore("live", false, "0123456789abcdef0123456789abcdef01234567", &Expected::Any, "unknown", None, None, t0).await.unwrap().0), "an unknown base");
    assert!(matches!(r.restore("live", false, &v1.to_string(), &Expected::At(v1.to_string()), "stale", None, None, t0).await.unwrap().0, RestoreOutcome::Moved { current } if current == c));
    let RestoreOutcome::Done(rb) = r.restore("live", false, &v1.to_string(), &Expected::At(c.to_string()), "rollback", None, None, t0 + 200).await.unwrap().0 else { panic!() };
    assert_eq!(r.commit_info(rb.new).await.unwrap().parents, vec![c], "one parent: the old tip");
    assert_eq!(r.commit_info(rb.new).await.unwrap().tree, r.commit_info(v1).await.unwrap().tree);
    agrees_with_git(&r, rb.new).await;
    // the preview says no_op (main is in live), and so does the merge
    let p = r.preview("main", false, "live", false).await.unwrap();
    assert_eq!((p.kind, p.base, p.target_tip), (MergeKind::NoOp, Some(c), rb.new));
    // restoring live to v1's tree again changes nothing: 412
    let v1_again = r.restore("live", false, &v1.to_string(), &Expected::At(rb.new.to_string()), "again", None, None, t0).await.unwrap().0;
    assert!(matches!(v1_again, RestoreOutcome::Refused { why, .. } if why.contains("no change")));
    // main moves on: the preview says merge_commit; the merge keeps the
    // rollback's reverts (git's three-way merge, the service's)
    let m1 = commit(&r, &root, "main", &[F::Up("new.txt", b"new")], t0 + 300).await;
    let p = r.preview("main", false, "live", false).await.unwrap();
    assert_eq!((p.kind, p.base, p.conflicts.len()), (MergeKind::MergeCommit, Some(c), 0));
    let MergeOutcome::Done(d) = r.merge(req(Some(rb.new), "deploy after rollback", Strategy::FfPrefer), t0 + 301).await.unwrap().0 else { panic!() };
    assert_eq!(d.kind, MergeKind::MergeCommit);
    let mc = d.new;
    assert_eq!(r.commit_info(mc).await.unwrap().parents, vec![rb.new, m1]);
    let read = |path: &'static str| {
        let r = r.clone();
        async move {
            let f = r.file(mc, path).await.unwrap()?;
            let mut b = r.open_blob(f.blob).await.unwrap();
            let mut out = Vec::new();
            while let Some(chunk) = b.next().await.unwrap() {
                out.extend(chunk);
            }
            Some(out)
        }
    };
    assert_eq!(read("index.html").await.unwrap(), b"v1", "the rollback's revert kept");
    assert_eq!(read("new.txt").await.unwrap(), b"new", "main's new file merged");
    assert!(read("f3").await.is_none(), "the rollback's removal kept");
    agrees_with_git(&r, mc).await;
    // a conflict: main changes index.html and deploys it, live rolls back
    // to v1, then main changes index.html again (contract 15)
    commit(&r, &root, "main", &[F::Up("index.html", b"v2")], t0 + 400).await;
    let live = r.branch("live", false).await.unwrap().unwrap();
    let MergeOutcome::Done(d) = r.merge(req(Some(live), "deploy v2", Strategy::FfPrefer), t0 + 401).await.unwrap().0 else { panic!() };
    agrees_with_git(&r, d.new).await;
    let RestoreOutcome::Done(rb2) = r.restore("live", false, &v1.to_string(), &Expected::At(d.new.to_string()), "rollback 2", None, None, t0 + 402).await.unwrap().0 else { panic!() };
    commit(&r, &root, "main", &[F::Up("index.html", b"v3")], t0 + 403).await;
    let p = r.preview("main", false, "live", false).await.unwrap();
    assert_eq!((p.kind, p.conflicts.clone()), (MergeKind::MergeCommit, vec!["index.html".to_string()]));
    assert!(matches!(r.merge(req(Some(rb2.new), "conflicted", Strategy::FfPrefer), t0 + 402).await.unwrap().0, MergeOutcome::Conflict { paths, .. } if paths == ["index.html"]));
    assert_eq!(r.branch("live", false).await.unwrap(), Some(rb2.new), "a conflict moves nothing");
    // ff_only refuses a diverged target; `merge` makes a merge commit even where it could fast-forward
    assert!(matches!(r.merge(req(None, "ff only", Strategy::FfOnly), t0 + 403).await.unwrap().0, MergeOutcome::NotFastForward { .. }));
    r.create_branch("side", false, &v1.to_string(), false, t0).await.unwrap();
    let side_req = MergeRequest { target: "side".into(), strategy: Strategy::Merge, ..req(None, "no ff", Strategy::Merge) };
    let MergeOutcome::Done(d) = r.merge(side_req, t0 + 404).await.unwrap().0 else { panic!() };
    assert_eq!((d.kind, r.commit_info(d.new).await.unwrap().parents.len()), (MergeKind::MergeCommit, 2));
    agrees_with_git(&r, d.new).await;
    let fsck = r.git.fsck().await.unwrap();
    assert!(fsck.ok, "{}", fsck.stderr);
}

use macrofiche_store::repo::MergeDone;

// Goal: what a killed writer leaves is cleared at the next open, and the
// repository is whole after. Method: leftovers planted (a stale ref lock,
// a temporary ref, a temporary object, a crash report, a spool, a
// half-made repository), then a reopen, a write, and fsck.
#[tokio::test(flavor = "multi_thread")]
async fn recovery_clears_a_killed_writers_leftovers() {
    let (s, root) = store().await;
    let m = s.create_repo("o", "r", "main", 1).await.unwrap();
    let r = s.repo("o", "r").await.unwrap();
    let c1 = commit(&r, &root, "main", &[F::Up("a", b"a")], 1_790_000_000).await;
    let dir = r.dir.clone();
    r.git.update_refs(&[macrofiche_git::RefOp::Create { name: "refs/macrofiche/tmp/dead".into(), new: c1 }]).await.unwrap().unwrap();
    std::fs::write(dir.join("refs/heads/main.lock"), b"").unwrap();
    std::fs::write(dir.join("objects/tmp_obj_abc"), b"x").unwrap();
    std::fs::write(dir.join("fast_import_crash_1"), b"x").unwrap();
    std::fs::create_dir_all(root.join("state/spool/x")).unwrap();
    std::fs::create_dir_all(root.join("state/tmp/half.git")).unwrap();
    drop((r, s));
    let (s, rec) = Store::open(root.join("state"), Settings::linux(macrofiche_testkit::git_prefix())).await.unwrap();
    assert_eq!((rec.locks, rec.temp_files, rec.crash_reports, rec.temp_refs, rec.spools, rec.half_made), (1, 1, 1, 1, 1, 1));
    let r = s.repo("o", &m.repo_name).await.unwrap();
    let c2 = commit(&r, &root, "main", &[F::Up("b", b"b")], 1_790_000_001).await;
    assert_eq!(r.commit_info(c2).await.unwrap().parents, vec![c1]);
    assert!(r.git.fsck().await.unwrap().ok);
    // and maintenance after: idle, it repacks under the lock
    let done = s.maintain(std::time::Duration::ZERO).await;
    assert_eq!(done.len(), 1, "{done:?}");
    assert!(done[0].ok && done[0].gc, "{done:?}");
    assert_eq!(s.dirty(), 0);
    agrees_with_git(&r, c2).await;
}

// Goal: a lost index is rebuilt from history, and the rebuilt map is
// git's. Method: the index directory removed, then a listing.
#[tokio::test(flavor = "multi_thread")]
async fn a_lost_index_is_rebuilt() {
    let (s, root) = store().await;
    s.create_repo("o", "r", "main", 1).await.unwrap();
    let r = s.repo("o", "r").await.unwrap();
    let mut c = commit(&r, &root, "main", &[F::Up("a", b"a"), F::Up("b", b"b")], 1_790_000_000).await;
    for i in 0..5 {
        c = commit(&r, &root, "main", &[F::Up("a", format!("{i}").as_bytes())], 1_790_000_001 + i).await;
    }
    std::fs::remove_dir_all(r.dir.join("macrofiche/lastcommit")).unwrap();
    drop(r);
    // a fresh handle: nothing cached from before
    let (s2, _) = Store::open(root.join("state"), Settings::linux(macrofiche_testkit::git_prefix())).await.unwrap();
    drop(s);
    let r = s2.repo("o", "r").await.unwrap();
    agrees_with_git(&r, c).await;
}
