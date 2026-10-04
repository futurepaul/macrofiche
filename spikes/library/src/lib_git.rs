//! The git CLI's plumbing as a library, one process per call (what Gitaly
//! does, and the reference the other two are checked against).

use std::collections::{BTreeMap, HashSet};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use crate::model::*;

pub struct Git {
    dir: PathBuf,
}

/// The direct git children's peak RssAnon and RssFile (KiB), sampled from
/// /proc/<pid>/status every millisecond while each runs.
static CHILD_ANON: AtomicU64 = AtomicU64::new(0);
static CHILD_FILE: AtomicU64 = AtomicU64::new(0);

pub fn reset_child_peaks() {
    CHILD_ANON.store(0, Ordering::Relaxed);
    CHILD_FILE.store(0, Ordering::Relaxed);
}

pub fn child_peaks() -> (u64, u64) {
    (
        CHILD_ANON.load(Ordering::Relaxed),
        CHILD_FILE.load(Ordering::Relaxed),
    )
}

/// Sample `pid` until it is gone or `stop` is set.
fn watch(pid: u32) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let h = std::thread::spawn(move || {
        let path = format!("/proc/{pid}/status");
        while !s.load(Ordering::Relaxed) {
            let Ok(st) = std::fs::read_to_string(&path) else {
                break;
            };
            for l in st.lines() {
                let v = |l: &str| {
                    l.split_whitespace()
                        .nth(1)
                        .and_then(|n| n.parse::<u64>().ok())
                        .unwrap_or(0)
                };
                if l.starts_with("RssAnon:") {
                    CHILD_ANON.fetch_max(v(l), Ordering::Relaxed);
                } else if l.starts_with("RssFile:") {
                    CHILD_FILE.fetch_max(v(l), Ordering::Relaxed);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    });
    (stop, h)
}

fn output_watched(mut c: Command) -> R<std::process::Output> {
    c.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = c.spawn()?;
    let (stop, h) = watch(child.id());
    let out = child.wait_with_output()?;
    stop.store(true, Ordering::Relaxed);
    let _ = h.join();
    Ok(out)
}

fn ensure(out: &std::process::Output, what: &str) -> R<()> {
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{what}: {} {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into())
    }
}

impl Git {
    fn git(&self) -> Command {
        let mut c = Command::new("git");
        c.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("GIT_DIR", &self.dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("HOME", self.dir.parent().unwrap_or(Path::new("/")))
            .env("LC_ALL", "C")
            .env("GIT_TERMINAL_PROMPT", "0");
        c
    }

    fn run(&self, args: &[&str]) -> R<String> {
        let mut c = self.git();
        c.args(args).stdin(Stdio::null());
        let out = output_watched(c)?;
        ensure(&out, &format!("git {}", args.join(" ")))?;
        Ok(String::from_utf8(out.stdout)?.trim().to_string())
    }

    fn with_stdin(
        &self,
        args: &[&str],
        envs: &[(&str, String)],
        input: &[u8],
    ) -> R<std::process::Output> {
        let mut c = self.git();
        c.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in envs {
            c.env(k, v);
        }
        let mut child = c.spawn()?;
        let (stop, h) = watch(child.id());
        let mut stdin = child.stdin.take().ok_or("stdin")?;
        let input = input.to_vec();
        let writer = std::thread::spawn(move || stdin.write_all(&input));
        let out = child.wait_with_output()?;
        stop.store(true, Ordering::Relaxed);
        let _ = h.join();
        writer.join().map_err(|_| "stdin writer panicked")??;
        Ok(out)
    }

    /// Apply ref updates in one `update-ref --stdin` transaction.
    fn cas(
        &self,
        branch: &str,
        expected: Option<&str>,
        new: &str,
        also: &str,
    ) -> R<Result<(), Refused>> {
        let name = format!("refs/heads/{branch}");
        let mut tx = match expected {
            None => format!("create {name} {new}\n"),
            Some(old) => format!("update {name} {new} {old}\n"),
        };
        tx.push_str(also);
        let out = self.with_stdin(&["update-ref", "--stdin"], &[], tx.as_bytes())?;
        if out.status.success() {
            return Ok(Ok(()));
        }
        let current = self.resolve(branch)?;
        let lock = self.dir.join(format!("{name}.lock"));
        if lock.exists() || String::from_utf8_lossy(&out.stderr).contains(".lock': File exists") {
            Ok(Err(Refused::Locked(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            )))
        } else if expected.is_none() && current.is_some() {
            Ok(Err(Refused::Exists))
        } else if current.as_deref() != expected {
            Ok(Err(Refused::Moved { current }))
        } else {
            Err(format!(
                "update-ref failed with the ref at its expected value: {}",
                String::from_utf8_lossy(&out.stderr)
            )
            .into())
        }
    }

    fn sig_env(time: i64) -> Vec<(&'static str, String)> {
        let date = format!("{time} +0000");
        vec![
            ("GIT_AUTHOR_NAME", NAME.into()),
            ("GIT_AUTHOR_EMAIL", EMAIL.into()),
            ("GIT_AUTHOR_DATE", date.clone()),
            ("GIT_COMMITTER_NAME", NAME.into()),
            ("GIT_COMMITTER_EMAIL", EMAIL.into()),
            ("GIT_COMMITTER_DATE", date),
        ]
    }

    /// Read many files in one `cat-file --batch` session (a long-lived
    /// process per repository, as Gitaly keeps them).
    pub fn read_batch(
        &self,
        branch: &str,
        paths: &[String],
        sink: &mut dyn Write,
    ) -> R<Vec<Read1>> {
        let mut child = self
            .git()
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let (stop, h) = watch(child.id());
        let mut stdin = child.stdin.take().ok_or("stdin")?;
        let mut out = BufReader::with_capacity(64 * 1024, child.stdout.take().ok_or("stdout")?);
        let mut got = Vec::new();
        for p in paths {
            writeln!(stdin, "refs/heads/{branch}:{p}")?;
            stdin.flush()?;
            let mut header = String::new();
            out.read_line(&mut header)?;
            let f: Vec<&str> = header.split_whitespace().collect();
            if f.len() == 2 && f[1] == "missing" {
                continue;
            }
            if f.len() != 3 {
                return Err(format!("cat-file: {header:?}").into());
            }
            let len: u64 = f[2].parse()?;
            let n = io::copy(&mut (&mut out).take(len), sink)?;
            assert_eq!(n, len);
            let mut nl = [0u8; 1];
            out.read_exact(&mut nl)?;
            got.push(Read1 {
                blob: f[0].to_string(),
                len,
                method: "stream",
            });
        }
        drop(stdin);
        child.wait()?;
        stop.store(true, Ordering::Relaxed);
        let _ = h.join();
        Ok(got)
    }
}

impl Store for Git {
    fn init(dir: &Path, durable: bool) -> R<Self> {
        let g = Git {
            dir: dir.to_path_buf(),
        };
        let out = g
            .git()
            .env_remove("GIT_DIR")
            .args(["init", "--bare", "-q", "-b", "main"])
            .arg(dir)
            .output()?;
        ensure(&out, "git init")?;
        for (k, v) in [
            // git's default hardens packs only (core.fsync=committed,-loose-object).
            ("core.fsync", if durable { "all" } else { "none" }),
            ("core.fsyncMethod", "fsync"),
            // blobs above it are stored whole (no delta) and streamed on
            // read; at git's default (512m) a 50 MiB blob is read in core
            ("core.bigFileThreshold", "1m"),
            ("gc.auto", "0"),
            ("maintenance.auto", "false"),
            ("core.logAllRefUpdates", "false"),
        ] {
            g.run(&["config", k, v])?;
        }
        Ok(g)
    }

    fn open(dir: &Path, _durable: bool) -> R<Self> {
        Ok(Git {
            dir: dir.to_path_buf(),
        })
    }

    fn write_blob_from(&self, r: &mut dyn Read) -> R<String> {
        // Spool to learn the size; above core.bigFileThreshold hash-object
        // streams the file into a pack of its own (bulk checkin).
        let spool = self
            .dir
            .join(format!("objects/incoming-{}", std::process::id()));
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&spool)?;
            io::copy(r, &mut f)?;
        }
        let id = self.run(&[
            "-c",
            "core.bigFileThreshold=1m",
            "hash-object",
            "-w",
            "--no-filters",
            spool.to_str().ok_or("path")?,
        ])?;
        std::fs::remove_file(&spool)?;
        Ok(id)
    }

    fn commit(&self, spec: &CommitSpec<'_>) -> R<Result<String, Refused>> {
        // fast-import writes the blobs, trees, and commit into one pack
        // under a temporary ref; update-ref then moves the branch (CAS).
        // unique per process and per call: threads share the pid and can
        // read the same nanosecond
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let tmp = format!(
            "refs/macrofiche/tmp/{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let mut s: Vec<u8> = Vec::with_capacity(64 * 1024);
        write!(s, "commit {tmp}\nmark :1\n")?;
        write!(
            s,
            "author {NAME} <{EMAIL}> {} +0000\ncommitter {NAME} <{EMAIL}> {} +0000\n",
            spec.time, spec.time
        )?;
        write!(s, "data {}\n{}\n", spec.message.len(), spec.message)?;
        if let Some(p) = spec.expected {
            writeln!(s, "from {p}")?;
        }
        for ch in spec.changes {
            match ch.op {
                Op::Upsert(bytes) => {
                    write!(s, "M 100644 inline {}\ndata {}\n", ch.path, bytes.len())?;
                    s.extend_from_slice(bytes);
                    s.push(b'\n');
                }
                Op::UpsertBlob(id) => writeln!(s, "M 100644 {id} {}", ch.path)?,
                Op::Delete => writeln!(s, "D {}", ch.path)?,
            }
        }
        write!(s, "\nget-mark :1\ndone\n")?;
        let out = self.with_stdin(
            &[
                "fast-import",
                "--quiet",
                "--done",
                "--date-format=raw",
                "--big-file-threshold=1m",
            ],
            &[],
            &s,
        )?;
        ensure(&out, "git fast-import")?;
        let id = String::from_utf8(out.stdout)?.trim().to_string();
        assert_eq!(id.len(), 40, "fast-import answered a commit id");
        let res = self.cas(
            spec.branch,
            spec.expected,
            &id,
            &format!("delete {tmp} {id}\n"),
        )?;
        if res.is_err() {
            self.run(&["update-ref", "-d", &tmp])?;
        }
        Ok(res.map(|()| id))
    }

    fn resolve(&self, branch: &str) -> R<Option<String>> {
        let out = self
            .git()
            .args([
                "rev-parse",
                "--verify",
                "-q",
                &format!("refs/heads/{branch}"),
            ])
            .output()?;
        if out.status.success() {
            Ok(Some(String::from_utf8(out.stdout)?.trim().to_string()))
        } else {
            Ok(None)
        }
    }

    fn read_file(
        &self,
        branch: &str,
        path: &str,
        _stream: bool,
        sink: &mut dyn Write,
    ) -> R<Option<Read1>> {
        Ok(self.read_batch(branch, &[path.to_string()], sink)?.pop())
    }

    fn last_commits(&self, branch: &str, paths: &[String]) -> R<(Vec<(String, String)>, usize)> {
        let mut left: HashSet<&str> = paths.iter().map(String::as_str).collect();
        let mut found = BTreeMap::new();
        let mut child = self
            .git()
            .args([
                "log",
                "--first-parent",
                "--diff-merges=first-parent",
                "--no-renames",
                "--name-only",
                "--format=%x01%H",
                &format!("refs/heads/{branch}"),
            ])
            .stdout(Stdio::piped())
            .spawn()?;
        let out = BufReader::new(child.stdout.take().ok_or("stdout")?);
        let (mut walked, mut current) = (0, String::new());
        for line in out.lines() {
            let line = line?;
            if let Some(c) = line.strip_prefix('\u{1}') {
                walked += 1;
                current = c.to_string();
            } else if !line.is_empty() && left.remove(line.as_str()) {
                found.insert(line, current.clone());
                if left.is_empty() {
                    break;
                }
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        Ok((found.into_iter().collect(), walked))
    }

    fn create_branch(&self, name: &str, at: &str) -> R<Result<(), Refused>> {
        self.cas(name, None, at, "")
    }

    fn update_branch(&self, name: &str, expected: &str, new: &str) -> R<Result<(), Refused>> {
        self.cas(name, Some(expected), new, "")
    }

    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> R<bool> {
        let st = self
            .git()
            .args(["merge-base", "--is-ancestor", ancestor, descendant])
            .status()?;
        match st.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(format!("merge-base --is-ancestor: {st}").into()),
        }
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
        if self.is_ancestor(source, expected)? {
            return Ok(MergeOutcome::FastForward(expected.to_string()));
        }
        if self.is_ancestor(expected, source)? {
            return Ok(match self.cas(target, Some(expected), source, "")? {
                Ok(()) => MergeOutcome::FastForward(source.to_string()),
                Err(r) => MergeOutcome::Refused(r),
            });
        }
        let mut c = self.git();
        c.args([
            "merge-tree",
            "--write-tree",
            "--no-messages",
            "--name-only",
            "-z",
            expected,
            source,
        ]);
        let out = output_watched(c)?;
        let fields: Vec<String> = out
            .stdout
            .split(|b| *b == 0)
            .filter(|f| !f.is_empty())
            .map(|f| String::from_utf8_lossy(f).into_owned())
            .collect();
        match out.status.code() {
            Some(1) => {
                let mut paths: Vec<String> = fields[1..].to_vec();
                paths.sort();
                paths.dedup();
                Ok(MergeOutcome::Conflict(paths))
            }
            Some(0) => {
                let tree = &fields[0];
                let c = self.with_stdin(
                    &["commit-tree", tree, "-p", expected, "-p", source],
                    &Self::sig_env(time),
                    message.as_bytes(),
                )?;
                ensure(&c, "git commit-tree")?;
                let id = String::from_utf8(c.stdout)?.trim().to_string();
                Ok(match self.cas(target, Some(expected), &id, "")? {
                    Ok(()) => MergeOutcome::Merged(id),
                    Err(r) => MergeOutcome::Refused(r),
                })
            }
            _ => Err(format!(
                "merge-tree: {} {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            )
            .into()),
        }
    }

    fn gc(&self) -> R<serde_json::Value> {
        let t = std::time::Instant::now();
        self.run(&["gc", "--prune=now", "--quiet"])?;
        Ok(
            serde_json::json!({ "pack_ms": t.elapsed().as_secs_f64() * 1e3, "deltas": "git gc (pack-objects, window 10, depth 50)" }),
        )
    }
}
