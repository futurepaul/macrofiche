//! The push webhook (docs/contract.md, section 6): one delivery per moved
//! branch, none for an ephemeral ref or a move that moves nothing, signed
//! `X-Pierre-Signature` as the cell verifies it, signed again on every
//! retry, retried until a 2xx or five attempts.

mod common;

use std::time::Duration;

use common::*;
use macrofiche_testkit::{Received, Receiver};
use serde_json::json;

const SECRET: &str = "0123456789abcdef0123456789abcdef";

/// The cell's check (`fragment_core::webhook`): `t=<t>,sha256=<hex>` over
/// `<t>.<body>`, and `t` within 300 s of now.
fn verifies(r: &Received, secret: &[u8]) -> Option<i64> {
    let sig = r.header("x-pierre-signature")?;
    let (t, mac) = sig.split_once(',')?;
    let t: i64 = t.strip_prefix("t=")?.parse().ok()?;
    let mac = mac.strip_prefix("sha256=")?;
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    let mut msg = format!("{t}.").into_bytes();
    msg.extend_from_slice(&r.body);
    let want: String = ring::hmac::sign(&key, &msg).as_ref().iter().map(|b| format!("{b:02x}")).collect();
    (want == mac && (macrofiche_testkit::now_s() - t).abs() <= 300).then_some(t)
}

// Goal: a commit is announced to the org's webhook, which answers 500
// twice and then 200: three attempts, each signed again (the cell's check
// passes on every one), one body naming the move; a repository's own
// registered hook gets its own delivery; an ephemeral branch and a merge
// that moves nothing are not announced; a branch made is, from zeros.
// Method: receivers scripted per attempt.
#[test]
fn deliveries_are_signed_retried_and_one_per_move() {
    let flaky = Receiver::start(vec![500, 500, 200]);
    let dir = macrofiche_testkit::scratch("webhook");
    let secret_file = dir.join("hook.secret");
    std::fs::write(&secret_file, format!("{SECRET}\n")).unwrap();
    let key = macrofiche_testkit::Key::generate();
    let srv_dir = macrofiche_testkit::scratch("webhook-server");
    let mut cfg = config(&srv_dir, &key, json!({}));
    cfg["orgs"][0]["webhooks"] = json!([{ "url": flaky.url, "secret_file": secret_file }]);
    let m = macrofiche_testkit::Macrofiche::start(&bin(), &srv_dir, cfg);
    let fx = Fx { m, key, dir: srv_dir };

    let repo = fx.create_repo("hooked");
    let own = Receiver::start(vec![200]);
    let b = json!({ "url": own.url, "secret": "per-repo-secret" }).to_string();
    assert_eq!(fx.call("PUT", &Fx::repo_path(&repo, "webhook"), &repo, &["repo:write"], Some(("application/json", b.as_bytes()))).status, 200);

    let tip = fx.commit_at_head(&repo, &[up("a.txt", b"a")]);
    let got = flaky.wait_for(3, Duration::from_secs(20));
    assert_eq!(got.len(), 3, "two refusals, then the delivery");
    let ts: Vec<i64> = got.iter().map(|r| verifies(r, SECRET.as_bytes()).expect("each attempt verifies")).collect();
    assert!(ts.windows(2).all(|w| w[1] > w[0]), "each attempt signed again, later: {ts:?}");
    let gap = got[2].at.duration_since(got[1].at);
    assert!(gap >= Duration::from_millis(1900), "the backoff grows: {gap:?}");
    for r in &got {
        assert_eq!((r.header("x-pierre-event"), r.header("content-type"), r.header("user-agent")), (Some("push"), Some("application/json"), Some("Pierre-Webhook/1.0")));
        assert_eq!(r.body, got[0].body, "one body, re-signed");
    }
    let v: serde_json::Value = serde_json::from_slice(&got[0].body).unwrap();
    assert_eq!((v["ref"].as_str(), v["before"].as_str(), v["after"].as_str()), (Some("refs/heads/main"), Some("0000000000000000000000000000000000000000"), Some(tip.as_str())));
    assert_eq!((v["repository"]["url"].as_str(), v["repository"]["repo_name"].as_str(), v["org"].as_str(), v["type"].as_str()), (Some("hooked"), Some("hooked"), Some(ORG), Some("push")));
    assert!(v["pushed_at"].as_str().unwrap().ends_with('Z'));
    let mine = own.wait_for(1, Duration::from_secs(5));
    assert_eq!(mine.len(), 1);
    assert!(verifies(&mine[0], b"per-repo-secret").is_some(), "the repository's hook signs with its own secret");

    // an ephemeral branch is not announced; live is, from zeros
    assert_eq!(fx.create_branch(&repo, &tip, "preview/0123456789ab", true).status, 200);
    assert_eq!(fx.create_branch(&repo, &tip, "live", false).status, 200);
    let mine = own.wait_for(2, Duration::from_secs(5));
    assert_eq!(mine.len(), 2);
    let v: serde_json::Value = serde_json::from_slice(&mine[1].body).unwrap();
    assert_eq!((v["ref"].as_str(), v["after"].as_str()), (Some("refs/heads/live"), Some(tip.as_str())));
    // a merge that moves nothing is not announced
    assert_eq!(fx.merge_live(&repo, &tip, "deploy").json()["result"], "no_op");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(own.received().len(), 2, "no delivery for a no_op");
}

// Goal: a target that never answers 2xx gets five attempts, no more.
// Method: a receiver that always answers 503.
#[test]
fn a_failing_target_gets_five_attempts() {
    let down = Receiver::start(vec![503]);
    let fx = Fx::start("webhook-down");
    let repo = fx.create_repo("r");
    let b = json!({ "url": down.url, "secret": "s" }).to_string();
    assert_eq!(fx.call("PUT", &Fx::repo_path(&repo, "webhook"), &repo, &["repo:write"], Some(("application/json", b.as_bytes()))).status, 200);
    fx.commit_at_head(&repo, &[up("a", b"a")]);
    let got = down.wait_for(6, Duration::from_secs(25));
    assert_eq!(got.len(), 5, "1 + 1 + 2 + 4 + 8 s, then it gives up");
}
