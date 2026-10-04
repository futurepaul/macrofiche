//! Boot recovery (docs/library.md, finding 7; docs/design.md, Recovery):
//! before macrofiche serves, no writer of its own can be alive, so what a
//! killed writer left is cleared: ref and config locks, temporary objects
//! and packs, quarantines, fast-import's crash reports, macrofiche's own
//! temporary refs and files, the spools, and half-made repositories.

use std::path::Path;

use serde::Serialize;

/// Directory entries one repository's walk visits before it refuses.
pub const ENTRIES_MAX: usize = 2_000_000;

/// What was removed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Recovery {
    pub repos: usize,
    pub locks: usize,
    pub temp_files: usize,
    pub quarantines: usize,
    pub crash_reports: usize,
    pub temp_refs: usize,
    pub spools: usize,
    pub half_made: usize,
    pub strays: usize,
}

fn is_temp_name(name: &str) -> bool {
    name.starts_with("tmp_obj_") || name.starts_with("tmp_pack_") || name.starts_with("tmp_idx_") || name.starts_with("tmp_rev_") || name.starts_with("tmp_mtimes_") || name.starts_with(".tmp-")
}

/// Clears one repository's leftovers; answers how many of each. The
/// caller deletes the temporary refs (they may be packed: git does it).
pub fn sweep_repo(dir: &Path, r: &mut Recovery) -> std::io::Result<()> {
    let mut visited = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    // Bounded by ENTRIES_MAX entries in all.
    while let Some(d) = stack.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for e in entries {
            let e = e?;
            visited += 1;
            if visited > ENTRIES_MAX {
                return Err(std::io::Error::other(format!("{} has more than {ENTRIES_MAX} entries", dir.display())));
            }
            let name = e.file_name().to_string_lossy().into_owned();
            let path = e.path();
            let ft = e.file_type()?;
            if ft.is_dir() {
                if name.starts_with("incoming-") && d.ends_with("objects") {
                    std::fs::remove_dir_all(&path)?;
                    r.quarantines += 1;
                } else if d == dir.join("macrofiche") && name == "tmp" {
                    std::fs::remove_dir_all(&path)?;
                    std::fs::create_dir_all(&path)?;
                } else {
                    stack.push(path);
                }
                continue;
            }
            if name.ends_with(".lock") || (d == *dir && name == "gc.pid") {
                std::fs::remove_file(&path)?;
                r.locks += 1;
            } else if is_temp_name(&name) {
                std::fs::remove_file(&path)?;
                r.temp_files += 1;
            } else if d == *dir && name.starts_with("fast_import_crash_") {
                std::fs::remove_file(&path)?;
                r.crash_reports += 1;
            }
        }
    }
    r.repos += 1;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Goal: what a killed writer leaves is cleared, and nothing else.
    // Method: a directory shaped like a repository, with each leftover and
    // with the files that must stay.
    #[test]
    fn leftovers_go_and_the_rest_stays() {
        let d = std::env::temp_dir().join(format!("mf-recovery-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for p in ["refs/heads", "objects/pack", "objects/ab", "objects/incoming-x1/pack", "macrofiche/tmp", "macrofiche/lastcommit/ab"] {
            std::fs::create_dir_all(d.join(p)).unwrap();
        }
        let stay = ["HEAD", "config", "refs/heads/main", "objects/pack/pack-1.pack", "objects/ab/cdef", "macrofiche/lastcommit/ab/cdef"];
        let go = ["refs/heads/main.lock", "packed-refs.lock", "config.lock", "gc.pid", "objects/ab/tmp_obj_x", "objects/pack/tmp_pack_y", "objects/pack/tmp_idx_y", "fast_import_crash_9", "macrofiche/lastcommit/ab/.tmp-1-2", "macrofiche/tmp/a"];
        for f in stay.iter().chain(&go) {
            std::fs::write(d.join(f), b"x").unwrap();
        }
        let mut r = Recovery::default();
        sweep_repo(&d, &mut r).unwrap();
        assert_eq!((r.locks, r.temp_files, r.quarantines, r.crash_reports), (4, 4, 1, 1));
        for f in stay {
            assert!(d.join(f).exists(), "{f} stays");
        }
        for f in go {
            assert!(!d.join(f).exists(), "{f} goes");
        }
        assert!(d.join("macrofiche/tmp").is_dir() && !d.join("objects/incoming-x1").exists());
        // a second sweep finds nothing: it is idempotent
        let mut again = Recovery::default();
        sweep_repo(&d, &mut again).unwrap();
        assert_eq!((again.locks, again.temp_files, again.quarantines, again.crash_reports), (0, 0, 0, 0));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
