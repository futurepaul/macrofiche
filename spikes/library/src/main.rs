//! macrofiche's library spike (docs/library.md): the same workload through
//! libgit2 (git2), gitoxide (gix), and the git CLI.
//!
//! `library-spike run <config> [samples]` runs every step, each in a child
//! process of its own so that its peak RSS is its own, and writes
//! `evidence/<config>.json`. `crash` and `contention` are the kill -9 and
//! the concurrent-writer runs.
//!
//! Configs: git2, git2-packed (each commit one pack), git2-nofsync, gix (no
//! fsync exists), git, git-nofsync.

mod lib_git;
mod lib_git2;
mod lib_gix;
mod model;

use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use model::*;

const STEPS: &[&str] = &[
    "init",
    "commit-initial",
    "history",
    "read",
    "last-commit",
    "branch-ff",
    "merge",
    "census-before-gc",
    "gc",
    "census-after-gc",
    "read-after-gc",
    "fsck",
];

fn ms(t: Instant) -> f64 {
    (t.elapsed().as_secs_f64() * 1e6).round() / 1e3
}

// ---------------------------------------------------------------- memory

fn status_kib(field: &str) -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    s.lines()
        .find_map(|l| {
            l.strip_prefix(field)
                .and_then(|r| r.trim_start_matches(':').split_whitespace().next())
                .and_then(|n| n.parse().ok())
        })
        .unwrap_or(0)
}

fn children_maxrss_kib() -> u64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru) };
    assert_eq!(rc, 0);
    ru.ru_maxrss as u64
}

