//! The commit pack (docs/contract.md, 5.8): NDJSON, a `metadata` line
//! first, then `blob_chunk` lines, each a piece of one upsert's bytes in
//! standard base64, the last of each with `eof: true`. Chunks of several
//! files may interleave.
//!
//! [`Parser`] is fed bytes as the body arrives and answers events: the
//! metadata once, checked, then each decoded chunk with the index of the
//! file it belongs to. It holds one line at a time (at most
//! `PACK_METADATA_BYTES_MAX` for the first, `PACK_LINE_BYTES_MAX` after),
//! never a whole blob or pack. Its refusals are the service's (400, 413),
//! each named as the service names it.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use base64::Engine as _;
use macrofiche_contract::limits::{
    BLOB_BYTES_MAX, CHUNK_DECODED_BYTES_MAX, CONTENT_ID_BYTES_MAX, MESSAGE_BYTES_MAX, PACK_DECODED_BYTES_MAX, PACK_FILES_MAX, PACK_LINE_BYTES_MAX, PACK_METADATA_BYTES_MAX,
};
use macrofiche_contract::names::{self, Expected};
use serde::Deserialize;
use thiserror::Error;

/// A person a commit names, as sent (checked by the store, as git checks
/// an ident).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Person {
    pub name: String,
    pub email: String,
}

/// What a file entry does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Upsert { content_id: String, mode: u32 },
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileOp {
    pub path: String,
    pub op: Op,
}

/// A pack's metadata, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Metadata {
    pub target_branch: String,
    pub expected: Expected,
    pub message: String,
    pub author: Option<Person>,
    pub committer: Option<Person>,
    pub files: Vec<FileOp>,
    /// The branch a new target starts from (the service's; fragment
    /// sends none).
    pub base_branch: Option<String>,
    pub ephemeral: bool,
    pub base_is_ephemeral: bool,
}

/// What the parser hands on.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Metadata(Metadata),
    /// A decoded piece of `files[file]`'s bytes (possibly empty).
    Chunk { file: usize, data: Vec<u8> },
}

/// How a refusal is answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// RFC 9457 problem details (an empty body, a first line that is no
    /// JSON).
    Problem,
    /// The commit pack's own answer, `result.status` `invalid` or
    /// `payload_too_large`.
    Result,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum PackError {
    #[error("empty pack or a bad metadata line")]
    Empty,
    #[error("first payload must be metadata")]
    FirstNotMetadata,
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    TooLarge(String),
}

impl PackError {
    pub fn status(&self) -> u16 {
        match self {
            PackError::TooLarge(_) => 413,
            _ => 400,
        }
    }

    pub fn shape(&self) -> Shape {
        match self {
            PackError::Empty => Shape::Problem,
            _ => Shape::Result,
        }
    }

    /// `result.status` for a `Shape::Result`.
    pub fn result_status(&self) -> &'static str {
        match self {
            PackError::TooLarge(_) => "payload_too_large",
            _ => "invalid",
        }
    }
}

fn invalid(s: impl Into<String>) -> PackError {
    PackError::Invalid(s.into())
}

#[derive(Deserialize)]
struct RawFile {
    path: Option<String>,
    operation: Option<String>,
    content_id: Option<String>,
    mode: Option<String>,
}

#[derive(Deserialize)]
struct RawMeta {
    target_branch: Option<String>,
    expected_target_sha: Option<String>,
    expected_head_sha: Option<String>,
    commit_message: Option<String>,
    author: Option<Person>,
    committer: Option<Person>,
    files: Option<Vec<RawFile>>,
    base_branch: Option<String>,
    ephemeral: Option<bool>,
    base_is_ephemeral: Option<bool>,
}

#[derive(Deserialize)]
struct RawChunk<'a> {
    #[serde(borrow)]
    content_id: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    data: Option<Cow<'a, str>>,
    #[serde(default)]
    eof: Option<bool>,
}

#[derive(Deserialize)]
struct ChunkLine<'a> {
    #[serde(borrow)]
    blob_chunk: Option<RawChunk<'a>>,
}

/// Standard base64; padding optional, as some encoders leave it off.
const B64: base64::engine::GeneralPurpose =
    base64::engine::GeneralPurpose::new(&base64::alphabet::STANDARD, base64::engine::GeneralPurposeConfig::new().with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent));

