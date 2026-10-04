//! gitoxide through gix 0.88.0.

use std::collections::{BTreeMap, HashSet};
use std::io::{self, BufReader, Read, Seek, Write};
use std::path::Path;
use std::sync::atomic::AtomicBool;

use gix::bstr::{BStr, ByteSlice};
use gix::hash::ObjectId;
use gix::objs::tree::EntryKind;
use gix::refs::transaction::{Change as RefChange, LogChange, PreviousValue, RefEdit, RefLog};
use gix::refs::Target;

use crate::model::*;

pub struct Gix {
    pub repo: gix::Repository,
}

fn sig(time: i64) -> gix::actor::Signature {
    gix::actor::Signature {
        name: NAME.into(),
        email: EMAIL.into(),
        time: gix::date::Time::new(time, 0),
    }
}

fn refname(branch: &str) -> String {
    format!("refs/heads/{branch}")
}

fn oid(hex: &str) -> R<ObjectId> {
    Ok(ObjectId::from_hex(hex.as_bytes())?)
}

impl Gix {
    fn tip(&self, branch: &str) -> R<Option<ObjectId>> {
        Ok(
            match self.repo.try_find_reference(refname(branch).as_str())? {
                Some(mut r) => Some(r.peel_to_id()?.detach()),
                None => None,
            },
        )
    }

    /// Move `branch` to `new` if it is at `expected` (or absent when `None`).
    fn cas(
        &self,
        branch: &str,
        expected: Option<ObjectId>,
        new: ObjectId,
    ) -> R<Result<(), Refused>> {
        let edit = RefEdit {
            change: RefChange::Update {
                log: LogChange {
                    mode: RefLog::AndReference,
                    force_create_reflog: false,
                    message: "macrofiche: update".into(),
                },
                expected: match expected {
                    None => PreviousValue::MustNotExist,
                    Some(old) => PreviousValue::MustExistAndMatch(Target::Object(old)),
                },
                new: Target::Object(new),
            },
            name: refname(branch).try_into()?,
            deref: false,
        };
        match self.repo.edit_reference(edit) {
            Ok(_) => Ok(Ok(())),
            Err(e) => {
                // gix's errors are erased; classify by what is on disk now.
                let current = self.tip(branch)?;
                let lock = self.repo.path().join(format!("{}.lock", refname(branch)));
                if lock.exists() {
                    Ok(Err(Refused::Locked(e.to_string())))
                } else if expected.is_none() && current.is_some() {
                    Ok(Err(Refused::Exists))
                } else if current != expected {
                    Ok(Err(Refused::Moved {
                        current: current.map(|c| c.to_string()),
                    }))
                } else {
                    Err(format!("ref edit failed with the ref at its expected value: {e:?}").into())
                }
            }
        }
    }

    fn write_commit(
        &self,
        tree: ObjectId,
        parents: &[ObjectId],
        message: &str,
        time: i64,
    ) -> R<ObjectId> {
        let c = gix::objs::Commit {
            tree,
            parents: parents.iter().copied().collect(),
            author: sig(time),
            committer: sig(time),
            encoding: None,
            message: message.into(),
            extra_headers: Vec::new(),
        };
        Ok(self.repo.write_object(&c)?.detach())
    }

    /// Everything reachable from every ref, commits first.
    fn reachable(&self) -> R<(Vec<ObjectId>, HashSet<ObjectId>)> {
        let mut tips = Vec::new();
        for r in self.repo.references()?.all()? {
            let mut r = r?;
            tips.push(r.peel_to_id()?.detach());
        }
        let mut commits = Vec::new();
        let mut seen = HashSet::new();
        for info in self.repo.rev_walk(tips).all()? {
            let info = info?;
            commits.push(info.id);
            seen.insert(info.id);
            let mut stack = vec![self.repo.find_commit(info.id)?.tree_id()?.detach()];
            while let Some(t) = stack.pop() {
                if !seen.insert(t) {
                    continue;
                }
                let tree = self.repo.find_tree(t)?;
                for e in tree.iter() {
                    let e = e?;
                    if e.mode().is_tree() {
                        stack.push(e.object_id());
                    } else {
                        seen.insert(e.object_id());
                    }
                }
            }
        }
        Ok((commits, seen))
    }
}

impl Store for Gix {
    fn init(dir: &Path, _durable: bool) -> R<Self> {
        // gix has no fsync at any layer (docs/library.md): `durable` is moot.
        gix::init_bare(dir)?;
        Self::open(dir, _durable)
    }

    fn open(dir: &Path, _durable: bool) -> R<Self> {
        let mut repo = gix::open_opts(dir, gix::open::Options::isolated())?;
        repo.object_cache_size_if_unset(16 * 1024 * 1024);
        Ok(Gix { repo })
    }

