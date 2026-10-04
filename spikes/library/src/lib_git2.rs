//! libgit2 1.9.7 through git2 0.21.0.

use std::collections::{BTreeMap, HashSet};
use std::io::{self, Read, Write};
use std::path::Path;

use git2::{
    build::TreeUpdateBuilder, ErrorCode, FileMode, ObjectType, Oid, Repository,
    RepositoryInitOptions, Signature, Time,
};

use crate::model::*;

pub struct Git2 {
    repo: Repository,
    /// With `PACKED`, every write lands in an in-memory backend and each
    /// commit is flushed as one pack (one fsync for the pack and one for
    /// its index, as fast-import does), not as a loose file per object.
    mempack: Option<*mut libgit2_sys::git_odb_backend>,
}

/// Set by the `git2-packed` config before any store opens.
pub static PACKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn set_fsync(durable: bool) {
    // git2 does not wrap this option; libgit2_sys::init is idempotent.
    libgit2_sys::init();
    let rc = unsafe {
        libgit2_sys::git_libgit2_opts(
            libgit2_sys::GIT_OPT_ENABLE_FSYNC_GITDIR as libc::c_int,
            durable as libc::c_int,
        )
    };
    assert_eq!(rc, 0, "GIT_OPT_ENABLE_FSYNC_GITDIR is accepted");
}

fn sig(time: i64) -> R<Signature<'static>> {
    Ok(Signature::new(NAME, EMAIL, &Time::new(time, 0))?)
}

fn refname(branch: &str) -> String {
    format!("refs/heads/{branch}")
}

impl Git2 {
    fn with_repo(repo: Repository) -> R<Git2> {
        let mempack = if PACKED.load(std::sync::atomic::Ordering::Relaxed) {
            let odb = repo.odb()?;
            // the odb (owned by the repository) owns the backend from here
            let mp = odb.add_new_mempack_backend(1000)?;
            Some(git2::Binding::raw(&mp))
        } else {
            None
        };
        Ok(Git2 { repo, mempack })
    }

    /// Write what the mempack holds as one pack and index, then empty it.
    ///
    /// Not `git_mempack_dump`: it inserts each commit with
    /// `git_packbuilder_insert_commit`, so every pack would carry the whole
    /// tree again. `git_mempack_write_thin_pack` (libgit2 1.9, not bound by
    /// libgit2-sys 0.18.8) inserts only the objects the mempack holds.
    fn flush(&self) -> R<()> {
        extern "C" {
            fn git_mempack_write_thin_pack(
                backend: *mut libgit2_sys::git_odb_backend,
                pb: *mut libgit2_sys::git_packbuilder,
            ) -> libc::c_int;
        }
        let Some(raw) = self.mempack else {
            return Ok(());
        };
        let mp: git2::Mempack<'_> = unsafe { git2::Binding::from_raw(raw) };
        let mut pb = self.repo.packbuilder()?;
        let rc = unsafe { git_mempack_write_thin_pack(raw, git2::Binding::raw(&pb)) };
        if rc != 0 {
            return Err(format!("git_mempack_write_thin_pack: {rc}").into());
        }
        if pb.object_count() > 0 {
            pb.write(&self.repo.path().join("objects/pack"), 0)?;
        }
        mp.reset()?;
        Ok(())
    }

    /// Move `branch` to `new` if it is at `expected` (or absent when `None`).
    fn cas(&self, branch: &str, expected: Option<Oid>, new: Oid) -> R<Result<(), Refused>> {
        let name = refname(branch);
        let res = match expected {
            None => self
                .repo
                .reference(&name, new, false, "macrofiche: create")
                .map(|_| ()),
            Some(old) => self
                .repo
                .reference_matching(&name, new, true, old, "macrofiche: update")
                .map(|_| ()),
        };
        match res {
            Ok(()) => Ok(Ok(())),
            Err(e) if e.code() == ErrorCode::Exists => Ok(Err(Refused::Exists)),
            Err(e) if e.code() == ErrorCode::Modified || e.code() == ErrorCode::NotFound => {
                Ok(Err(Refused::Moved {
                    current: self.resolve(branch)?,
                }))
            }
            Err(e) if e.code() == ErrorCode::Locked => {
                Ok(Err(Refused::Locked(e.message().to_string())))
            }
            Err(e) => Err(e.into()),
        }
    }

    fn reachable(&self) -> R<HashSet<Oid>> {
        let mut seen = HashSet::new();
        let mut walk = self.repo.revwalk()?;
        walk.push_glob("*")?;
        for c in walk {
            let c = self.repo.find_commit(c?)?;
            seen.insert(c.id());
            let mut stack = vec![c.tree_id()];
            while let Some(t) = stack.pop() {
                if !seen.insert(t) {
                    continue;
                }
                for e in self.repo.find_tree(t)?.iter() {
                    match e.kind() {
                        Some(ObjectType::Tree) => stack.push(e.id()),
                        _ => {
                            seen.insert(e.id());
                        }
                    }
                }
            }
        }
        Ok(seen)
    }
}

