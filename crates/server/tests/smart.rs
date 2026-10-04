//! git's smart HTTP (docs/design.md, phase 5), proved with the real git
//! client: the system's git 2.55.0, which has curl (macrofiche's own
//! build has none, being a server). Clone, fetch and push with the REST
//! API's JWT; a push keeps the compare-and-swap, the last-commit index,
//! and the webhook.

mod common;

use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use common::*;
use macrofiche_testkit::{Receiver, Send};
use serde_json::json;

/// The client: the system's git, with nothing of the user's config.
const CLIENT: &str = "/usr/bin/git";

fn git(dir: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(CLIENT)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "agent")
        .env("GIT_AUTHOR_EMAIL", "agent@computer.fragment")
        .env("GIT_COMMITTER_NAME", "agent")
        .env("GIT_COMMITTER_EMAIL", "agent@computer.fragment")
        .env("LC_ALL", "C")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("the git client runs")
}

fn ok(o: &Output) -> String {
    assert!(o.status.success(), "git failed: {}\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// `http://t:<JWT>@host/<repo>.git`, as the service's remotes are.
fn remote(fx: &Fx, repo: &str, scopes: &[&str]) -> String {
    let token = fx.key.token(ORG, "editor:ab12", repo, scopes, 900);
    format!("http://t:{token}@{}/{repo}.git", fx.m.url.strip_prefix("http://").unwrap())
}

// Goal: the real git client clones what the REST API wrote, pushes back,
// and the push is one more write: the branch moves for the REST API, the
// pushed commits' last commits are git's, its webhook goes out, a stale
// push is refused by the compare-and-swap, a hidden ref cannot be
// pushed, and fsck stays clean. Method: clone (protocol v2 and v0),
// commit, push, read back over REST.
#[test]
fn clone_fetch_and_push_with_the_real_git() {
    let fx = Fx::start("smart");
    let repo = fx.create_repo("smart--paul");
    let c1 = fx.commit_at_head(&repo, &[up("a.txt", b"alpha\n"), up("dir/b.txt", b"beta\n")]);
    let hook = Receiver::start(vec![200]);
    let b = json!({ "url": hook.url, "secret": "s" }).to_string();
    assert_eq!(fx.call("PUT", &Fx::repo_path(&repo, "webhook"), &repo, &["repo:write"], Some(("application/json", b.as_bytes()))).status, 200);
    let home = macrofiche_testkit::scratch("smart-home");
    let url = remote(&fx, &repo, &["git:read", "git:write"]);

    // clone, protocol v2 (the client's default) and v0
    ok(&git(&home, &home, &["clone", "--quiet", &url, "w"]));
    let w = home.join("w");
    assert_eq!(std::fs::read(w.join("a.txt")).unwrap(), b"alpha\n");
    assert_eq!(ok(&git(&w, &home, &["rev-parse", "HEAD"])), c1);
    ok(&git(&home, &home, &["-c", "protocol.version=0", "clone", "--quiet", &url, "w0"]));
    assert_eq!(ok(&git(&home.join("w0"), &home, &["rev-parse", "HEAD"])), c1);

    // a commit pushed: the REST API sees it, with git's last commits
    std::fs::write(w.join("a.txt"), b"alpha 2\n").unwrap();
    std::fs::write(w.join("c.txt"), b"gamma\n").unwrap();
    ok(&git(&w, &home, &["add", "-A"]));
    ok(&git(&w, &home, &["commit", "--quiet", "-m", "from the agent"]));
    let pushed = ok(&git(&w, &home, &["rev-parse", "HEAD"]));
    ok(&git(&w, &home, &["push", "--quiet", "origin", "main"]));
    assert_eq!(fx.head(&repo, "main").as_deref(), Some(pushed.as_str()));
    let mut files = fx.list(&repo, "main", 1000);
    files.sort();
    assert_eq!(files, vec![("a.txt".into(), 8, pushed.clone()), ("c.txt".into(), 6, pushed.clone()), ("dir/b.txt".into(), 5, c1.clone())]);
    assert_eq!(fx.read(&repo, "c.txt", "main").unwrap(), b"gamma\n");
    let h = fx.history(&repo, "main", 10);
    assert_eq!((h[0]["sha"].as_str(), h[0]["author_name"].as_str(), h[0]["message"].as_str()), (Some(pushed.as_str()), Some("agent"), Some("from the agent\n")));
    let got = hook.wait_for(1, Duration::from_secs(5));
    assert_eq!(got.len(), 1, "the push is announced");
    let v: serde_json::Value = serde_json::from_slice(&got[0].body).unwrap();
    assert_eq!((v["ref"].as_str(), v["before"].as_str(), v["after"].as_str()), (Some("refs/heads/main"), Some(c1.as_str()), Some(pushed.as_str())));

    // a REST commit, then a fetch sees it
    let c3 = fx.commit_at_head(&repo, &[up("d.txt", b"delta\n")]);
    ok(&git(&w, &home, &["pull", "--quiet", "--ff-only", "origin", "main"]));
    assert_eq!(ok(&git(&w, &home, &["rev-parse", "HEAD"])), c3);

    // a stale push: the old clone's lease names main as it was; refused
    let w0 = home.join("w0");
    std::fs::write(w0.join("a.txt"), b"stale\n").unwrap();
    ok(&git(&w0, &home, &["commit", "--quiet", "-am", "stale"]));
    let stale = git(&w0, &home, &["push", "--porcelain", &format!("--force-with-lease=main:{c1}"), "origin", "main"]);
    assert!(!stale.status.success(), "a push on a stale lease is refused");
    assert_eq!(fx.head(&repo, "main").as_deref(), Some(c3.as_str()), "main unmoved");
    // a non-fast-forward without force: refused before it is sent
    assert!(!git(&w0, &home, &["push", "--quiet", "origin", "main"]).status.success());
    // a hidden ref: macrofiche's own namespace
    let hidden = git(&w, &home, &["push", "--porcelain", "origin", "HEAD:refs/macrofiche/tmp/evil"]);
    assert!(!hidden.status.success(), "{}", String::from_utf8_lossy(&hidden.stdout));
    // a new branch by push, then deleted by push: both announced
    ok(&git(&w, &home, &["push", "--quiet", "origin", "HEAD:refs/heads/topic"]));
    assert_eq!(fx.head(&repo, "topic").as_deref(), Some(c3.as_str()));
    ok(&git(&w, &home, &["push", "--quiet", "origin", ":refs/heads/topic"]));
    assert_eq!(fx.head(&repo, "topic"), None);
    // four deliveries: the push to main, the REST commit, the branch made
    // by push, and deleted by push
    let got = hook.wait_for(4, Duration::from_secs(5));
    let moves: Vec<(String, String)> = got
        .iter()
        .map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).unwrap())
        .map(|v| (v["ref"].as_str().unwrap().to_string(), v["after"].as_str().unwrap().to_string()))
        .collect();
    assert_eq!(moves.len(), 4, "{moves:?}");
    assert_eq!(moves[1], ("refs/heads/main".to_string(), c3.clone()), "the REST commit");
    assert_eq!(moves[2..], [("refs/heads/topic".to_string(), c3.clone()), ("refs/heads/topic".to_string(), "0".repeat(40))], "the branch made, then deleted");
    assert!(fx.fsck_all().iter().all(|(_, ok, e)| *ok || panic!("{e}")));
}

