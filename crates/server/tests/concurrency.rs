//! The compare-and-swap under contention (docs/contract.md, section 9):
//! of two packs naming the same expected head exactly one lands, so many
//! writers retrying as fragment's clients retry make one linear history
//! with every write in it once.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use common::*;

const WRITERS: usize = 16;
const COMMITS_EACH: usize = 20;
/// A writer's attempts per commit before the test fails (the CLI makes
/// 3, the cell 5; here they race far harder than any fragment).
const ATTEMPTS_MAX: usize = 1000;

// Goal: sixteen writers racing on one branch, each until 20 of its
// commits land, retrying on 409 by reading the head again (the CLI's
// loop), make 321 commits of linear history with all 320 files, and no
// write lands twice or is lost. Method: threads over HTTP; then the
// history, the listing, and fsck.
#[test]
fn sixteen_writers_race_on_one_branch() {
    let fx = Arc::new(Fx::start("race"));
    let repo = fx.create_repo("race");
    let root = fx.commit_at_head(&repo, &[up("root.txt", b"root")]);
    let (attempts, refused) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let t0 = Instant::now();
    let threads: Vec<_> = (0..WRITERS)
        .map(|w| {
            let (fx, repo, attempts, refused) = (fx.clone(), repo.clone(), attempts.clone(), refused.clone());
            std::thread::spawn(move || {
                let mut landed = Vec::new();
                for k in 0..COMMITS_EACH {
                    let path = format!("w{w:02}/n{k:02}.txt");
                    let body = format!("writer {w} commit {k}");
                    let mut done = false;
                    for _ in 0..ATTEMPTS_MAX {
                        attempts.fetch_add(1, Ordering::Relaxed);
                        let head = fx.head(&repo, "main").unwrap();
                        let a = fx.commit(&repo, Some(&head), &[up(&path, body.as_bytes())]);
                        match a.status {
                            201 => {
                                landed.push(committed(&a.json()).unwrap());
                                done = true;
                                break;
                            }
                            409 => {
                                refused.fetch_add(1, Ordering::Relaxed);
                            }
                            _ => panic!("writer {w}: {a}"),
                        }
                    }
                    assert!(done, "writer {w} commit {k} never landed");
                }
                landed
            })
        })
        .collect();
    let landed: Vec<String> = threads.into_iter().flat_map(|t| t.join().unwrap()).collect();
    let secs = t0.elapsed().as_secs_f64();
    let history = fx.shas(&repo, "main", 100);
    assert_eq!(history.len(), 100);
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    // Bounded: 10 pages of 100 cover 321 commits.
    for _ in 0..10 {
        let q = match &cursor {
            None => "ref=main&limit=100".to_string(),
            Some(c) => format!("ref=main&limit=100&cursor={}", enc(c)),
        };
        let v = fx.get(&repo, "commits", &q).json();
        all.extend(v["commits"].as_array().unwrap().iter().map(|c| (c["sha"].as_str().unwrap().to_string(), c["parent_shas"].as_array().unwrap().len())));
        match v["next_cursor"].as_str() {
            Some(c) if v["has_more"] == true => cursor = Some(c.to_string()),
            _ => break,
        }
    }
    assert_eq!(all.len(), 1 + WRITERS * COMMITS_EACH, "every commit once");
    assert!(all.iter().all(|(_, parents)| *parents <= 1), "linear: no merge");
    assert_eq!(all.last().unwrap().0, root);
    let shas: std::collections::HashSet<&String> = all.iter().map(|(s, _)| s).collect();
    assert!(landed.iter().all(|s| shas.contains(s)), "every acknowledged commit is in the history");
    let files = fx.list(&repo, "main", 1000);
    assert_eq!(files.len(), 1 + WRITERS * COMMITS_EACH, "every file present");
    assert!(fx.fsck_all().iter().all(|(_, ok, e)| *ok || panic!("{e}")));
    eprintln!(
        "{{\"evidence\":\"race\",\"writers\":{WRITERS},\"commits_each\":{COMMITS_EACH},\"landed\":{},\"attempts\":{},\"refused_409\":{},\"secs\":{secs:.3},\"commits_per_s\":{:.1}}}",
        landed.len(),
        attempts.load(Ordering::Relaxed),
        refused.load(Ordering::Relaxed),
        landed.len() as f64 / secs
    );
}

// Goal: writers on repositories of their own do not wait on one another
// (a lock per repository). Method: sixteen threads, each committing 20
// times to its own repository; every commit lands on its first try.
#[test]
fn sixteen_writers_on_their_own_repositories() {
    let fx = Arc::new(Fx::start("own-repos"));
    let repos: Vec<String> = (0..WRITERS).map(|i| fx.create_repo(&format!("own-{i:02}"))).collect();
    let t0 = Instant::now();
    let threads: Vec<_> = repos
        .into_iter()
        .map(|repo| {
            let fx = fx.clone();
            std::thread::spawn(move || {
                let mut head: Option<String> = None;
                for k in 0..COMMITS_EACH {
                    let a = fx.commit(&repo, head.as_deref(), &[up(&format!("n{k}.txt"), b"x")]);
                    assert_eq!(a.status, 201, "{a}");
                    head = committed(&a.json());
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let secs = t0.elapsed().as_secs_f64();
    eprintln!("{{\"evidence\":\"own-repos\",\"writers\":{WRITERS},\"commits\":{},\"secs\":{secs:.3},\"commits_per_s\":{:.1}}}", WRITERS * COMMITS_EACH, (WRITERS * COMMITS_EACH) as f64 / secs);
    assert!(fx.fsck_all().iter().all(|(_, ok, _)| *ok));
}