impl Store for Git2 {
    fn init(dir: &Path, durable: bool) -> R<Self> {
        set_fsync(durable);
        let mut o = RepositoryInitOptions::new();
        o.bare(true).initial_head("main");
        Self::with_repo(Repository::init_opts(dir, &o)?)
    }

    fn open(dir: &Path, durable: bool) -> R<Self> {
        set_fsync(durable);
        Self::with_repo(Repository::open_bare(dir)?)
    }

    fn write_blob_from(&self, r: &mut dyn Read) -> R<String> {
        // git_blob_create_from_stream: spooled to objects/streamed, then
        // streamed into a loose object (the size is unknown up front).
        let mut w = self.repo.blob_writer(None)?;
        io::copy(r, &mut w)?;
        Ok(w.commit()?.to_string())
    }

    fn commit(&self, spec: &CommitSpec<'_>) -> R<Result<String, Refused>> {
        let parent = spec
            .expected
            .map(|p| self.repo.find_commit(Oid::from_str(p)?))
            .transpose()?;
        let base = match &parent {
            Some(c) => c.tree()?,
            None => self.repo.find_tree(self.repo.treebuilder(None)?.write()?)?,
        };
        let mut b = TreeUpdateBuilder::new();
        for ch in spec.changes {
            match ch.op {
                Op::Upsert(bytes) => {
                    let id = self.repo.blob(bytes)?;
                    b.upsert(ch.path, id, FileMode::Blob);
                }
                Op::UpsertBlob(id) => {
                    b.upsert(ch.path, Oid::from_str(id)?, FileMode::Blob);
                }
                Op::Delete => {
                    b.remove(ch.path);
                }
            }
        }
        let tree = self.repo.find_tree(b.create_updated(&self.repo, &base)?)?;
        let s = sig(spec.time)?;
        let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
        let id = self
            .repo
            .commit(None, &s, &s, spec.message, &tree, &parents)?;
        self.flush()?;
        Ok(self
            .cas(spec.branch, parent.map(|p| p.id()), id)?
            .map(|()| id.to_string()))
    }

