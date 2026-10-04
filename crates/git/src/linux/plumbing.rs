//! Each plumbing call macrofiche makes, typed: what it takes, what it
//! answers, and how it fails. Every call runs one jailed git
//! (`process::Repo`) but the `cat-file` reads (`catfile`).

use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::process::{Engine, Output, Pipes, Repo, RunOpts, READ_DEADLINE};
use crate::error::{classify_update_ref, is_unknown_object, is_unrelated, GitError, RefRefusal, STDERR_KEPT_BYTES};
use crate::fastimport::{self, Head, Ident};
use crate::jail::Limits;
use crate::oid::Oid;
use crate::parse::{self, DiffEntry, TreeEntry};

/// The empty tree's id (SHA-1): a root commit's diff is against it.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// A write's deadline: a minute, and a second for every 16 KiB it
/// carries (half the CLI's 32 KiB/s floor, so the client's own deadline
/// always ends first; contract section 8).
pub fn write_deadline(bytes: u64) -> Duration {
    Duration::from_secs(60 + bytes / (16 * 1024))
}
/// Maintenance's deadline.
pub const MAINTENANCE_DEADLINE: Duration = Duration::from_secs(600);
/// Entries a tree listing or a diff may have.
pub const TREE_ENTRIES_MAX: usize = 200_000;
/// Commits a history page or a walk asks git for at once.
pub const REVS_MAX: usize = 1_000_000;
/// Refs a listing may hold.
pub const REFS_MAX: usize = 100_000;

/// The config every repository gets at creation (docs/design.md, On
/// disk): durable, streaming large blobs, no maintenance of git's own, no
/// hooks, refs of macrofiche's own hidden from transfer.
pub const REPO_CONFIG: &str = "\
[core]
\tfsync = all
\tfsyncMethod = fsync
\tbigFileThreshold = 1m
\tlogAllRefUpdates = false
\tpackedGitLimit = 512m
\tpackedGitWindowSize = 32m
\thooksPath = macrofiche/hooks
[gc]
\tauto = 0
[maintenance]
\tauto = false
[receive]
\tfsckObjects = true
\tdenyDeleteCurrent = true
[transfer]
\thideRefs = refs/namespaces
\thideRefs = refs/macrofiche
[uploadpack]
\tallowAnySHA1InWant = false
";

/// A file change for `fast_import`: its bytes come from a file (a spool)
/// or from memory.
pub enum Change<'a> {
    Upsert { path: &'a str, mode: u32, source: Source<'a> },
    Delete { path: &'a str },
}

pub enum Source<'a> {
    Bytes(&'a [u8]),
    /// A spool file of exactly `len` bytes.
    File { path: &'a Path, len: u64 },
}

impl Source<'_> {
    fn len(&self) -> u64 {
        match self {
            Source::Bytes(b) => b.len() as u64,
            Source::File { len, .. } => *len,
        }
    }
}

/// One line of an `update-ref --stdin` transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefOp {
    /// The ref must not exist.
    Create { name: String, new: Oid },
    /// The ref must be at `old` (compare-and-swap).
    Update { name: String, new: Oid, old: Oid },
    Delete { name: String, old: Oid },
}

/// merge-tree's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Merged {
    Clean { tree: Oid },
    Conflict { tree: Oid, paths: Vec<String> },
    Unrelated,
}

/// `fsck --full --strict`'s verdict.
#[derive(Clone, Debug)]
pub struct Fsck {
    pub ok: bool,
    pub stderr: String,
}

fn ref_name_ok(name: &str) -> bool {
    name.starts_with("refs/") && !name.bytes().any(|b| b <= b' ' || b == 0x7f)
}

/// `GIT_AUTHOR_*` and `GIT_COMMITTER_*` for a commit made at `time`.
fn people(author: &Ident, committer: &Ident, time: i64) -> Vec<(&'static str, String)> {
    let date = format!("{time} +0000");
    vec![
        ("GIT_AUTHOR_NAME", author.name.clone()),
        ("GIT_AUTHOR_EMAIL", author.email.clone()),
        ("GIT_AUTHOR_DATE", date.clone()),
        ("GIT_COMMITTER_NAME", committer.name.clone()),
        ("GIT_COMMITTER_EMAIL", committer.email.clone()),
        ("GIT_COMMITTER_DATE", date),
    ]
}