fn check_metadata(raw: RawMeta) -> Result<Metadata, PackError> {
    let target_branch = raw.target_branch.ok_or_else(|| invalid("metadata needs target_branch"))?;
    if !names::valid_branch(&target_branch) {
        return Err(invalid(format!("invalid target_branch {target_branch:?}")));
    }
    let expected = names::parse_expected(raw.expected_target_sha.as_deref().or(raw.expected_head_sha.as_deref())).ok_or_else(|| invalid("expected_target_sha is a 40-hex sha, or empty"))?;
    let message = raw.commit_message.unwrap_or_default();
    if message.len() > MESSAGE_BYTES_MAX || message.contains('\0') {
        return Err(invalid(format!("commit_message is at most {MESSAGE_BYTES_MAX} bytes, with no NUL")));
    }
    if let Some(b) = &raw.base_branch {
        if !names::valid_branch(b) {
            return Err(invalid(format!("invalid base_branch {b:?}")));
        }
    }
    let raw_files = raw.files.unwrap_or_default();
    if raw_files.len() > PACK_FILES_MAX {
        return Err(PackError::TooLarge(format!("a pack changes at most {PACK_FILES_MAX} files")));
    }
    let mut files = Vec::with_capacity(raw_files.len());
    let mut paths = HashSet::with_capacity(raw_files.len());
    let mut ids = HashSet::new();
    for f in raw_files {
        let path = f.path.ok_or_else(|| invalid("a file needs path and operation"))?;
        let operation = f.operation.ok_or_else(|| invalid("a file needs path and operation"))?;
        if let Err(fault) = names::check_path(&path) {
            return Err(invalid(format!("invalid path {path:?}: {}", fault.describe())));
        }
        if !paths.insert(path.clone()) {
            return Err(invalid(format!("{path:?} appears twice")));
        }
        let op = match operation.as_str() {
            "upsert" => {
                let content_id = f.content_id.filter(|c| !c.is_empty()).ok_or_else(|| invalid(format!("missing content_id for {path}")))?;
                if content_id.len() > CONTENT_ID_BYTES_MAX {
                    return Err(invalid(format!("a content_id longer than {CONTENT_ID_BYTES_MAX} bytes for {path}")));
                }
                if !ids.insert(content_id.clone()) {
                    return Err(invalid(format!("content_id {content_id:?} names two upserts")));
                }
                let mode = match f.mode.as_deref() {
                    None => 0o100644,
                    Some(m) => names::parse_mode(m).ok_or_else(|| invalid(format!("mode {m:?} for {path}: 100644 or 100755")))?,
                };
                Op::Upsert { content_id, mode }
            }
            "delete" => Op::Delete,
            other => return Err(invalid(format!("operation {other:?} for {path}: upsert or delete"))),
        };
        files.push(FileOp { path, op });
    }
    Ok(Metadata {
        target_branch,
        expected,
        message,
        author: raw.author,
        committer: raw.committer,
        files,
        base_branch: raw.base_branch,
        ephemeral: raw.ephemeral.unwrap_or(false),
        base_is_ephemeral: raw.base_is_ephemeral.unwrap_or(false),
    })
}

/// One upsert's stream: its bytes so far, and whether it ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stream {
    pub bytes: u64,
    pub eof: bool,
}

/// The parser of one pack.
#[derive(Default)]
pub struct Parser {
    line: Vec<u8>,
    /// The metadata's files, once read.
    files: Option<Vec<FileOp>>,
    /// An upsert's `content_id` → its file's index.
    ids: HashMap<String, usize>,
    streams: Vec<Stream>,
    decoded: u64,
}

impl Parser {
    pub fn new() -> Parser {
        Parser::default()
    }

    fn line_max(&self) -> usize {
        if self.files.is_some() {
            PACK_LINE_BYTES_MAX
        } else {
            PACK_METADATA_BYTES_MAX
        }
    }

