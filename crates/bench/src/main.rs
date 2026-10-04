//! `macrofiche-bench`: the evidence driver (docs/design.md, Evidence).
//! Each scenario starts the release binary on a scratch state directory,
//! drives it over HTTP as fragment's clients do, and prints its numbers as
//! one JSON document (written to `docs/evidence/<scenario>.json` by
//! `--write`).
//!
//! `cargo run --release -p macrofiche-bench -- <latency|blob|race|crash|maintenance|all> [--bin <macrofiche>] [--n <samples>] [--write]`

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use macrofiche_testkit::fragment::{commit_pack, committed, enc, up, BigPack, Change};
use macrofiche_testkit::{request, sample_memory, workspace, Answer, Key, Macrofiche, Send, Sink};
use serde_json::{json, Value};

const ORG: &str = "fragment-dev";
const AUTHOR: (&str, &str) = ("fragment/1a2b3c4d", "1a2b3c4d@users.fragment");

struct Fx {
    m: Macrofiche,
    key: Key,
}

impl Fx {
    fn start(bin: &std::path::Path, name: &str, extra: Value) -> Fx {
        let dir = macrofiche_testkit::scratch(name);
        let key = Key::generate();
        let public = dir.join("org.pub.pem");
        std::fs::write(&public, key.public_pem()).unwrap();
        let mut cfg = json!({ "orgs": [{ "name": ORG, "keys": [{ "pem_file": public }] }] });
        if let Value::Object(e) = extra {
            for (k, v) in e {
                cfg[k] = v;
            }
        }
        Fx { m: Macrofiche::start(bin, &dir, cfg), key }
    }

    fn call(&self, method: &str, path: &str, repo: &str, scopes: &[&str], body: Option<(&str, &[u8])>) -> Answer {
        let auth = format!("Bearer {}", self.key.token(ORG, "fragment-runtime", repo, scopes, 300));
        let mut h = vec![("authorization", auth.as_str())];
        let send = match body {
            Some((ct, b)) => {
                h.push(("content-type", ct));
                Send::Bytes(b)
            }
            None => Send::None,
        };
        request(&self.m.url, method, path, &h, send, Sink::Keep).expect("an answer")
    }

    fn path(repo: &str, op: &str) -> String {
        format!("/api/repos/{}/{op}", enc(repo))
    }

    fn create(&self, name: &str) {
        let b = json!({ "repo_name": name, "default_branch": "main" }).to_string();
        let a = self.call("POST", "/api/repos", name, &["repo:write"], Some(("application/json", b.as_bytes())));
        assert_eq!(a.status, 200, "{a}");
    }

    fn pack(&self, repo: &str, body: &[u8]) -> Answer {
        self.call("POST", &Fx::path(repo, "commit-pack"), repo, &["git:write"], Some(("application/x-ndjson", body)))
    }

    fn get(&self, repo: &str, op: &str, q: &str) -> Answer {
        self.call("GET", &format!("{}?{q}", Fx::path(repo, op)), repo, &["git:read"], None)
    }

    fn json(&self, repo: &str, op: &str, body: &Value) -> Answer {
        let b = body.to_string();
        self.call("POST", &Fx::path(repo, op), repo, &["git:write"], Some(("application/json", b.as_bytes())))
    }

    fn head(&self, repo: &str, branch: &str) -> Option<String> {
        let a = self.get(repo, "branch", &format!("name={branch}"));
        (a.status == 200).then(|| a.json()["branch"]["head_sha"].as_str().unwrap().to_string())
    }
}

/// Times in ms: median, p95, p99, max, mean, and the count.
fn stats(mut ms: Vec<f64>) -> Value {
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |q: f64| ms[((ms.len() as f64 - 1.0) * q).round() as usize];
    let r = |x: f64| (x * 1000.0).round() / 1000.0;
    json!({ "n": ms.len(), "p50": r(at(0.5)), "p95": r(at(0.95)), "p99": r(at(0.99)), "max": r(*ms.last().unwrap()), "mean": r(ms.iter().sum::<f64>() / ms.len() as f64) })
}

fn timed<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let t0 = Instant::now();
    let r = f();
    (r, t0.elapsed().as_secs_f64() * 1000.0)
}

