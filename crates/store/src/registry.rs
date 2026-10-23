//! Where things live under the state directory, and the registry of
//! repositories: each one's identity in its own `macrofiche/repo.json`,
//! read at boot into maps by name and by id (macrofiche is the only
//! writer, so memory is the truth while it runs). A repository's
//! directory is named by its id, never its name. A deleted repository
//! keeps its `repo.json`, marked, and nothing else: its name is never made
//! again (contract 5.13).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use macrofiche_contract::limits::REPOS_PER_ORG_MAX;
use serde::{Deserialize, Serialize};

/// The state directory's places (docs/design.md, On disk).
#[derive(Clone, Debug)]
pub struct Layout {
    pub state: PathBuf,
}

impl Layout {
    pub fn repos(&self) -> PathBuf {
        self.state.join("repos")
    }
    pub fn org(&self, org: &str) -> PathBuf {
        self.repos().join(org)
    }
    pub fn repo(&self, org: &str, id: &str) -> PathBuf {
        self.org(org).join(format!("{id}.git"))
    }
    /// Repositories being made, before their rename into place.
    pub fn tmp(&self) -> PathBuf {
        self.state.join("tmp")
    }
    /// Commit packs' blobs while they upload.
    pub fn spool(&self) -> PathBuf {
        self.state.join("spool")
    }
    /// An empty directory the engine's version check runs jailed in.
    pub fn scratch(&self) -> PathBuf {
        self.state.join("engine-check")
    }
}

/// A repository's identity, as `repo.json` holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoMeta {
    pub repo_id: String,
    pub repo_name: String,
    pub org: String,
    pub default_branch: String,
    pub created_at_ms: i64,
    /// When it was deleted (contract 5.13): it serves nothing, leaves the
    /// org's list, and its name stays taken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at_ms: Option<i64>,
}

pub const META_FILE: &str = "macrofiche/repo.json";

/// Every repository of every org, by name and by id, the deleted ones
/// included (`entry`); the lookups that serve skip those.
#[derive(Default)]
pub struct Registry {
    by_name: HashMap<(String, String), RepoMeta>,
    by_id: HashMap<(String, String), String>,
    /// Names being created now: a second create of one waits for none,
    /// it is a 409 at once.
    creating: Vec<(String, String)>,
}

