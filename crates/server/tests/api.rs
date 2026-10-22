//! Every route of the contract (docs/contract.md, sections 5 and 15),
//! driven over HTTP against the real binary as fragment's clients drive
//! it: each one valid, invalid, and replayed.

mod common;

use common::*;
use macrofiche_testkit::{blob_sha, Send};
use serde_json::json;

// Goal: 5.1 to 5.3 as the cell's `ensure_repo` uses them: create answers
// an id, the id answers the url form (the repo's name: question 1), a
// name made again is 409 and found by paging the org's list (newest
// first, 100 a page, past 150 newer repos: e2e `create`), a bad cursor
// is 400. Method: the cell's calls.
#[test]
fn repositories_create_list_and_resolve() {
    let fx = Fx::start("api-repos");
    let a = fx.create("todo--paul");
    assert_eq!(a.status, 200, "{a}");
    let v = a.json();
    let id = v["repo_id"].as_str().unwrap();
    assert_eq!((v["repo_name"].as_str(), v["http_url"].as_str(), v["message"].as_str()), (Some("todo--paul"), Some("todo--paul"), Some("repository created")));
    let u = fx.call("GET", &format!("/api/repo-urls/{id}"), id, &["org:read"], None);
    assert_eq!((u.status, u.json()["url"].as_str()), (200, Some("todo--paul")));
    assert_eq!(fx.call("GET", "/api/repo-urls/nope", "nope", &["org:read"], None).status, 404);
    // replayed: 409, a problem body with a code and a title
    let again = fx.create("todo--paul");
    assert_eq!(again.status, 409);
    assert!(again.header("content-type").unwrap().starts_with("application/problem+json"));
    assert!(again.json()["code"].is_string() && again.json()["title"].is_string());
    // 150 newer repos, then the old one found by paging, as the cell does
    for i in 0..150 {
        assert_eq!(fx.create(&format!("filler-{i:03}")).status, 200);
    }
    let mut cursor: Option<String> = None;
    let mut found = None;
    let mut pages = 0;
    for _ in 0..100 {
        let q = match &cursor {
            None => "limit=100".to_string(),
            Some(c) => format!("limit=100&cursor={}", enc(c)),
        };
        let a = fx.call("GET", &format!("/api/repos?{q}"), ORG, &["org:read"], None);
        assert_eq!(a.status, 200, "{a}");
        pages += 1;
        let v = a.json();
        assert!(v["repos"].as_array().unwrap().len() <= 100);
        if let Some(r) = v["repos"].as_array().unwrap().iter().find(|r| r["repo_name"] == "todo--paul") {
            found = r["url"].as_str().map(str::to_string);
            break;
        }
        match (v["has_more"].as_bool(), v["next_cursor"].as_str()) {
            (Some(true), Some(c)) if !c.is_empty() => cursor = Some(c.to_string()),
            _ => break,
        }
    }
    assert_eq!((found.as_deref(), pages), (Some("todo--paul"), 2), "found on the second page");
    let first = fx.call("GET", "/api/repos", ORG, &["org:read"], None).json();
    assert_eq!(first["repos"].as_array().unwrap().len(), 20, "20 by default");
    assert_eq!(first["repos"][0]["repo_name"], "filler-149", "newest first");
    assert_eq!(fx.call("GET", "/api/repos?cursor=page-2", ORG, &["org:read"], None).status, 400);
    // invalid: a name that is no name, a repo_name unlike the claim
    let body = json!({ "repo_name": "other", "default_branch": "main" }).to_string();
    let a = fx.call("POST", "/api/repos", "mine", &["repo:write"], Some(("application/json", body.as_bytes())));
    assert_eq!((a.status, a.json()["code"].as_str()), (400, Some("repo_name_mismatch")));
    let body = json!({ "repo_name": "a b" }).to_string();
    assert_eq!(fx.call("POST", "/api/repos", "a b", &["repo:write"], Some(("application/json", body.as_bytes()))).status, 400);
}