/// Each route's latency over `n` samples, sequential, on loopback: a
/// repository of 100 files, a deploy cycle (commit, preview, merge,
/// restore) per sample.
fn latency(bin: &std::path::Path, n: usize) -> Value {
    let fx = Fx::start(bin, "bench-latency", json!({}));
    let repo = "lat--bench";
    fx.create(repo);
    let files: Vec<(String, Vec<u8>)> = (0..100).map(|i| (format!("dir{}/f{i:03}.txt", i % 10), format!("{i} {}", "x".repeat(1000)).into_bytes())).collect();
    let changes: Vec<Change<'_>> = files.iter().map(|(p, b)| up(p, b)).collect();
    let mut head = committed(&fx.pack(repo, &commit_pack("main", None, "seed", AUTHOR, &changes)).json()).unwrap();
    assert_eq!(fx.json(repo, "branches/create", &json!({ "base_ref": head, "target_branch": "live", "target_is_ephemeral": false })).status, 200);
    let mut t: std::collections::BTreeMap<&str, Vec<f64>> = Default::default();
    for i in 0..n {
        // a commit of one file (the cell's mutation), then of five
        let body = commit_pack("main", Some(&head), "a mutation", AUTHOR, &[up(&format!("rot/r{}.txt", i % 7), format!("{i}").as_bytes())]);
        let (a, ms) = timed(|| fx.pack(repo, &body));
        t.entry("POST commit-pack, 1 file").or_default().push(ms);
        let prev = std::mem::replace(&mut head, committed(&a.json()).expect("committed"));
        let five: Vec<(String, Vec<u8>)> = (0..5).map(|k| (format!("five/f{k}.txt"), format!("{i} {k}").into_bytes())).collect();
        let five_c: Vec<Change<'_>> = five.iter().map(|(p, b)| up(p, b)).collect();
        let body = commit_pack("main", Some(&head), "a sync", AUTHOR, &five_c);
        let (a, ms) = timed(|| fx.pack(repo, &body));
        t.entry("POST commit-pack, 5 files").or_default().push(ms);
        head = committed(&a.json()).expect("committed");
        let (_, ms) = timed(|| fx.get(repo, "files/metadata", &format!("ref={head}&limit=1000")));
        t.entry("GET files/metadata, 106 files, a new head").or_default().push(ms);
        let (a, ms) = timed(|| fx.get(repo, "files/metadata", &format!("ref={head}&limit=1000")));
        assert_eq!(a.status, 200);
        t.entry("GET files/metadata, 106 files, again").or_default().push(ms);
        let (_, ms) = timed(|| fx.get(repo, "branch", "name=main"));
        t.entry("GET branch").or_default().push(ms);
        let (a, ms) = timed(|| fx.get(repo, "file", &format!("path=dir3%2Ff013.txt&ref={head}")));
        assert_eq!(a.status, 200);
        t.entry("GET file, 1 KiB").or_default().push(ms);
        let (_, ms) = timed(|| fx.call("HEAD", &format!("{}?path=dir3%2Ff013.txt&ref={head}", Fx::path(repo, "file")), repo, &["git:read"], None));
        t.entry("HEAD file").or_default().push(ms);
        let (_, ms) = timed(|| fx.get(repo, "commits", "ref=main&limit=30"));
        t.entry("GET commits, 30").or_default().push(ms);
        // a deploy as fragment makes one since PR #127: the preview, then
        // what it calls for (contract section 15)
        let (p, ms) = timed(|| fx.get(repo, "merge/preview", "source_branch=main&target_branch=live"));
        t.entry("GET merge/preview").or_default().push(ms);
        let p = p.json();
        let live = p["target_tip_sha"].as_str().unwrap().to_string();
        let restore = |base: &str, expected: &str, message: &str| {
            let line = json!({ "metadata": { "target_branch": "live", "base_ref": base, "expected_target_sha": expected, "commit_message": message, "author": { "name": AUTHOR.0, "email": AUTHOR.1 } } });
            let body = format!("{line}\n");
            timed(|| fx.call("POST", &Fx::path(repo, "restore-commit"), repo, &["git:write"], Some(("application/x-ndjson", body.as_bytes()))))
        };
        let merge = |expected: &str| {
            let m = json!({ "target_branch": "live", "source_ref": "main", "strategy": "ff_prefer", "expected_target_sha": expected, "commit_message": "deploy", "author": { "name": AUTHOR.0, "email": AUTHOR.1 } });
            timed(|| fx.json(repo, "merge", &m))
        };
        let live = match p["result"].as_str().unwrap() {
            "fast_forward" => {
                let (a, ms) = merge(&live);
                assert_eq!(a.status, 200, "{a}");
                t.entry("POST merge, fast-forward").or_default().push(ms);
                a.json()["target"]["new_sha"].as_str().unwrap().to_string()
            }
            "merge_commit" => {
                let base = p["merge_base_sha"].as_str().unwrap().to_string();
                let (a, ms) = restore(&base, &live, "deploy (first, live back to the base)");
                t.entry("POST restore-commit").or_default().push(ms);
                let live = if a.status == 201 { committed(&a.json()).unwrap() } else { live };
                let (a, ms) = merge(&live);
                assert_eq!(a.status, 200, "{a}");
                t.entry("POST merge, three-way").or_default().push(ms);
                a.json()["target"]["new_sha"].as_str().unwrap().to_string()
            }
            other => panic!("a preview of {other}"),
        };
        // then a rollback to the commit before
        let (a, ms) = restore(&prev, &live, "rollback");
        assert!(matches!(a.status, 201 | 412), "{a}");
        t.entry("POST restore-commit").or_default().push(ms);
        let (_, ms) = timed(|| fx.json(repo, "branches/create", &json!({ "base_ref": head, "target_branch": format!("preview/{i:012x}"), "target_is_ephemeral": true })));
        t.entry("POST branches/create").or_default().push(ms);
        if i < 50 {
            let name = format!("made--{i}");
            let b = json!({ "repo_name": name }).to_string();
            let (_, ms) = timed(|| fx.call("POST", "/api/repos", &name, &["repo:write"], Some(("application/json", b.as_bytes()))));
            t.entry("POST /api/repos").or_default().push(ms);
            let (_, ms) = timed(|| fx.call("GET", "/api/repos?limit=100", ORG, &["org:read"], None));
            t.entry("GET /api/repos").or_default().push(ms);
        }
    }
    let (rss, hwm, anon) = fx.m.memory_kib();
    json!({
        "scenario": "latency", "samples_per_route": n, "loopback": true, "client": "a fresh TCP connection per request (macrofiche-testkit)",
        "ms": t.into_iter().map(|(k, v)| (k.to_string(), stats(v))).collect::<serde_json::Map<_, _>>(),
        "macrofiche_after_kib": { "vm_rss": rss, "vm_hwm": hwm, "rss_anon": anon },
    })
}

