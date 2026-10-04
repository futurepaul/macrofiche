//! The workload, the same for every library: what is written, when, and
//! what each read must answer. Every commit has a fixed author, committer,
//! time, and message, so the three implementations must produce the same
//! object ids; the driver checks that they do.

use std::io::{self, Read};

pub type R<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub const SMALL_FILES: usize = 1000;
pub const BIG_PATH: &str = "assets/big.bin";
pub const BIG_LEN: u64 = 50 * 1024 * 1024;
pub const HISTORY: usize = 200;
pub const TOUCH_PER_COMMIT: usize = 5;
/// Files 990..1000 are never changed after the first commit, so finding
/// their last commit walks the whole history.
pub const NEVER_TOUCHED_FROM: usize = 990;
pub const LINES: usize = 20;
/// The first commit's time (2026-09-21); commit `i` is at `T0 + i`.
pub const T0: i64 = 1_790_000_000;
pub const NAME: &str = "Spike";
pub const EMAIL: &str = "spike@macrofiche.invalid";

pub fn small_path(i: usize) -> String {
    assert!(i < SMALL_FILES);
    format!("d{}/f{:03}.txt", i / 100, i % 100)
}

/// File `i` as of history commit `rev` (0 is the first commit).
pub fn small_content(i: usize, rev: usize) -> Vec<u8> {
    edited(i, rev, None)
}

/// File `i` as of `rev`, with line `line` replaced by `tag` when given.
pub fn edited(i: usize, rev: usize, edit: Option<(usize, &str)>) -> Vec<u8> {
    let path = small_path(i);
    let mut out = String::with_capacity(LINES * 48);
    for j in 0..LINES {
        match edit {
            Some((line, tag)) if line == j => out.push_str(&format!("line {j} of {path}: {tag}\n")),
            _ if rev > 0 && j == rev % LINES => {
                out.push_str(&format!("line {j} of {path}: changed in commit {rev}\n"))
            }
            _ => out.push_str(&format!("line {j} of {path}\n")),
        }
    }
    out.into_bytes()
}

/// The small files history commit `c` (1..=HISTORY) changes.
pub fn touched(c: usize) -> Vec<usize> {
    assert!((1..=HISTORY).contains(&c));
    (0..TOUCH_PER_COMMIT)
        .map(|k| (c * 37 + k * 211) % NEVER_TOUCHED_FROM)
        .collect()
}

/// The last history commit at or before `upto` that changed file `i`.
pub fn last_rev(i: usize, upto: usize) -> usize {
    (1..=upto)
        .rev()
        .find(|&c| touched(c).contains(&i))
        .unwrap_or(0)
}

/// The 100 paths whose last commit is asked: 90 spread over the touched
/// files and the 10 never touched.
pub fn query_files() -> Vec<usize> {
    let mut v: Vec<usize> = (0..90).map(|k| k * 11).collect();
    v.extend(NEVER_TOUCHED_FROM..SMALL_FILES);
    assert_eq!(v.len(), 100);
    v
}

pub fn commit_message(c: usize) -> String {
    if c == 0 {
        "initial: 1000 small files and a 50 MiB binary\n".into()
    } else {
        format!("history commit {c}\n")
    }
}

/// One change to a tree.
pub enum Op<'a> {
    Upsert(&'a [u8]),
    /// A blob already written (the big file).
    UpsertBlob(&'a str),
    Delete,
}

pub struct Change<'a> {
    pub path: &'a str,
    pub op: Op<'a>,
}

/// A commit on `branch`: its first parent is `expected` (the branch's
/// current tip, checked when the ref moves), or the branch must not exist.
pub struct CommitSpec<'a> {
    pub branch: &'a str,
    pub expected: Option<&'a str>,
    pub changes: &'a [Change<'a>],
    pub message: &'a str,
    pub time: i64,
}

