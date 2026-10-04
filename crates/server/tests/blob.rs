//! Good blob support (Paul's brief; docs/contract.md, section 8): a blob
//! far beyond fragment's 1 MiB goes in as a commit pack and comes out as a
//! file, streamed both ways, with macrofiche's memory and its git's
//! bounded, and its identity git's.

mod common;

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use common::*;
use macrofiche_testkit::{request, sample_memory, Send, Sink};

/// The blob: 100 MiB of incompressible bytes.
const BLOB_BYTES: u64 = 100 << 20;

/// A commit pack of one blob of `len` pseudo-random bytes, written as it
/// is read: never whole in the test's memory either. Its SHA-256 and git
/// blob id are computed as it goes.
struct PackReader {
    head: Vec<u8>,
    pending: Vec<u8>,
    at: usize,
    left: u64,
    x: u64,
    sha256: ring::digest::Context,
    sha1: ring::digest::Context,
}

fn chunk_line(data: &[u8], eof: bool) -> Vec<u8> {
    format!("{{\"blob_chunk\":{{\"content_id\":\"c0\",\"data\":\"{}\",\"eof\":{eof}}}}}\n", macrofiche_testkit::b64(data)).into_bytes()
}

impl PackReader {
    fn new(expected: Option<&str>, len: u64) -> PackReader {
        let mut meta = serde_json::json!({"metadata": {"target_branch": "main", "commit_message": "a big blob", "author": {"name": "fragment/1a2b3c4d", "email": "1a2b3c4d@users.fragment"}, "files": [{"path": "media/big.bin", "operation": "upsert", "content_id": "c0", "mode": "100644"}]}});
        if let Some(e) = expected {
            meta["metadata"]["expected_target_sha"] = e.into();
        }
        let mut sha1 = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
        sha1.update(format!("blob {len}\0").as_bytes());
        PackReader { head: format!("{meta}\n").into_bytes(), pending: Vec::new(), at: 0, left: len, x: 0x9e37_79b9_7f4a_7c15, sha256: ring::digest::Context::new(&ring::digest::SHA256), sha1 }
    }

    /// The body's length, known before it is written.
    fn body_len(&self) -> u64 {
        let line = |n: u64, eof: bool| chunk_line(b"", eof).len() as u64 + 4 * n.div_ceil(3);
        let full = self.left / CHUNK_MAX as u64;
        let rest = self.left % CHUNK_MAX as u64;
        let mut total = self.head.len() as u64;
        for i in 0..full {
            total += line(CHUNK_MAX as u64, rest == 0 && i + 1 == full);
        }
        if rest > 0 {
            total += line(rest, true);
        }
        total
    }

    fn refill(&mut self) {
        if !self.head.is_empty() {
            self.pending = std::mem::take(&mut self.head);
            self.at = 0;
            return;
        }
        if self.left == 0 {
            return;
        }
        let n = self.left.min(CHUNK_MAX as u64) as usize;
        let mut data = vec![0u8; n];
        for b in data.chunks_mut(8) {
            self.x ^= self.x << 13;
            self.x ^= self.x >> 7;
            self.x ^= self.x << 17;
            let w = self.x.to_le_bytes();
            b.copy_from_slice(&w[..b.len()]);
        }
        self.sha256.update(&data);
        self.sha1.update(&data);
        self.left -= n as u64;
        self.pending = chunk_line(&data, self.left == 0);
        self.at = 0;
    }
}

impl Read for PackReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.at == self.pending.len() {
            self.refill();
            if self.pending.is_empty() || self.at == self.pending.len() {
                return Ok(0);
            }
        }
        let n = buf.len().min(self.pending.len() - self.at);
        buf[..n].copy_from_slice(&self.pending[self.at..self.at + n]);
        self.at += n;
        if self.at == self.pending.len() {
            self.pending.clear();
            self.at = 0;
        }
        Ok(n)
    }
}

/// A writer that hashes what it is given, and counts it.
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