    /// Feeds bytes as they arrive; answers the events their whole lines
    /// make.
    pub fn push(&mut self, mut bytes: &[u8]) -> Result<Vec<Event>, PackError> {
        let mut events = Vec::new();
        // Bounded by the input: each pass takes at least one byte.
        while !bytes.is_empty() {
            let (piece, rest, whole) = match bytes.iter().position(|b| *b == b'\n') {
                Some(i) => (&bytes[..i], &bytes[i + 1..], true),
                None => (bytes, &[][..], false),
            };
            if self.line.len() + piece.len() > self.line_max() {
                return Err(PackError::TooLarge(format!("a pack line longer than {} bytes", self.line_max())));
            }
            self.line.extend_from_slice(piece);
            if whole {
                let line = std::mem::take(&mut self.line);
                if let Some(e) = self.take_line(&line)? {
                    events.push(e);
                }
            }
            bytes = rest;
        }
        Ok(events)
    }

    /// At the body's end: the last line, if it had no newline, then the
    /// checks only the whole pack can make.
    pub fn finish(&mut self) -> Result<Vec<Event>, PackError> {
        let line = std::mem::take(&mut self.line);
        let events: Vec<Event> = self.take_line(&line)?.into_iter().collect();
        let files = self.files.as_ref().ok_or(PackError::Empty)?;
        for (f, s) in files.iter().zip(&self.streams) {
            if matches!(f.op, Op::Upsert { .. }) && !s.eof {
                return Err(invalid(format!("incomplete content stream for {}", f.path)));
            }
        }
        Ok(events)
    }

    /// Each file's stream (an upsert's bytes; a delete's stays empty).
    pub fn streams(&self) -> &[Stream] {
        &self.streams
    }

    /// Decoded bytes so far, every file together.
    pub fn decoded(&self) -> u64 {
        self.decoded
    }