// Goal: smart HTTP has the REST API's auth: no credentials is a 401 with
// a Basic challenge (so git sends the URL's), a read token clones but
// cannot push (403), another repo's token finds nothing (404). Method:
// the client, and raw requests.
#[test]
fn smart_http_has_the_same_auth() {
    let fx = Fx::start("smart-auth");
    let repo = fx.create_repo("r");
    fx.commit_at_head(&repo, &[up("a", b"a")]);
    let other = fx.create_repo("other");
    let home = macrofiche_testkit::scratch("smart-auth-home");
    let a = fx.raw("GET", &format!("/{repo}.git/info/refs?service=git-upload-pack"), &[], Send::None);
    assert_eq!((a.status, a.header("www-authenticate")), (401, Some("Basic realm=\"macrofiche\"")));
    let bare = format!("{}/{repo}.git", fx.m.url);
    assert!(!git(&home, &home, &["ls-remote", &bare]).status.success(), "no credentials, no answer");
    let read = remote(&fx, &repo, &["git:read"]);
    ok(&git(&home, &home, &["clone", "--quiet", &read, "w"]));
    let w = home.join("w");
    std::fs::write(w.join("b"), b"b").unwrap();
    ok(&git(&w, &home, &["add", "-A"]));
    ok(&git(&w, &home, &["commit", "--quiet", "-m", "b"]));
    let push = git(&w, &home, &["push", "origin", "main"]);
    assert!(!push.status.success() && String::from_utf8_lossy(&push.stderr).contains("403"), "{}", String::from_utf8_lossy(&push.stderr));
    let wrong = remote(&fx, &other, &["git:read"]).replace(&format!("/{other}.git"), &format!("/{repo}.git"));
    let r = git(&home, &home, &["ls-remote", &wrong]);
    // git reads the 404 as "repository … not found"
    assert!(!r.status.success() && String::from_utf8_lossy(&r.stderr).contains("not found"), "{}", String::from_utf8_lossy(&r.stderr));
    let dumb = fx.call("GET", &format!("/{repo}.git/info/refs"), &repo, &["git:read"], None);
    assert_eq!(dumb.status, 403, "only smart HTTP");
}