// Goal: 5.13 as the cell's `delete_repo` calls it (a wiped person's
// fragments): 200 once, then the repo answers 404 on every route, leaves
// the org's list, its id resolves no more, a delete again is 409
// `repository_deleted` (what the cell reads as done), and its name is
// never made again; refused, deleting nothing, with git's scopes or
// another repo's token; all of it across a kill -9. Method: the cell's
// calls.
#[test]
fn a_repo_is_deleted() {
    let fx = Fx::start("api-delete");
    let made = fx.create("gone");
    let id = made.json()["repo_id"].as_str().unwrap().to_string();
    let repo = fx.call("GET", &format!("/api/repo-urls/{id}"), &id, &["org:read"], None).json()["url"].as_str().unwrap().to_string();
    let tip = fx.commit_at_head(&repo, &[up("a.txt", b"a")]);
    fx.create_repo("kept");
    let delete = |fx: &Fx, claim: &str, scopes: &[&str]| fx.call("DELETE", &format!("/api/repos/{}", enc(&repo)), claim, scopes, None);
    // invalid: git's scopes, another repo's token
    assert_eq!(delete(&fx, &repo, &["git:write"]).status, 403);
    assert_eq!(delete(&fx, "kept", &["repo:write"]).status, 404);
    assert_eq!(fx.head(&repo, "main").as_deref(), Some(tip.as_str()), "a refused delete deletes nothing");
    let a = delete(&fx, &repo, &["repo:write"]);
    assert_eq!(a.status, 200, "{a}");
    assert_eq!((a.json()["repo_name"].as_str(), a.json()["repo_id"].as_str()), (Some("gone"), Some(id.as_str())));
    let gone = |fx: &Fx| {
        let b = fx.get(&repo, "branch", "name=main");
        let f = fx.file(&repo, "a.txt", &tip);
        let c = fx.commit(&repo, Some(&tip), &[up("b.txt", b"b")]);
        let u = fx.call("GET", &format!("/api/repo-urls/{id}"), &id, &["org:read"], None);
        let listed = fx.call("GET", "/api/repos?limit=100", ORG, &["org:read"], None).json();
        let names: Vec<String> = listed["repos"].as_array().unwrap().iter().map(|r| r["repo_name"].as_str().unwrap().to_string()).collect();
        assert_eq!((b.status, b.json()["code"].as_str()), (404, Some("repository_not_found")));
        assert_eq!((f.status, c.status, u.status), (404, 404, 404));
        assert_eq!(names, ["kept"], "it leaves the list");
        // replayed: done, as the cell reads it; its name is never made again
        let again = delete(fx, &repo, &["repo:write"]);
        assert_eq!((again.status, again.json()["code"].as_str()), (409, Some("repository_deleted")));
        let remade = fx.create("gone");
        assert_eq!((remade.status, remade.json()["code"].as_str()), (409, Some("repository_deleted")));
        assert_eq!(fx.head("kept", "main"), None, "another repo serves on");
    };
    gone(&fx);
    assert_eq!(fx.call("DELETE", "/api/repos/never", "never", &["repo:write"], None).status, 404, "an unknown repo");
    assert_eq!(fx.call("GET", "/api/repos/kept", "kept", &["repo:write"], None).status, 404, "a repo's other methods are no route");
    let fx = fx.restart();
    gone(&fx);
}