fn hex(d: ring::digest::Digest) -> String {
    d.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

// Goal: a 100 MiB blob goes in and comes out whole, byte for byte, its
// x-blob-sha git's, with macrofiche's heap and its git's bounded far
// below the blob. Method: the pack generated as it is sent (25 chunks of
// 4 MiB), the file read into a hash; every millisecond, the peak RssAnon
// of macrofiche and of its git children sampled; the timings.
#[test]
fn a_hundred_mib_blob_in_and_out() {
    let fx = Fx::start("blob");
    let repo = fx.create_repo("blob");
    let head = fx.commit_at_head(&repo, &[up("readme.md", b"small")]);
    let base_kib = fx.m.memory_kib();

    let mut pack = PackReader::new(Some(&head), BLOB_BYTES);
    let len = pack.body_len();
    let auth = format!("Bearer {}", fx.key.token(ORG, "editor:ab", &repo, &["git:read", "git:write"], 900));
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = sample_memory(fx.m.pid, stop.clone());
    let t0 = Instant::now();
    let a = request(&fx.m.url, "POST", &Fx::repo_path(&repo, "commit-pack"), &[("authorization", &auth), ("content-type", "application/x-ndjson")], Send::Reader(&mut pack, len), Sink::Keep).unwrap();
    let in_s = t0.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    let (in_own, in_git) = sampler.join().unwrap();
    assert_eq!(a.status, 201, "{a}");
    let tip = committed(&a.json()).unwrap();
    let want_sha256 = hex(pack.sha256.finish());
    let want_blob = hex(pack.sha1.finish());

    let stop = Arc::new(AtomicBool::new(false));
    let sampler = sample_memory(fx.m.pid, stop.clone());
    let mut h = Hasher(ring::digest::Context::new(&ring::digest::SHA256), 0);
    let t0 = Instant::now();
    let got = request(&fx.m.url, "GET", &format!("{}?path=media%2Fbig.bin&ref={tip}", Fx::repo_path(&repo, "file")), &[("authorization", &auth)], Send::None, Sink::Into(&mut h)).unwrap();
    let out_s = t0.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    let (out_own, out_git) = sampler.join().unwrap();
    assert_eq!(got.status, 200);
    assert_eq!(got.header("content-length"), Some(BLOB_BYTES.to_string().as_str()));
    assert_eq!(h.1, BLOB_BYTES);
    assert_eq!(hex(h.0.finish()), want_sha256, "the bytes out are the bytes in");
    assert_eq!(got.header("x-blob-sha"), Some(want_blob.as_str()), "git's blob id");
    assert_eq!(got.header("x-last-commit-sha"), Some(tip.as_str()));
    let listed = fx.list(&repo, &tip, 1000);
    assert!(listed.iter().any(|f| f.0 == "media/big.bin" && f.1 == BLOB_BYTES));
    let after_kib = fx.m.memory_kib();
    let mib = |k: u64| k as f64 / 1024.0;
    eprintln!(
        "{}",
        serde_json::json!({
            "evidence": "blob", "blob_bytes": BLOB_BYTES, "pack_body_bytes": len,
            "in_s": (in_s * 1000.0).round() / 1000.0, "in_mib_per_s": (BLOB_BYTES as f64 / 1048576.0 / in_s * 10.0).round() / 10.0,
            "out_s": (out_s * 1000.0).round() / 1000.0, "out_mib_per_s": (BLOB_BYTES as f64 / 1048576.0 / out_s * 10.0).round() / 10.0,
            "macrofiche_rss_anon_peak_in_mib": mib(in_own), "macrofiche_rss_anon_peak_out_mib": mib(out_own),
            "git_rss_anon_peak_in_mib": mib(in_git), "git_rss_anon_peak_out_mib": mib(out_git),
            "macrofiche_vmhwm_before_mib": mib(base_kib.1), "macrofiche_vmhwm_after_mib": mib(after_kib.1),
        })
    );
    // Bounded memory: macrofiche holds a line and a chunk at a time, git
    // streams above core.bigFileThreshold.
    assert!(in_own < 64 * 1024 && out_own < 64 * 1024, "macrofiche's heap: {in_own} KiB in, {out_own} KiB out");
    assert!(in_git < 64 * 1024 && out_git < 64 * 1024, "git's heap: {in_git} KiB in, {out_git} KiB out");
    assert!(fx.fsck_all().iter().all(|(_, ok, _)| *ok));
}
