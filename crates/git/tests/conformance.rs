//! One test per git message macrofiche reads: the pinned engine produces
//! it, and the reading is checked (docs/library.md, Risks: "a conformance
//! test per message keeps a 409 a 409"). A new git that rewords one fails
//! here first.

use std::path::PathBuf;

use macrofiche_git::error::{classify_update_ref, is_unknown_object, is_unrelated};
use macrofiche_git::fastimport::Head;
use macrofiche_git::jail::Settings;
use macrofiche_git::linux::plumbing::unknown_object;
use macrofiche_git::parse::mode;
use macrofiche_git::{Change, Engine, GitError, Ident, Oid, RefRefusal, Repo, RunOpts, Source};

const GHOST: &str = "0123456789012345678901234567890123456789";

async fn setup() -> (Repo, Oid, PathBuf) {
    let root = macrofiche_testkit::scratch("conformance");
    std::fs::create_dir_all(root.join("scratch")).unwrap();
    std::fs::create_dir_all(root.join("r.git")).unwrap();
    let e = Engine::open(Settings::linux(macrofiche_testkit::git_prefix()), &root.join("scratch")).await.unwrap();
    let r = e.init(&root.join("r.git"), "main").await.unwrap();
    let a = Ident::new("A", "a@x").unwrap();
    let c = r
        .fast_import(&Head { tmp_ref: "refs/macrofiche/tmp/c", author: &a, committer: &a, time: 1, message: b"c", from: None }, &[Change::Upsert { path: "f", mode: mode::FILE, source: Source::Bytes(b"f") }])
        .await
        .unwrap();
    let tx = format!("create refs/heads/main {c}\n");
    assert!(r.run("setup", &["update-ref", "--stdin"], RunOpts { stdin: Some(tx.into_bytes()), ..RunOpts::read() }).await.unwrap().success());
    (r, c, root)
}

/// git's stderr for an `update-ref --stdin` transaction.
async fn refused(r: &Repo, tx: String) -> String {
    let out = r.run("update-ref", &["update-ref", "--stdin"], RunOpts { stdin: Some(tx.into_bytes()), ..RunOpts::read() }).await.unwrap();
    assert!(!out.success(), "refused");
    out.stderr
}

// Goal: "reference already exists" reads as Exists (a branch created
// twice: 409).
#[tokio::test(flavor = "multi_thread")]
async fn message_reference_already_exists() {
    let (r, c, _) = setup().await;
    let e = refused(&r, format!("create refs/heads/main {c}\n")).await;
    assert!(e.contains("reference already exists"), "{e}");
    assert_eq!(classify_update_ref(&e), Some(RefRefusal::Exists));
}

// Goal: "is at X but expected Y" reads as Moved, naming X (a stale CAS:
// 409 with the current tip).
#[tokio::test(flavor = "multi_thread")]
async fn message_is_at_but_expected() {
    let (r, c, _) = setup().await;
    let e = refused(&r, format!("update refs/heads/main {c} {GHOST}\n")).await;
    assert!(e.contains(&format!("is at {c} but expected {GHOST}")), "{e}");
    assert_eq!(classify_update_ref(&e), Some(RefRefusal::Moved { current: c }));
}

// Goal: "unable to resolve reference" reads as Missing (a CAS on a
// branch that does not exist: 409 or 404 by route).
#[tokio::test(flavor = "multi_thread")]
async fn message_unable_to_resolve_reference() {
    let (r, c, _) = setup().await;
    let e = refused(&r, format!("update refs/heads/live {c} {c}\n")).await;
    assert!(e.contains("unable to resolve reference"), "{e}");
    assert_eq!(classify_update_ref(&e), Some(RefRefusal::Missing));
}

// Goal: a held lock reads as Locked (a stale lock after a crash, which
// recovery clears at boot: 503 if one is ever met while serving).
#[tokio::test(flavor = "multi_thread")]
async fn message_lock_file_exists() {
    let (r, c, _) = setup().await;
    std::fs::write(r.dir().join("refs/heads/main.lock"), b"").unwrap();
    let e = refused(&r, format!("update refs/heads/main {c} {c}\n")).await;
    assert!(e.contains(".lock': File exists"), "{e}");
    assert_eq!(classify_update_ref(&e), Some(RefRefusal::Locked));
}

