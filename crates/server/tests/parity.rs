//! fragment's own tests of code.storage, run against macrofiche where they
//! describe the contract rather than the fake: the fake's
//! (`crates/fakes/src/codestorage.rs`), the CLI's
//! (`cli/src/codestorage.rs`), and PR #127's deploy after a rollback,
//! each ported with its goal, the fake's levers and statuses left out
//! (the service's statuses in their place: contract section 3).

mod common;

use common::*;
use serde_json::{json, Value};

// The fake's `jwt_scopes_and_repo_claims`, with the service's statuses:
// a token for another repo is 404 (the fake: 403), garbage is 403 (401).
#[test]
fn fake_jwt_scopes_and_repo_claims() {
    let fx = Fx::start("parity-jwt");
    let r = fx.create_repo("r");
    let s = fx.create_repo("s");
    fx.commit_at_head(&r, &[up("a", b"1")]);
    let good = fx.call("GET", &format!("{}?name=main", Fx::repo_path(&r, "branch")), &r, &["git:read"], None);
    assert_eq!(good.status, 200, "{good}");
    let other = fx.call("GET", &format!("{}?name=main", Fx::repo_path(&s, "branch")), &r, &["git:read"], None);
    assert_eq!(other.status, 404, "another repo's token");
    let garbage = fx.raw("GET", &format!("{}?name=main", Fx::repo_path(&r, "branch")), &[("authorization", "Bearer garbage")], macrofiche_testkit::Send::None);
    assert_eq!(garbage.status, 403);
    let write_only = fx.call("GET", &format!("{}?ref=main", Fx::repo_path(&r, "files/metadata")), &r, &["git:write"], None);
    assert_eq!(write_only.status, 403, "missing git:read");
}