/// Why a ref did not move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// The ref is not at the expected value (code.storage's 409).
    Moved { current: Option<String> },
    /// The ref was to be created and already exists.
    Exists,
    /// Another writer holds the ref's lock.
    Locked(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    FastForward(String),
    Merged(String),
    Conflict(Vec<String>),
    Refused(Refused),
}

/// What a read returned.
pub struct Read1 {
    pub blob: String,
    pub len: u64,
    /// "stream" when the library handed the bytes over in pieces,
    /// "in-memory" when it materialised the whole blob first.
    pub method: &'static str,
}

/// The big file: incompressible bytes from xorshift64*, produced in
/// pieces as an HTTP body would arrive, never held whole.
pub struct BigReader {
    left: u64,
    state: u64,
    word: [u8; 8],
    pos: usize,
}

impl BigReader {
    pub fn new() -> BigReader {
        BigReader {
            left: BIG_LEN,
            state: 0x9E37_79B9_7F4A_7C15,
            word: [0; 8],
            pos: 8,
        }
    }
}

impl Default for BigReader {
    fn default() -> BigReader {
        BigReader::new()
    }
}

impl Read for BigReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = (buf.len() as u64).min(self.left) as usize;
        for b in &mut buf[..n] {
            if self.pos == 8 {
                self.state ^= self.state >> 12;
                self.state ^= self.state << 25;
                self.state ^= self.state >> 27;
                self.word = self.state.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes();
                self.pos = 0;
            }
            *b = self.word[self.pos];
            self.pos += 1;
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// FNV-1a over a stream, to check a read without keeping it.
#[derive(Default)]
pub struct Fnv {
    pub h: u64,
    pub len: u64,
}

impl Fnv {
    pub fn new() -> Fnv {
        Fnv {
            h: 0xcbf2_9ce4_8422_2325,
            len: 0,
        }
    }
}

impl io::Write for Fnv {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        for b in buf {
            self.h ^= *b as u64;
            self.h = self.h.wrapping_mul(0x0100_0000_01b3);
        }
        self.len += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn big_fnv() -> u64 {
    let mut f = Fnv::new();
    io::copy(&mut BigReader::new(), &mut f).expect("an in-memory copy");
    assert_eq!(f.len, BIG_LEN);
    f.h
}

/// One library behind the operations macrofiche needs.
pub trait Store: Sized {
    fn init(dir: &std::path::Path, durable: bool) -> R<Self>;
    fn open(dir: &std::path::Path, durable: bool) -> R<Self>;
    /// Write a blob whose size is not known in advance (an HTTP body).
    fn write_blob_from(&self, r: &mut dyn Read) -> R<String>;
    fn commit(&self, spec: &CommitSpec<'_>) -> R<Result<String, Refused>>;
    fn resolve(&self, branch: &str) -> R<Option<String>>;
    /// The file at `path` on `branch`, into `sink`; `stream` asks for the
    /// library's streaming read where it has one.
    fn read_file(
        &self,
        branch: &str,
        path: &str,
        stream: bool,
        sink: &mut dyn io::Write,
    ) -> R<Option<Read1>>;
    /// The last commit (first-parent history) that changed each path, and
    /// how many commits were walked.
    fn last_commits(&self, branch: &str, paths: &[String]) -> R<(Vec<(String, String)>, usize)>;
    fn create_branch(&self, name: &str, at: &str) -> R<Result<(), Refused>>;
    fn update_branch(&self, name: &str, expected: &str, new: &str) -> R<Result<(), Refused>>;
    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> R<bool>;
    /// Merge `source` into `target`, whose tip must be `expected`:
    /// fast-forward when possible, else a merge commit, else the conflicts.
    fn merge(
        &self,
        target: &str,
        source: &str,
        expected: &str,
        message: &str,
        time: i64,
    ) -> R<MergeOutcome>;
    /// Pack everything reachable and drop the loose copies.
    fn gc(&self) -> R<serde_json::Value>;
}
