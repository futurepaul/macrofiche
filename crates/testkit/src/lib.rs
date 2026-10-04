//! Test support (docs/design.md, Crates). Nothing here is used by the
//! service; it is what its tests and its evidence driver share: scratch
//! directories, the engine's prefix, ES256 keys and tokens as fragment
//! signs them, a blocking HTTP/1.1 client that can stream a body of any
//! size both ways, a webhook receiver, and the real `macrofiche` binary
//! started on a temporary state directory.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;

/// The workspace root.
pub fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).expect("crates/testkit sits two below the root").to_path_buf()
}

/// The engine's prefix: `MACROFICHE_GIT_PREFIX`, else what `cargo xtask
/// engine` builds. A test that needs git fails here, saying how to build
/// it, rather than skipping.
pub fn git_prefix() -> PathBuf {
    let p = std::env::var_os("MACROFICHE_GIT_PREFIX").map(PathBuf::from).unwrap_or_else(|| workspace().join("target/engine/git-2.55.0"));
    assert!(p.join("bin/git").is_file(), "no git at {} (run `cargo xtask engine`, or set MACROFICHE_GIT_PREFIX)", p.display());
    p
}

/// A fresh, empty directory for one test, under `target/test-tmp`
/// (outside every jail's read-only places), unique per process and call.
pub fn scratch(name: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let d = workspace().join("target/test-tmp").join(format!("{name}-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("a scratch directory");
    d
}

// ---- keys and tokens ----

/// An org's P-256 key, as fragment holds `CODESTORAGE_PRIVATE_KEY`.
pub struct Key {
    pair: ring::signature::EcdsaKeyPair,
    pub pkcs8: Vec<u8>,
}

impl Key {
    pub fn generate() -> Key {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::EcdsaKeyPair::generate_pkcs8(&ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING, &rng).expect("a key").as_ref().to_vec();
        let pair = ring::signature::EcdsaKeyPair::from_pkcs8(&ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING, &pkcs8, &rng).expect("its pair");
        Key { pair, pkcs8 }
    }

    /// PKCS#8 PEM: what `macrofiche pubkey` reads.
    pub fn private_pem(&self) -> String {
        pem("PRIVATE KEY", &self.pkcs8)
    }

    /// SPKI PEM: what macrofiche's config holds.
    pub fn public_pem(&self) -> String {
        let mut der = vec![0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00];
        der.extend_from_slice(ring::signature::KeyPair::public_key(&self.pair).as_ref());
        pem("PUBLIC KEY", &der)
    }

    /// A JWT as `fragment_core::codestorage::OrgKey::token` signs one:
    /// `{"alg":"ES256","typ":"JWT"}`, claims `{iss, sub, repo, scopes, iat,
    /// exp}`, the signature `r‖s`, all base64url without padding.
    pub fn token(&self, org: &str, sub: &str, repo: &str, scopes: &[&str], ttl_s: i64) -> String {
        let now = now_s();
        self.sign(r#"{"alg":"ES256","typ":"JWT"}"#, &serde_json::json!({ "iss": org, "sub": sub, "repo": repo, "scopes": scopes, "iat": now, "exp": now + ttl_s }))
    }

    pub fn sign(&self, header: &str, claims: &serde_json::Value) -> String {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let input = format!("{}.{}", b64.encode(header), b64.encode(claims.to_string()));
        let sig = self.pair.sign(&ring::rand::SystemRandom::new(), input.as_bytes()).expect("a signature");
        format!("{input}.{}", b64.encode(sig.as_ref()))
    }
}

fn pem(label: &str, der: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let lines: Vec<&str> = b64.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).expect("ASCII")).collect();
    format!("-----BEGIN {label}-----\n{}\n-----END {label}-----\n", lines.join("\n"))
}

pub fn now_s() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// The base64 of `b`, standard and padded: a `blob_chunk`'s `data`.
pub fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

/// git's blob id of `bytes` (SHA-1 over `blob <len>\0<bytes>`), computed
/// with ring, so a test checks `x-blob-sha` against git's rule rather
/// than against macrofiche.
pub fn blob_sha(bytes: &[u8]) -> String {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
    ctx.update(format!("blob {}\0", bytes.len()).as_bytes());
    ctx.update(bytes);
    ctx.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

// ---- a blocking HTTP/1.1 client ----

/// An answer.
#[derive(Debug, Clone)]
pub struct Answer {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Answer {
    pub fn header(&self, k: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

impl std::fmt::Display for Answer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.status, self.text().chars().take(400).collect::<String>())
    }
}

/// A request body: bytes, or a reader of a known length streamed out.
pub enum Send<'a> {
    None,
    Bytes(&'a [u8]),
    Reader(&'a mut dyn Read, u64),
}

/// Where an answer's body goes.
pub enum Sink<'a> {
    Keep,
    Into(&'a mut dyn Write),
}

/// The deadline every call gets, generous: macrofiche's own bound it.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(300);

/// One request on a fresh connection.
pub fn request(base: &str, method: &str, path: &str, headers: &[(&str, &str)], body: Send<'_>, sink: Sink<'_>) -> std::io::Result<Answer> {
    let hostport = base.strip_prefix("http://").expect("an http base");
    let mut s = TcpStream::connect(hostport)?;
    s.set_read_timeout(Some(CALL_TIMEOUT))?;
    s.set_write_timeout(Some(CALL_TIMEOUT))?;
    s.set_nodelay(true)?;
    let mut head = format!("{method} {path} HTTP/1.1\r\nhost: {hostport}\r\nconnection: close\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    match &body {
        Send::None => {}
        Send::Bytes(b) => head.push_str(&format!("content-length: {}\r\n", b.len())),
        Send::Reader(_, n) => head.push_str(&format!("content-length: {n}\r\n")),
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes())?;
    match body {
        Send::None => {}
        Send::Bytes(b) => s.write_all(b)?,
        Send::Reader(r, n) => {
            let copied = std::io::copy(&mut r.take(n), &mut s)?;
            assert_eq!(copied, n, "the reader held its length");
        }
    }
    s.flush()?;
    read_answer(&mut BufReader::with_capacity(64 * 1024, s), method == "HEAD", sink)
}

fn read_answer(r: &mut BufReader<TcpStream>, head: bool, sink: Sink<'_>) -> std::io::Result<Answer> {
    let mut line = String::new();
    r.read_line(&mut line)?;
    let status: u16 = line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or_else(|| std::io::Error::other(format!("a status line: {line:?}")))?;
    let mut headers = Vec::new();
    // Bounded by the server: it ends its headers with an empty line.
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return Err(std::io::Error::other("the headers ended early"));
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let get = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let mut kept = Vec::new();
    {
        let out: &mut dyn Write = match sink {
            Sink::Keep => &mut kept,
            Sink::Into(w) => w,
        };
        if !head && status != 304 && status != 204 {
            if get("transfer-encoding").is_some_and(|t| t.eq_ignore_ascii_case("chunked")) {
                // Bounded by the server: a zero-length chunk ends it.
                loop {
                    line.clear();
                    r.read_line(&mut line)?;
                    let n = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16).map_err(|_| std::io::Error::other("a chunk size"))?;
                    if n == 0 {
                        line.clear();
                        r.read_line(&mut line)?;
                        break;
                    }
                    let copied = std::io::copy(&mut (&mut *r).take(n as u64), out)?;
                    if copied != n as u64 {
                        return Err(std::io::Error::other("a chunk ended early"));
                    }
                    line.clear();
                    r.read_line(&mut line)?;
                }
            } else if let Some(n) = get("content-length").and_then(|v| v.parse::<u64>().ok()) {
                let copied = std::io::copy(&mut (&mut *r).take(n), out)?;
                if copied != n {
                    return Err(std::io::Error::other(format!("the body ended at {copied} of {n} bytes")));
                }
            } else {
                std::io::copy(r, out)?;
            }
        }
    }
    Ok(Answer { status, headers, body: kept })
}

// ---- a webhook receiver ----

/// One request a receiver took.
#[derive(Clone, Debug)]
pub struct Received {
    pub at: Instant,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Received {
    pub fn header(&self, k: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str())
    }
}

/// A webhook's target: answers each request with the next status of its
/// script (the last one over and over), keeping what it took.
pub struct Receiver {
    pub url: String,
    got: Arc<Mutex<Vec<Received>>>,
}

impl Receiver {
    pub fn start(script: Vec<u16>) -> Receiver {
        assert!(!script.is_empty());
        let l = TcpListener::bind("127.0.0.1:0").expect("a port");
        let url = format!("http://{}/api/f/probe/webhook", l.local_addr().expect("an address"));
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        std::thread::spawn(move || {
            // Unbounded by design: it serves until the test process ends.
            for s in l.incoming().flatten() {
                let n = g.lock().expect("the lock").len();
                let status = *script.get(n).unwrap_or(script.last().expect("a script"));
                let _ = serve_one(s, status, &g);
            }
        });
        Receiver { url, got }
    }

    pub fn received(&self) -> Vec<Received> {
        self.got.lock().expect("the lock").clone()
    }

    /// Waits until `n` requests arrived, at most `within`.
    pub fn wait_for(&self, n: usize, within: Duration) -> Vec<Received> {
        let t0 = Instant::now();
        // Bounded by `within`.
        while t0.elapsed() < within {
            let r = self.received();
            if r.len() >= n {
                return r;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.received()
    }
}

fn serve_one(s: TcpStream, status: u16, got: &Mutex<Vec<Received>>) -> std::io::Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut r = BufReader::new(s.try_clone()?);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut headers = Vec::new();
    // Bounded by the client's empty line.
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            break;
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let n: usize = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0u8; n.min(1 << 20)];
    r.read_exact(&mut body)?;
    got.lock().expect("the lock").push(Received { at: Instant::now(), headers, body });
    let mut w = s;
    w.write_all(format!("HTTP/1.1 {status} X\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok").as_bytes())
}

// ---- the real binary ----

/// A running `macrofiche`, on its own state directory.
pub struct Macrofiche {
    pub url: String,
    pub pid: u32,
    pub dir: PathBuf,
    pub config: PathBuf,
    pub log: PathBuf,
    child: Child,
}

/// The binary's readiness bound: it clears leftovers first.
pub const READY_TIMEOUT: Duration = Duration::from_secs(60);

impl Macrofiche {
    /// Writes `config` (a JSON object; `listen`, `state_dir` and
    /// `git_prefix` are filled in when absent) to `dir/macrofiche.json`,
    /// starts `bin serve`, and waits for its serving line.
    pub fn start(bin: &Path, dir: &Path, mut config: serde_json::Value) -> Macrofiche {
        std::fs::create_dir_all(dir).expect("its directory");
        if config["listen"].is_null() {
            config["listen"] = "127.0.0.1:0".into();
        }
        if config["state_dir"].is_null() {
            config["state_dir"] = dir.join("state").to_string_lossy().into_owned().into();
        }
        if config["git_prefix"].is_null() {
            config["git_prefix"] = git_prefix().to_string_lossy().into_owned().into();
        }
        let cfg = dir.join("macrofiche.json");
        std::fs::write(&cfg, serde_json::to_vec_pretty(&config).expect("a config")).expect("the config");
        Macrofiche::spawn(bin, dir, cfg)
    }

    /// Starts again on the same config and state (a restart).
    pub fn restart(bin: &Path, dir: &Path) -> Macrofiche {
        Macrofiche::spawn(bin, dir, dir.join("macrofiche.json"))
    }

    fn spawn(bin: &Path, dir: &Path, cfg: PathBuf) -> Macrofiche {
        let log = dir.join("macrofiche.log");
        let err = std::fs::OpenOptions::new().create(true).append(true).open(&log).expect("its log");
        let mut child = Command::new(bin).arg("serve").arg("--config").arg(&cfg).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(err).spawn().expect("macrofiche starts");
        let out = child.stdout.take().expect("piped");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut lines = BufReader::new(out).lines();
            if let Some(Ok(l)) = lines.next() {
                let _ = tx.send(l);
            }
            // Drain the rest so the server never blocks on its stdout.
            for _ in lines {}
        });
        let line = match rx.recv_timeout(READY_TIMEOUT) {
            Ok(l) => l,
            Err(_) => {
                let _ = child.kill();
                panic!("macrofiche did not serve within {READY_TIMEOUT:?}:\n{}", std::fs::read_to_string(&log).unwrap_or_default());
            }
        };
        let v: serde_json::Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("a serving line, not {line:?}"));
        assert_eq!(v["event"], "serving", "{line}");
        let url = format!("http://{}", v["addr"].as_str().expect("an address"));
        Macrofiche { url, pid: child.id(), dir: dir.to_path_buf(), config: cfg, log, child }
    }

    /// SIGKILL, as a crash: nothing of its own runs after.
    pub fn kill9(mut self) {
        // SAFETY: kill(2) on our own child.
        unsafe { libc::kill(self.pid as i32, libc::SIGKILL) };
        let _ = self.child.wait();
    }

    /// SIGTERM, and its exit awaited.
    pub fn stop(mut self) -> std::process::ExitStatus {
        // SAFETY: kill(2) on our own child.
        unsafe { libc::kill(self.pid as i32, libc::SIGTERM) };
        self.child.wait().expect("it exits")
    }

    /// Its memory from `/proc/<pid>/status`, KiB: (VmRSS, VmHWM, RssAnon).
    pub fn memory_kib(&self) -> (u64, u64, u64) {
        let s = std::fs::read_to_string(format!("/proc/{}/status", self.pid)).unwrap_or_default();
        let field = |k: &str| s.lines().find_map(|l| l.strip_prefix(k)).and_then(|v| v.split_whitespace().next()?.parse().ok()).unwrap_or(0);
        (field("VmRSS:"), field("VmHWM:"), field("RssAnon:"))
    }

    /// Its children's pids now (the git processes it runs).
    pub fn children(&self) -> Vec<u32> {
        let mut out = Vec::new();
        for task in std::fs::read_dir(format!("/proc/{}/task", self.pid)).into_iter().flatten().flatten() {
            if let Ok(s) = std::fs::read_to_string(task.path().join("children")) {
                out.extend(s.split_whitespace().filter_map(|p| p.parse::<u32>().ok()));
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

impl Drop for Macrofiche {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The peak `RssAnon` (KiB) of `pid` and every descendant, sampled every
/// millisecond on a thread until `stop` is set: (macrofiche's own, the
/// largest child's).
pub fn sample_memory(pid: u32, stop: Arc<std::sync::atomic::AtomicBool>) -> std::thread::JoinHandle<(u64, u64)> {
    std::thread::spawn(move || {
        let anon = |p: u32| -> u64 {
            std::fs::read_to_string(format!("/proc/{p}/status")).ok().and_then(|s| s.lines().find_map(|l| l.strip_prefix("RssAnon:")).and_then(|v| v.split_whitespace().next()?.parse().ok())).unwrap_or(0)
        };
        let kids = |p: u32| -> Vec<u32> {
            let mut out = Vec::new();
            for task in std::fs::read_dir(format!("/proc/{p}/task")).into_iter().flatten().flatten() {
                if let Ok(s) = std::fs::read_to_string(task.path().join("children")) {
                    out.extend(s.split_whitespace().filter_map(|x| x.parse::<u32>().ok()));
                }
            }
            out
        };
        let (mut own, mut child) = (0u64, 0u64);
        // Bounded by `stop`.
        while !stop.load(Ordering::Relaxed) {
            own = own.max(anon(pid));
            let mut stack = kids(pid);
            let mut seen = 0;
            // Bounded: at most 4096 descendants a sample.
            while let Some(c) = stack.pop() {
                seen += 1;
                if seen > 4096 {
                    break;
                }
                child = child.max(anon(c));
                stack.extend(kids(c));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        (own, child)
    })
}
