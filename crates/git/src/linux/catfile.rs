//! Long-lived `cat-file --batch-command -Z` processes, as Gitaly keeps
//! them: a pool per repository answers ref lookups, object headers, and
//! object bytes without a process per read. A blob of any size is read
//! as a stream (git streams blobs above `core.bigFileThreshold`), and a
//! reader that stops halfway takes its process with it rather than
//! returning one with unread output to the pool.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::process::{Pipes, Proc, Repo, READ_DEADLINE};
use crate::error::GitError;
use crate::jail::Limits;
use crate::oid::Oid;
use crate::parse::{self, CatHeader, Kind};

/// Idle processes a repository keeps.
pub const IDLE_MAX: usize = 4;
/// An idle process older than this is ended.
pub const IDLE_TTL: Duration = Duration::from_secs(60);
/// A process serves at most this long, or this many requests, then is
/// retired (its CPU limit counts its whole life).
pub const AGE_MAX: Duration = Duration::from_secs(600);
pub const REQUESTS_MAX: u64 = 100_000;
/// A header's bytes: an object name and its answer.
const HEADER_BYTES_MAX: usize = 8192;
/// The limits a long-lived reader runs under: its CPU over its whole life.
const LIMITS: Limits = Limits { cpu_s: 1800, ..Limits::OPERATION };
const OP: &str = "cat-file";

/// An object's header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjInfo {
    pub oid: Oid,
    pub kind: Kind,
    pub size: u64,
}

pub struct CatFile {
    proc: Proc,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    born: Instant,
    last_used: Instant,
    requests: u64,
    /// Bytes of an object still unread: nonzero means the process is
    /// mid-answer and must not be reused.
    pending: u64,
}

impl CatFile {
    pub async fn spawn(repo: &Repo) -> Result<CatFile, GitError> {
        let mut proc = repo.spawn(OP, &["cat-file", "--batch-command", "-Z"], &[], LIMITS, Pipes { stdin: true, stdout: true }).await?;
        let stdin = proc.stdin().expect("piped");
        let stdout = BufReader::with_capacity(64 * 1024, proc.stdout().expect("piped"));
        // Its stderr is unused: a failure shows as a closed pipe.
        drop(proc.stderr());
        let now = Instant::now();
        Ok(CatFile { proc, stdin, stdout, born: now, last_used: now, requests: 0, pending: 0 })
    }

    fn reusable(&self) -> bool {
        self.pending == 0 && self.born.elapsed() < AGE_MAX && self.requests < REQUESTS_MAX
    }

    async fn ask(&mut self, verb: &str, name: &str) -> Result<CatHeader, GitError> {
        assert_eq!(self.pending, 0, "an answer is read whole before the next question");
        if name.is_empty() || name.contains('\0') || name.len() > HEADER_BYTES_MAX / 2 {
            return Err(GitError::Protocol { op: OP, detail: "an object name that is empty, too long, or holds NUL".into() });
        }
        self.requests += 1;
        self.last_used = Instant::now();
        let io = |source| GitError::Io { op: OP, source };
        let mut line = Vec::with_capacity(verb.len() + name.len() + 2);
        line.extend_from_slice(verb.as_bytes());
        line.push(b' ');
        line.extend_from_slice(name.as_bytes());
        line.push(0);
        let work = async {
            self.stdin.write_all(&line).await.map_err(io)?;
            self.stdin.flush().await.map_err(io)?;
            let mut header = Vec::new();
            // Bounded by HEADER_BYTES_MAX.
            loop {
                let buf = self.stdout.fill_buf().await.map_err(io)?;
                if buf.is_empty() {
                    return Err(GitError::Protocol { op: OP, detail: "cat-file closed its output".into() });
                }
                match buf.iter().position(|b| *b == 0) {
                    Some(i) => {
                        header.extend_from_slice(&buf[..i]);
                        self.stdout.consume(i + 1);
                        break;
                    }
                    None => {
                        let n = buf.len();
                        header.extend_from_slice(buf);
                        self.stdout.consume(n);
                    }
                }
                if header.len() > HEADER_BYTES_MAX {
                    return Err(GitError::Protocol { op: OP, detail: "a header past its bound".into() });
                }
            }
            Ok(parse::cat_header(&header)?)
        };
        tokio::time::timeout(READ_DEADLINE, work).await.map_err(|_| GitError::Timeout { op: OP, after: READ_DEADLINE })?
    }

    /// An object's header by any name git resolves (`refs/heads/main`,
    /// `<commit>:<path>`, an id); `None` when it does not exist.
    pub async fn info(&mut self, name: &str) -> Result<Option<ObjInfo>, GitError> {
        match self.ask("info", name).await? {
            CatHeader::Found { oid, kind, size } => Ok(Some(ObjInfo { oid, kind, size })),
            CatHeader::Missing => Ok(None),
            CatHeader::Ambiguous => Err(GitError::Protocol { op: OP, detail: format!("{name:?} is ambiguous") }),
        }
    }

    /// Starts reading an object's bytes: its header, then `size` bytes
    /// to take with `read` and a terminator `finish` takes.
    pub async fn start(&mut self, oid: Oid) -> Result<Option<ObjInfo>, GitError> {
        match self.ask("contents", &oid.to_string()).await? {
            CatHeader::Found { oid, kind, size } => {
                self.pending = size;
                Ok(Some(ObjInfo { oid, kind, size }))
            }
            CatHeader::Missing => Ok(None),
            CatHeader::Ambiguous => Err(GitError::Protocol { op: OP, detail: format!("{oid} is ambiguous") }),
        }
    }