impl Registry {
    /// Reads every `repo.json` under `layout.repos()`. A directory without
    /// one (a create that crashed before its rename never leaves one here)
    /// is reported, not served.
    pub fn load(layout: &Layout) -> std::io::Result<(Registry, Vec<PathBuf>)> {
        let mut r = Registry::default();
        let mut strays = Vec::new();
        let repos = layout.repos();
        std::fs::create_dir_all(&repos)?;
        for org in std::fs::read_dir(&repos)? {
            let org = org?;
            if !org.file_type()?.is_dir() {
                continue;
            }
            let mut count = 0usize;
            for repo in std::fs::read_dir(org.path())? {
                let repo = repo?;
                count += 1;
                if count > REPOS_PER_ORG_MAX {
                    return Err(std::io::Error::other(format!("{} holds more than {REPOS_PER_ORG_MAX} repositories", org.path().display())));
                }
                let meta: Option<RepoMeta> = std::fs::read(repo.path().join(META_FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok());
                match meta {
                    Some(m) if repo.file_name().to_str() == Some(&format!("{}.git", m.repo_id)) && org.file_name().to_str() == Some(m.org.as_str()) => r.insert(m),
                    _ => strays.push(repo.path()),
                }
            }
        }
        Ok((r, strays))
    }

    pub fn insert(&mut self, m: RepoMeta) {
        self.by_id.insert((m.org.clone(), m.repo_id.clone()), m.repo_name.clone());
        self.by_name.insert((m.org.clone(), m.repo_name.clone()), m);
    }

    /// The repository named `name`, deleted or not.
    pub fn entry(&self, org: &str, name: &str) -> Option<&RepoMeta> {
        self.by_name.get(&(org.to_string(), name.to_string()))
    }

    /// The repository named `name`, unless it was deleted.
    pub fn by_name(&self, org: &str, name: &str) -> Option<&RepoMeta> {
        self.entry(org, name).filter(|m| m.deleted_at_ms.is_none())
    }

    /// The repository with the id `id`, unless it was deleted.
    pub fn by_id(&self, org: &str, id: &str) -> Option<&RepoMeta> {
        let name = self.by_id.get(&(org.to_string(), id.to_string()))?;
        self.by_name(org, name)
    }

    /// Marks `name` deleted at `at_ms` (its `repo.json` says so already).
    pub fn mark_deleted(&mut self, org: &str, name: &str, at_ms: i64) {
        let m = self.by_name.get_mut(&(org.to_string(), name.to_string())).expect("a repository deleted is registered");
        m.deleted_at_ms = Some(at_ms);
    }

    /// Claims `name` for a create: false when it exists or is being made.
    pub fn begin(&mut self, org: &str, name: &str) -> bool {
        let key = (org.to_string(), name.to_string());
        if self.by_name.contains_key(&key) || self.creating.contains(&key) {
            return false;
        }
        if self.by_name.keys().filter(|(o, _)| o == org).count() >= REPOS_PER_ORG_MAX {
            return false;
        }
        self.creating.push(key);
        true
    }

    /// Ends a create: its repository, if it was made.
    pub fn end(&mut self, org: &str, name: &str, made: Option<RepoMeta>) {
        self.creating.retain(|(o, n)| !(o == org && n == name));
        if let Some(m) = made {
            self.insert(m);
        }
    }

    /// An org's repositories newest first (by creation, then id), after
    /// `after` (the last of the page before), at most `limit`, those whose
    /// name holds `q` (lowercase) when given; and whether more follow.
    pub fn page(&self, org: &str, after: Option<(i64, &str)>, limit: usize, q: Option<&str>) -> (Vec<RepoMeta>, bool) {
        let mut all: Vec<&RepoMeta> = self.by_name.values().filter(|m| m.org == org && m.deleted_at_ms.is_none() && q.is_none_or(|q| m.repo_name.to_ascii_lowercase().contains(q))).collect();
        all.sort_by(|a, b| b.created_at_ms.cmp(&a.created_at_ms).then_with(|| b.repo_id.cmp(&a.repo_id)));
        let start = match after {
            None => 0,
            Some((ms, id)) => all.iter().position(|m| (m.created_at_ms, m.repo_id.as_str()) < (ms, id)).unwrap_or(all.len()),
        };
        let page: Vec<RepoMeta> = all.iter().skip(start).take(limit).map(|m| (*m).clone()).collect();
        let more = start + page.len() < all.len();
        (page, more)
    }

    /// Every repository, the deleted included, for boot.
    pub fn all(&self) -> Vec<RepoMeta> {
        self.by_name.values().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

/// Renames `from` to `to` unless `to` exists (`renameat2` with
/// `RENAME_NOREPLACE`), then syncs `to`'s parent: a repository appears
/// whole or not at all.
pub fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let f = std::ffi::CString::new(from.as_os_str().as_bytes()).map_err(|_| std::io::Error::other("a path with NUL"))?;
    let t = std::ffi::CString::new(to.as_os_str().as_bytes()).map_err(|_| std::io::Error::other("a path with NUL"))?;
    // SAFETY: two NUL-terminated paths, both relative to nothing (AT_FDCWD
    // with absolute paths).
    let r = unsafe { libc::renameat2(libc::AT_FDCWD, f.as_ptr(), libc::AT_FDCWD, t.as_ptr(), libc::RENAME_NOREPLACE) };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    sync_dir(to.parent().expect("a repository has a parent"))
}

/// fsync on a directory: its entries are durable.
pub fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(org: &str, name: &str, id: &str, ms: i64) -> RepoMeta {
        RepoMeta { repo_id: id.into(), repo_name: name.into(), org: org.into(), default_branch: "main".into(), created_at_ms: ms, deleted_at_ms: None }
    }

    // Goal: pages run newest first and a page's last names where the next
    // starts, so a repo made between pages never repeats one and never
    // hides one (the cell pages through 150 newer repos to find its own:
    // contract 5.2). Method: five repos, pages of two, a sixth made
    // between pages.
    #[test]
    fn pages_newest_first() {
        let mut r = Registry::default();
        for (i, id) in ["a", "b", "c", "d", "e"].iter().enumerate() {
            r.insert(meta("o", &format!("n{i}"), id, 100 + i as i64));
        }
        r.insert(meta("other", "x", "z", 999));
        let (p1, more) = r.page("o", None, 2, None);
        assert_eq!(p1.iter().map(|m| m.repo_id.as_str()).collect::<Vec<_>>(), ["e", "d"]);
        assert!(more);
        r.insert(meta("o", "late", "f", 200));
        let last = p1.last().unwrap();
        let (p2, more) = r.page("o", Some((last.created_at_ms, &last.repo_id)), 2, None);
        assert_eq!(p2.iter().map(|m| m.repo_id.as_str()).collect::<Vec<_>>(), ["c", "b"]);
        assert!(more);
        let last = p2.last().unwrap();
        let (p3, more) = r.page("o", Some((last.created_at_ms, &last.repo_id)), 2, None);
        assert_eq!((p3.len(), more), (1, false));
        // the same millisecond: ties break by id
        r.insert(meta("o", "tie", "ab", 100));
        let (all, _) = r.page("o", None, 100, None);
        assert_eq!(all.iter().map(|m| m.repo_id.as_str()).collect::<Vec<_>>(), ["f", "e", "d", "c", "b", "ab", "a"]);
        let (late, _) = r.page("o", None, 100, Some("lat"));
        assert_eq!(late.iter().map(|m| m.repo_name.as_str()).collect::<Vec<_>>(), ["late"]);
    }

    // Goal: a name is created once: a second create while the first runs,
    // or after, is refused; a failed create frees the name. Method: begin
    // and end.
    #[test]
    fn a_name_is_created_once() {
        let mut r = Registry::default();
        assert!(r.begin("o", "n"));
        assert!(!r.begin("o", "n"), "while it is made");
        assert!(r.begin("p", "n"), "another org's name is its own");
        r.end("o", "n", None);
        assert!(r.begin("o", "n"), "a failed create frees it");
        r.end("o", "n", Some(meta("o", "n", "id1", 1)));
        assert!(!r.begin("o", "n"), "after");
        assert_eq!(r.by_id("o", "id1").unwrap().repo_name, "n");
        assert!(r.by_id("p", "id1").is_none(), "ids are per org");
    }

    // Goal: a deleted repository serves nothing and leaves the org's list,
    // and its name and id stay taken: a create of it is refused, never a
    // new repository under a wiped person's name (contract 5.13). Method:
    // two repos, one deleted.
    #[test]
    fn a_deleted_name_stays_taken() {
        let mut r = Registry::default();
        r.insert(meta("o", "gone", "g", 1));
        r.insert(meta("o", "kept", "k", 2));
        r.mark_deleted("o", "gone", 5);
        assert!(r.by_name("o", "gone").is_none() && r.by_id("o", "g").is_none(), "serves nothing");
        assert_eq!(r.entry("o", "gone").and_then(|m| m.deleted_at_ms), Some(5));
        let (all, more) = r.page("o", None, 100, None);
        assert_eq!((all.iter().map(|m| m.repo_name.as_str()).collect::<Vec<_>>(), more), (vec!["kept"], false), "leaves the list");
        assert!(!r.begin("o", "gone"), "its name is never made again");
        assert_eq!(r.by_id("o", "k").unwrap().repo_name, "kept");
    }
}
