//! Imports (docs/design.md, Import): a repository made elsewhere, from a
//! fast-import stream, keeping the url form and the commit ids its
//! clients hold, and what an import refuses.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use macrofiche_contract::names::Expected;
use macrofiche_git::jail::Settings;
use macrofiche_git::{Oid, RunOpts};
use macrofiche_pack::{FileOp, Metadata, Op};
use macrofiche_store::repo::{CommitOutcome, MergeOutcome, MergeRequest, PackChange, PackFile, Strategy};
use macrofiche_store::{Import, Store, StoreError};

const ORG: &str = "fragment-dev";
/// The url form fragment's code.storage fake names a repository by.
const URL: &str = "0f3c2a1e-5b6d-4e7f-8a9b-0c1d2e3f4a5b";
/// Two commit ids the fake made (not git's).
const OLD_ONE: &str = "1111111111111111111111111111111111111111";
const OLD_TWO: &str = "2222222222222222222222222222222222222222";

async fn open(root: &Path) -> Result<(Arc<Store>, macrofiche_store::Recovery), StoreError> {
    Store::open(root.join("state"), Settings::linux(macrofiche_testkit::git_prefix())).await
}

/// A history as fragment's carry writes one: two blobs, two commits each
/// on its legacy id's ref, main at the second and live at the first.
fn stream(extra: &str, done: bool) -> String {
    let mut s = String::new();
    s.push_str("blob\nmark :1\ndata 5\nalpha\n");
    s.push_str("blob\nmark :2\ndata 4\nbeta\n");
    s.push_str(&format!("commit refs/macrofiche/legacy/{OLD_ONE}\nmark :3\nauthor e2e <e2e@users.fragment> 1000 +0000\ncommitter e2e <e2e@users.fragment> 1000 +0000\ndata 3\none\nM 100644 :1 a.txt\n\n"));
    s.push_str(&format!("commit refs/macrofiche/legacy/{OLD_TWO}\nmark :4\nauthor e2e <e2e@users.fragment> 2000 +0000\ncommitter e2e <e2e@users.fragment> 2000 +0000\ndata 3\ntwo\nfrom :3\nM 100644 :2 a.txt\nM 100644 :1 b/c.txt\n\n"));
    s.push_str("reset refs/heads/main\nfrom :4\n\nreset refs/heads/live\nfrom :3\n\n");
    s.push_str(extra);
    if done {
        s.push_str("done\n");
    }
    s
}

fn write_stream(root: &Path, name: &str, body: &str) -> PathBuf {
    let p = root.join(format!("{name}.fi"));
    std::fs::write(&p, body).unwrap();
    p
}

fn req<'a>(name: &'a str, url: Option<&'a str>, id: Option<&'a str>, stream: &'a Path) -> Import<'a> {
    Import { org: ORG, name, url, repo_id: id, default_branch: "main", created_at_ms: 1_000, stream }
}

/// One upsert, as a commit pack writes it, expecting `expected`.
async fn pack(r: &macrofiche_store::RepoHandle, root: &Path, expected: Expected, path: &str, bytes: &[u8]) -> CommitOutcome {
    let spool = root.join(format!("spool-{path}"));
    std::fs::create_dir_all(&spool).unwrap();
    let s = spool.join("0");
    std::fs::write(&s, bytes).unwrap();
    let meta = Metadata {
        target_branch: "main".into(),
        expected,
        message: format!("write {path}"),
        author: None,
        committer: None,
        files: vec![FileOp { path: path.into(), op: Op::Upsert { content_id: "c0".into(), mode: 0o100644 } }],
        base_branch: None,
        ephemeral: false,
        base_is_ephemeral: false,
    };
    let files = [PackFile { path: path.into(), change: PackChange::Upsert { mode: 0o100644, spool: s, len: bytes.len() as u64 } }];
    r.commit_pack(&meta, &files, None, None, 3_000).await.unwrap().0
}