/// Samples RssAnon and RssFile every millisecond: the kernel keeps only the
/// total's peak (VmHWM), and a library that maps its packs shows up as file
/// pages, not as heap.
struct Sampler {
    stop: Arc<AtomicBool>,
    anon: Arc<AtomicU64>,
    file: Arc<AtomicU64>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Sampler {
    fn start() -> Sampler {
        let (stop, anon, file) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
        );
        let (s, a, f) = (stop.clone(), anon.clone(), file.clone());
        let handle = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                a.fetch_max(status_kib("RssAnon"), Ordering::Relaxed);
                f.fetch_max(status_kib("RssFile"), Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        Sampler {
            stop,
            anon,
            file,
            handle: Some(handle),
        }
    }

    fn snapshot(&self) -> Value {
        json!({
            "vm_hwm_kib": status_kib("VmHWM"),
            "peak_rss_anon_kib": self.anon.load(Ordering::Relaxed).max(status_kib("RssAnon")),
            "peak_rss_file_kib": self.file.load(Ordering::Relaxed).max(status_kib("RssFile")),
            "children_maxrss_kib": children_maxrss_kib(),
            "git_child_peak_rss_anon_kib": lib_git::child_peaks().0,
            "git_child_peak_rss_file_kib": lib_git::child_peaks().1,
        })
    }

    fn finish(mut self) -> Value {
        let v = self.snapshot();
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        v
    }
}

// ---------------------------------------------------------------- helpers

/// Delete loose objects that `packed` says are now in a pack; return
/// (removed, kept).
pub fn prune_loose(repo: &Path, packed: impl Fn(&str) -> bool) -> R<(usize, usize)> {
    let (mut removed, mut kept) = (0, 0);
    for d in std::fs::read_dir(repo.join("objects"))? {
        let d = d?;
        let name = d.file_name().to_string_lossy().into_owned();
        if name.len() != 2 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        for f in std::fs::read_dir(d.path())? {
            let f = f?;
            let rest = f.file_name().to_string_lossy().into_owned();
            if rest.len() != 38 || !rest.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            if packed(&format!("{name}{rest}")) {
                std::fs::remove_file(f.path())?;
                removed += 1;
            } else {
                kept += 1;
            }
        }
        let _ = std::fs::remove_dir(d.path());
    }
    Ok((removed, kept))
}

/// Delete every pack in objects/pack but `keep` (a `pack-<hash>` stem):
/// its .pack, .idx, .rev, and .keep. Returns the packs removed.
pub fn drop_packs_except(repo: &Path, keep: &str) -> R<usize> {
    let mut removed = 0;
    for e in std::fs::read_dir(repo.join("objects/pack"))? {
        let p = e?.path();
        let (Some(stem), Some(ext)) = (
            p.file_stem().and_then(|s| s.to_str()),
            p.extension().and_then(|s| s.to_str()),
        ) else {
            continue;
        };
        if stem.starts_with("pack-")
            && stem != keep
            && matches!(ext, "pack" | "idx" | "rev" | "keep")
        {
            std::fs::remove_file(&p)?;
            removed += usize::from(ext == "pack");
        }
    }
    Ok(removed)
}

fn git_inspect(repo: &Path, args: &[&str]) -> R<std::process::Output> {
    Ok(Command::new("git")
        .env("GIT_DIR", repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(args)
        .stdin(Stdio::null())
        .output()?)
}

/// What git itself sees in the object store.
fn object_census(repo: &Path) -> R<Value> {
    let out = git_inspect(repo, &["count-objects", "-v"])?;
    let mut v = serde_json::Map::new();
    for l in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some((k, n)) = l.split_once(": ") {
            if let Ok(n) = n.parse::<u64>() {
                v.insert(k.replace('-', "_"), json!(n));
            }
        }
    }
    let (mut packed, mut deltas) = (0u64, 0u64);
    for e in std::fs::read_dir(repo.join("objects/pack"))? {
        let p = e?.path();
        if p.extension().is_some_and(|x| x == "idx") {
            let out = git_inspect(repo, &["verify-pack", "-v", p.to_str().ok_or("path")?])?;
            for l in String::from_utf8_lossy(&out.stdout).lines() {
                let f: Vec<&str> = l.split_whitespace().collect();
                if f.len() >= 5
                    && f[0].len() == 40
                    && matches!(f[1], "blob" | "tree" | "commit" | "tag")
                {
                    packed += 1;
                    if f.len() == 7 {
                        deltas += 1;
                    }
                }
            }
        }
    }
    v.insert("packed_objects".into(), json!(packed));
    v.insert("deltified_objects".into(), json!(deltas));
    Ok(Value::Object(v))
}

fn load_state(dir: &Path) -> R<Value> {
    Ok(serde_json::from_slice(&std::fs::read(
        dir.join("state.json"),
    )?)?)
}

fn save_state(dir: &Path, v: &Value) -> R<()> {
    std::fs::write(dir.join("state.json"), serde_json::to_vec_pretty(v)?)?;
    Ok(())
}

fn commits(state: &Value) -> Vec<String> {
    state["c"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn ok<T>(r: Result<T, Refused>, what: &str) -> R<T> {
    r.map_err(|e| format!("{what}: refused {e:?}").into())
}

fn percentile(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    if v.is_empty() {
        return 0.0;
    }
    v[((v.len() - 1) as f64 * p).round() as usize]
}

// ---------------------------------------------------------------- steps

/// `read_many` for the CLI keeps one cat-file process; the libraries read
/// through one open handle.
trait ReadMany: Store {
    fn read_many(&self, branch: &str, paths: &[String], sink: &mut dyn Write) -> R<Vec<Read1>> {
        let mut out = Vec::new();
        for p in paths {
            out.push(
                self.read_file(branch, p, false, sink)?
                    .ok_or("missing file")?,
            );
        }
        Ok(out)
    }
}
impl ReadMany for lib_git2::Git2 {}
impl ReadMany for lib_gix::Gix {}
impl ReadMany for lib_git::Git {
    fn read_many(&self, branch: &str, paths: &[String], sink: &mut dyn Write) -> R<Vec<Read1>> {
        self.read_batch(branch, paths, sink)
    }
}

fn step<S: ReadMany>(name: &str, dir: &Path, durable: bool) -> R<Value> {
    let repo = dir.join("repo.git");
    let sampler = Sampler::start();
    let baseline = status_kib("VmHWM");
    let mut out = json!({ "step": name });
    match name {
        "init" => {
            let t = Instant::now();
            S::init(&repo, durable)?;
            out["ms"] = json!(ms(t));
            save_state(dir, &json!({ "c": [] }))?;
        }
        "commit-initial" => {
            let s = S::open(&repo, durable)?;
            let paths: Vec<String> = (0..SMALL_FILES).map(small_path).collect();
            let contents: Vec<Vec<u8>> = (0..SMALL_FILES).map(|i| small_content(i, 0)).collect();
            // what producing the 50 MiB costs, included in big_write_ms
            let t = Instant::now();
            io::copy(&mut BigReader::new(), &mut io::sink())?;
            out["generator_ms"] = json!(ms(t));
            let t = Instant::now();
            let big = s.write_blob_from(&mut BigReader::new())?;
            out["big_write_ms"] = json!(ms(t));
            out["rss_after_big_write"] = sampler.snapshot();
            let mut changes: Vec<Change<'_>> = paths
                .iter()
                .zip(&contents)
                .map(|(p, c)| Change {
                    path: p,
                    op: Op::Upsert(c),
                })
                .collect();
            changes.push(Change {
                path: BIG_PATH,
                op: Op::UpsertBlob(&big),
            });
            let t2 = Instant::now();
            let msg = commit_message(0);
            let c0 = ok(
                s.commit(&CommitSpec {
                    branch: "main",
                    expected: None,
                    changes: &changes,
                    message: &msg,
                    time: T0,
                })?,
                "first commit",
            )?;
            out["small_files_and_commit_ms"] = json!(ms(t2));
            out["ms"] = json!(ms(t));
            out["big_blob"] = json!(big);
            out["commit"] = json!(c0);
            save_state(dir, &json!({ "c": [c0], "big": big }))?;
        }
        "history" => {
            let s = S::open(&repo, durable)?;
            let mut state = load_state(dir)?;
            let mut c = commits(&state);
            let mut each = Vec::new();
            let t = Instant::now();
            for n in 1..=HISTORY {
                let files = touched(n);
                let (paths, contents): (Vec<String>, Vec<Vec<u8>>) = files
                    .iter()
                    .map(|&i| (small_path(i), small_content(i, n)))
                    .unzip();
                let changes: Vec<Change<'_>> = paths
                    .iter()
                    .zip(&contents)
                    .map(|(p, b)| Change {
                        path: p,
                        op: Op::Upsert(b),
                    })
                    .collect();
                let msg = commit_message(n);
                let t1 = Instant::now();
                let id = ok(
                    s.commit(&CommitSpec {
                        branch: "main",
                        expected: c.last().map(String::as_str),
                        changes: &changes,
                        message: &msg,
                        time: T0 + n as i64,
                    })?,
                    "history commit",
                )?;
                each.push(ms(t1));
                c.push(id);
            }
            out["ms"] = json!(ms(t));
            out["per_commit_ms"] = json!({ "median": percentile(&mut each.clone(), 0.5), "p95": percentile(&mut each.clone(), 0.95), "max": percentile(&mut each, 1.0) });
            out["tip"] = json!(c.last());
            state["c"] = json!(c);
            save_state(dir, &state)?;
        }
        "read" | "read-after-gc" => {
            let expected_fnv = big_fnv();
            let s = S::open(&repo, durable)?;
            let state = load_state(dir)?;
            // the big file first, so its peak is its own
            let mut f = Fnv::new();
            let t = Instant::now();
            let r = s
                .read_file("main", BIG_PATH, true, &mut f)?
                .ok_or("no big file")?;
            out["big_read_ms"] = json!(ms(t));
            out["big_read_method"] = json!(r.method);
            out["rss_after_big_read"] = sampler.snapshot();
            assert_eq!(
                (f.len, f.h, r.len),
                (BIG_LEN, expected_fnv, BIG_LEN),
                "the big file reads back"
            );
            assert_eq!(
                r.blob,
                state["big"].as_str().unwrap_or(""),
                "the big file's blob id"
            );
            // one small file, fresh handle
            let i = 342;
            let s = S::open(&repo, durable)?;
            let mut buf = Vec::new();
            let t = Instant::now();
            let one = s
                .read_file("main", &small_path(i), false, &mut buf)?
                .ok_or("no small file")?;
            out["one_small_read_ms"] = json!(ms(t));
            out["one_small_blob"] = json!(one.blob);
            assert!(
                buf == small_content(i, last_rev(i, HISTORY)),
                "the small file reads back"
            );
            // a hundred through one handle
            let q: Vec<usize> = query_files();
            let paths: Vec<String> = q.iter().map(|&i| small_path(i)).collect();
            let mut all = Vec::new();
            let t = Instant::now();
            let got = s.read_many("main", &paths, &mut all)?;
            out["hundred_small_reads_ms"] = json!(ms(t));
            // after the merge step, file 0 carries main's edit
            let merged = name == "read-after-gc";
            let want: Vec<u8> = q
                .iter()
                .flat_map(|&i| {
                    if merged && i == 0 {
                        edited(0, last_rev(0, HISTORY), Some((5, "main edit")))
                    } else {
                        small_content(i, last_rev(i, HISTORY))
                    }
                })
                .collect();
            assert_eq!(got.len(), 100);
            assert!(all == want, "the hundred files read back");
        }
        "last-commit" => {
            let s = S::open(&repo, durable)?;
            let c = commits(&load_state(dir)?);
            let q = query_files();
            let paths: Vec<String> = q.iter().map(|&i| small_path(i)).collect();
            let t = Instant::now();
            let (found, walked) = s.last_commits("main", &paths)?;
            out["ms"] = json!(ms(t));
            out["commits_walked"] = json!(walked);
            let mut want: Vec<(String, String)> = q
                .iter()
                .map(|&i| (small_path(i), c[last_rev(i, HISTORY)].clone()))
                .collect();
            want.sort();
            assert_eq!(found, want, "the last commit of every path");
        }
        "branch-ff" => {
            let s = S::open(&repo, durable)?;
            let c = commits(&load_state(dir)?);
            let t = Instant::now();
            ok(s.create_branch("live", &c[100])?, "create live")?;
            out["create_ms"] = json!(ms(t));
            assert_eq!(
                s.create_branch("live", &c[1])?,
                Err(Refused::Exists),
                "a second create is refused"
            );
            let t = Instant::now();
            let stale = s.update_branch("live", &c[99], &c[HISTORY])?;
            out["stale_cas_ms"] = json!(ms(t));
            assert_eq!(
                stale,
                Err(Refused::Moved {
                    current: Some(c[100].clone())
                }),
                "a stale expected sha is refused"
            );
            let t = Instant::now();
            assert!(s.is_ancestor(&c[100], &c[HISTORY])?, "live is behind main");
            ok(
                s.update_branch("live", &c[100], &c[HISTORY])?,
                "fast-forward live",
            )?;
            out["ff_ms"] = json!(ms(t));
            assert_eq!(s.resolve("live")?.as_deref(), Some(c[HISTORY].as_str()));
        }
        "merge" => {
            let s = S::open(&repo, durable)?;
            let mut state = load_state(dir)?;
            let c = commits(&state);
            let base = c[HISTORY].clone();
            let at = |i: usize, tag: &str| edited(i, last_rev(i, HISTORY), Some((5, tag)));
            let (p0, p1, p2, p5, p6) = (
                small_path(0),
                small_path(101),
                small_path(202),
                small_path(505),
                small_path(606),
            );
            ok(s.create_branch("feature", &base)?, "create feature")?;
            let (a, b) = (at(0, "feature edit"), at(101, "feature edit"));
            let f1 = ok(
                s.commit(&CommitSpec {
                    branch: "feature",
                    expected: Some(&base),
                    changes: &[
                        Change {
                            path: &p0,
                            op: Op::Upsert(&a),
                        },
                        Change {
                            path: &p1,
                            op: Op::Upsert(&b),
                        },
                    ],
                    message: "feature: conflicting edit\n",
                    time: T0 + 1001,
                })?,
                "feature commit",
            )?;
            let (a, b) = (at(0, "main edit"), at(202, "main edit"));
            let m1 = ok(
                s.commit(&CommitSpec {
                    branch: "main",
                    expected: Some(&base),
                    changes: &[
                        Change {
                            path: &p0,
                            op: Op::Upsert(&a),
                        },
                        Change {
                            path: &p2,
                            op: Op::Upsert(&b),
                        },
                    ],
                    message: "main: conflicting edit\n",
                    time: T0 + 1002,
                })?,
                "main commit",
            )?;
            let t = Instant::now();
            let conflict = s.merge("main", &f1, &m1, "merge feature\n", T0 + 1003)?;
            out["conflict_merge_ms"] = json!(ms(t));
            assert_eq!(
                conflict,
                MergeOutcome::Conflict(vec![p0.clone()]),
                "the conflict is found"
            );
            assert_eq!(
                s.resolve("main")?.as_deref(),
                Some(m1.as_str()),
                "a conflict leaves main alone"
            );
            ok(s.create_branch("feature2", &base)?, "create feature2")?;
            let a = at(505, "feature2 edit");
            let f2 = ok(
                s.commit(&CommitSpec {
                    branch: "feature2",
                    expected: Some(&base),
                    changes: &[
                        Change {
                            path: &p5,
                            op: Op::Upsert(&a),
                        },
                        Change {
                            path: "new/added.txt",
                            op: Op::Upsert(b"added on feature2\n"),
                        },
                        Change {
                            path: &p6,
                            op: Op::Delete,
                        },
                    ],
                    message: "feature2: clean edit\n",
                    time: T0 + 1004,
                })?,
                "feature2 commit",
            )?;
            let t = Instant::now();
            let merged = s.merge("main", &f2, &m1, "merge feature2\n", T0 + 1005)?;
            out["clean_merge_ms"] = json!(ms(t));
            let MergeOutcome::Merged(m) = merged else {
                return Err(format!("expected a merge commit, got {merged:?}").into());
            };
            assert_eq!(
                s.merge("main", &f2, &m1, "merge feature2 again\n", T0 + 1006)?,
                MergeOutcome::Refused(Refused::Moved {
                    current: Some(m.clone())
                }),
                "a merge against a stale target is refused"
            );
            let read = |p: &str| -> R<Option<Vec<u8>>> {
                let mut b = Vec::new();
                Ok(s.read_file("main", p, false, &mut b)?.map(|_| b))
            };
            assert_eq!(read(&p0)?, Some(at(0, "main edit")));
            assert_eq!(read(&p2)?, Some(at(202, "main edit")));
            assert_eq!(read(&p5)?, Some(at(505, "feature2 edit")));
            assert_eq!(
                read("new/added.txt")?,
                Some(b"added on feature2\n".to_vec())
            );
            assert_eq!(read(&p6)?, None, "the delete is merged");
            out["merge_commit"] = json!(m);
            state["f1"] = json!(f1);
            state["m1"] = json!(m1);
            state["merge"] = json!(m);
            save_state(dir, &state)?;
        }
        "gc" => {
            let s = S::open(&repo, durable)?;
            let t = Instant::now();
            out["gc"] = s.gc()?;
            out["ms"] = json!(ms(t));
        }
        "census-before-gc" | "census-after-gc" => {
            // git's view of the store, in a step of its own so that its
            // children are not counted against gc
            out["objects"] = object_census(&repo)?;
            out["repo_bytes"] = json!(sh("du", &["-sb", repo.to_str().ok_or("path")?])
                .split_whitespace()
                .next()
                .and_then(|n| n.parse::<u64>().ok()));
        }
        "fsck" => {
            let o = git_inspect(
                &repo,
                &[
                    "fsck",
                    "--full",
                    "--strict",
                    "--no-progress",
                    "--no-dangling",
                ],
            )?;
            out["git_fsck_ok"] = json!(o.status.success());
            out["git_fsck_output"] = json!(String::from_utf8_lossy(&o.stderr)
                .lines()
                .chain(String::from_utf8_lossy(&o.stdout).lines())
                .take(20)
                .map(String::from)
                .collect::<Vec<_>>());
            let state = load_state(dir)?;
            let c = commits(&state);
            out["ids"] = json!({ "c0": c.first(), "c200": c.get(HISTORY), "big": state["big"], "f1": state["f1"], "m1": state["m1"], "merge": state["merge"] });
        }
        other => return Err(format!("no step {other}").into()),
    }
    out["baseline_vm_hwm_kib"] = json!(baseline);
    out["rss"] = sampler.finish();
    Ok(out)
}

// ---------------------------------------------------------------- dispatch

fn durable_of(config: &str) -> bool {
    !config.ends_with("-nofsync")
}

macro_rules! with_store {
    ($config:expr, $f:ident ( $($arg:expr),* )) => {
        match $config.split('-').next().unwrap_or("") {
            "git2" => $f::<lib_git2::Git2>($($arg),*),
            "gix" => $f::<lib_gix::Gix>($($arg),*),
            "git" => $f::<lib_git::Git>($($arg),*),
            other => Err(format!("no library {other}").into()),
        }
    };
}

fn run_step(config: &str, name: &str, dir: &Path) -> R<Value> {
    let durable = durable_of(config);
    with_store!(config, step(name, dir, durable))
}

fn exe() -> PathBuf {
    std::env::current_exe().expect("own path")
}

fn spike_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn sh(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn machine(runs: &Path) -> Value {
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .unwrap_or_default()
        .lines()
        .find_map(|l| {
            l.strip_prefix("model name")
                .map(|r| r.trim_start_matches([' ', '\t', ':']).to_string())
        })
        .unwrap_or_default();
    let rustc = match sh("rustc", &["--version"]) {
        v if v.is_empty() => sh(
            &format!(
                "{}/.cargo/bin/rustc",
                std::env::var("HOME").unwrap_or_default()
            ),
            &["--version"],
        ),
        v => v,
    };
    json!({
        "cpu": cpu,
        "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "kernel": std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim(),
        "filesystem": sh("stat", &["-f", "-c", "%T", runs.to_str().unwrap_or(".")]),
        "git": sh("git", &["--version"]),
        "rustc": rustc,
        "libraries": { "git2": "0.21.0", "libgit2": "1.9.7 (vendored, static)", "gix": "0.88.0" },
        "page_cache": "warm: every read follows its write; dropping caches needs root",
    })
}

fn runs_dir() -> PathBuf {
    spike_dir().join("target/runs")
}

fn fresh_dir(tag: &str) -> R<PathBuf> {
    let d = runs_dir().join(format!(
        "{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis()
    ));
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

fn run(config: &str, samples: usize) -> R<()> {
    let mut all = Vec::new();
    for n in 0..samples {
        let dir = fresh_dir(config)?;
        let mut sample = serde_json::Map::new();
        for st in STEPS {
            let o = Command::new(exe())
                .args(["step", config, st])
                .arg(&dir)
                .stderr(Stdio::inherit())
                .output()?;
            if !o.status.success() {
                return Err(format!(
                    "{config} sample {n} step {st} failed: {} (kept {})",
                    o.status,
                    dir.display()
                )
                .into());
            }
            let line = String::from_utf8_lossy(&o.stdout)
                .lines()
                .last()
                .unwrap_or("")
                .to_string();
            let v: Value = serde_json::from_str(&line)?;
            eprintln!("{config} #{n} {st}: {}", summary_line(&v));
            sample.insert(st.to_string(), v);
        }
        all.push(Value::Object(sample));
        std::fs::remove_dir_all(&dir)?;
    }
    let ev = json!({
        "config": config,
        "durable": if config == "gix" { json!("n/a: gix never fsyncs") } else { json!(durable_of(config)) },
        "machine": machine(&runs_dir()),
        "summary": summarize(&all),
        "samples": all,
    });
    let path = spike_dir().join(format!("evidence/{config}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(&ev)?)?;
    eprintln!("wrote {}", path.display());
    Ok(())
}

fn summary_line(v: &Value) -> String {
    let keys = [
        "ms",
        "big_write_ms",
        "big_read_ms",
        "conflict_merge_ms",
        "clean_merge_ms",
        "ff_ms",
        "hundred_small_reads_ms",
    ];
    let mut s: Vec<String> = keys
        .iter()
        .filter_map(|k| v.get(*k).map(|x| format!("{k}={x}")))
        .collect();
    s.push(format!("hwm={}KiB", v["rss"]["vm_hwm_kib"]));
    s.join(" ")
}

/// Median, min, and max of every numeric leaf across samples.
fn summarize(samples: &[Value]) -> Value {
    fn leaves(prefix: String, v: &Value, out: &mut std::collections::BTreeMap<String, Vec<f64>>) {
        match v {
            Value::Number(n) => out
                .entry(prefix)
                .or_default()
                .push(n.as_f64().unwrap_or(0.0)),
            Value::Object(m) => {
                for (k, x) in m {
                    leaves(
                        if prefix.is_empty() {
                            k.clone()
                        } else {
                            format!("{prefix}.{k}")
                        },
                        x,
                        out,
                    );
                }
            }
            _ => {}
        }
    }
    let mut acc = std::collections::BTreeMap::new();
    for s in samples {
        leaves(String::new(), s, &mut acc);
    }
    let mut out = serde_json::Map::new();
    for (k, mut v) in acc {
        let med = percentile(&mut v, 0.5);
        out.insert(
            k,
            json!({ "median": med, "min": v[0], "max": v[v.len() - 1], "n": v.len() }),
        );
    }
    Value::Object(out)
}

// ---------------------------------------------------------------- crash

fn rand_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// The writer the crash test kills: commits on main until killed; exits 3
/// when a stale lock stops it.
fn crash_writer<S: Store>(dir: &Path, durable: bool) -> R<()> {
    let s = S::open(dir, durable)?;
    let start = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos() as u64;
    for n in 0u64.. {
        let tip = s.resolve("main")?;
        let big = rand_bytes(start ^ n, 1 << 20);
        let counter = format!("{start} {n}\n");
        let p = format!("crash/{}.bin", n % 50);
        let changes = [
            Change {
                path: &p,
                op: Op::Upsert(&big),
            },
            Change {
                path: "crash/counter.txt",
                op: Op::Upsert(counter.as_bytes()),
            },
        ];
        match s.commit(&CommitSpec {
            branch: "main",
            expected: tip.as_deref(),
            changes: &changes,
            message: "crash writer\n",
            time: T0 + n as i64,
        })? {
            Ok(_) => {}
            Err(Refused::Locked(m)) => {
                println!("locked: {m}");
                std::process::exit(3);
            }
            Err(r) => {
                println!("refused: {r:?}");
                std::process::exit(4);
            }
        }
    }
    Ok(())
}

fn stray_files(repo: &Path) -> R<(Vec<String>, Vec<String>)> {
    let (mut locks, mut temps) = (Vec::new(), Vec::new());
    let mut stack = vec![repo.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            let p = e.path();
            if e.file_type()?.is_dir() {
                stack.push(p);
                continue;
            }
            let rel = p.strip_prefix(repo)?.to_string_lossy().into_owned();
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".lock") {
                locks.push(rel);
            } else if name.starts_with("tmp")
                || name.starts_with(".tmp")
                || name.starts_with("incoming-")
                || name.starts_with("streamed")
            {
                temps.push(rel);
            }
        }
    }
    Ok((locks, temps))
}

fn crash(config: &str, rounds: usize) -> R<()> {
    let durable = durable_of(config);
    let dir = fresh_dir(&format!("crash-{config}"))?;
    let repo = dir.join("repo.git");
    with_store!(config, init_only(&repo, durable))?;
    let mut rng = 0x1234_5678_9abc_def1u64;
    let (mut fsck_failures, mut stale_lock_rounds, mut blocked_writers) =
        (Vec::new(), Vec::new(), 0);
    let mut writer_exits = std::collections::BTreeMap::<String, usize>::new();
    for round in 0..rounds {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let wait = 20 + rng % 400;
        let mut child = Command::new(exe())
            .args(["crash-writer", config])
            .arg(&repo)
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()?;
        std::thread::sleep(Duration::from_millis(wait));
        // the whole group: the CLI's git children die with their parent
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
        let st = child.wait()?;
        *writer_exits.entry(st.to_string()).or_default() += 1;
        std::thread::sleep(Duration::from_millis(20));
        let (locks, _) = stray_files(&repo)?;
        if !locks.is_empty() {
            // Does the stale lock block the next writer? A lock on the
            // branch does (it exits 3 at once); a lock on, say, the CLI's
            // temporary ref does not, and that writer runs until stopped.
            let mut probe = Command::new(exe())
                .args(["crash-writer", config])
                .arg(&repo)
                .process_group(0)
                .stdout(Stdio::piped())
                .spawn()?;
            let deadline = Instant::now() + Duration::from_secs(3);
            let blocked = loop {
                if let Some(st) = probe.try_wait()? {
                    break st.code() == Some(3);
                }
                if Instant::now() > deadline {
                    unsafe { libc::kill(-(probe.id() as i32), libc::SIGKILL) };
                    break false;
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            let o = probe.wait_with_output()?;
            if blocked {
                blocked_writers += 1;
            }
            let next = if blocked {
                String::from_utf8_lossy(&o.stdout).trim().to_string()
            } else {
                "not blocked: it committed until stopped after 3 s".into()
            };
            stale_lock_rounds.push(json!({ "round": round, "killed_after_ms": wait, "locks": locks, "next_writer": next }));
            for l in &locks {
                let _ = std::fs::remove_file(repo.join(l));
            }
        }
        let o = git_inspect(&repo, &["fsck", "--full", "--no-progress", "--no-dangling"])?;
        let head = git_inspect(
            &repo,
            &["rev-parse", "--verify", "-q", "refs/heads/main^{commit}"],
        )?;
        if !o.status.success() || !head.status.success() {
            fsck_failures.push(json!({ "round": round, "killed_after_ms": wait, "writer_status": st.to_string(), "fsck": String::from_utf8_lossy(&o.stderr).lines().take(10).map(String::from).collect::<Vec<_>>() }));
        }
    }
    let count = git_inspect(&repo, &["rev-list", "--count", "refs/heads/main"])?;
    let (_, temps) = stray_files(&repo)?;
    let ev = json!({
        "config": config,
        "what": "kill -9 of a writer (its whole process group) committing a 1 MiB file and a counter in a loop, after 20 to 420 ms; then git fsck --full and refs/heads/main resolved. Process death only: the page cache survives, so this tests atomic renames and locks, not fsync.",
        "rounds": rounds,
        "writer_exits": writer_exits,
        "commits_on_main": String::from_utf8_lossy(&count.stdout).trim(),
        "fsck_failures": fsck_failures,
        "stale_ref_lock_rounds": stale_lock_rounds,
        "writers_blocked_by_stale_lock": blocked_writers,
        "temp_files_left": temps.len(),
        "temp_files_example": temps.iter().take(5).collect::<Vec<_>>(),
        "machine": machine(&runs_dir()),
    });
    let path = spike_dir().join(format!("evidence/crash-{config}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(&ev)?)?;
    eprintln!(
        "{}",
        serde_json::to_string_pretty(
            &json!({ "fsck_failures": ev["fsck_failures"].as_array().map(|a| a.len()), "stale_lock_rounds": ev["stale_ref_lock_rounds"].as_array().map(|a| a.len()), "temps": ev["temp_files_left"], "commits": ev["commits_on_main"] })
        )?
    );
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}

fn init_only<S: Store>(repo: &Path, durable: bool) -> R<()> {
    let s = S::init(repo, durable)?;
    let paths: Vec<String> = (0..SMALL_FILES).map(small_path).collect();
    let contents: Vec<Vec<u8>> = (0..SMALL_FILES).map(|i| small_content(i, 0)).collect();
    let changes: Vec<Change<'_>> = paths
        .iter()
        .zip(&contents)
        .map(|(p, c)| Change {
            path: p,
            op: Op::Upsert(c),
        })
        .collect();
    ok(
        s.commit(&CommitSpec {
            branch: "main",
            expected: None,
            changes: &changes,
            message: "base\n",
            time: T0,
        })?,
        "base commit",
    )?;
    Ok(())
}

// ---------------------------------------------------------------- contention

/// `threads` writers race for one branch, each until it has `each`
/// commits in; compare-and-swap must lose none.
fn concurrent<S: ReadMany>(config: &str, threads: usize, each: usize) -> R<Value> {
    let durable = durable_of(config);
    let dir = fresh_dir(&format!("concurrent-{config}"))?;
    let repo = dir.join("repo.git");
    init_only::<S>(&repo, durable)?;
    let t = Instant::now();
    let results: Vec<R<(usize, usize, usize)>> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..threads)
            .map(|w| {
                let repo = repo.clone();
                sc.spawn(move || -> R<(usize, usize, usize)> {
                    let s = S::open(&repo, durable)?;
                    let (mut moved, mut locked, mut attempts) = (0, 0, 0);
                    let mut done = 0;
                    while done < each {
                        attempts += 1;
                        if attempts > 100 * each {
                            return Err("too many attempts".into());
                        }
                        let tip = s.resolve("main")?;
                        let p = format!("w{w:02}/n{done:03}.txt");
                        let body = format!("writer {w} commit {done}\n");
                        let changes = [Change {
                            path: &p,
                            op: Op::Upsert(body.as_bytes()),
                        }];
                        let msg = format!("writer {w} commit {done}\n");
                        match s.commit(&CommitSpec {
                            branch: "main",
                            expected: tip.as_deref(),
                            changes: &changes,
                            message: &msg,
                            time: T0 + 10 + done as i64,
                        })? {
                            Ok(_) => done += 1,
                            Err(Refused::Moved { .. }) => moved += 1,
                            Err(Refused::Locked(_)) => locked += 1,
                            Err(Refused::Exists) => return Err("exists on an update".into()),
                        }
                    }
                    Ok((attempts, moved, locked))
                })
            })
            .collect();
        hs.into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err("writer panicked".into())))
            .collect()
    });
    let elapsed = ms(t);
    let (mut attempts, mut moved, mut locked) = (0, 0, 0);
    for r in results {
        let (a, m, l) = r?;
        attempts += a;
        moved += m;
        locked += l;
    }
    // every commit landed: the history is the base plus all of them, and
    // every writer's every file is in the tip
    let count = git_inspect(
        &repo,
        &["rev-list", "--count", "--first-parent", "refs/heads/main"],
    )?;
    let count: usize = String::from_utf8_lossy(&count.stdout).trim().parse()?;
    let s = S::open(&repo, durable)?;
    let paths: Vec<String> = (0..threads)
        .flat_map(|w| (0..each).map(move |n| format!("w{w:02}/n{n:03}.txt")))
        .collect();
    let mut sink = Vec::new();
    let got = s.read_many("main", &paths, &mut sink)?;
    let fsck = git_inspect(&repo, &["fsck", "--full", "--no-progress", "--no-dangling"])?;
    let v = json!({
        "threads": threads,
        "commits_each": each,
        "ms": elapsed,
        "commits_per_s": (threads * each) as f64 / (elapsed / 1e3),
        "attempts": attempts,
        "refused_moved": moved,
        "refused_locked": locked,
        "history_len": count,
        "history_len_expected": 1 + threads * each,
        "files_present": got.len(),
        "lost_updates": (1 + threads * each) as i64 - count as i64,
        "git_fsck_ok": fsck.status.success(),
    });
    assert_eq!(count, 1 + threads * each, "no lost update");
    std::fs::remove_dir_all(&dir)?;
    Ok(v)
}

/// `threads` writers, each on a repository of its own.
fn parallel<S: Store>(config: &str, threads: usize, each: usize) -> R<Value> {
    let durable = durable_of(config);
    let dir = fresh_dir(&format!("parallel-{config}"))?;
    for w in 0..threads {
        init_only::<S>(&dir.join(format!("r{w}.git")), durable)?;
    }
    let t = Instant::now();
    let results: Vec<R<()>> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..threads)
            .map(|w| {
                let repo = dir.join(format!("r{w}.git"));
                sc.spawn(move || -> R<()> {
                    let s = S::open(&repo, durable)?;
                    let mut tip = s.resolve("main")?;
                    for n in 0..each {
                        let p = format!("p/n{n:03}.txt");
                        let body = format!("commit {n}\n");
                        let changes = [Change {
                            path: &p,
                            op: Op::Upsert(body.as_bytes()),
                        }];
                        let id = ok(
                            s.commit(&CommitSpec {
                                branch: "main",
                                expected: tip.as_deref(),
                                changes: &changes,
                                message: "parallel\n",
                                time: T0 + n as i64,
                            })?,
                            "parallel commit",
                        )?;
                        tip = Some(id);
                    }
                    Ok(())
                })
            })
            .collect();
        hs.into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err("writer panicked".into())))
            .collect()
    });
    let elapsed = ms(t);
    for r in results {
        r?;
    }
    std::fs::remove_dir_all(&dir)?;
    Ok(
        json!({ "threads": threads, "commits_each": each, "ms": elapsed, "commits_per_s": (threads * each) as f64 / (elapsed / 1e3) }),
    )
}