/// A blob of `mib` MiB in and out, with memory sampled.
fn blob(bin: &std::path::Path, mib: u64) -> Value {
    let fx = Fx::start(bin, "bench-blob", json!({}));
    let repo = "blob--bench";
    fx.create(repo);
    let head = committed(&fx.pack(repo, &commit_pack("main", None, "seed", AUTHOR, &[up("readme.md", b"small")])).json()).unwrap();
    let len = mib << 20;
    let mut pack = BigPack::new(Some(&head), "media/big.bin", len);
    let body_len = pack.body_len();
    let auth = format!("Bearer {}", fx.key.token(ORG, "editor:ab", repo, &["git:read", "git:write"], 900));
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = sample_memory(fx.m.pid, stop.clone());
    let (a, in_ms) = timed(|| request(&fx.m.url, "POST", &Fx::path(repo, "commit-pack"), &[("authorization", &auth), ("content-type", "application/x-ndjson")], Send::Reader(&mut pack, body_len), Sink::Keep).unwrap());
    stop.store(true, Ordering::Relaxed);
    let (in_own, in_git) = sampler.join().unwrap();
    assert_eq!(a.status, 201, "{a}");
    let tip = committed(&a.json()).unwrap();
    let (want_sha256, want_blob) = pack.finish();
    struct Hasher(ring::digest::Context, u64);
    impl std::io::Write for Hasher {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.update(b);
            self.1 += b.len() as u64;
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut h = Hasher(ring::digest::Context::new(&ring::digest::SHA256), 0);
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = sample_memory(fx.m.pid, stop.clone());
    let (got, out_ms) = timed(|| request(&fx.m.url, "GET", &format!("{}?path=media%2Fbig.bin&ref={tip}", Fx::path(repo, "file")), &[("authorization", &auth)], Send::None, Sink::Into(&mut h)).unwrap());
    stop.store(true, Ordering::Relaxed);
    let (out_own, out_git) = sampler.join().unwrap();
    let got_sha: String = h.0.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!((got.status, got_sha.as_str(), got.header("x-blob-sha")), (200, want_sha256.as_str(), Some(want_blob.as_str())));
    let (_, hwm, _) = fx.m.memory_kib();
    let mibf = |k: u64| (k as f64 / 1024.0 * 10.0).round() / 10.0;
    json!({
        "scenario": "blob", "blob_mib": mib, "pack_body_bytes": body_len, "bytes_checked": "SHA-256 of the bytes out equals the bytes in; x-blob-sha is git's id",
        "in": { "ms": in_ms.round(), "mib_per_s": (mib as f64 / (in_ms / 1000.0)).round(), "macrofiche_rss_anon_peak_mib": mibf(in_own), "git_rss_anon_peak_mib": mibf(in_git) },
        "out": { "ms": out_ms.round(), "mib_per_s": (mib as f64 / (out_ms / 1000.0)).round(), "macrofiche_rss_anon_peak_mib": mibf(out_own), "git_rss_anon_peak_mib": mibf(out_git) },
        "macrofiche_vm_hwm_mib": mibf(hwm),
    })
}

/// Sixteen writers racing on one branch; sixteen on their own repositories.
fn race(bin: &std::path::Path) -> Value {
    const W: usize = 16;
    const EACH: usize = 20;
    let fx = Arc::new(Fx::start(bin, "bench-race", json!({})));
    let repo = "race--bench";
    fx.create(repo);
    fx.pack(repo, &commit_pack("main", None, "root", AUTHOR, &[up("root.txt", b"root")]));
    let (attempts, refused) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let lat = Arc::new(Mutex::new(Vec::new()));
    let t0 = Instant::now();
    let threads: Vec<_> = (0..W)
        .map(|w| {
            let (fx, attempts, refused, lat) = (fx.clone(), attempts.clone(), refused.clone(), lat.clone());
            std::thread::spawn(move || {
                for k in 0..EACH {
                    let body_text = format!("{w} {k}");
                    let path = format!("w{w:02}/n{k:02}.txt");
                    // Bounded: a writer gives up after 10 000 tries.
                    for _ in 0..10_000 {
                        attempts.fetch_add(1, Ordering::Relaxed);
                        let head = fx.head(repo, "main");
                        let body = commit_pack("main", head.as_deref(), "race", AUTHOR, &[up(&path, body_text.as_bytes())]);
                        let (a, ms) = timed(|| fx.pack(repo, &body));
                        lat.lock().unwrap().push(ms);
                        if a.status == 201 {
                            break;
                        }
                        assert_eq!(a.status, 409, "{a}");
                        refused.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let one_branch_s = t0.elapsed().as_secs_f64();
    let files = fx.get(repo, "files/metadata", "ref=main&limit=1000").json()["files"].as_array().unwrap().len();
    let fx2 = Arc::new(Fx::start(bin, "bench-own", json!({})));
    for i in 0..W {
        fx2.create(&format!("own--{i}"));
    }
    let t1 = Instant::now();
    let threads: Vec<_> = (0..W)
        .map(|i| {
            let fx2 = fx2.clone();
            std::thread::spawn(move || {
                let repo = format!("own--{i}");
                let mut head: Option<String> = None;
                for k in 0..EACH {
                    let a = fx2.pack(&repo, &commit_pack("main", head.as_deref(), "own", AUTHOR, &[up(&format!("n{k}.txt"), b"x")]));
                    assert_eq!(a.status, 201);
                    head = committed(&a.json());
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let own_s = t1.elapsed().as_secs_f64();
    json!({
        "scenario": "race",
        "one_branch": { "writers": W, "commits_each": EACH, "landed": W * EACH, "files_after": files, "attempts": attempts.load(Ordering::Relaxed), "refused_409": refused.load(Ordering::Relaxed), "secs": (one_branch_s * 1000.0).round() / 1000.0, "commits_per_s": ((W * EACH) as f64 / one_branch_s).round(), "pack_ms": stats(lat.lock().unwrap().clone()) },
        "own_repositories": { "writers": W, "commits": W * EACH, "secs": (own_s * 1000.0).round() / 1000.0, "commits_per_s": ((W * EACH) as f64 / own_s).round() },
    })
}

/// git fsck of every repository under a state directory.
fn fsck_all(state: &std::path::Path) -> (usize, usize) {
    let git = macrofiche_testkit::git_prefix().join("bin/git");
    let (mut n, mut bad) = (0, 0);
    for org in std::fs::read_dir(state.join("repos")).unwrap().flatten() {
        for repo in std::fs::read_dir(org.path()).unwrap().flatten() {
            n += 1;
            let ok = std::process::Command::new(&git)
                .env_clear()
                .env("GIT_DIR", repo.path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args(["fsck", "--full", "--strict", "--no-progress", "--no-dangling"])
                .status()
                .unwrap()
                .success();
            bad += usize::from(!ok);
        }
    }
    (n, bad)
}

/// `rounds` of kill -9 while four writers commit 256 KiB files, each
/// followed by a restart, fsck, and the acknowledged commits checked.
fn crash(bin: &std::path::Path, rounds: usize) -> Value {
    let fx = Fx::start(bin, "bench-crash", json!({}));
    let repo = "crash--bench";
    fx.create(repo);
    fx.pack(repo, &commit_pack("main", None, "seed", AUTHOR, &[up("seed.txt", b"seed")]));
    let token = fx.key.token(ORG, "fragment-runtime", repo, &["git:read", "git:write"], 3000);
    let Fx { m, key } = fx;
    let dir = m.dir.clone();
    let mut m = Some(m);
    let acked = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut out = Vec::new();
    let mut x = 0x2545_f491_u64;
    for round in 0..rounds {
        let server = m.take().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let threads: Vec<_> = (0..4)
            .map(|w| {
                let (url, token, acked, stop) = (server.url.clone(), token.clone(), acked.clone(), stop.clone());
                std::thread::spawn(move || {
                    let big: Vec<u8> = (0..256 * 1024u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8 ^ w as u8).collect();
                    let auth = format!("Bearer {token}");
                    let mut n = 0u64;
                    // Bounded by the server's death.
                    while !stop.load(Ordering::Relaxed) {
                        let Ok(b) = request(&url, "GET", &format!("{}?name=main", Fx::path(repo, "branch")), &[("authorization", &auth)], Send::None, Sink::Keep) else { return };
                        let head = b.json()["branch"]["head_sha"].as_str().map(str::to_string);
                        n += 1;
                        let counter = format!("{w} {n}");
                        let path = format!("w{w}/big.bin");
                        let body = commit_pack("main", head.as_deref(), "crash", AUTHOR, &[up(&path, &big), up("counter.txt", counter.as_bytes())]);
                        match request(&url, "POST", &Fx::path(repo, "commit-pack"), &[("authorization", &auth), ("content-type", "application/x-ndjson")], Send::Bytes(&body), Sink::Keep) {
                            Ok(a) if a.status == 201 => acked.lock().unwrap().push(committed(&a.json()).unwrap()),
                            Ok(_) => {}
                            Err(_) => return,
                        }
                    }
                })
            })
            .collect();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let after = 100 + x % 600;
        std::thread::sleep(Duration::from_millis(after));
        let git_children = server.children().len();
        server.kill9();
        stop.store(true, Ordering::Relaxed);
        for t in threads {
            t.join().unwrap();
        }
        let (restarted, restart_ms) = timed(|| Macrofiche::restart(bin, &dir));
        let (repos, fsck_bad) = fsck_all(&dir.join("state"));
        let fx = Fx { m: restarted, key: Key::from_pkcs8(&key.pkcs8) };
        let mut history = std::collections::HashSet::new();
        let mut cursor: Option<String> = None;
        // Bounded: 1000 pages of 100.
        for _ in 0..1000 {
            let q = match &cursor {
                None => "ref=main&limit=100".to_string(),
                Some(c) => format!("ref=main&limit=100&cursor={}", enc(c)),
            };
            let v = fx.get(repo, "commits", &q).json();
            history.extend(v["commits"].as_array().unwrap().iter().map(|c| c["sha"].as_str().unwrap().to_string()));
            match v["next_cursor"].as_str() {
                Some(c) if v["has_more"] == true => cursor = Some(c.to_string()),
                _ => break,
            }
        }
        let acked_now = acked.lock().unwrap().clone();
        let lost = acked_now.iter().filter(|s| !history.contains(*s)).count();
        let next = fx.pack(repo, &commit_pack("main", fx.head(repo, "main").as_deref(), "after", AUTHOR, &[up("after.txt", format!("{round}").as_bytes())]));
        let log = std::fs::read_to_string(&fx.m.log).unwrap_or_default();
        let rec = log.lines().rfind(|l| l.contains("\"recovered\"")).and_then(|l| serde_json::from_str::<Value>(l).ok()).map(|v| v["recovery"].clone()).unwrap_or_default();
        out.push(json!({ "round": round, "kill_after_ms": after, "git_children_at_kill": git_children, "restart_ms": restart_ms.round(), "repos_fscked": repos, "fsck_failures": fsck_bad, "acked_total": acked_now.len(), "lost": lost, "next_write": next.status, "recovered": rec }));
        assert_eq!((fsck_bad, lost, next.status), (0, 0, 201), "round {round}");
        m = Some(fx.m);
    }
    let acked_n = acked.lock().unwrap().len();
    json!({ "scenario": "crash", "rounds": rounds, "writers": 4, "file_bytes": 256 * 1024, "acked": acked_n, "lost": 0, "fsck_failures": 0, "rounds_detail": out })
}

/// `count-objects -v` of a repository, as numbers.
fn objects(dir: &std::path::Path) -> Value {
    let git = macrofiche_testkit::git_prefix().join("bin/git");
    let o = std::process::Command::new(git).env_clear().env("GIT_DIR", dir).env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null").args(["count-objects", "-v"]).output().unwrap();
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    let mut m = serde_json::Map::new();
    for l in text.lines() {
        if let Some((k, v)) = l.split_once(": ") {
            m.insert(k.to_string(), v.parse::<u64>().map(Value::from).unwrap_or(Value::Null));
        }
    }
    Value::Object(m)
}

/// 300 commits, then idle maintenance: loose objects and packs before
/// and after, and a listing's latency at a new head before and after.
fn maintenance(bin: &std::path::Path) -> Value {
    let fx = Fx::start(bin, "bench-maint", json!({ "maintenance_idle_s": 2 }));
    let repo = "maint--bench";
    fx.create(repo);
    let mut head: Option<String> = None;
    for i in 0..300 {
        let a = fx.pack(repo, &commit_pack("main", head.as_deref(), "m", AUTHOR, &[up(&format!("d{}/f{}.txt", i % 10, i % 37), format!("{i}").as_bytes())]));
        head = committed(&a.json());
    }
    let repo_dir = std::fs::read_dir(fx.m.dir.join("state/repos").join(ORG)).unwrap().flatten().next().unwrap().path();
    let before = objects(&repo_dir);
    let read = |fx: &Fx| {
        let h = fx.head(repo, "main").unwrap();
        let (_, ms) = timed(|| fx.get(repo, "commits", &format!("ref={h}&limit=100")));
        ms
    };
    let read_before: Vec<f64> = (0..20).map(|_| read(&fx)).collect();
    let t0 = Instant::now();
    // Bounded: 120 s.
    let mut done = None;
    while t0.elapsed() < Duration::from_secs(120) {
        let log = std::fs::read_to_string(&fx.m.log).unwrap_or_default();
        if let Some(l) = log.lines().find(|l| l.contains("\"maintained\"")) {
            done = serde_json::from_str::<Value>(l).ok();
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let after = objects(&repo_dir);
    let read_after: Vec<f64> = (0..20).map(|_| read(&fx)).collect();
    let (_, bad) = fsck_all(&fx.m.dir.join("state"));
    json!({ "scenario": "maintenance", "commits": 300, "objects_before": before, "objects_after": after, "maintained": done.map(|v| v["repo"].clone()), "fsck_failures": bad, "commits_route_ms_before": stats(read_before), "commits_route_ms_after": stats(read_after) })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let bin = flag("--bin").map(std::path::PathBuf::from).unwrap_or_else(|| workspace().join("target/release/macrofiche"));
    assert!(bin.is_file(), "no binary at {} (cargo build --release -p macrofiche)", bin.display());
    let n: usize = flag("--n").and_then(|v| v.parse().ok()).unwrap_or(200);
    let write = args.iter().any(|a| a == "--write");
    let which = args.first().map(String::as_str).unwrap_or("all");
    let scenarios: Vec<&str> = if which == "all" { vec!["latency", "blob", "race", "crash", "maintenance"] } else { vec![which] };
    for s in scenarios {
        let mut v = match s {
            "latency" => latency(&bin, n),
            "blob" => blob(&bin, flag("--mib").and_then(|v| v.parse().ok()).unwrap_or(100)),
            "race" => race(&bin),
            "crash" => crash(&bin, flag("--rounds").and_then(|v| v.parse().ok()).unwrap_or(20)),
            "maintenance" => maintenance(&bin),
            other => panic!("no scenario {other}"),
        };
        v["date"] = "2026-10-03".into();
        v["machine"] = "AMD Ryzen 9 9950X3D (32 threads), Linux 7.2.5, btrfs on NVMe; release build".into();
        let text = serde_json::to_string_pretty(&v).unwrap();
        println!("{text}");
        if write {
            std::fs::write(workspace().join("docs/evidence").join(format!("{s}.json")), format!("{text}\n")).unwrap();
        }
    }
}