    fn take_line(&mut self, line: &[u8]) -> Result<Option<Event>, PackError> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        if self.files.is_none() {
            let v: serde_json::Value = serde_json::from_slice(line).map_err(|_| PackError::Empty)?;
            let Some(meta) = v.get("metadata").filter(|m| m.is_object()) else { return Err(PackError::FirstNotMetadata) };
            let raw: RawMeta = serde_json::from_value(meta.clone()).map_err(|e| invalid(format!("metadata: {e}")))?;
            let meta = check_metadata(raw)?;
            for (i, f) in meta.files.iter().enumerate() {
                if let Op::Upsert { content_id, .. } = &f.op {
                    self.ids.insert(content_id.clone(), i);
                }
            }
            self.streams = vec![Stream::default(); meta.files.len()];
            self.files = Some(meta.files.clone());
            return Ok(Some(Event::Metadata(meta)));
        }
        let parsed: ChunkLine<'_> = serde_json::from_slice(line).map_err(|_| invalid("a line is not JSON"))?;
        let chunk = parsed.blob_chunk.ok_or_else(|| invalid("a line is not a blob_chunk"))?;
        let id = chunk.content_id.ok_or_else(|| invalid("a blob_chunk without content_id"))?;
        let file = *self.ids.get(id.as_ref()).ok_or_else(|| invalid(format!("unexpected content_id {:?}", id.as_ref())))?;
        let path = &self.files.as_ref().expect("metadata came first")[file].path;
        let stream = &mut self.streams[file];
        if stream.eof {
            return Err(invalid(format!("content after eof for {path}")));
        }
        let data = chunk.data.unwrap_or_default();
        // A chunk's base64 is at most 4/3 of its bytes, plus padding: one
        // longer than that is refused before it is decoded.
        if data.len() > CHUNK_DECODED_BYTES_MAX / 3 * 4 + 8 {
            return Err(PackError::TooLarge(format!("blob chunk size {} exceeds maximum {CHUNK_DECODED_BYTES_MAX}", data.len() / 4 * 3)));
        }
        let bytes = B64.decode(data.as_bytes()).map_err(|_| invalid(format!("blob_chunk data for {path} is not base64")))?;
        if bytes.len() > CHUNK_DECODED_BYTES_MAX {
            return Err(PackError::TooLarge(format!("blob chunk size {} exceeds maximum {CHUNK_DECODED_BYTES_MAX}", bytes.len())));
        }
        stream.bytes += bytes.len() as u64;
        stream.eof = chunk.eof.unwrap_or(false);
        if stream.bytes > BLOB_BYTES_MAX {
            return Err(PackError::TooLarge(format!("{path} passes {BLOB_BYTES_MAX} bytes")));
        }
        self.decoded += bytes.len() as u64;
        if self.decoded > PACK_DECODED_BYTES_MAX {
            return Err(PackError::TooLarge(format!("the pack passes {PACK_DECODED_BYTES_MAX} decoded bytes")));
        }
        Ok(Some(Event::Chunk { file, data: bytes }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn b64(b: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(b)
    }

    fn meta(files: serde_json::Value) -> String {
        json!({"metadata": {"target_branch": "main", "expected_target_sha": "3033acb8fd0d39124facfc02ff13d974de0e6568", "commit_message": "fragment sync todo.paul: +1 -1",
            "author": {"name": "fragment/1a2b3c4d", "email": "1a2b3c4d@users.fragment"}, "files": files}})
        .to_string()
    }

    fn chunk(id: &str, data: &[u8], eof: bool) -> String {
        json!({"blob_chunk": {"content_id": id, "data": b64(data), "eof": eof}}).to_string()
    }

    /// Feeds `text` in pieces of `step` bytes; answers each file's bytes.
    fn run(text: &str, step: usize) -> Result<(Metadata, Vec<Vec<u8>>), PackError> {
        let mut p = Parser::new();
        let mut events = Vec::new();
        for piece in text.as_bytes().chunks(step) {
            events.extend(p.push(piece)?);
        }
        events.extend(p.finish()?);
        let mut meta = None;
        let mut bytes: Vec<Vec<u8>> = Vec::new();
        for e in events {
            match e {
                Event::Metadata(m) => {
                    bytes = vec![Vec::new(); m.files.len()];
                    meta = Some(m);
                }
                Event::Chunk { file, data } => bytes[file].extend(data),
            }
        }
        Ok((meta.expect("metadata"), bytes))
    }

    // Goal: the pack fragment's encoder writes (contract 5.8, its
    // example: an upsert and a delete that streams nothing) parses, fed
    // whole or a byte at a time. Method: the example, both ways.
    #[test]
    fn fragments_pack_parses() {
        let text = format!(
            "{}\n{}\n",
            meta(json!([{"path": "notes/a.md", "operation": "upsert", "content_id": "c0", "mode": "100644"}, {"path": "old.md", "operation": "delete", "content_id": "c1"}])),
            chunk("c0", b"hello\n", true)
        );
        for step in [text.len(), 7, 1] {
            let (m, bytes) = run(&text, step).unwrap();
            assert_eq!(m.target_branch, "main");
            assert_eq!(m.expected, Expected::At("3033acb8fd0d39124facfc02ff13d974de0e6568".into()));
            assert_eq!(m.files[1], FileOp { path: "old.md".into(), op: Op::Delete });
            assert_eq!(bytes, vec![b"hello\n".to_vec(), vec![]]);
        }
    }

    // Goal: chunks interleave, an empty file is one empty chunk, CRLF and
    // blank lines and a last line with no newline are taken, and the
    // service SDKs' `expected_head_sha` is read. Method: such a pack.
    #[test]
    fn interleaved_empty_and_loose_framing() {
        let m = json!({"metadata": {"target_branch": "main", "expected_head_sha": "", "files": [
            {"path": "a.txt", "operation": "upsert", "content_id": "c0"}, {"path": "b.txt", "operation": "upsert", "content_id": "c1", "mode": "100755"},
            {"path": "e.txt", "operation": "upsert", "content_id": "c2"}]}});
        let text = format!("{m}\r\n\n{}\r\n{}\n{}\n{}", chunk("c0", b"al", false), chunk("c1", b"beta", true), chunk("c0", b"pha", true), chunk("c2", b"", true));
        let (m, bytes) = run(&text, 5).unwrap();
        assert_eq!(m.expected, Expected::Absent);
        assert_eq!(m.files[1].op, Op::Upsert { content_id: "c1".into(), mode: 0o100755 });
        assert_eq!(bytes, vec![b"alpha".to_vec(), b"beta".to_vec(), vec![]]);
    }

    // Goal: each refusal of the contract's table (5.8), with the
    // service's status and message. Method: one pack per fault.
    #[test]
    fn refusals_are_the_services() {
        let up = |p: &str, id: &str| json!({"path": p, "operation": "upsert", "content_id": id});
        let err = |text: &str| run(text, 64).unwrap_err();
        assert_eq!(err(""), PackError::Empty);
        assert_eq!(err("not json\n"), PackError::Empty);
        assert_eq!(err(&format!("{}\n", chunk("c0", b"x", true))), PackError::FirstNotMetadata);
        assert_eq!(err(&format!("{}\n", meta(json!([{"path": "a", "operation": "upsert"}])))), PackError::Invalid("missing content_id for a".into()));
        let delete = json!({"path": "old.md", "operation": "delete", "content_id": "c1"});
        assert_eq!(err(&format!("{}\n{}\n", meta(json!([delete])), chunk("c1", b"", true))), PackError::Invalid("unexpected content_id \"c1\"".into()), "a delete streams nothing");
        assert_eq!(err(&format!("{}\n{}\n", meta(json!([up("a", "c0")])), chunk("c9", b"x", true))), PackError::Invalid("unexpected content_id \"c9\"".into()));
        assert_eq!(err(&format!("{}\n{}\n", meta(json!([up("a", "c0")])), chunk("c0", b"x", false))), PackError::Invalid("incomplete content stream for a".into()));
        assert_eq!(err(&format!("{}\n", meta(json!([up("a", "c0")])))), PackError::Invalid("incomplete content stream for a".into()));
        assert!(matches!(err(&format!("{}\n{}\n{}\n", meta(json!([up("a", "c0")])), chunk("c0", b"x", true), chunk("c0", b"y", true))), PackError::Invalid(m) if m.contains("after eof")));
        let big = vec![7u8; CHUNK_DECODED_BYTES_MAX + 1];
        let e = err(&format!("{}\n{}\n", meta(json!([up("a", "c0")])), chunk("c0", &big, true)));
        assert!(matches!(&e, PackError::TooLarge(m) if m.contains("exceeds maximum 4194304")), "{e:?}");
        assert_eq!((e.status(), e.result_status()), (413, "payload_too_large"));
        assert_eq!(PackError::Empty.shape(), Shape::Problem);
        assert_eq!(PackError::FirstNotMetadata.shape(), Shape::Result);
        // metadata faults
        for (files, says) in [
            (json!([up("a", "c0"), up("a", "c1")]), "appears twice"),
            (json!([up("a", "c0"), up("b", "c0")]), "names two upserts"),
            (json!([up(".git/config", "c0")]), "invalid path"),
            (json!([up("../x", "c0")]), "invalid path"),
            (json!([{"path": "a", "operation": "rename"}]), "upsert or delete"),
            (json!([{"path": "a", "operation": "upsert", "content_id": "c0", "mode": "120000"}]), "100644 or 100755"),
        ] {
            assert!(matches!(err(&format!("{}\n", meta(files.clone()))), PackError::Invalid(m) if m.contains(says)), "{files} → {says}");
        }
        assert!(matches!(err(&format!("{}\n", json!({"metadata": {"files": []}}))), PackError::Invalid(m) if m.contains("target_branch")));
        assert!(matches!(err(&format!("{}\n", json!({"metadata": {"target_branch": "a..b"}}))), PackError::Invalid(_)));
        assert!(matches!(err(&format!("{}\n", json!({"metadata": {"target_branch": "main", "expected_target_sha": "main"}}))), PackError::Invalid(_)));
        assert!(matches!(err(&format!("{}\n{}\n", meta(json!([up("a", "c0")])), json!({"blob_chunk": {"content_id": "c0", "data": "!!", "eof": true}}))), PackError::Invalid(m) if m.contains("base64")));
    }

    // Goal: a line past its bound is refused before it is held whole, and
    // a file or a pack past its decoded bound is refused. Method: a line
    // that never ends, fed in pieces.
    #[test]
    fn bounds_hold() {
        let mut p = Parser::new();
        let piece = vec![b'x'; 1 << 20];
        let mut refused = false;
        for _ in 0..(PACK_METADATA_BYTES_MAX >> 20) + 2 {
            if let Err(e) = p.push(&piece) {
                assert_eq!(e.status(), 413);
                refused = true;
                break;
            }
        }
        assert!(refused);
        assert!(p.line.len() <= PACK_METADATA_BYTES_MAX);
    }

    // Goal: a pack sent again parses to the same events (a client's
    // replay is the store's to refuse, by its compare-and-swap). Method:
    // one text, two parsers.
    #[test]
    fn a_replay_parses_the_same() {
        let text = format!("{}\n{}\n", meta(json!([{"path": "a", "operation": "upsert", "content_id": "c0"}])), chunk("c0", b"x", true));
        assert_eq!(run(&text, 3).unwrap(), run(&text, 1000).unwrap());
    }
}