// The fake's `a_pack_that_changes_nothing_is_refused` (fragment PR #163):
// a pack that would change nothing is refused as the service refuses it
// (412, "no changes to commit"), never an empty commit.
#[test]
fn fake_a_pack_that_changes_nothing_is_refused() {
    let fx = Fx::start("parity-unchanged");
    let r = fx.create_repo("r");
    let head = fx.commit_at_head(&r, &[up("a", b"1")]);
    let post = |changes: &[Change<'_>]| fx.commit(&r, Some(&head), changes).status;
    assert_eq!(post(&[up("a", b"1")]), 412, "the same bytes");
    assert_eq!(post(&[Change::Delete { path: "nope" }]), 412, "a file that is not there");
    assert_eq!(fx.head(&r, "main").as_deref(), Some(head.as_str()), "no commit was made");
    assert_eq!(post(&[up("a", b"1"), up("b", b"2")]), 201, "one file changed");
    assert_ne!(fx.head(&r, "main").as_deref(), Some(head.as_str()));
}

// The CLI's `commit_triple_success_replay_conflict`.
#[test]
fn cli_commit_triple_success_replay_conflict() {
    let fx = Fx::start("parity-triple");
    let t = fx.create_repo("t");
    let head0 = fx.commit_at_head(&t, &[up("base.txt", b"base")]);
    let tip1 = committed(&fx.commit(&t, Some(&head0), &[up("a.txt", b"A")]).json()).unwrap();
    assert_ne!(tip1, head0);
    assert_eq!(fx.read(&t, "base.txt", "main").unwrap(), b"base");
    assert_eq!(fx.commit(&t, Some(&head0), &[up("a.txt", b"A")]).status, 409, "replay-same-parent");
    assert_eq!(fx.head(&t, "main").unwrap(), tip1, "replay committed nothing");
    let tip2 = committed(&fx.commit(&t, Some(&tip1), &[up("b.txt", b"B")]).json()).unwrap();
    assert_eq!(fx.commit(&t, Some(&tip1), &[up("a.txt", b"A2")]).status, 409, "conflicting-parent");
    assert_eq!(fx.head(&t, "main").unwrap(), tip2);
    let tip3 = committed(&fx.commit(&t, Some(&tip2), &[up("a.txt", b"A2")]).json()).unwrap();
    assert_eq!(fx.read(&t, "a.txt", "main").unwrap(), b"A2");
    assert_eq!(fx.read(&t, "b.txt", "main").unwrap(), b"B");
    assert_ne!(tip3, tip2);
}

// The CLI's `a_commit_whose_answer_is_lost_is_not_resent`, without the
// fake's lever: the client sends a whole pack and hangs up before its
// answer; the commit is on the branch when it reads again (contract
// section 9).
#[test]
fn cli_a_commit_whose_answer_is_lost_has_landed() {
    use std::io::Write;
    let fx = Fx::start("parity-lost");
    let t = fx.create_repo("t");
    let head0 = fx.commit_at_head(&t, &[up("base.txt", b"base")]);
    let body = commit_pack("main", Some(&head0), "one", ("fragment/deadbeef", "deadbeef@users.fragment"), &[up("a.txt", b"A")]);
    let auth = format!("Bearer {}", fx.token(&t, &["git:write"]));
    let host = fx.m.url.strip_prefix("http://").unwrap();
    let mut s = std::net::TcpStream::connect(host).unwrap();
    write!(s, "POST {} HTTP/1.1\r\nhost: {host}\r\nauthorization: {auth}\r\ncontent-type: application/x-ndjson\r\ncontent-length: {}\r\n\r\n", Fx::repo_path(&t, "commit-pack"), body.len()).unwrap();
    s.write_all(&body).unwrap();
    s.flush().unwrap();
    drop(s);
    // Bounded: 100 reads of 50 ms.
    let mut landed = None;
    for _ in 0..100 {
        let h = fx.head(&t, "main").unwrap();
        if h != head0 {
            landed = Some(h);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(landed.is_some(), "the commit landed");
    assert_eq!(fx.read(&t, "a.txt", "main").unwrap(), b"A");
}

// The CLI's `commit_deletes_and_oversized_chunking`: a file bigger than
// one 4 MiB chunk, and a delete, in one pack.
#[test]
fn cli_commit_deletes_and_oversized_chunking() {
    let fx = Fx::start("parity-chunking");
    let t = fx.create_repo("t");
    let head = fx.commit_at_head(&t, &[up("small.txt", b"s")]);
    let big = vec![7u8; CHUNK_MAX + 1024];
    let a = fx.commit(&t, Some(&head), &[up("huge.bin", &big), Change::Delete { path: "small.txt" }]);
    assert_eq!(a.status, 201, "{a}");
    assert_eq!(fx.read(&t, "huge.bin", "main").unwrap().len(), CHUNK_MAX + 1024);
    assert_eq!(fx.read(&t, "small.txt", "main"), None, "deleted remotely");
}

// The CLI's `list_files_paginates` (the mock pages by 2; here the client
// asks for 2), and every entry names its commit.
#[test]
fn cli_list_files_paginates() {
    let fx = Fx::start("parity-paginates");
    let t = fx.create_repo("t");
    let files: Vec<(String, Vec<u8>)> = (0..5).map(|i| (format!("f{i}.txt"), vec![7u8; i + 1])).collect();
    let changes: Vec<Change<'_>> = files.iter().map(|(p, b)| up(p, b)).collect();
    fx.commit_at_head(&t, &changes);
    let listed = fx.list(&t, "main", 2);
    let mut paths: Vec<&str> = listed.iter().map(|f| f.0.as_str()).collect();
    paths.sort();
    assert_eq!(paths, vec!["f0.txt", "f1.txt", "f2.txt", "f3.txt", "f4.txt"]);
    assert_eq!(listed.iter().find(|f| f.0 == "f3.txt").unwrap().1, 4);
    assert!(listed.iter().all(|f| is_sha(&f.2)), "every entry names its commit");
}

// The CLI's `first_commit_creates_branch_without_expected`.
#[test]
fn cli_first_commit_creates_branch_without_expected() {
    let fx = Fx::start("parity-first");
    let t = fx.create_repo("fresh");
    assert_eq!(fx.head(&t, "main"), None);
    let tip = committed(&fx.commit(&t, None, &[up("root.txt", b"r")]).json()).unwrap();
    assert_eq!(fx.head(&t, "main").unwrap(), tip);
    assert_eq!(fx.read(&t, "root.txt", "main").unwrap(), b"r");
}

// The CLI's `promote_with_stale_expected_live_is_cas_rejected` (its
// `merge_live`, PR #127).
#[test]
fn cli_promote_with_stale_expected_live_is_cas_rejected() {
    let fx = Fx::start("parity-stale-live");
    let t = fx.create_repo("t");
    let tip1 = fx.commit_at_head(&t, &[up("v1.txt", b"1")]);
    assert_eq!(fx.create_branch(&t, &tip1, "live", false).status, 200);
    let tip2 = fx.commit_at_head(&t, &[up("v2.txt", b"2")]);
    assert_eq!(fx.merge_live(&t, &tip1, "first deploy").status, 200);
    let a = fx.merge_live(&t, &tip1, "second deploy");
    assert_eq!(a.status, 409);
    assert_ne!(a.json()["conflict_type"], "merge_conflict", "a moved live, not a conflict: the clients retry it");
    assert_eq!(fx.head(&t, "live").unwrap(), tip2);
}

/// `fragment_core::codestorage::Promotion` (PR #127), ported.
#[derive(Debug, PartialEq, Eq)]
enum Promotion {
    Current { live: String },
    FastForward { live: String },
    Restore { live: String, main: String },
    RestoreThenMerge { live: String, base: String },
}

/// `fragment_core::codestorage::promotion`.
fn promotion(v: &Value) -> Option<Promotion> {
    let sha = |k: &str| v[k].as_str().filter(|s| is_sha(s)).map(str::to_string);
    let (live, main) = (sha("target_tip_sha")?, sha("source_tip_sha")?);
    match v["result"].as_str()? {
        _ if live == main => Some(Promotion::Current { live }),
        "fast_forward" => Some(Promotion::FastForward { live }),
        "no_op" => Some(Promotion::Restore { live, main }),
        "merge_commit" => Some(Promotion::RestoreThenMerge { live, base: sha("merge_base_sha")? }),
        _ => None,
    }
}

/// `fragment_core::codestorage::merge_conflicted`.
fn merge_conflicted(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body).is_ok_and(|v| v["conflict_type"] == "merge_conflict" || v["code"] == "merge_conflict")
}

fn back_to_base_message(deploy: &str, base: &str) -> String {
    format!("{deploy} (first, live back to {}: the last of main it holds)", &base[..12.min(base.len())])
}

/// The CLI's `promote_live` after PR #127: preview, then its steps, each
/// guarded by the live the preview read. `Err(status)` for a refusal.
fn promote_live(fx: &Fx, t: &str, message: &str) -> Result<String, u16> {
    let p = fx.preview(t);
    assert_eq!(p.status, 200, "{p}");
    let merge = |expected: &str| {
        let a = fx.merge_live(t, expected, message);
        match a.status {
            200 => Ok(a.json()["target"]["new_sha"].as_str().unwrap().to_string()),
            409 if merge_conflicted(&a.body) => Err(4090),
            s => Err(s),
        }
    };
    match promotion(&p.json()).expect("a preview that names a deploy") {
        Promotion::Current { live } => Ok(live),
        Promotion::FastForward { live } => merge(&live),
        Promotion::Restore { live, main } => {
            let a = fx.restore_live(t, &main, &live, message);
            match a.status {
                201 => Ok(committed(&a.json()).unwrap()),
                412 if fx.head(t, "live").as_deref() == Some(live.as_str()) => Ok(live),
                s => Err(s),
            }
        }
        Promotion::RestoreThenMerge { live, base } => {
            let a = fx.restore_live(t, &base, &live, &back_to_base_message(message, &base));
            let live = match a.status {
                201 => committed(&a.json()).unwrap(),
                412 => live,
                s => return Err(s),
            };
            merge(&live)
        }
    }
}

// PR #127's `a_deploy_after_a_rollback_serves_main`: macrofiche merges as
// the service does (three ways), so after a rollback a merge alone keeps
// the reverts, conflicts where main changed a reverted file, and a deploy
// that follows the preview serves main every time. Method: the PR's test,
// step for step, over HTTP.
#[test]
fn pr127_a_deploy_after_a_rollback_serves_main() {
    let fx = Fx::start("parity-pr127");
    let t = fx.create_repo("t");
    let v1 = fx.commit_at_head(&t, &[up("index.html", b"v1")]);
    assert_eq!(fx.create_branch(&t, &v1, "live", false).status, 200);
    let serves_main = || {
        let main: Vec<(String, u64, String)> = fx.list(&t, "main", 1000);
        let live: Vec<(String, u64, String)> = fx.list(&t, "live", 1000);
        let paths = |l: &[(String, u64, String)]| l.iter().map(|e| e.0.clone()).collect::<Vec<_>>();
        paths(&main) == paths(&live) && main.iter().all(|e| fx.read(&t, &e.0, "live") == fx.read(&t, &e.0, "main"))
    };
    let rollback = || {
        let live = fx.head(&t, "live").unwrap();
        let a = fx.restore_live(&t, &v1, &live, "rollback");
        assert_eq!(a.status, 201, "{a}");
        assert_eq!(fx.read(&t, "index.html", "live").unwrap(), b"v1", "rolled back");
    };
    let edit = |path: &str, bytes: &[u8]| {
        fx.commit_at_head(&t, &[up(path, bytes)]);
    };

    edit("index.html", b"v2");
    promote_live(&fx, &t, "deploy v2").unwrap();
    assert!(serves_main(), "a fast-forward");

    rollback();
    promote_live(&fx, &t, "deploy, main unchanged").unwrap();
    assert!(serves_main(), "main unchanged since the rollback: a restore of main's tip");

    rollback();
    edit("new.txt", b"new");
    promote_live(&fx, &t, "deploy, main moved on").unwrap();
    assert!(serves_main(), "main moved on: live back to the merge base, then the merge");

    rollback();
    edit("other.txt", b"other");
    let live = fx.head(&t, "live").unwrap();
    assert_eq!(fx.merge_live(&t, &live, "a merge alone").status, 200);
    assert_eq!(fx.read(&t, "index.html", "live").unwrap(), b"v1", "a merge alone keeps what the rollback reverted");
    promote_live(&fx, &t, "deploy after the merge").unwrap();
    assert!(serves_main(), "and a deploy then serves main");

    rollback();
    edit("index.html", b"v3");
    let live = fx.head(&t, "live").unwrap();
    let a = fx.merge_live(&t, &live, "a merge alone");
    assert!(a.status == 409 && merge_conflicted(&a.body), "a conflict is a merge_conflict, not a moved live: {a}");
    promote_live(&fx, &t, "deploy, a reverted file changed").unwrap();
    assert!(serves_main(), "main changed the file the rollback reverted");

    edit("index.html", b"v4");
    let before = fx.head(&t, "live");
    promote_live(&fx, &t, "deploy after a deploy after a rollback").unwrap();
    assert!(serves_main() && fx.head(&t, "live") != before, "the deploy after (its restore a no-op)");
    assert_eq!(fx.read(&t, "index.html", "live").unwrap(), b"v4");
    // PR #127 reads live's history by first parents, as the fake walks it:
    // [the last deploy, the one before, that one's restore]. macrofiche
    // orders by commit date, as the service does (contract question 4),
    // so main's commits merged into live sort among them by their dates;
    // what holds either way: the newest is the last deploy, that deploy
    // was one commit (its restore changed nothing), and the deploy before
    // took two, a restore and then the merge.
    let history: Vec<String> = fx.history(&t, "live", 100).iter().map(|c| c["message"].as_str().unwrap().to_string()).collect();
    assert_eq!(history[0], "deploy after a deploy after a rollback", "{history:?}");
    assert_eq!(history.iter().filter(|m| m.starts_with("deploy after a deploy after a rollback")).count(), 1, "one commit: its restore changed nothing");
    assert!(history.iter().any(|m| m == "deploy, a reverted file changed"), "{history:?}");
    assert!(history.iter().any(|m| m.starts_with("deploy, a reverted file changed (first, live back to ")), "the deploy before took two: {history:?}");
    assert!(fx.fsck_all().iter().all(|(_, ok, _)| *ok));
}

// The CLI's `promote_and_rollback_flow`, after PR #127: the first deploy
// makes live, the next fast-forwards, a deploy again moves nowhere, a
// preview is ephemeral at main's tip, a rollback restores, a bad target is
// 412, and history starts at the restore.
#[test]
fn cli_promote_and_rollback_flow() {
    let fx = Fx::start("parity-flow");
    let t = fx.create_repo("t");
    let tip1 = fx.commit_at_head(&t, &[up("v1.txt", b"1")]);
    let live1 = fx.create_branch(&t, &tip1, "live", false).json()["commit_sha"].as_str().unwrap().to_string();
    assert_eq!(live1, tip1);
    let tip2 = fx.commit_at_head(&t, &[up("v2.txt", b"2")]);
    assert_eq!(promote_live(&fx, &t, "deploy v2").unwrap(), tip2);
    assert_eq!(promote_live(&fx, &t, "deploy again").unwrap(), tip2, "live at main's tip moves nowhere");
    let p = fx.create_branch(&t, &tip2, "preview/abc123", true);
    assert_eq!(p.json()["commit_sha"], json!(tip2));
    let restored = committed(&fx.restore_live(&t, &tip1, &tip2, "rollback").json()).unwrap();
    assert_ne!(restored, tip2);
    assert_eq!(fx.head(&t, "live").unwrap(), restored);
    assert_eq!(fx.restore_live(&t, &"f".repeat(40), &restored, "bad").status, 412);
    assert_eq!(fx.shas(&t, "live", 10)[0], restored);
}