    fn write_blob_from(&self, r: &mut dyn Read) -> R<String> {
        // `Repository::write_blob_stream` copies the whole stream into
        // memory first. The odb's own `write_stream` streams into a loose
        // object but needs the size up front, so spool to learn it.
        let spool_path = self
            .repo
            .path()
            .join(format!("objects/incoming-{}", std::process::id()));
        let mut spool = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&spool_path)?;
        let size = io::copy(r, &mut spool)?;
        spool.rewind()?;
        let mut rd = BufReader::with_capacity(64 * 1024, spool);
        let id = gix::objs::Write::write_stream(
            &self.repo.objects,
            gix::objs::Kind::Blob,
            size,
            &mut rd,
        )?;
        std::fs::remove_file(&spool_path)?;
        Ok(id.to_string())
    }

    fn commit(&self, spec: &CommitSpec<'_>) -> R<Result<String, Refused>> {
        let parent = spec.expected.map(oid).transpose()?;
        let base = match parent {
            Some(p) => self.repo.find_commit(p)?.tree_id()?.detach(),
            None => ObjectId::empty_tree(gix::hash::Kind::Sha1),
        };
        let mut ed = self.repo.edit_tree(base)?;
        for ch in spec.changes {
            match ch.op {
                Op::Upsert(bytes) => {
                    let id = self.repo.write_blob(bytes)?.detach();
                    ed.upsert(ch.path, EntryKind::Blob, id)?;
                }
                Op::UpsertBlob(id) => {
                    ed.upsert(ch.path, EntryKind::Blob, oid(id)?)?;
                }
                Op::Delete => {
                    ed.remove(ch.path)?;
                }
            }
        }
        let tree = ed.write()?.detach();
        let parents: Vec<ObjectId> = parent.into_iter().collect();
        let id = self.write_commit(tree, &parents, spec.message, spec.time)?;
        Ok(self.cas(spec.branch, parent, id)?.map(|()| id.to_string()))
    }

    fn resolve(&self, branch: &str) -> R<Option<String>> {
        Ok(self.tip(branch)?.map(|i| i.to_string()))
    }

    fn read_file(
        &self,
        branch: &str,
        path: &str,
        _stream: bool,
        sink: &mut dyn Write,
    ) -> R<Option<Read1>> {
        let Some(tip) = self.tip(branch)? else {
            return Ok(None);
        };
        let tree = self.repo.find_commit(tip)?.tree()?;
        let Some(entry) = tree.lookup_entry_by_path(path)? else {
            return Ok(None);
        };
        // gix has no streaming read: a blob is decoded whole into memory.
        let blob = self.repo.find_blob(entry.object_id())?;
        sink.write_all(&blob.data)?;
        Ok(Some(Read1 {
            blob: entry.object_id().to_string(),
            len: blob.data.len() as u64,
            method: "in-memory",
        }))
    }

    fn last_commits(&self, branch: &str, paths: &[String]) -> R<(Vec<(String, String)>, usize)> {
        let tip = self.tip(branch)?.ok_or("no such branch")?;
        let mut left: HashSet<&BStr> = paths.iter().map(|p| p.as_bytes().as_bstr()).collect();
        let mut found = BTreeMap::new();
        let mut walked = 0;
        let empty = self.repo.empty_tree();
        for info in self.repo.rev_walk([tip]).first_parent_only().all()? {
            let info = info?;
            walked += 1;
            let c = self.repo.find_commit(info.id)?;
            let tree = c.tree()?;
            let ptree = match info.parent_ids.first() {
                Some(p) => self.repo.find_commit(*p)?.tree()?,
                None => empty.clone(),
            };
            let changes = self.repo.diff_tree_to_tree(
                Some(&ptree),
                Some(&tree),
                gix::diff::Options::default(),
            )?;
            for ch in &changes {
                let loc = ch.location();
                if left.remove(loc) {
                    found.insert(loc.to_string(), info.id.to_string());
                }
            }
            if left.is_empty() {
                break;
            }
        }
        Ok((found.into_iter().collect(), walked))
    }

    fn create_branch(&self, name: &str, at: &str) -> R<Result<(), Refused>> {
        self.cas(name, None, oid(at)?)
    }

    fn update_branch(&self, name: &str, expected: &str, new: &str) -> R<Result<(), Refused>> {
        self.cas(name, Some(oid(expected)?), oid(new)?)
    }

    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> R<bool> {
        let (a, d) = (oid(ancestor)?, oid(descendant)?);
        if a == d {
            return Ok(true);
        }
        Ok(self.repo.merge_base(a, d)?.detach() == a)
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
        let (ours, theirs) = (oid(expected)?, oid(source)?);
        if self.is_ancestor(source, expected)? {
            return Ok(MergeOutcome::FastForward(expected.to_string()));
        }
        if self.is_ancestor(expected, source)? {
            return Ok(match self.cas(target, Some(ours), theirs)? {
                Ok(()) => MergeOutcome::FastForward(source.to_string()),
                Err(r) => MergeOutcome::Refused(r),
            });
        }
        let labels = gix::merge::blob::builtin_driver::text::Labels {
            ancestor: None,
            current: Some("ours".into()),
            other: Some("theirs".into()),
        };
        let opts: gix::merge::commit::Options = self.repo.tree_merge_options()?.into();
        let mut out = self.repo.merge_commits(ours, theirs, labels, opts)?;
        let how = gix::merge::tree::TreatAsUnresolved::git();
        if out.tree_merge.has_unresolved_conflicts(how) {
            let mut paths: Vec<String> = out
                .tree_merge
                .conflicts
                .iter()
                .filter(|c| c.is_unresolved(how))
                .map(|c| c.ours.location().to_string())
                .collect();
            paths.sort();
            paths.dedup();
            return Ok(MergeOutcome::Conflict(paths));
        }
        let tree = out.tree_merge.tree.write()?.detach();
        let id = self.write_commit(tree, &[ours, theirs], message, time)?;
        Ok(match self.cas(target, Some(ours), id)? {
            Ok(()) => MergeOutcome::Merged(id.to_string()),
            Err(r) => MergeOutcome::Refused(r),
        })
    }

    fn gc(&self) -> R<serde_json::Value> {
        // gix has no gc and its pack writer has no delta compression: it
        // copies entries already in packs and deflates loose objects as
        // whole bases. Count, write the pack, index it, drop the loose.
        use gix::odb::pack;
        let t = std::time::Instant::now();
        let (commits, reachable) = self.reachable()?;
        // the shared handle wants the store's only owner: a fresh open
        let sync = gix::open_opts(self.repo.path(), gix::open::Options::isolated())?.into_sync();
        let mut handle = sync.objects.into_shared_arc().to_cache_arc();
        handle.prevent_pack_unload();
        let interrupt = AtomicBool::new(false);
        let ids: Vec<ObjectId> = commits.clone();
        let (counts, _count_stats) = pack::data::output::count::objects(
            handle.clone(),
            Box::new(ids.into_iter().map(Ok)),
            &gix::progress::Discard,
            &interrupt,
            pack::data::output::count::objects::Options {
                thread_limit: None,
                chunk_size: 50,
                input_object_expansion:
                    pack::data::output::count::objects::ObjectExpansion::TreeContents,
            },
        )?;
        let count_ms = t.elapsed().as_secs_f64() * 1e3;
        let num = counts.len();
        let entries = pack::data::output::entry::iter_from_counts(
            counts,
            handle.clone(),
            Box::new(gix::progress::Discard),
            pack::data::output::entry::iter_from_counts::Options {
                thread_limit: None,
                mode: pack::data::output::entry::iter_from_counts::Mode::PackCopyAndBaseObjects,
                allow_thin_pack: false,
                chunk_size: 10,
                version: Default::default(),
                compression: Default::default(),
            },
        );
        let mut in_order = gix::parallel::InOrderIter::from(entries);
        let tmp_path = self
            .repo
            .path()
            .join(format!("objects/pack/tmp-gix-{}.pack", std::process::id()));
        let mut tmp = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&tmp_path)?;
        {
            let mut w = io::BufWriter::new(&mut tmp);
            let mut bytes = pack::data::output::bytes::FromEntriesIter::new(
                in_order.by_ref(),
                &mut w,
                num as u32,
                pack::data::Version::default(),
                gix::hash::Kind::Sha1,
            );
            for r in bytes.by_ref() {
                r?;
            }
            w.flush()?;
        }
        let write_ms = t.elapsed().as_secs_f64() * 1e3 - count_ms;
        // Index it: Bundle::write_to_directory re-reads the pack, writes
        // the pack and its index into objects/pack under their hash.
        tmp.rewind()?;
        let mut rd = BufReader::with_capacity(1 << 20, tmp);
        let outcome = pack::Bundle::write_to_directory(
            &mut rd,
            Some(&self.repo.path().join("objects/pack")),
            &mut gix::progress::Discard,
            &interrupt,
            None::<gix::odb::Cache<gix::odb::store::Handle<std::sync::Arc<gix::odb::Store>>>>,
            gix::hash::Kind::Sha1,
            pack::bundle::write::Options::default(),
        )?;
        std::fs::remove_file(&tmp_path)?;
        // the bundle writer leaves a .keep until refs point at the pack;
        // here they already do
        if let Some(keep) = &outcome.keep_path {
            std::fs::remove_file(keep)?;
        }
        let name = outcome
            .data_path
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().into_owned())
            .ok_or("the bundle names its pack")?;
        let pack_ms = t.elapsed().as_secs_f64() * 1e3;
        let t = std::time::Instant::now();
        let packs_removed = crate::drop_packs_except(self.repo.path(), &name)?;
        let (removed, kept) = crate::prune_loose(self.repo.path(), |hex| {
            ObjectId::from_hex(hex.as_bytes())
                .map(|o| reachable.contains(&o))
                .unwrap_or(false)
        })?;
        Ok(serde_json::json!({
            "count_ms": count_ms,
            "write_ms": write_ms,
            "pack_ms": pack_ms,
            "prune_ms": t.elapsed().as_secs_f64() * 1e3,
            "objects": num,
            "index_objects": outcome.index.num_objects,
            "packs_removed": packs_removed,
            "loose_removed": removed,
            "loose_kept": kept,
            "deltas": "none: PackCopyAndBaseObjects is gix-pack's only mode",
        }))
    }
}