fn contention(config: &str) -> R<()> {
    let mut v = serde_json::Map::new();
    v.insert(
        "concurrent_16x20_one_branch".into(),
        with_store!(config, concurrent(config, 16, 20))?,
    );
    for threads in [1, 8, 32] {
        v.insert(
            format!("parallel_{threads}x100_own_repos"),
            with_store!(config, parallel(config, threads, 100))?,
        );
    }
    v.insert("machine".into(), machine(&runs_dir()));
    let path = spike_dir().join(format!("evidence/contention-{config}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(&Value::Object(v.clone()))?)?;
    eprintln!("{}", serde_json::to_string_pretty(&Value::Object(v))?);
    Ok(())
}

// ---------------------------------------------------------------- tables

const CONFIGS: &[&str] = &[
    "git2",
    "git2-packed",
    "git2-nofsync",
    "gix",
    "git",
    "git-nofsync",
];

fn read_evidence(name: &str) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(spike_dir().join(format!("evidence/{name}.json"))).ok()?)
        .ok()
}

/// "median (min to max)" of a summary key, scaled.
fn cell(ev: &Value, key: &str, scale: f64, digits: usize) -> String {
    let s = &ev["summary"][key];
    let (Some(med), Some(lo), Some(hi)) =
        (s["median"].as_f64(), s["min"].as_f64(), s["max"].as_f64())
    else {
        return "n/a".into();
    };
    let f = |x: f64| format!("{:.*}", digits, x * scale);
    if f(lo) == f(hi) {
        f(med)
    } else {
        format!("{} ({}–{})", f(med), f(lo), f(hi))
    }
}