// Goal: an imported repository answers its clients as the store they
// knew did: by its old url form (not its name), at its old commit ids
// (a read at one, a write compare-and-swapped on one, a merge expecting
// one), with git's last commits for its files; and it keeps all of that
// across a restart. Method: a stream of two commits imported under a
// UUID url form and an id of its own, read and written through the store,
// then the state opened again.
#[tokio::test]
async fn an_import_answers_its_old_url_form_and_commit_ids() {
    let root = macrofiche_testkit::scratch("import");
    let (store, _) = open(&root).await.unwrap();
    let s = write_stream(&root, "todo", &stream("", true));
    let made = store.import_repo(&req("todo--abc", Some(URL), Some("repo_0123abcd"), &s)).await.unwrap();
    assert_eq!((made.meta.url(), made.meta.repo_id.as_str(), made.meta.created_at_ms, made.legacy), (URL, "repo_0123abcd", 1_000, 2));
    assert_eq!(made.branches.iter().map(|(b, _)| b.as_str()).collect::<Vec<_>>(), ["refs/heads/live", "refs/heads/main"]);
    assert!(matches!(store.repo(ORG, "todo--abc").await, Err(StoreError::NotFound { .. })), "its clients call it by its url form, not its name");
    assert_eq!(store.repo_by_id(ORG, "repo_0123abcd").map(|m| m.repo_name), Some("todo--abc".into()));
    let r = store.repo(ORG, URL).await.unwrap();

    let main = r.branch("main", false).await.unwrap().unwrap();
    let live = r.branch("live", false).await.unwrap().unwrap();
    assert_eq!(r.resolve(OLD_TWO, false).await.unwrap(), Some(main), "an old id resolves to the commit it names");
    assert_eq!(r.resolve(OLD_ONE, false).await.unwrap(), Some(live));
    assert_eq!(r.resolve("3333333333333333333333333333333333333333", false).await.unwrap(), None, "an id no one holds is none");
    let f = r.file(main, "a.txt").await.unwrap().unwrap();
    assert_eq!((f.size, f.last_commit), (4, main), "last commits are git's, its index rebuilt");
    assert_eq!(r.file(main, "b/c.txt").await.unwrap().unwrap().last_commit, main);
    let listing = r.listing(live).await.unwrap();
    assert_eq!(listing.entries.iter().map(|e| (e.path.as_str(), e.last_commit)).collect::<Vec<_>>(), [("a.txt", live)]);

    // a write expecting main at its old id lands; one expecting live's does not
    let stale = pack(&r, &root, Expected::At(OLD_ONE.into()), "stale.txt", b"x").await;
    assert!(matches!(stale, CommitOutcome::Moved { current: Some(c) } if c == main), "a write on live's old id is refused, main moved past it");
    let landed = match pack(&r, &root, Expected::At(OLD_TWO.into()), "new.txt", b"new").await {
        CommitOutcome::Committed(c) => c.new,
        CommitOutcome::Moved { current } => panic!("a write on main's old id: moved, main at {current:?}"),
        CommitOutcome::Unchanged { .. } => panic!("a write on main's old id: unchanged"),
    };
    let r_info = r.commit_info(landed).await.unwrap();
    assert_eq!(r_info.parents, [main]);
    // a merge into live expecting live's old id
    let merge = MergeRequest {
        target: "live".into(),
        target_ephemeral: false,
        source: "main".into(),
        source_ephemeral: false,
        strategy: Strategy::FfPrefer,
        expected: Oid::parse(OLD_ONE),
        squash: false,
        allow_unrelated: false,
        message: "deploy".into(),
        author: None,
        committer: None,
    };
    let (merged, _) = r.merge(merge, 4_000).await.unwrap();
    assert!(matches!(&merged, MergeOutcome::Done(d) if d.new == landed), "a merge expecting live's old id moves live to main");

    // a restart: the url form, the id and the old ids hold
    drop(r);
    drop(store);
    let (store, _) = open(&root).await.unwrap();
    let r = store.repo(ORG, URL).await.unwrap();
    assert_eq!(r.resolve(OLD_TWO, false).await.unwrap(), Some(main));
    assert_eq!(r.branch("live", false).await.unwrap(), Some(landed));
    // and the old ids stay out of what git clients are shown
    let refs = r.git.run("for-each-ref", &["for-each-ref", "--format=%(refname)"], RunOpts::read()).await.unwrap();
    assert!(String::from_utf8_lossy(&refs.stdout).contains("refs/macrofiche/legacy/"), "kept as refs");
    let config = std::fs::read_to_string(r.dir.join("config")).unwrap();
    assert!(config.contains("hideRefs = refs/macrofiche"), "hidden from transfer");
}

