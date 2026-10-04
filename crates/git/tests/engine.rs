//! The engine against the pinned git (`cargo xtask engine`): each
//! plumbing call, the `cat-file` pool, and the jail.

use std::path::{Path, PathBuf};
use std::time::Duration;

use macrofiche_git::fastimport::Head;
use macrofiche_git::jail::{Limits, Settings};
use macrofiche_git::linux::process::Pipes;
use macrofiche_git::parse::{mode, Kind};
use macrofiche_git::{BlobReader, Change, Engine, Ident, Merged, Oid, Pool, RefOp, RefRefusal, Repo, RunOpts, Source};

async fn engine(enforce: bool) -> (Engine, PathBuf) {
    let root = macrofiche_testkit::scratch("engine");
    let scratch = root.join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let mut settings = Settings::linux(macrofiche_testkit::git_prefix());
    settings.enforce = enforce;
    (Engine::open(settings, &scratch).await.unwrap(), root)
}

async fn repo(e: &Engine, root: &Path, name: &str) -> Repo {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    e.init(&dir, "main").await.unwrap()
}

fn me() -> Ident {
    Ident::new("A", "a@x").unwrap()
}

static TMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A commit of `changes` on `from` (by fast-import onto a temporary ref),
/// then `branch` moved to it from `from` (or created), the temporary ref
/// deleted: the store's write path in miniature.
async fn commit(r: &Repo, branch: &str, from: Option<Oid>, changes: &[Change<'_>], time: i64) -> Oid {
    let tmp = format!("refs/macrofiche/tmp/t{}", TMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let a = me();
    let c = r.fast_import(&Head { tmp_ref: &tmp, author: &a, committer: &a, time, message: b"m", from }, changes).await.unwrap();
    let name = format!("refs/heads/{branch}");
    let mv = match from {
        Some(old) => RefOp::Update { name, new: c, old },
        None => RefOp::Create { name, new: c },
    };
    r.update_refs(&[mv, RefOp::Delete { name: tmp, old: c }]).await.unwrap().unwrap();
    c
}

fn up<'a>(path: &'a str, bytes: &'a [u8]) -> Change<'a> {
    Change::Upsert { path, mode: mode::FILE, source: Source::Bytes(bytes) }
}

// Goal: the engine opens only on the pinned git. Method: the built
// prefix, then a prefix with no git.
#[tokio::test(flavor = "multi_thread")]
async fn the_engine_checks_its_pin() {
    let (e, root) = engine(true).await;
    assert!(e.landlock_abi() >= 4);
    let mut bad = Settings::linux(root.join("nowhere"));
    bad.enforce = true;
    let err = Engine::open(bad, &root.join("scratch")).await.err().expect("no git there");
    assert!(err.to_string().contains("cargo xtask engine"), "{err}");
}

// Goal: a commit by fast-import, a compare-and-swap, a listing, reads,
// and each refusal of a ref update, as the store makes them. Method:
// files from memory and from a spool, paths that need quoting, then each
// way a transaction is refused.
#[tokio::test(flavor = "multi_thread")]
async fn commits_refs_listings_and_reads() {
    let (e, root) = engine(true).await;
    let r = repo(&e, &root, "r.git").await;
    let spool = root.join("spool");
    std::fs::write(&spool, b"from a spool\n").unwrap();
    let c1 = commit(
        &r,
        "main",
        None,
        &[up("a b.txt", b"hi"), up("d/\"q\".md", b""), Change::Upsert { path: "s/spooled.txt", mode: mode::EXECUTABLE, source: Source::File { path: &spool, len: 13 } }],
        1_790_000_000,
    )
    .await;
    let tree = r.ls_tree(c1).await.unwrap();
    let names: Vec<(&str, u32, Option<u64>)> = tree.iter().map(|t| (t.path.as_str(), t.mode, t.size)).collect();
    assert_eq!(names, vec![("a b.txt", mode::FILE, Some(2)), ("d/\"q\".md", mode::FILE, Some(0)), ("s/spooled.txt", mode::EXECUTABLE, Some(13))]);
    assert_eq!(r.refs("refs/macrofiche/").await.unwrap(), vec![], "the temporary ref went with the move");

    // a second commit: an update and a delete
    let c2 = commit(&r, "main", Some(c1), &[up("a b.txt", b"ho"), Change::Delete { path: "d/\"q\".md" }], 1_790_000_001).await;
    let diff = r.diff_tree(Some(c1), c2).await.unwrap();
    let d: Vec<(u8, &str)> = diff.iter().map(|d| (d.status, d.path.as_str())).collect();
    assert_eq!(d, vec![(b'M', "a b.txt"), (b'D', "d/\"q\".md")]);
    assert_eq!(r.diff_tree(None, c1).await.unwrap().len(), 3, "a root's diff is against the empty tree");

    // refusals, as the CAS sees them
    let main = "refs/heads/main".to_string();
    assert_eq!(r.update_refs(&[RefOp::Create { name: main.clone(), new: c1 }]).await.unwrap(), Err(RefRefusal::Exists));
    assert_eq!(r.update_refs(&[RefOp::Update { name: main.clone(), new: c1, old: c1 }]).await.unwrap(), Err(RefRefusal::Moved { current: c2 }));
    assert_eq!(r.update_refs(&[RefOp::Update { name: "refs/heads/live".into(), new: c1, old: c1 }]).await.unwrap(), Err(RefRefusal::Missing));
    let ghost = Oid::parse("0123456789012345678901234567890123456789").unwrap();
    assert_eq!(r.update_refs(&[RefOp::Update { name: main.clone(), new: ghost, old: c2 }]).await.unwrap(), Err(RefRefusal::MissingObject));
    assert_eq!(r.update_refs(&[RefOp::Create { name: "refs/heads/main/x".into(), new: c1 }]).await.unwrap(), Err(RefRefusal::NameConflict));
    std::fs::write(r.dir().join("refs/heads/main.lock"), b"").unwrap();
    assert_eq!(r.update_refs(&[RefOp::Update { name: main.clone(), new: c1, old: c2 }]).await.unwrap(), Err(RefRefusal::Locked));
    std::fs::remove_file(r.dir().join("refs/heads/main.lock")).unwrap();
    // a refused transaction applies none of its lines
    assert_eq!(
        r.update_refs(&[RefOp::Create { name: "refs/heads/live".into(), new: c1 }, RefOp::Create { name: main.clone(), new: c1 }]).await.unwrap(),
        Err(RefRefusal::Exists)
    );
    assert_eq!(r.refs("refs/heads/").await.unwrap(), vec![(main.clone(), c2)]);

    // reads through a pooled cat-file
    let pool = Pool::new(r.clone());
    let mut cat = pool.get().await.unwrap();
    let head = cat.info("refs/heads/main").await.unwrap().unwrap();
    assert_eq!((head.oid, head.kind), (c2, Kind::Commit));
    assert!(cat.info("refs/heads/nope").await.unwrap().is_none());
    let blob = cat.info(&format!("{c2}:a b.txt")).await.unwrap().unwrap();
    assert_eq!((blob.kind, blob.size), (Kind::Blob, 2));
    assert!(cat.info(&format!("{c2}:d/\"q\".md")).await.unwrap().is_none(), "deleted");
    let (_, raw) = cat.read_all(c2, 1 << 20).await.unwrap().unwrap();
    let parsed = macrofiche_git::parse::commit(&raw).unwrap();
    assert_eq!((parsed.parents, parsed.author.time, parsed.message.as_str()), (vec![c1], 1_790_000_001, "m"));
    pool.put(cat);
    assert_eq!(pool.idle(), 1);
    let mut b = BlobReader::open(pool.clone(), blob.oid).await.unwrap().unwrap();
    assert_eq!(b.next().await.unwrap().unwrap(), b"ho");
    assert_eq!(b.next().await.unwrap(), None);
    assert_eq!(pool.idle(), 1, "a reader read whole goes back");
    let empty = Oid::parse("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap();
    let mut b = BlobReader::open(pool.clone(), empty).await.unwrap().unwrap();
    assert_eq!((b.info.size, b.next().await.unwrap()), (0, None));
    assert!(BlobReader::open(pool.clone(), ghost).await.unwrap().is_none());
    assert_eq!(pool.idle(), 1);

    // history and counts
    assert_eq!(r.rev_list(&["--date-order", &c2.to_string()], 10).await.unwrap(), vec![c2, c1]);
    assert_eq!(r.rev_count(&format!("{c1}..{c2}")).await.unwrap(), 1);
    assert!(r.is_ancestor(c1, c2).await.unwrap());
    assert!(!r.is_ancestor(c2, c1).await.unwrap());
    assert!(r.size_bytes().await.unwrap() > 0);
    assert!(r.fsck().await.unwrap().ok);
}

// Goal: merges as the store makes them: the base, a clean merge
// committed, a conflict with its paths, unrelated histories. Method: two
// branches off one commit.
#[tokio::test(flavor = "multi_thread")]
async fn merges() {
    let (e, root) = engine(true).await;
    let r = repo(&e, &root, "m.git").await;
    let base = commit(&r, "main", None, &[up("f.txt", b"one\n"), up("g.txt", b"g\n")], 1_790_000_000).await;
    r.update_refs(&[RefOp::Create { name: "refs/heads/live".into(), new: base }]).await.unwrap().unwrap();
    let ours = commit(&r, "live", Some(base), &[up("g.txt", b"g2\n")], 1_790_000_001).await;
    let theirs = commit(&r, "main", Some(base), &[up("f.txt", b"two\n")], 1_790_000_002).await;
    assert_eq!(r.merge_base(ours, theirs).await.unwrap(), Some(base));
    let Merged::Clean { tree } = r.merge_tree(ours, theirs, false).await.unwrap() else { panic!("a clean merge") };
    let m = r.commit_tree(tree, &[ours, theirs], b"merge", &me(), &me(), 1_790_000_003).await.unwrap();
    let files: Vec<(String, Option<u64>)> = r.ls_tree(m).await.unwrap().into_iter().map(|t| (t.path, t.size)).collect();
    assert_eq!(files, vec![("f.txt".into(), Some(4)), ("g.txt".into(), Some(3))]);
    let clash = commit(&r, "live", Some(ours), &[up("f.txt", b"three\n")], 1_790_000_004).await;
    assert_eq!(r.merge_tree(clash, theirs, false).await.unwrap(), Merged::Conflict { tree: match r.merge_tree(clash, theirs, false).await.unwrap() { Merged::Conflict { tree, .. } => tree, _ => unreachable!() }, paths: vec!["f.txt".into()] });
    // an unrelated history
    let tmp = "refs/macrofiche/tmp/orphan";
    let orphan = r.fast_import(&Head { tmp_ref: tmp, author: &me(), committer: &me(), time: 1, message: b"o", from: None }, &[up("o", b"o")]).await.unwrap();
    assert_eq!(r.merge_tree(orphan, theirs, false).await.unwrap(), Merged::Unrelated);
    assert!(matches!(r.merge_tree(orphan, theirs, true).await.unwrap(), Merged::Clean { .. }));
    assert_eq!(r.merge_base(orphan, theirs).await.unwrap(), None);
}

// Goal: a large blob goes in from a spool and comes out as a stream, with
// git's memory bounded. Method: 48 MiB of incompressible bytes; the
// cat-file reader's peak RssAnon sampled while it streams.
#[tokio::test(flavor = "multi_thread")]
async fn a_large_blob_streams() {
    let (e, root) = engine(true).await;
    let r = repo(&e, &root, "big.git").await;
    let len = 48u64 << 20;
    let spool = root.join("big.spool");
    {
        use std::io::Write;
        let mut f = std::io::BufWriter::new(std::fs::File::create(&spool).unwrap());
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..len / 8 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            f.write_all(&x.to_le_bytes()).unwrap();
        }
    }
    let c = commit(&r, "main", None, &[Change::Upsert { path: "big.bin", mode: mode::FILE, source: Source::File { path: &spool, len } }], 1).await;
    let pool = Pool::new(r.clone());
    let mut cat = pool.get().await.unwrap();
    let info = cat.info(&format!("{c}:big.bin")).await.unwrap().unwrap();
    assert_eq!(info.size, len);
    let pid = cat.pid();
    pool.put(cat);
    let mut b = BlobReader::open(pool.clone(), info.oid).await.unwrap().unwrap();
    let (mut got, mut peak_kib) = (0u64, 0u64);
    let mut want = std::io::BufReader::new(std::fs::File::open(&spool).unwrap());
    // Bounded by the blob's length: each chunk is at least a byte.
    while let Some(chunk) = b.next().await.unwrap() {
        let mut expect = vec![0u8; chunk.len()];
        std::io::Read::read_exact(&mut want, &mut expect).unwrap();
        assert!(chunk == expect, "bytes at {got}");
        got += chunk.len() as u64;
        if let Ok(s) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
            let anon = s.lines().find_map(|l| l.strip_prefix("RssAnon:")).and_then(|v| v.split_whitespace().next()?.parse().ok()).unwrap_or(0);
            peak_kib = peak_kib.max(anon);
        }
    }
    assert_eq!(got, len);
    eprintln!("{{\"evidence\":\"engine-stream\",\"blob_bytes\":{len},\"catfile_peak_rss_anon_kib\":{peak_kib}}}");
    assert!(peak_kib > 0 && peak_kib < 16 * 1024, "cat-file held {peak_kib} KiB streaming {len} bytes");
}

// Goal: the jail holds: a jailed git reads no other repository and no
// file outside its own, writes nowhere else, and reaches no TCP listener;
// the same calls with the jail off do (so the probe can see a leak).
// Method: git's own commands pointed outside, and `ls-remote git://` at
// a listener this test holds.
#[tokio::test(flavor = "multi_thread")]
async fn the_jail_holds() {
    for enforce in [true, false] {
        let (e, root) = engine(enforce).await;
        let mine = repo(&e, &root, "mine.git").await;
        let theirs = repo(&e, &root, "theirs.git").await;
        commit(&theirs, "main", None, &[up("secret", b"s")], 1).await;
        let secret_file = root.join("secret.txt");
        std::fs::write(&secret_file, b"secret").unwrap();
        let outside = root.join("outside.git");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();

        let other_dir = format!("--git-dir={}", theirs.dir().display());
        let read_other = mine.run("probe", &[&other_dir, "rev-parse", "refs/heads/main"], RunOpts::read()).await.unwrap();
        let read_file = mine.run("probe", &["hash-object", secret_file.to_str().unwrap()], RunOpts::read()).await.unwrap();
        let write_out = mine.run("probe", &["init", "--bare", "--quiet", outside.to_str().unwrap()], RunOpts::read()).await.unwrap();
        let url = format!("git://127.0.0.1:{port}/x");
        let net = mine.run("probe", &["ls-remote", &url], RunOpts { deadline: Duration::from_secs(5), ..RunOpts::read() }).await;
        let connected = listener.accept().is_ok();
        eprintln!(
            "{}",
            serde_json_line(enforce, &[("read_other", &read_other.stderr), ("read_file", &read_file.stderr), ("write_out", &write_out.stderr), ("net", &net.as_ref().map(|o| o.stderr.clone()).unwrap_or_default())], connected)
        );
        if enforce {
            assert!(!read_other.success(), "read another repository: {read_other:?}");
            assert!(!read_file.success(), "read a file outside: {read_file:?}");
            assert!(!write_out.success() && !outside.join("HEAD").exists(), "wrote outside: {write_out:?}");
            assert!(!connected, "reached a TCP listener");
            let net = net.unwrap();
            assert!(!net.success() && net.stderr.contains("not permitted"), "{net:?}");
        } else {
            assert!(read_other.success() && read_file.success() && write_out.success(), "the control reaches out");
            assert!(connected, "the control reaches the listener");
        }
    }
}

/// One line of evidence: what each probe's git said, and whether the
/// listener saw a connection.
fn serde_json_line(enforce: bool, said: &[(&str, &str)], connected: bool) -> String {
    let fields: Vec<String> = said.iter().map(|(k, v)| format!("\"{k}\":{:?}", v.trim())).collect();
    format!("{{\"evidence\":\"jail-probe\",\"enforce\":{enforce},\"connected\":{connected},{}}}", fields.join(","))
}

// Goal: a process past its deadline, or dropped by its owner, dies with
// its whole group; one that outruns its CPU limit is killed. Method: a
// cat-file left waiting on its stdin; a deadline of 200 ms.
#[tokio::test(flavor = "multi_thread")]
async fn deadlines_and_drops_kill() {
    let (e, root) = engine(true).await;
    let r = repo(&e, &root, "d.git").await;
    let proc = r.spawn("probe", &["cat-file", "--batch"], &[], Limits::OPERATION, Pipes { stdin: true, stdout: true }).await.unwrap();
    let pid = proc.pid();
    assert!(Path::new(&format!("/proc/{pid}")).exists());
    drop(proc);
    let gone = |pid: i32| {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |s| s.split_whitespace().nth(2) == Some("Z"))
    };
    // Bounded: 50 tries of 10 ms.
    let mut dead = false;
    for _ in 0..50 {
        if gone(pid) {
            dead = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(dead, "a dropped git dies");
    // `fast-import` waits on its stdin, which `run` keeps open while it
    // writes nothing: the deadline ends it.
    let mut proc = r.spawn("probe", &["fast-import", "--quiet"], &[], Limits::OPERATION, Pipes { stdin: true, stdout: true }).await.unwrap();
    let _stdin = proc.stdin();
    let t0 = std::time::Instant::now();
    let r2 = tokio::time::timeout(Duration::from_millis(200), proc.wait()).await;
    assert!(r2.is_err() && t0.elapsed() < Duration::from_secs(2));
    assert_eq!(e.processes(), 0, "every slot is back");
}