    /// Up to `buf.len()` more bytes of the object being read; 0 at its
    /// end. Each read has the read deadline.
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, GitError> {
        if self.pending == 0 {
            return Ok(0);
        }
        let want = buf.len().min(usize::try_from(self.pending).unwrap_or(usize::MAX));
        let n = tokio::time::timeout(READ_DEADLINE, self.stdout.read(&mut buf[..want]))
            .await
            .map_err(|_| GitError::Timeout { op: OP, after: READ_DEADLINE })?
            .map_err(|source| GitError::Io { op: OP, source })?;
        if n == 0 {
            return Err(GitError::Protocol { op: OP, detail: "cat-file ended inside an object".into() });
        }
        self.pending -= n as u64;
        if self.pending == 0 {
            self.finish().await?;
        }
        Ok(n)
    }

    /// The NUL after an object's bytes.
    async fn finish(&mut self) -> Result<(), GitError> {
        let mut nul = [0u8; 1];
        tokio::time::timeout(READ_DEADLINE, self.stdout.read_exact(&mut nul))
            .await
            .map_err(|_| GitError::Timeout { op: OP, after: READ_DEADLINE })?
            .map_err(|source| GitError::Io { op: OP, source })?;
        if nul[0] != 0 {
            return Err(GitError::Protocol { op: OP, detail: "an object not followed by NUL".into() });
        }
        Ok(())
    }

    /// An object's bytes whole, refusing one larger than `max`: a commit,
    /// a small file.
    pub async fn read_all(&mut self, oid: Oid, max: u64) -> Result<Option<(ObjInfo, Vec<u8>)>, GitError> {
        let Some(info) = self.info(&oid.to_string()).await? else { return Ok(None) };
        if info.size > max {
            return Err(GitError::Protocol { op: OP, detail: format!("{oid} is {} bytes, past {max}", info.size) });
        }
        let Some(info) = self.start(oid).await? else { return Ok(None) };
        let mut out = vec![0u8; info.size as usize];
        let mut at = 0;
        // Bounded by the object's size: each read takes at least a byte.
        while at < out.len() {
            at += self.read(&mut out[at..]).await?;
        }
        if info.size == 0 {
            self.finish().await?;
        }
        Ok(Some((info, out)))
    }

    /// The pid, for a test that measures or kills it.
    pub fn pid(&self) -> i32 {
        self.proc.pid()
    }
}

/// One repository's readers.
pub struct Pool {
    repo: Repo,
    idle: Mutex<Vec<CatFile>>,
}

impl Pool {
    pub fn new(repo: Repo) -> Arc<Pool> {
        Arc::new(Pool { repo, idle: Mutex::new(Vec::new()) })
    }

    pub fn repo(&self) -> &Repo {
        &self.repo
    }

    /// An idle reader, or a new one.
    pub async fn get(&self) -> Result<CatFile, GitError> {
        let reused = {
            let mut idle = self.idle.lock().expect("the pool's lock");
            idle.retain(|c| c.last_used.elapsed() < IDLE_TTL && c.reusable());
            idle.pop()
        };
        match reused {
            Some(c) => Ok(c),
            None => CatFile::spawn(&self.repo).await,
        }
    }

    /// Returns a reader that finished its answers; one mid-answer, old, or
    /// past the idle bound is ended instead.
    pub fn put(&self, c: CatFile) {
        if !c.reusable() {
            return;
        }
        let mut idle = self.idle.lock().expect("the pool's lock");
        if idle.len() < IDLE_MAX {
            idle.push(c);
        }
    }

    /// Ends readers idle past `IDLE_TTL`.
    pub fn reap(&self) -> usize {
        let mut idle = self.idle.lock().expect("the pool's lock");
        let before = idle.len();
        idle.retain(|c| c.last_used.elapsed() < IDLE_TTL && c.reusable());
        before - idle.len()
    }

    /// Readers idle now.
    pub fn idle(&self) -> usize {
        self.idle.lock().expect("the pool's lock").len()
    }
}

/// A blob being read from a pooled reader, chunk by chunk; its reader goes
/// back to the pool once the blob is read whole, and is ended if the blob
/// is dropped first (its client went away).
pub struct BlobReader {
    cat: Option<CatFile>,
    pool: Arc<Pool>,
    pub info: ObjInfo,
}

/// Bytes a chunk carries.
pub const CHUNK_BYTES: usize = 64 * 1024;

impl BlobReader {
    /// Opens `oid` for reading; `None` when it does not exist.
    pub async fn open(pool: Arc<Pool>, oid: Oid) -> Result<Option<BlobReader>, GitError> {
        let mut cat = pool.get().await?;
        match cat.start(oid).await? {
            None => {
                pool.put(cat);
                Ok(None)
            }
            Some(info) => {
                let mut r = BlobReader { cat: Some(cat), pool, info };
                if info.size == 0 {
                    let mut c = r.cat.take().expect("just set");
                    c.finish().await?;
                    r.pool.put(c);
                }
                Ok(Some(r))
            }
        }
    }

    /// The next chunk, or `None` at the end.
    pub async fn next(&mut self) -> Result<Option<Vec<u8>>, GitError> {
        let Some(cat) = self.cat.as_mut() else { return Ok(None) };
        let mut buf = vec![0u8; CHUNK_BYTES.min(usize::try_from(cat.pending).unwrap_or(CHUNK_BYTES))];
        let n = cat.read(&mut buf).await?;
        buf.truncate(n);
        if cat.pending == 0 {
            let c = self.cat.take().expect("present");
            self.pool.put(c);
        }
        Ok(Some(buf))
    }
}
