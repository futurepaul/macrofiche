//! kill -9 in the middle of writes, then a restart (docs/contract.md,
//! sections 9 and 13; docs/library.md, finding 7): every repository passes
//! `git fsck --full --strict`, every commit macrofiche acknowledged is on
//! its branch, leftovers are cleared at boot, and the next write lands.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use macrofiche_testkit::{request, Key, Macrofiche, Send, Sink};

const ROUNDS: usize = 12;
const WRITERS: usize = 4;

/// A writer's loop until the server dies: each commit a 256 KiB file
/// (fast-import is mid-pack often when the kill lands) and a counter,
/// each acknowledged sha kept.
fn writer(url: String, token: String, repo: String, w: usize, acked: Arc<Mutex<Vec<String>>>, stop: Arc<AtomicBool>) {
    let mut n = 0u64;
    let big: Vec<u8> = (0..256 * 1024).map(|i: u32| (i.wrapping_mul(2_654_435_761) >> 13) as u8 ^ w as u8).collect();
    let auth = format!("Bearer {token}");
    // Bounded by the server's death: every call fails once it is gone.
    while !stop.load(Ordering::Relaxed) {
        let branch = request(&url, "GET", &format!("{}?name=main", Fx::repo_path(&repo, "branch")), &[("authorization", &auth)], Send::None, Sink::Keep);
        let Ok(b) = branch else { return };
        let head = b.json()["branch"]["head_sha"].as_str().map(str::to_string);
        n += 1;
        let counter = format!("{w} {n}");
        let path = format!("w{w}/big.bin");
        let body = commit_pack("main", head.as_deref(), "crash round", ("fragment/1a2b3c4d", "1a2b3c4d@users.fragment"), &[up(&path, &big), up("counter.txt", counter.as_bytes())]);
        let a = request(&url, "POST", &Fx::repo_path(&repo, "commit-pack"), &[("authorization", &auth), ("content-type", "application/x-ndjson")], Send::Bytes(&body), Sink::Keep);
        match a {
            Ok(a) if a.status == 201 => acked.lock().unwrap().push(committed(&a.json()).unwrap()),
            Ok(a) if a.status == 409 => {}
            Ok(a) => panic!("writer {w}: {a}"),
            Err(_) => return,
        }
    }
}

// Goal: a crash at any point of a write leaves nothing torn and loses
// nothing acknowledged. Method: twelve rounds of four writers racing on
// one repository, the server killed with SIGKILL after 150 to 700 ms,
// restarted on the same state; after each restart fsck --strict, every
// acknowledged commit in main's history, and a new commit landing.
#[test]
fn kill_nine_mid_write_then_restart() {
    let fx = Fx::start("crash");
    let repo = fx.create_repo("crash");
    fx.commit_at_head(&repo, &[up("seed.txt", b"seed")]);
    let token = fx.key.token(ORG, "fragment-runtime", &repo, &["git:read", "git:write"], 3000);
    let Fx { m, key, dir } = fx;
    let mut m = Some(m);
    let acked = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut kills = Vec::new();
    let mut seed = 0x2545_f491u64;
    for round in 0..ROUNDS {
        let server = m.take().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let threads: Vec<_> = (0..WRITERS)
            .map(|w| {
                let (url, token, repo, acked, stop) = (server.url.clone(), token.clone(), repo.clone(), acked.clone(), stop.clone());
                std::thread::spawn(move || writer(url, token, repo, w, acked, stop))
            })
            .collect();
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let after = Duration::from_millis(150 + seed % 550);
        std::thread::sleep(after);
        let children = server.children().len();
        let t = Instant::now();
        server.kill9();
        stop.store(true, Ordering::Relaxed);
        for t in threads {
            t.join().unwrap();
        }
        // The jail's PDEATHSIG: every git of the dead server dies with it.
        let restarted = Macrofiche::restart(&bin(), &dir);
        let fresh = Fx { m: restarted, key: Key::from_pkcs8(&key.pkcs8), dir: dir.clone() };
        let fsck = fresh.fsck_all();
        assert!(fsck.iter().all(|(_, ok, _)| *ok), "round {round}: {fsck:?}");
        let history: std::collections::HashSet<String> = all_history(&fresh, &repo).into_iter().collect();
        let acked_now = acked.lock().unwrap().clone();
        let lost: Vec<&String> = acked_now.iter().filter(|s| !history.contains(*s)).collect();
        assert!(lost.is_empty(), "round {round}: acknowledged commits lost: {lost:?}");
        let next = fresh.commit_at_head(&repo, &[up("after.txt", format!("{round}").as_bytes())]);
        assert!(is_sha(&next));
        let recovered = std::fs::read_to_string(&fresh.m.log).unwrap_or_default().lines().rfind(|l| l.contains("\"recovered\"")).unwrap_or("").to_string();
        kills.push(serde_json::json!({ "round": round, "kill_after_ms": after.as_millis(), "git_children_at_kill": children, "restart_ms": t.elapsed().as_millis(), "acked_so_far": acked_now.len(), "history": history.len(), "recovered": serde_json::from_str::<serde_json::Value>(&recovered).map(|v| v["recovery"].clone()).unwrap_or_default() }));
        let Fx { m: next_m, .. } = fresh;
        m = Some(next_m);
    }
    let acked_n = acked.lock().unwrap().len();
    assert!(acked_n > ROUNDS, "the writers made progress: {acked_n}");
    eprintln!("{}", serde_json::json!({ "evidence": "crash", "rounds": ROUNDS, "writers": WRITERS, "acked": acked_n, "lost": 0, "fsck_failures": 0, "kills": kills }));
}

fn all_history(fx: &Fx, repo: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    // Bounded: 1000 pages of 100.
    for _ in 0..1000 {
        let q = match &cursor {
            None => "ref=main&limit=100".to_string(),
            Some(c) => format!("ref=main&limit=100&cursor={}", enc(c)),
        };
        let v = fx.get(repo, "commits", &q).json();
        out.extend(v["commits"].as_array().unwrap().iter().map(|c| c["sha"].as_str().unwrap().to_string()));
        match v["next_cursor"].as_str() {
            Some(c) if v["has_more"] == true => cursor = Some(c.to_string()),
            _ => return out,
        }
    }
    out
}