/// The doc's tables (docs/library.md), from the evidence files.
fn table() -> R<()> {
    let evs: Vec<(&str, Value)> = CONFIGS
        .iter()
        .filter_map(|c| read_evidence(c).map(|e| (*c, e)))
        .collect();
    const MIB: f64 = 1.0 / 1024.0;
    let rows: &[(&str, &str, f64, usize)] = &[
        ("init a bare repo (ms)", "init.ms", 1.0, 2),
        (
            "write the 50 MiB blob, streamed in (ms)",
            "commit-initial.big_write_ms",
            1.0,
            0,
        ),
        (
            "  its peak heap (MiB, the git child's for the CLI)",
            "commit-initial.rss_after_big_write.peak_rss_anon_kib",
            MIB,
            1,
        ),
        (
            "1,000 files and the first commit (ms)",
            "commit-initial.small_files_and_commit_ms",
            1.0,
            1,
        ),
        (
            "one commit of 5 files, history (ms, median of 200)",
            "history.per_commit_ms.median",
            1.0,
            2,
        ),
        ("  p95 of the 200 (ms)", "history.per_commit_ms.p95", 1.0, 2),
        (
            "read one small file, fresh handle (ms)",
            "read.one_small_read_ms",
            1.0,
            2,
        ),
        (
            "read 100 small files, one handle (ms)",
            "read.hundred_small_reads_ms",
            1.0,
            2,
        ),
        (
            "read the 50 MiB blob, before gc (ms)",
            "read.big_read_ms",
            1.0,
            0,
        ),
        (
            "  its peak heap (MiB)",
            "read.rss_after_big_read.peak_rss_anon_kib",
            MIB,
            1,
        ),
        (
            "  its peak mapped file pages (MiB)",
            "read.rss_after_big_read.peak_rss_file_kib",
            MIB,
            1,
        ),
        (
            "last commit of 100 paths over 201 commits (ms)",
            "last-commit.ms",
            1.0,
            1,
        ),
        ("create a branch (ms)", "branch-ff.create_ms", 1.0, 2),
        ("refuse a stale CAS (ms)", "branch-ff.stale_cas_ms", 1.0, 3),
        ("fast-forward with CAS (ms)", "branch-ff.ff_ms", 1.0, 2),
        (
            "merge that conflicts, detected (ms)",
            "merge.conflict_merge_ms",
            1.0,
            2,
        ),
        (
            "true merge, committed with CAS (ms)",
            "merge.clean_merge_ms",
            1.0,
            2,
        ),
        ("gc / repack (ms)", "gc.ms", 1.0, 0),
        ("  its peak heap (MiB)", "gc.rss.peak_rss_anon_kib", MIB, 1),
        (
            "  pack after (MiB)",
            "census-after-gc.objects.size_pack",
            MIB,
            2,
        ),
        (
            "  deltified objects of 3,435",
            "census-after-gc.objects.deltified_objects",
            1.0,
            0,
        ),
        (
            "read the 50 MiB blob, after gc (ms)",
            "read-after-gc.big_read_ms",
            1.0,
            0,
        ),
        (
            "  its peak heap (MiB)",
            "read-after-gc.rss_after_big_read.peak_rss_anon_kib",
            MIB,
            1,
        ),
        (
            "read 100 small files, after gc (ms)",
            "read-after-gc.hundred_small_reads_ms",
            1.0,
            2,
        ),
    ];
    // for the CLI the work happens in git's own processes: their heap
    let cli_keys: &[(&str, &str)] = &[
        (
            "commit-initial.rss_after_big_write.peak_rss_anon_kib",
            "commit-initial.rss_after_big_write.git_child_peak_rss_anon_kib",
        ),
        (
            "read.rss_after_big_read.peak_rss_anon_kib",
            "read.rss_after_big_read.git_child_peak_rss_anon_kib",
        ),
        (
            "read.rss_after_big_read.peak_rss_file_kib",
            "read.rss_after_big_read.git_child_peak_rss_file_kib",
        ),
        ("gc.rss.peak_rss_anon_kib", "gc.rss.children_maxrss_kib"),
        (
            "read-after-gc.rss_after_big_read.peak_rss_anon_kib",
            "read-after-gc.rss_after_big_read.git_child_peak_rss_anon_kib",
        ),
    ];
    println!(
        "| Step | {} |",
        evs.iter()
            .map(|(c, _)| format!("`{c}`"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    println!("|---|{}", "---|".repeat(evs.len()));
    for (label, key, scale, digits) in rows {
        let cells: Vec<String> = evs
            .iter()
            .map(|(c, e)| {
                let k = if c.starts_with("git") && !c.starts_with("git2") {
                    cli_keys
                        .iter()
                        .find(|(a, _)| a == key)
                        .map_or(*key, |(_, b)| b)
                } else {
                    key
                };
                cell(e, k, *scale, *digits)
            })
            .collect();
        println!("| {label} | {} |", cells.join(" | "));
    }
    let methods: Vec<String> = evs
        .iter()
        .map(|(_, e)| {
            let s = &e["samples"][0];
            format!(
                "{} / {}",
                s["read"]["big_read_method"].as_str().unwrap_or("?"),
                s["read-after-gc"]["big_read_method"]
                    .as_str()
                    .unwrap_or("?")
            )
        })
        .collect();
    println!(
        "| the big read, before / after gc | {} |",
        methods.join(" | ")
    );
    let fsck: Vec<String> = evs
        .iter()
        .map(|(_, e)| {
            let all = e["samples"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter(|s| s["fsck"]["git_fsck_ok"] == json!(true))
                        .count()
                })
                .unwrap_or(0);
            format!(
                "{all} of {}",
                e["samples"].as_array().map_or(0, |a| a.len())
            )
        })
        .collect();
    println!(
        "| `git fsck --full --strict` passes | {} |",
        fsck.join(" | ")
    );
    let ids: std::collections::BTreeSet<String> = evs
        .iter()
        .flat_map(|(_, e)| e["samples"].as_array().cloned().unwrap_or_default())
        .map(|s| s["fsck"]["ids"].to_string())
        .collect();
    println!(
        "
Distinct id sets across every sample of every config: {} ({}).",
        ids.len(),
        ids.iter().next().cloned().unwrap_or_default()
    );

    println!(
        "
| Contention | {} |",
        evs.iter()
            .map(|(c, _)| format!("`{c}`"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    println!("|---|{}", "---|".repeat(evs.len()));
    let cont: Vec<(&str, Option<Value>)> = evs
        .iter()
        .map(|(c, _)| (*c, read_evidence(&format!("contention-{c}"))))
        .collect();
    let crow = |label: &str, f: &dyn Fn(&Value) -> String| {
        let cells: Vec<String> = cont
            .iter()
            .map(|(_, v)| v.as_ref().map_or("n/a".into(), f))
            .collect();
        println!("| {label} | {} |", cells.join(" | "));
    };
    let n = |v: &Value| v.as_f64().map_or("n/a".into(), |x| format!("{x:.0}"));
    crow("16 writers, one branch: commits landed / expected", &|v| {
        let c = &v["concurrent_16x20_one_branch"];
        format!("{} / {}", c["history_len"], c["history_len_expected"])
    });
    crow("  attempts (refused: moved, locked)", &|v| {
        let c = &v["concurrent_16x20_one_branch"];
        format!(
            "{} ({}, {})",
            c["attempts"], c["refused_moved"], c["refused_locked"]
        )
    });
    crow("  commits/s", &|v| {
        n(&v["concurrent_16x20_one_branch"]["commits_per_s"])
    });
    for t in [1, 8, 32] {
        crow(&format!("{t} writer(s), own repos: commits/s"), &|v| {
            n(&v[format!("parallel_{t}x100_own_repos")]["commits_per_s"])
        });
    }

    println!(
        "
| kill -9 | {} |",
        evs.iter()
            .map(|(c, _)| format!("`{c}`"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    println!("|---|{}", "---|".repeat(evs.len()));
    let crash: Vec<Option<Value>> = evs
        .iter()
        .map(|(c, _)| read_evidence(&format!("crash-{c}")))
        .collect();
    let krow = |label: &str, f: &dyn Fn(&Value) -> String| {
        let cells: Vec<String> = crash
            .iter()
            .map(|v| v.as_ref().map_or("n/a".into(), f))
            .collect();
        println!("| {label} | {} |", cells.join(" | "));
    };
    krow("rounds / commits on main after", &|v| {
        format!(
            "{} / {}",
            v["rounds"],
            v["commits_on_main"].as_str().unwrap_or("?")
        )
    });
    krow("fsck failures or unresolvable main", &|v| {
        v["fsck_failures"]
            .as_array()
            .map_or(0, |a| a.len())
            .to_string()
    });
    krow(
        "stale ref locks left (each blocked the next writer)",
        &|v| {
            format!(
                "{} ({})",
                v["stale_ref_lock_rounds"].as_array().map_or(0, |a| a.len()),
                v["writers_blocked_by_stale_lock"]
            )
        },
    );
    krow("temporary files left", &|v| {
        v["temp_files_left"].to_string()
    });
    Ok(())
}

/// `git cat-file --batch` reading a packed 50 MiB blob at git's default
/// `core.bigFileThreshold` (512m) and at the spike's (1m): git streams a
/// packed blob only when it is above the threshold.
fn probe_git_threshold() -> R<()> {
    use model::Store as _;
    let mut v = serde_json::Map::new();
    for threshold in ["512m", "1m"] {
        let dir = fresh_dir("probe-git")?;
        let repo = dir.join("repo.git");
        let g = lib_git::Git::init(&repo, false)?;
        let big = g.write_blob_from(&mut BigReader::new())?;
        let changes = [Change {
            path: BIG_PATH,
            op: Op::UpsertBlob(&big),
        }];
        ok(
            g.commit(&CommitSpec {
                branch: "main",
                expected: None,
                changes: &changes,
                message: "big\n",
                time: T0,
            })?,
            "probe commit",
        )?;
        let o = git_inspect(&repo, &["gc", "--prune=now", "--quiet"])?;
        assert!(o.status.success(), "git gc");
        let o = git_inspect(&repo, &["config", "core.bigFileThreshold", threshold])?;
        assert!(o.status.success(), "git config");
        lib_git::reset_child_peaks();
        let mut f = Fnv::new();
        let t = Instant::now();
        let r = g
            .read_file("main", BIG_PATH, true, &mut f)?
            .ok_or("no big file")?;
        let elapsed = ms(t);
        assert_eq!((f.len, r.len), (BIG_LEN, BIG_LEN));
        let (anon, file) = lib_git::child_peaks();
        v.insert(
            format!("core.bigFileThreshold={threshold}"),
            json!({ "read_ms": elapsed, "cat_file_peak_rss_anon_kib": anon, "cat_file_peak_rss_file_kib": file, "census": object_census(&repo)? }),
        );
        std::fs::remove_dir_all(&dir)?;
    }
    let path = spike_dir().join("evidence/probe-git-bigfilethreshold.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&Value::Object(v.clone()))?)?;
    eprintln!("{}", serde_json::to_string_pretty(&Value::Object(v))?);
    Ok(())
}

/// gix's own `Repository::write_blob_stream`, against the spool and
/// `write_stream` the spike uses: the peak heap of each for the 50 MiB.
fn probe_gix_write_blob_stream() -> R<()> {
    let mut v = serde_json::Map::new();
    for (name, naive) in [
        ("write_blob_stream", true),
        ("spool_then_odb_write_stream", false),
    ] {
        let dir = fresh_dir("probe-gix")?;
        let o = Command::new(exe())
            .args(["probe-gix-one", if naive { "naive" } else { "spool" }])
            .arg(dir.join("repo.git"))
            .output()?;
        if !o.status.success() {
            return Err(format!("probe {name}: {}", String::from_utf8_lossy(&o.stderr)).into());
        }
        v.insert(name.into(), serde_json::from_slice(&o.stdout)?);
        std::fs::remove_dir_all(&dir)?;
    }
    let path = spike_dir().join("evidence/probe-gix-write-blob-stream.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&Value::Object(v.clone()))?)?;
    eprintln!("{}", serde_json::to_string_pretty(&Value::Object(v))?);
    Ok(())
}

fn probe_gix_one(how: &str, repo: &Path) -> R<()> {
    use model::Store as _;
    let s = lib_gix::Gix::init(repo, false)?;
    let sampler = Sampler::start();
    let t = Instant::now();
    let id = if how == "naive" {
        s.repo.write_blob_stream(BigReader::new())?.to_string()
    } else {
        s.write_blob_from(&mut BigReader::new())?
    };
    let elapsed = ms(t);
    println!(
        "{}",
        json!({ "blob": id, "ms": elapsed, "rss": sampler.finish() })
    );
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let a = |i: usize| args.get(i).map(String::as_str).unwrap_or("");
    if a(2) == "git2-packed" {
        lib_git2::PACKED.store(true, Ordering::Relaxed);
    }
    let res: R<()> = match a(1) {
        "step" => run_step(a(2), a(3), Path::new(a(4))).and_then(|v| {
            println!("{}", serde_json::to_string(&v)?);
            io::stdout().flush()?;
            Ok(())
        }),
        "run" => run(a(2), a(3).parse().unwrap_or(5)),
        "crash" => crash(a(2), a(3).parse().unwrap_or(40)),
        "crash-writer" => {
            let durable = durable_of(a(2));
            with_store!(a(2), crash_writer(Path::new(a(3)), durable))
        }
        "contention" => contention(a(2)),
        "table" => table(),
        "probe-gix-write-blob-stream" => probe_gix_write_blob_stream(),
        "probe-gix-one" => probe_gix_one(a(2), Path::new(a(3))),
        "probe-git-threshold" => probe_git_threshold(),
        _ => Err(
            "usage: library-spike run|crash|contention <git2|git2-nofsync|gix|git|git-nofsync> [n]"
                .into(),
        ),
    };
    if let Err(e) = res {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