impl Engine {
    /// Initializes a bare repository in `dir`, an empty directory, with
    /// `default_branch` as its HEAD and macrofiche's config, and answers
    /// its handle.
    pub async fn init(&self, dir: &Path, default_branch: &str) -> Result<Repo, GitError> {
        let op = "init";
        let io = |source| GitError::Io { op, source };
        let empty = std::fs::read_dir(dir).map_err(io)?.next().is_none();
        if !empty {
            return Err(GitError::Protocol { op, detail: format!("{} is not empty", dir.display()) });
        }
        let repo = self.repo(dir)?;
        let branch_arg = format!("--initial-branch={default_branch}");
        repo.run(op, &["init", "--bare", "--quiet", "--template=", &branch_arg], RunOpts::read()).await?.ok(op)?;
        let mut config = std::fs::read_to_string(dir.join("config")).map_err(io)?;
        config.push_str(REPO_CONFIG);
        write_synced(&dir.join("config"), config.as_bytes()).map_err(io)?;
        for d in ["macrofiche/tmp", "macrofiche/hooks"] {
            std::fs::create_dir_all(dir.join(d)).map_err(io)?;
        }
        Ok(repo)
    }
}

/// Writes a file and syncs it.
pub fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

impl Repo {
    /// One commit by `fast-import`: `head`, then `changes`, written onto
    /// `head.tmp_ref`; answers the commit's id. The blobs stream from
    /// their sources into git, never whole in memory; above 1 MiB git
    /// streams them into its pack without a delta search.
    pub async fn fast_import(&self, head: &Head<'_>, changes: &[Change<'_>]) -> Result<Oid, GitError> {
        let op = "fast-import";
        let bytes: u64 = changes.iter().map(|c| if let Change::Upsert { source, .. } = c { source.len() } else { 0 }).sum();
        let deadline = write_deadline(bytes);
        let limits = Limits::OPERATION;
        let mut proc = self
            .spawn(op, &["fast-import", "--quiet", "--done", "--force", "--date-format=raw", "--big-file-threshold=1m"], &[], limits, Pipes { stdin: true, stdout: true })
            .await?;
        let mut input = proc.stdin().expect("piped");
        let mut out = proc.stdout().expect("piped");
        let mut err = proc.stderr().expect("piped");
        let io = |source| GitError::Io { op, source };
        let feed = async {
            input.write_all(&fastimport::head(head)).await?;
            for c in changes {
                match c {
                    Change::Upsert { path, mode, source } => {
                        input.write_all(&fastimport::upsert(*mode, path, source.len())).await?;
                        match source {
                            Source::Bytes(b) => input.write_all(b).await?,
                            Source::File { path: spool, len } => {
                                let f = tokio::fs::File::open(spool).await?;
                                let copied = tokio::io::copy(&mut f.take(*len), &mut input).await?;
                                if copied != *len {
                                    return Err(std::io::Error::other(format!("{} held {copied} bytes, not {len}", spool.display())));
                                }
                            }
                        }
                        input.write_all(b"\n").await?;
                    }
                    Change::Delete { path } => input.write_all(&fastimport::delete(path)).await?,
                }
            }
            input.write_all(fastimport::TAIL).await?;
            input.shutdown().await
        };
        let read_out = async {
            let mut buf = Vec::new();
            (&mut out).take(4096).read_to_end(&mut buf).await.map(|_| buf)
        };
        let read_err = async {
            let mut buf = Vec::new();
            let r = (&mut err).take(STDERR_KEPT_BYTES as u64).read_to_end(&mut buf).await;
            let _ = tokio::io::copy(&mut err, &mut tokio::io::sink()).await;
            r.map(|_| buf)
        };
        let work = async { tokio::join!(feed, read_out, read_err) };
        let (fed, stdout, stderr) = tokio::time::timeout(deadline, work).await.map_err(|_| GitError::Timeout { op, after: deadline })?;
        let status = tokio::time::timeout(deadline, proc.wait()).await.map_err(|_| GitError::Timeout { op, after: deadline })??;
        let stderr = String::from_utf8_lossy(&stderr.map_err(io)?).into_owned();
        if !status.success() {
            return Err(GitError::Failed { op, code: status.code().unwrap_or(-1), stderr });
        }
        // git took the whole stream (it exited 0), so a failure to feed it
        // is ours, a spool that was short.
        fed.map_err(io)?;
        let id = String::from_utf8_lossy(&stdout.map_err(io)?).trim().to_string();
        Oid::parse(&id).ok_or_else(|| GitError::Protocol { op, detail: format!("get-mark answered {id:?}") })
    }

    /// One `update-ref --stdin` transaction: all of `ops` or none.
    /// `Ok(Err(_))` is git's refusal, read from its stderr.
    pub async fn update_refs(&self, ops: &[RefOp]) -> Result<Result<(), RefRefusal>, GitError> {
        let op = "update-ref";
        let mut tx = String::new();
        for o in ops {
            let line = match o {
                RefOp::Create { name, new } => format!("create {name} {new}\n"),
                RefOp::Update { name, new, old } => format!("update {name} {new} {old}\n"),
                RefOp::Delete { name, old } => format!("delete {name} {old}\n"),
            };
            let name = match o {
                RefOp::Create { name, .. } | RefOp::Update { name, .. } | RefOp::Delete { name, .. } => name,
            };
            assert!(ref_name_ok(name), "ref names are checked before a transaction: {name:?}");
            tx.push_str(&line);
        }
        let out = self.run(op, &["update-ref", "--stdin"], RunOpts { stdin: Some(tx.into_bytes()), deadline: write_deadline(0), ..RunOpts::read() }).await?;
        if out.success() {
            return Ok(Ok(()));
        }
        match classify_update_ref(&out.stderr) {
            Some(r) => Ok(Err(r)),
            None => Err(out.error(op)),
        }
    }

    /// `merge-tree --write-tree` of `ours` and `theirs` from their merge
    /// base: the merged tree (written as objects, referenced by nothing),
    /// or the conflicted paths.
    pub async fn merge_tree(&self, ours: Oid, theirs: Oid, allow_unrelated: bool) -> Result<Merged, GitError> {
        let op = "merge-tree";
        let (o, t) = (ours.to_string(), theirs.to_string());
        let mut args = vec!["merge-tree", "--write-tree", "--no-messages", "--name-only", "-z"];
        if allow_unrelated {
            args.push("--allow-unrelated-histories");
        }
        args.extend([o.as_str(), t.as_str()]);
        let out = self.run(op, &args, RunOpts { deadline: write_deadline(0), stdout_max: 64 << 20, ..RunOpts::read() }).await?;
        match out.code {
            Some(0) => Ok(Merged::Clean { tree: parse::merge_tree(&out.stdout)?.0 }),
            Some(1) => {
                let (tree, paths) = parse::merge_tree(&out.stdout)?;
                Ok(Merged::Conflict { tree, paths })
            }
            _ if is_unrelated(&out.stderr) => Ok(Merged::Unrelated),
            _ => Err(out.error(op)),
        }
    }

    /// The best common ancestor of `a` and `b`; `None` when they share no
    /// history.
    pub async fn merge_base(&self, a: Oid, b: Oid) -> Result<Option<Oid>, GitError> {
        let op = "merge-base";
        let out = self.run(op, &["merge-base", &a.to_string(), &b.to_string()], RunOpts::read()).await?;
        match out.code {
            Some(0) => Ok(parse::oids(&out.stdout, 1)?.into_iter().next()),
            Some(1) if out.stderr.trim().is_empty() => Ok(None),
            _ => Err(out.error(op)),
        }
    }

    /// Whether `ancestor` is reachable from `descendant` (itself
    /// included).
    pub async fn is_ancestor(&self, ancestor: Oid, descendant: Oid) -> Result<bool, GitError> {
        let op = "merge-base --is-ancestor";
        let out = self.run(op, &["merge-base", "--is-ancestor", &ancestor.to_string(), &descendant.to_string()], RunOpts::read()).await?;
        match out.code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(out.error(op)),
        }
    }

    /// A commit of `tree` with `parents`, by `author` and `committer` at
    /// `time`, its message exactly `message`.
    pub async fn commit_tree(&self, tree: Oid, parents: &[Oid], message: &[u8], author: &Ident, committer: &Ident, time: i64) -> Result<Oid, GitError> {
        let op = "commit-tree";
        let tree = tree.to_string();
        let parents: Vec<String> = parents.iter().map(Oid::to_string).collect();
        let mut args = vec!["commit-tree", tree.as_str()];
        for p in &parents {
            args.extend(["-p", p.as_str()]);
        }
        let out = self
            .run(op, &args, RunOpts { stdin: Some(message.to_vec()), env: people(author, committer, time), deadline: write_deadline(0), ..RunOpts::read() })
            .await?;
        let stdout = out.ok(op)?;
        parse::oids(&stdout, 1)?.into_iter().next().ok_or(GitError::Protocol { op, detail: "no commit id".into() })
    }

    /// Every file at `commit`, recursively, at most `TREE_ENTRIES_MAX`.
    pub async fn ls_tree(&self, commit: Oid) -> Result<Vec<TreeEntry>, GitError> {
        let op = "ls-tree";
        let out = self.run(op, &["ls-tree", "-r", "-l", "-z", "--full-tree", &commit.to_string()], RunOpts { stdout_max: 512 << 20, ..RunOpts::read() }).await?;
        Ok(parse::ls_tree(&out.ok(op)?, TREE_ENTRIES_MAX)?)
    }

    /// The paths that differ between `a` (none: the empty tree) and `b`.
    pub async fn diff_tree(&self, a: Option<Oid>, b: Oid) -> Result<Vec<DiffEntry>, GitError> {
        let op = "diff-tree";
        let a = a.map_or_else(|| EMPTY_TREE.to_string(), |a| a.to_string());
        let out = self.run(op, &["diff-tree", "-r", "-z", "--no-renames", "--no-commit-id", &a, &b.to_string()], RunOpts { stdout_max: 512 << 20, ..RunOpts::read() }).await?;
        Ok(parse::diff_tree(&out.ok(op)?, TREE_ENTRIES_MAX)?)
    }

    /// `rev-list <args>`: commit ids, at most `max`.
    pub async fn rev_list(&self, args: &[&str], max: usize) -> Result<Vec<Oid>, GitError> {
        let op = "rev-list";
        let mut a = vec!["rev-list"];
        a.extend_from_slice(args);
        let out = self.run(op, &a, RunOpts { stdout_max: max.saturating_mul(41).max(STDOUT_MIN), ..RunOpts::read() }).await?;
        Ok(parse::oids(&out.ok(op)?, max)?)
    }

    /// `rev-list --count <range>`.
    pub async fn rev_count(&self, range: &str) -> Result<u64, GitError> {
        let op = "rev-list --count";
        let out = self.run(op, &["rev-list", "--count", range], RunOpts::read()).await?;
        let text = String::from_utf8_lossy(&out.ok(op)?).trim().to_string();
        text.parse().map_err(|_| GitError::Protocol { op, detail: format!("a count of {text:?}") })
    }

    /// Refs under `prefix`, with their ids.
    pub async fn refs(&self, prefix: &str) -> Result<Vec<(String, Oid)>, GitError> {
        let op = "for-each-ref";
        let out = self.run(op, &["for-each-ref", "--format=%(objectname) %(refname)", prefix], RunOpts { stdout_max: 64 << 20, ..RunOpts::read() }).await?;
        Ok(parse::refs(&out.ok(op)?, REFS_MAX)?)
    }

    /// `git maintenance run` with `tasks`, under the maintenance limits.
    pub async fn maintenance(&self, tasks: &[&str]) -> Result<Output, GitError> {
        let mut args = vec!["maintenance", "run", "--quiet"];
        let flags: Vec<String> = tasks.iter().map(|t| format!("--task={t}")).collect();
        args.extend(flags.iter().map(String::as_str));
        self.run("maintenance", &args, RunOpts { limits: Limits::MAINTENANCE, deadline: MAINTENANCE_DEADLINE, ..RunOpts::read() }).await
    }

    /// `gc --cruft` dropping unreachable objects older than `expire`.
    pub async fn gc(&self, expire: &str) -> Result<Output, GitError> {
        let prune = format!("--prune={expire}");
        self.run("gc", &["gc", "--quiet", "--cruft", &prune], RunOpts { limits: Limits::MAINTENANCE, deadline: MAINTENANCE_DEADLINE, ..RunOpts::read() }).await
    }

    /// `fsck --full --strict`.
    pub async fn fsck(&self) -> Result<Fsck, GitError> {
        let out = self.run("fsck", &["fsck", "--full", "--strict", "--no-progress", "--no-dangling"], RunOpts { limits: Limits::MAINTENANCE, deadline: MAINTENANCE_DEADLINE, ..RunOpts::read() }).await?;
        Ok(Fsck { ok: out.success() && out.stderr.trim().is_empty(), stderr: out.stderr })
    }

    /// `count-objects -v`: the repository's bytes, loose and packed.
    pub async fn size_bytes(&self) -> Result<u64, GitError> {
        let op = "count-objects";
        let out = self.run(op, &["count-objects", "-v"], RunOpts::read()).await?;
        let text = String::from_utf8_lossy(&out.ok(op)?).into_owned();
        let kib = |k: &str| text.lines().find_map(|l| l.strip_prefix(k)).and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0);
        Ok((kib("size:") + kib("size-pack:") + kib("size-garbage:")) * 1024)
    }
}

/// The least stdout bound a listing call gets.
const STDOUT_MIN: usize = 4096;

/// Whether `e` says an object or revision does not exist.
pub fn unknown_object(e: &GitError) -> bool {
    e.stderr().is_some_and(is_unknown_object)
}

/// A read deadline, for callers that time their own work.
pub const fn read_deadline() -> Duration {
    READ_DEADLINE
}