// Goal: a branch whose name is another's directory reads as NameConflict
// (400: the name cannot be stored beside the other).
#[tokio::test(flavor = "multi_thread")]
async fn message_exists_cannot_create() {
    let (r, c, _) = setup().await;
    let e = refused(&r, format!("create refs/heads/main/x {c}\n")).await;
    assert!(e.contains("exists; cannot create"), "{e}");
    assert_eq!(classify_update_ref(&e), Some(RefRefusal::NameConflict));
}

// Goal: a ref to no object reads as MissingObject (never a write
// macrofiche makes; a 500 if it ever did).
#[tokio::test(flavor = "multi_thread")]
async fn message_nonexistent_object() {
    let (r, c, _) = setup().await;
    let e = refused(&r, format!("update refs/heads/main {GHOST} {c}\n")).await;
    assert!(e.contains("with nonexistent object"), "{e}");
    assert_eq!(classify_update_ref(&e), Some(RefRefusal::MissingObject));
}

// Goal: an id that names no commit reads as unknown, in each call that
// takes one (a restore's base, a merge's source, a listing's ref: 404 or
// 412 by route).
#[tokio::test(flavor = "multi_thread")]
async fn message_unknown_objects() {
    let (r, c, _) = setup().await;
    let ghost = Oid::parse(GHOST).unwrap();
    let e = r.merge_base(ghost, c).await.unwrap_err();
    assert!(unknown_object(&e), "merge-base: {e}");
    let e = r.is_ancestor(ghost, c).await.unwrap_err();
    assert!(unknown_object(&e), "is-ancestor: {e}");
    let e = r.rev_list(&[GHOST], 10).await.unwrap_err();
    assert!(unknown_object(&e), "rev-list: {e}");
    let a = Ident::new("A", "a@x").unwrap();
    let e = r.commit_tree(ghost, &[c], b"m", &a, &a, 1).await.unwrap_err();
    assert!(unknown_object(&e), "commit-tree: {e}");
    let e = r.ls_tree(ghost).await.unwrap_err();
    assert!(unknown_object(&e), "ls-tree: {e}");
    assert!(!is_unknown_object("fatal: out of memory"));
}

// Goal: merge-tree's refusal of unrelated histories reads as such (400
// unless allow_unrelated_histories).
#[tokio::test(flavor = "multi_thread")]
async fn message_unrelated_histories() {
    let (r, c, _) = setup().await;
    let a = Ident::new("A", "a@x").unwrap();
    let o = r.fast_import(&Head { tmp_ref: "refs/macrofiche/tmp/o", author: &a, committer: &a, time: 1, message: b"o", from: None }, &[Change::Upsert { path: "o", mode: mode::FILE, source: Source::Bytes(b"o") }]).await.unwrap();
    let out = r.run("merge-tree", &["merge-tree", "--write-tree", "-z", &c.to_string(), &o.to_string()], RunOpts::read()).await.unwrap();
    assert!(is_unrelated(&out.stderr), "{out:?}");
}

// Goal: fast-import refuses what the contract's checks refuse first (a
// `.git` segment, an empty one) and a parent that does not exist: a
// failure, never a commit. Method: the stream with each fault.
#[tokio::test(flavor = "multi_thread")]
async fn fast_import_refusals() {
    let (r, _, root) = setup().await;
    let a = Ident::new("A", "a@x").unwrap();
    for (path, from, says) in [(".git/config", None, "invalid path"), ("a//b", None, "invalid path"), ("ok", Some(Oid::parse(GHOST).unwrap()), "not a valid commit")] {
        let e = r
            .fast_import(&Head { tmp_ref: "refs/macrofiche/tmp/x", author: &a, committer: &a, time: 1, message: b"x", from }, &[Change::Upsert { path, mode: mode::FILE, source: Source::Bytes(b"x") }])
            .await
            .unwrap_err();
        assert!(matches!(&e, GitError::Failed { stderr, .. } if stderr.contains(says)), "{path}: {e}");
    }
    // fast-import leaves a crash report beside the repository: what boot
    // recovery removes.
    let reports = std::fs::read_dir(root.join("r.git")).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with("fast_import_crash_")).count();
    assert_eq!(reports, 3);
}