// Goal: an import takes branches and legacy ids alone, whole or not at
// all, under no name, url form or id another repository holds. Method:
// a stream writing a tag, one writing a legacy id that is no sha, one
// cut before `done`, and collisions; after each, nothing is left.
#[tokio::test]
async fn an_import_refuses_what_it_does_not_take() {
    let root = macrofiche_testkit::scratch("import-refused");
    let (store, _) = open(&root).await.unwrap();
    let bad = [
        ("tag", stream("reset refs/tags/v1\nfrom :4\n\n", true)),
        ("short", stream("reset refs/macrofiche/legacy/not-a-sha\nfrom :4\n\n", true)),
        ("other", stream("reset refs/notes/x\nfrom :4\n\n", true)),
        ("cut", stream("", false)),
    ];
    for (name, body) in &bad {
        let s = write_stream(&root, name, body);
        let e = store.import_repo(&req(name, Some(&format!("{URL}-{name}")), None, &s)).await.unwrap_err();
        assert!(matches!(e, StoreError::Invalid { code: "invalid_stream", .. }), "{name}: {e:?}");
        assert!(store.repo_meta(ORG, name).is_none() && store.repo_meta(ORG, &format!("{URL}-{name}")).is_none(), "{name}: nothing made");
    }
    let tmp = std::fs::read_dir(root.join("state/tmp")).unwrap().count();
    assert_eq!(tmp, 0, "no half-made repository is left");

    let good = write_stream(&root, "good", &stream("", true));
    store.import_repo(&req("todo--abc", Some(URL), Some("repo_1"), &good)).await.unwrap();
    store.create_repo(ORG, "plain", "main", 5).await.unwrap();
    for (name, url, id) in [("todo--abc", None, None), ("again", Some(URL), None), ("again", Some("plain"), None), ("again", None, Some("repo_1")), ("again", Some("again"), None)] {
        let e = store.import_repo(&req(name, url, id, &good)).await.unwrap_err();
        assert!(matches!(e, StoreError::Conflict { .. }), "{name} {url:?} {id:?}: {e:?}");
    }
    let e = store.create_repo(ORG, URL, "main", 6).await.unwrap_err();
    assert!(matches!(e, StoreError::Conflict { .. }), "a create may not take an imported url form: {e:?}");
    for (url, id) in [(Some("a b"), None), (None, Some("x/y"))] {
        let e = store.import_repo(&req("valid", url, id, &good)).await.unwrap_err();
        assert!(matches!(e, StoreError::Invalid { .. }), "{url:?} {id:?}: {e:?}");
    }
}

// Goal: a legacy id is an alias only for an id that is no commit here: a
// stream that names one of its own commits' real ids as another's legacy
// id is refused, and a write expecting a real commit that is not the tip
// is refused (409) whatever legacy ref names it. Method: a stream aliasing
// the first commit's real id to the second, then a write expecting the
// first while main holds the second, through a stream that only aliases.
#[tokio::test]
async fn a_real_commit_id_is_never_an_alias() {
    let root = macrofiche_testkit::scratch("import-alias");
    let (store, _) = open(&root).await.unwrap();
    // the first commit's real id, as git makes it from the stream
    let clean = write_stream(&root, "clean", &stream("", true));
    let made = store.import_repo(&req("clean", Some("clean-url"), None, &clean)).await.unwrap();
    let r = store.repo(ORG, "clean-url").await.unwrap();
    let first = r.branch("live", false).await.unwrap().unwrap();
    let main = r.branch("main", false).await.unwrap().unwrap();
    assert_eq!(made.legacy, 2);
    // a stream that names that real id as the second commit's legacy id
    let aliased = write_stream(&root, "aliased", &stream(&format!("reset refs/macrofiche/legacy/{first}\nfrom :4\n\n"), true));
    let e = store.import_repo(&req("aliased", Some("aliased-url"), None, &aliased)).await.unwrap_err();
    assert!(matches!(e, StoreError::Invalid { code: "invalid_stream", .. }), "an alias that is a real commit's id: {e:?}");
    assert!(store.repo_meta(ORG, "aliased-url").is_none(), "nothing made");
    // and a write expecting a real commit that is not the tip is a 409,
    // even with a legacy ref of that name (written past the import)
    r.git.update_refs(&[macrofiche_git::RefOp::Create { name: format!("refs/macrofiche/legacy/{first}"), new: main }]).await.unwrap().unwrap();
    let stale = pack(&r, &root, Expected::At(first.to_string()), "stale.txt", b"x").await;
    assert!(matches!(stale, CommitOutcome::Moved { current: Some(c) } if c == main), "a write on a real, older commit is refused");
}

// Goal: an import writes no ref outside its allowlist, a root ref
// (`ORIG_HEAD`, which a `refs/` listing never shows) and `shallow` (which
// hides history from every walk, and which fsck passes) included. Method:
// a stream that also resets each.
#[tokio::test]
async fn an_import_refuses_a_root_ref() {
    let root = macrofiche_testkit::scratch("import-root-ref");
    let (store, _) = open(&root).await.unwrap();
    for (name, extra) in [("orig", "reset ORIG_HEAD\nfrom :4\n\n"), ("shallow", "reset shallow\nfrom :4\n\n")] {
        let s = write_stream(&root, name, &stream(extra, true));
        let e = store.import_repo(&req(name, Some(&format!("{name}-url")), None, &s)).await.unwrap_err();
        assert!(matches!(e, StoreError::Invalid { code: "invalid_stream", .. }), "{name}: {e:?}");
        assert!(store.repo_meta(ORG, &format!("{name}-url")).is_none(), "{name}: nothing made");
    }
    assert_eq!(std::fs::read_dir(root.join("state/tmp")).unwrap().count(), 0, "no half-made repository is left");
}