// Goal: the auth floor answers with the service's statuses (contract
// section 3's table), on every kind of route. Method: one request per
// fault.
#[test]
fn auth_answers_as_the_service() {
    let fx = Fx::start("api-auth");
    let repo = fx.create_repo("r");
    fx.create_repo("s");
    let path = format!("{}?name=main", Fx::repo_path(&repo, "branch"));
    let status = |headers: &[(&str, &str)]| fx.raw("GET", &path, headers, Send::None);
    let a = status(&[]);
    assert_eq!((a.status, a.json()["code"].as_str()), (401, Some("unauthorized")));
    let a = status(&[("authorization", "Bearer garbage")]);
    assert_eq!((a.status, a.json()["detail"].as_str()), (403, Some("Invalid or expired token")));
    let other = macrofiche_testkit::Key::generate().token(ORG, "x", &repo, &["git:read"], 300);
    assert_eq!(status(&[("authorization", &format!("Bearer {other}"))]).status, 403, "another key");
    let expired = fx.key.sign(r#"{"alg":"ES256","typ":"JWT"}"#, &json!({"iss": ORG, "sub": "x", "repo": repo, "scopes": ["git:read"], "iat": 1, "exp": 2}));
    assert_eq!(status(&[("authorization", &format!("Bearer {expired}"))]).status, 403, "expired");
    let other_org = fx.key.token("another-org", "x", &repo, &["git:read"], 300);
    assert_eq!(status(&[("authorization", &format!("Bearer {other_org}"))]).status, 403, "another org");
    let a = fx.call("GET", &path, &repo, &["git:write"], None);
    assert_eq!((a.status, a.json()["code"].as_str()), (403, Some("insufficient_scope")), "no scope implies another");
    let a = fx.call("GET", &path, "", &["git:read"], None);
    assert_eq!((a.status, a.json()["code"].as_str()), (400, Some("repo_claim_missing")));
    let a = fx.call("GET", &path, "s", &["git:read"], None);
    assert_eq!((a.status, a.json()["code"].as_str()), (404, Some("repository_not_found")), "a token for another repo");
    assert_eq!(fx.call("GET", &format!("{}?name=main", Fx::repo_path("nope", "branch")), "nope", &["git:read"], None).status, 404, "no such repo");
    assert_eq!(fx.call("GET", "/api/repos", ORG, &["git:read"], None).status, 403);
    assert_eq!(fx.raw("GET", "/healthz", &[], Send::None).status, 200, "health needs no token");
}

// Goal: 5.4 and 5.8 as the clients use them: a fresh repo's branch is 404
// `branch not found` (e2e `files` asserts it exactly), a first commit
// with no expectation creates main, then each commit names the head it
// read; a stale or replayed expectation is 409 with nothing applied.
// Method: the CLI's commit triple (success, replay-same-parent,
// conflicting-parent), over HTTP.
#[test]
fn branches_and_commit_packs() {
    let fx = Fx::start("api-commits");
    let repo = fx.create_repo("r");
    let a = fx.get(&repo, "branch", "name=main");
    assert_eq!((a.status, a.json()["detail"].as_str()), (404, Some("branch not found")));
    let a = fx.commit(&repo, None, &[up("base.txt", b"base")]);
    assert_eq!(a.status, 201, "{a}");
    let v = a.json();
    let head0 = committed(&v).unwrap();
    assert_eq!((v["result"]["old_sha"].as_str(), v["result"]["status"].as_str(), v["commit"]["blob_count"].as_u64()), (Some("0000000000000000000000000000000000000000"), Some("ok"), Some(1)));
    assert!(is_sha(v["commit"]["tree_sha"].as_str().unwrap()) && v["commit"]["tree_sha"] != v["commit"]["commit_sha"], "a real tree");
    let b = fx.get(&repo, "branch", "name=main").json();
    assert_eq!(b["branch"]["head_sha"], head0.as_str());
    assert!(b["branch"]["created_at"].as_str().unwrap().ends_with('Z'));
    // success, replay-same-parent, conflicting-parent
    let tip1 = committed(&fx.commit(&repo, Some(&head0), &[up("a.txt", b"A")]).json()).unwrap();
    let replay = fx.commit(&repo, Some(&head0), &[up("a.txt", b"A")]);
    assert_eq!(replay.status, 409);
    let r = replay.json();
    assert_eq!((r["result"]["status"].as_str(), r["result"]["old_sha"].as_str(), r["result"]["success"].as_bool()), (Some("precondition_failed"), Some(tip1.as_str()), Some(false)));
    assert_eq!(fx.head(&repo, "main").as_deref(), Some(tip1.as_str()), "replay committed nothing");
    let tip2 = committed(&fx.commit(&repo, Some(&tip1), &[up("b.txt", b"B")]).json()).unwrap();
    assert_eq!(fx.commit(&repo, Some(&tip1), &[up("a.txt", b"A2")]).status, 409);
    let tip3 = committed(&fx.commit(&repo, Some(&tip2), &[up("a.txt", b"A2")]).json()).unwrap();
    assert_eq!(fx.read(&repo, "a.txt", "main").unwrap(), b"A2");
    assert_eq!(fx.read(&repo, "b.txt", &tip3).unwrap(), b"B");
    // "must not exist" on a branch that does
    assert_eq!(fx.commit(&repo, Some(""), &[up("x", b"x")]).status, 409);
    // the service SDKs' expected_head_sha
    let line = json!({"metadata": {"target_branch": "main", "expected_head_sha": tip3, "files": [{"path": "sdk.txt", "operation": "upsert", "content_id": "c0"}]}});
    let body = format!("{line}\n{}\n", json!({"blob_chunk": {"content_id": "c0", "data": macrofiche_testkit::b64(b"sdk"), "eof": true}}));
    assert_eq!(fx.pack(&repo, body.as_bytes()).status, 201);
    // an invalid branch name is 400
    assert_eq!(fx.get(&repo, "branch", "name=a..b").status, 400);
}

// Goal: each refusal of a commit pack (contract 5.8's table) answers the
// service's status and shape, and lands nothing. Method: one pack per
// fault, main's head checked after each.
#[test]
fn commit_pack_refusals() {
    let fx = Fx::start("api-refusals");
    let repo = fx.create_repo("r");
    let head = fx.commit_at_head(&repo, &[up("a", b"a")]);
    let unmoved = |what: &str| assert_eq!(fx.head(&repo, "main").as_deref(), Some(head.as_str()), "{what} moved main");
    let meta = |files: serde_json::Value| json!({"metadata": {"target_branch": "main", "expected_target_sha": head, "files": files}}).to_string();
    let chunk = |id: &str, data: &[u8], eof: bool| json!({"blob_chunk": {"content_id": id, "data": macrofiche_testkit::b64(data), "eof": eof}}).to_string();
    let a = fx.pack(&repo, b"");
    assert_eq!((a.status, a.header("content-type").map(|c| c.starts_with("application/problem+json"))), (400, Some(true)));
    assert_eq!(fx.pack(&repo, b"not json\n").status, 400);
    let a = fx.pack(&repo, format!("{}\n", chunk("c0", b"x", true)).as_bytes());
    assert_eq!((a.status, a.json()["result"]["message"].as_str(), a.json()["result"]["status"].as_str()), (400, Some("first payload must be metadata"), Some("invalid")));
    let a = fx.pack(&repo, format!("{}\n", meta(json!([{"path": "b", "operation": "upsert"}]))).as_bytes());
    assert_eq!((a.status, a.json()["result"]["message"].as_str()), (400, Some("missing content_id for b")));
    let delete = json!([{"path": "a", "operation": "delete", "content_id": "c0"}]);
    let a = fx.pack(&repo, format!("{}\n{}\n", meta(delete), chunk("c0", b"", true)).as_bytes());
    assert_eq!((a.status, a.json()["result"]["status"].as_str()), (400, Some("invalid")), "a chunk for a delete");
    unmoved("a delete's chunk");
    let up1 = json!([{"path": "b", "operation": "upsert", "content_id": "c0"}]);
    let a = fx.pack(&repo, format!("{}\n{}\n", meta(up1.clone()), chunk("c0", b"x", false)).as_bytes());
    assert_eq!((a.status, a.json()["result"]["message"].as_str()), (400, Some("incomplete content stream for b")));
    let big = vec![7u8; CHUNK_MAX + 1];
    let a = fx.pack(&repo, format!("{}\n{}\n", meta(up1.clone()), chunk("c0", &big, true)).as_bytes());
    assert_eq!((a.status, a.json()["result"]["status"].as_str()), (413, Some("payload_too_large")));
    for (files, what) in [(json!([{"path": ".git/config", "operation": "upsert", "content_id": "c0"}]), ".git"), (json!([{"path": "../x", "operation": "upsert", "content_id": "c0"}]), "..")] {
        let a = fx.pack(&repo, format!("{}\n{}\n", meta(files), chunk("c0", b"x", true)).as_bytes());
        assert_eq!(a.status, 400, "{what}: {a}");
    }
    let bad_author = json!({"metadata": {"target_branch": "main", "expected_target_sha": head, "author": {"name": "a<b", "email": "x"}, "files": up1}}).to_string();
    assert_eq!(fx.pack(&repo, format!("{bad_author}\n{}\n", chunk("c0", b"x", true)).as_bytes()).status, 400);
    unmoved("every refusal");
    assert!(fx.fsck_all().iter().all(|(_, ok, _)| *ok));
}

// Goal: 5.5 to 5.7: a listing names each file once across its pages,
// recursively, with git's last commits (a second commit moves only what
// it changed), and `commits` gives each last commit's metadata; GET file
// streams the bytes with their identity headers; HEAD names the same
// with no body; an absent path or ref is 404; an etag replayed is 304.
// Method: the e2e codestore probe's own pack (interleaved chunks, an
// empty file), then each read.
#[test]
fn listings_and_files() {
    let fx = Fx::start("api-files");
    let repo = fx.create_repo("r");
    let lines = [
        json!({"metadata": {"target_branch": "main", "commit_message": "probe", "author": {"name": "e2e", "email": "e2e@users.fragment"}, "files": [
            {"path": "a.txt", "operation": "upsert", "content_id": "c0", "mode": "100644"}, {"path": "dir/b.txt", "operation": "upsert", "content_id": "c1", "mode": "100644"},
            {"path": "dir/sub/c.txt", "operation": "upsert", "content_id": "c2", "mode": "100644"}]}}),
        json!({"blob_chunk": {"content_id": "c0", "data": macrofiche_testkit::b64(b"al"), "eof": false}}),
        json!({"blob_chunk": {"content_id": "c1", "data": macrofiche_testkit::b64(b"beta"), "eof": true}}),
        json!({"blob_chunk": {"content_id": "c0", "data": macrofiche_testkit::b64(b"pha"), "eof": true}}),
        json!({"blob_chunk": {"content_id": "c2", "data": "", "eof": true}}),
    ];
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    let c1 = committed(&fx.pack(&repo, body.as_bytes()).json()).unwrap();
    assert_eq!(fx.read(&repo, "a.txt", &c1).unwrap(), b"alpha");
    let h = fx.call("HEAD", &format!("{}?path=dir%2Fb.txt&ref={c1}", Fx::repo_path(&repo, "file")), &repo, &["git:read"], None);
    let beta = blob_sha(b"beta");
    assert_eq!((h.status, h.header("content-length"), h.header("x-blob-sha"), h.header("x-last-commit-sha")), (200, Some("4"), Some(beta.as_str()), Some(c1.as_str())));
    assert!(h.body.is_empty());
    let g = fx.file(&repo, "dir/b.txt", &c1);
    assert_eq!((g.status, g.body.as_slice(), g.header("etag")), (200, &b"beta"[..], Some(format!("\"{beta}\"").as_str())));
    assert_eq!(g.header("content-type"), Some("application/octet-stream"));
    assert!(g.header("last-modified").unwrap().ends_with("GMT"));
    let etag = format!("\"{beta}\"");
    let auth = format!("Bearer {}", fx.token(&repo, &["git:read"]));
    let cached = fx.raw("GET", &format!("{}?path=dir%2Fb.txt&ref={c1}", Fx::repo_path(&repo, "file")), &[("authorization", &auth), ("if-none-match", &etag)], Send::None);
    assert_eq!((cached.status, cached.body.len()), (304, 0));
    assert_eq!(fx.file(&repo, "nope.txt", &c1).status, 404);
    assert_eq!(fx.file(&repo, "a.txt", "0123456789abcdef0123456789abcdef01234567").status, 404);
    assert_eq!(fx.file(&repo, "dir", &c1).status, 404, "a directory is no file");
    assert_eq!(fx.file(&repo, "../x", &c1).status, 400);
    let empty = fx.file(&repo, "dir/sub/c.txt", &c1);
    assert_eq!((empty.status, empty.header("content-length"), empty.body.len()), (200, Some("0"), 0));
    // a second commit: last commits move only for what it changed
    let c2 = committed(&fx.commit(&repo, Some(&c1), &[up("a.txt", b"alpha 2")]).json()).unwrap();
    let mut got = fx.list(&repo, &c2, 1);
    got.sort();
    assert_eq!(got, vec![("a.txt".to_string(), 7, c2.clone()), ("dir/b.txt".to_string(), 4, c1.clone()), ("dir/sub/c.txt".to_string(), 0, c1.clone())]);
    assert_eq!(fx.list(&repo, &c2, 1000).len(), 3);
    let page = fx.get(&repo, "files/metadata", &format!("ref={c2}")).json();
    assert_eq!((page["has_more"].as_bool(), page["ref"].as_str()), (Some(false), Some(c2.as_str())));
    let commits = page["commits"].as_object().unwrap();
    assert_eq!(commits.len(), 2, "each last commit once");
    assert_eq!((commits[&c2]["order"].as_u64(), commits[&c1]["order"].as_u64()), (Some(0), Some(1)));
    assert_eq!(commits[&c1]["message"], "probe");
    assert_eq!(page["files"][0]["type"], "blob");
    assert_eq!(page["files"][0]["mode"], "100644");
    // a subtree, a bad cursor, the default branch
    let sub = fx.get(&repo, "files/metadata", "ref=main&path=dir").json();
    assert_eq!(sub["files"].as_array().unwrap().len(), 2);
    assert_eq!(fx.get(&repo, "files/metadata", "ref=main&cursor=nope").status, 400);
    assert_eq!(fx.get(&repo, "files/metadata", "").json()["files"].as_array().unwrap().len(), 3, "no ref: the default branch");
    assert_eq!(fx.get(&repo, "files/metadata", "ref=nope").status, 404);
}

// Goal: 5.9, 5.10, 5.11, 5.12 and 15.1, as a deploy and a rollback use
// them: live made at main's tip (once), a preview ephemeral and hidden
// from plain reads, a merge that fast-forwards, a stale one refused, a
// restore with the service's 412s, and history newest first. Method: the
// e2e codestore probe's sequence.
#[test]
fn deploys_previews_merges_restores_history() {
    let fx = Fx::start("api-deploy");
    let repo = fx.create_repo("r");
    let c1 = fx.commit_at_head(&repo, &[up("a.txt", b"alpha")]);
    let c2 = fx.commit_at_head(&repo, &[up("a.txt", b"alpha 2")]);
    let a = fx.create_branch(&repo, &c1, "live", false);
    assert_eq!((a.status, a.json()["commit_sha"].as_str(), a.json()["message"].as_str()), (200, Some(c1.as_str()), Some("branch created")));
    let again = fx.create_branch(&repo, &c1, "live", false);
    assert_eq!(again.status, 409, "one creator wins");
    assert_eq!(fx.create_branch(&repo, "0123456789abcdef0123456789abcdef01234567", "other", false).status, 404);
    // a preview: ephemeral, at main's tip, invisible to a plain read
    let p = fx.create_branch(&repo, &c2, "preview/0123456789ab", true);
    assert_eq!((p.status, p.json()["target_is_ephemeral"].as_bool()), (200, Some(true)));
    assert_eq!(fx.head(&repo, "preview/0123456789ab"), None);
    let e = fx.get(&repo, "branch", "name=preview%2F0123456789ab&ephemeral=true").json();
    assert_eq!(e["branch"]["head_sha"], c2.as_str());
    // the merge preview, then the merge
    let pv = fx.preview(&repo).json();
    assert_eq!((pv["result"].as_str(), pv["status"].as_str(), pv["target_tip_sha"].as_str(), pv["source_tip_sha"].as_str(), pv["merge_base_sha"].as_str()), (Some("fast_forward"), Some("clean"), Some(c1.as_str()), Some(c2.as_str()), Some(c1.as_str())));
    let stale = fx.merge_live(&repo, &c2, "deploy");
    assert_eq!((stale.status, stale.json()["conflict_type"].as_str()), (409, Some("target_moved")));
    let m = fx.merge_live(&repo, &c1, "deploy");
    assert_eq!(m.status, 200, "{m}");
    let mv = m.json();
    assert_eq!((mv["target"]["new_sha"].as_str(), mv["result"].as_str(), mv["promoted_commits"].as_u64(), mv["target"]["old_sha"].as_str()), (Some(c2.as_str()), Some("fast_forward"), Some(1), Some(c1.as_str())));
    let replayed = fx.merge_live(&repo, &c2, "deploy again");
    assert_eq!((replayed.status, replayed.json()["result"].as_str()), (200, Some("no_op")), "a merge replayed is a no_op");
    // restores: a commit live never held, its own tip: 412; to c1: a commit
    let c3 = fx.commit_at_head(&repo, &[up("d.txt", b"d")]);
    let r = fx.restore_live(&repo, &c3, &c2, "rollback");
    assert_eq!((r.status, r.json()["result"]["status"].as_str()), (412, Some("precondition_failed")));
    assert_eq!(fx.restore_live(&repo, &c2, &c2, "to the tip").status, 412);
    assert_eq!(fx.restore_live(&repo, "f".repeat(40).as_str(), &c2, "unknown").status, 412, "the CLI's rollback to a sha no history holds");
    assert_eq!(fx.restore_live(&repo, &c1, &c1, "stale").status, 409);
    let r = fx.restore_live(&repo, &c1, &c2, &format!("rollback r to {c1}"));
    assert_eq!(r.status, 201, "{r}");
    let restored = committed(&r.json()).unwrap();
    assert_eq!(fx.read(&repo, "a.txt", "live").unwrap(), b"alpha");
    assert_eq!(fx.shas(&repo, "live", 30)[..2], [restored.clone(), c2.clone()]);
    assert_eq!(fx.history(&repo, "live", 30)[0]["parent_shas"], json!([c2]), "one parent: the old tip");
    assert_eq!(fx.restore_live(&repo, &c1, &restored, "no change").status, 412, "the restore would change nothing");
    assert_eq!(fx.restore_live(&repo, &restored, &restored, "to the tip").status, 412);
    // history: newest first, the tip first, paged with a cursor
    assert_eq!(fx.shas(&repo, "main", 30), [c3.clone(), c2.clone(), c1.clone()]);
    let h = fx.history(&repo, "main", 30);
    assert_eq!((h[0]["author_name"].as_str(), h[0]["message"].as_str()), (Some("fragment/1a2b3c4d"), Some("a commit")));
    let p1 = fx.get(&repo, "commits", "ref=main&limit=2").json();
    assert_eq!((p1["commits"].as_array().unwrap().len(), p1["has_more"].as_bool()), (2, Some(true)));
    let p2 = fx.get(&repo, "commits", &format!("ref=main&limit=2&cursor={}", enc(p1["next_cursor"].as_str().unwrap()))).json();
    assert_eq!((p2["commits"][0]["sha"].as_str(), p2["has_more"].as_bool()), (Some(c1.as_str()), Some(false)));
    assert_eq!(fx.get(&repo, "commits", "ref=nope").status, 404);
    // the preview's refusals
    assert_eq!(fx.get(&repo, "merge/preview", "source_branch=main&target_branch=nope").status, 404);
    assert_eq!(fx.get(&repo, "merge/preview", "source_branch=main").status, 400);
    // merge's: no strategy, a bad sha, an unknown source
    assert_eq!(fx.post_json(&repo, "merge", &json!({"target_branch": "live", "source_ref": "main"})).status, 400);
    assert_eq!(fx.post_json(&repo, "merge", &json!({"target_branch": "live", "source_ref": "main", "strategy": "ff_prefer", "expected_target_sha": "main"})).status, 400);
    assert_eq!(fx.post_json(&repo, "merge", &json!({"target_branch": "live", "source_ref": "nope", "strategy": "ff_prefer"})).status, 404);
    assert!(fx.fsck_all().iter().all(|(_, ok, _)| *ok));
}

// Goal: macrofiche's extension route registers a repository's webhook
// (contract question 2's option, the coordinator's ask), refuses a bad
// one, and deletes it. Method: PUT, DELETE, and their refusals.
#[test]
fn a_repositorys_webhook_is_registered() {
    let fx = Fx::start("api-hooks");
    let repo = fx.create_repo("r");
    let put = |body: serde_json::Value, scopes: &[&str]| {
        let b = body.to_string();
        fx.call("PUT", &Fx::repo_path(&repo, "webhook"), &repo, scopes, Some(("application/json", b.as_bytes())))
    };
    assert_eq!(put(json!({"url": "http://127.0.0.1:9/hook", "secret": "s3cret"}), &["git:write"]).status, 403, "repo:write");
    assert_eq!(put(json!({"url": "ftp://x", "secret": "s"}), &["repo:write"]).status, 400);
    assert_eq!(put(json!({"url": "http://127.0.0.1:9/hook", "secret": ""}), &["repo:write"]).status, 400);
    let a = put(json!({"url": "http://127.0.0.1:9/hook", "secret": "s3cret"}), &["repo:write"]);
    assert_eq!((a.status, a.json()["registered"].as_bool()), (200, Some(true)));
    assert_eq!(put(json!({"url": "http://127.0.0.1:9/hook", "secret": "again"}), &["repo:write"]).status, 200, "replayed: replaced");
    let del = |url: &str| fx.call("DELETE", &format!("{}?url={}", Fx::repo_path(&repo, "webhook"), enc(url)), &repo, &["repo:write"], None);
    assert_eq!(del("http://127.0.0.1:9/hook").status, 200);
    assert_eq!(del("http://127.0.0.1:9/hook").status, 404, "deleted once");
}
