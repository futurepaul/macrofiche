//! Good blob support (Paul's brief; docs/contract.md, section 8): a blob
//! far beyond fragment's 1 MiB goes in as a commit pack and comes out as a
//! file, streamed both ways, with macrofiche's memory and its git's
//! bounded, and its identity git's.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use common::*;
use macrofiche_testkit::fragment::BigPack;
use macrofiche_testkit::{request, sample_memory, Send, Sink};

/// The blob: 100 MiB of incompressible bytes.
const BLOB_BYTES: u64 = 100 << 20;

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

    let mut pack = BigPack::new(Some(&head), "media/big.bin", BLOB_BYTES);
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
    let (want_sha256, want_blob) = pack.finish();

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