/// A pkt-line.
fn pkt(s: &[u8]) -> Vec<u8> {
    let mut v = format!("{:04x}", s.len() + 4).into_bytes();
    v.extend_from_slice(s);
    v
}

/// An empty pack: the header and its SHA-1 trailer.
fn empty_pack() -> Vec<u8> {
    let mut p = b"PACK".to_vec();
    p.extend_from_slice(&2u32.to_be_bytes());
    p.extend_from_slice(&0u32.to_be_bytes());
    let d = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, &p);
    p.extend_from_slice(d.as_ref());
    p
}

// Goal: receive-pack's own transaction is the compare-and-swap of a push:
// a command whose old value is not the ref's (a REST commit landed after
// the client read the refs) is refused and moves nothing; with the right
// old value the same command lands. Method: the protocol by hand (the
// client checks its lease before sending, so it never shows the server's
// refusal).
#[test]
fn a_push_on_a_stale_ref_is_refused_by_the_server() {
    let fx = Fx::start("smart-cas");
    let repo = fx.create_repo("r");
    let c1 = fx.commit_at_head(&repo, &[up("a", b"a")]);
    let c2 = fx.commit_at_head(&repo, &[up("b", b"b")]);
    let auth = format!("Bearer {}", fx.token(&repo, &["git:write"]));
    let push = |old: &str, new: &str| {
        let mut body = pkt(format!("{old} {new} refs/heads/main\0report-status\n").as_bytes());
        body.extend_from_slice(b"0000");
        body.extend_from_slice(&empty_pack());
        fx.raw("POST", &format!("/{repo}.git/git-receive-pack"), &[("authorization", &auth), ("content-type", "application/x-git-receive-pack-request")], Send::Bytes(&body))
    };
    // main is at c2; a push that read it at c1 and moves it back to c1
    let a = push(&c1, &c1);
    assert_eq!(a.status, 200);
    let report = a.text();
    assert!(report.contains("ng refs/heads/main"), "{report}");
    assert_eq!(fx.head(&repo, "main").as_deref(), Some(c2.as_str()), "nothing moved");
    // the same move from the right old value lands (a force push back)
    let a = push(&c2, &c1);
    assert!(a.text().contains("ok refs/heads/main"), "{}", a.text());
    assert_eq!(fx.head(&repo, "main").as_deref(), Some(c1.as_str()));
    assert_eq!(fx.raw("POST", &format!("/{repo}.git/git-receive-pack"), &[("authorization", &auth), ("content-type", "text/plain")], Send::Bytes(b"0000")).status, 415);
}