    fn resolve(&self, branch: &str) -> R<Option<String>> {
        match self.repo.refname_to_id(&refname(branch)) {
            Ok(id) => Ok(Some(id.to_string())),
            Err(e) if e.code() == ErrorCode::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn read_file(
        &self,
        branch: &str,
        path: &str,
        stream: bool,
        sink: &mut dyn Write,
    ) -> R<Option<Read1>> {
        let tip = self.repo.refname_to_id(&refname(branch))?;
        let tree = self.repo.find_commit(tip)?.tree()?;
        let entry = match tree.get_path(Path::new(path)) {
            Ok(e) => e,
            Err(e) if e.code() == ErrorCode::NotFound => return Ok(None),
            Err(e) => Err(e)?,
        };
        let id = entry.id();
        if stream {
            // Only the loose backend implements a read stream; a packed
            // object fails here and falls back to a whole read.
            let odb = self.repo.odb()?;
            let streamed = match odb.reader(id) {
                Ok((mut rd, len, _)) => {
                    let n = io::copy(&mut rd, sink)?;
                    assert_eq!(n, len as u64);
                    Some(n)
                }
                Err(_) => None,
            };
            if let Some(n) = streamed {
                return Ok(Some(Read1 {
                    blob: id.to_string(),
                    len: n,
                    method: "stream",
                }));
            }
        }
        let blob = self.repo.find_blob(id)?;
        sink.write_all(blob.content())?;
        Ok(Some(Read1 {
            blob: id.to_string(),
            len: blob.size() as u64,
            method: "in-memory",
        }))
    }

    fn last_commits(&self, branch: &str, paths: &[String]) -> R<(Vec<(String, String)>, usize)> {
        let mut left: HashSet<&[u8]> = paths.iter().map(|p| p.as_bytes()).collect();
        let mut found = BTreeMap::new();
        let mut walk = self.repo.revwalk()?;
        walk.push(self.repo.refname_to_id(&refname(branch))?)?;
        walk.simplify_first_parent()?;
        let mut opts = git2::DiffOptions::new();
        opts.skip_binary_check(true);
        let mut walked = 0;
        for id in walk {
            let c = self.repo.find_commit(id?)?;
            walked += 1;
            let tree = c.tree()?;
            let ptree = if c.parent_count() > 0 {
                Some(c.parent(0)?.tree()?)
            } else {
                None
            };
            let diff = self
                .repo
                .diff_tree_to_tree(ptree.as_ref(), Some(&tree), Some(&mut opts))?;
            for d in diff.deltas() {
                for f in [d.new_file(), d.old_file()] {
                    if let Some(p) = f.path_bytes() {
                        if left.remove(p) {
                            found.insert(
                                String::from_utf8_lossy(p).into_owned(),
                                c.id().to_string(),
                            );
                        }
                    }
                }
            }
            if left.is_empty() {
                break;
            }
        }
        Ok((found.into_iter().collect(), walked))
    }

    fn create_branch(&self, name: &str, at: &str) -> R<Result<(), Refused>> {
        self.cas(name, None, Oid::from_str(at)?)
    }

    fn update_branch(&self, name: &str, expected: &str, new: &str) -> R<Result<(), Refused>> {
        self.cas(name, Some(Oid::from_str(expected)?), Oid::from_str(new)?)
    }

    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> R<bool> {
        let (a, d) = (Oid::from_str(ancestor)?, Oid::from_str(descendant)?);
        Ok(a == d || self.repo.graph_descendant_of(d, a)?)
    }

    fn merge(
        &self,
        target: &str,
        source: &str,
        expected: &str,
        message: &str,
        time: i64,
    ) -> R<MergeOutcome> {
        let current = self.resolve(target)?;
        if current.as_deref() != Some(expected) {
            return Ok(MergeOutcome::Refused(Refused::Moved { current }));
        }
        let (ours, theirs) = (Oid::from_str(expected)?, Oid::from_str(source)?);
        if self.is_ancestor(source, expected)? {
            return Ok(MergeOutcome::FastForward(expected.to_string()));
        }
        if self.is_ancestor(expected, source)? {
            return Ok(match self.cas(target, Some(ours), theirs)? {
                Ok(()) => MergeOutcome::FastForward(source.to_string()),
                Err(r) => MergeOutcome::Refused(r),
            });
        }
        let (oc, tc) = (self.repo.find_commit(ours)?, self.repo.find_commit(theirs)?);
        let mut idx = self.repo.merge_commits(&oc, &tc, None)?;
        if idx.has_conflicts() {
            let mut paths = Vec::new();
            for c in idx.conflicts()? {
                let c = c?;
                if let Some(e) = c.our.or(c.their).or(c.ancestor) {
                    paths.push(String::from_utf8_lossy(&e.path).into_owned());
                }
            }
            paths.sort();
            paths.dedup();
            return Ok(MergeOutcome::Conflict(paths));
        }
        let tree = self.repo.find_tree(idx.write_tree_to(&self.repo)?)?;
        let s = sig(time)?;
        let id = self
            .repo
            .commit(None, &s, &s, message, &tree, &[&oc, &tc])?;
        self.flush()?;
        Ok(match self.cas(target, Some(ours), id)? {
            Ok(()) => MergeOutcome::Merged(id.to_string()),
            Err(r) => MergeOutcome::Refused(r),
        })
    }

    fn gc(&self) -> R<serde_json::Value> {
        // libgit2 has no gc: its pack builder (with delta search) writes a
        // pack and index of everything reachable; dropping the old packs and
        // the loose copies is ours to do.
        let t = std::time::Instant::now();
        let mut pb = self.repo.packbuilder()?;
        pb.set_threads(0);
        let mut walk = self.repo.revwalk()?;
        walk.push_glob("*")?;
        pb.insert_walk(&mut walk)?;
        let objects = pb.object_count();
        let pack_dir = self.repo.path().join("objects/pack");
        pb.write(&pack_dir, 0)?;
        let written = pb.written();
        let name = format!(
            "pack-{}",
            pb.name()?.ok_or("the pack builder names its pack")?
        );
        let pack_ms = t.elapsed().as_secs_f64() * 1e3;
        let t = std::time::Instant::now();
        // every reachable object is in the new pack: the old packs go
        let packs_removed = crate::drop_packs_except(self.repo.path(), &name)?;
        let packed = self.reachable()?;
        let (removed, kept) = crate::prune_loose(self.repo.path(), |hex| {
            Oid::from_str(hex)
                .map(|o| packed.contains(&o))
                .unwrap_or(false)
        })?;
        Ok(serde_json::json!({
            "pack_ms": pack_ms,
            "prune_ms": t.elapsed().as_secs_f64() * 1e3,
            "objects": objects,
            "written": written,
            "packs_removed": packs_removed,
            "loose_removed": removed,
            "loose_kept": kept,
            "deltas": "libgit2 packbuilder delta search",
        }))
    }
}
