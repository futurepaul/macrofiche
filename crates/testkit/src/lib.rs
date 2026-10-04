//! Test support (docs/design.md, Crates). Nothing here is used by the
//! service; it is what its tests share.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
