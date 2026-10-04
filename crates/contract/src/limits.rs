//! Every limit of the API, as a constant (docs/design.md, Limits). Each is
//! checked where its input arrives; a value past one is refused, never
//! truncated.

/// Decoded bytes of one `blob_chunk` (the service's: 413 above it).
pub const CHUNK_DECODED_BYTES_MAX: usize = 4 << 20;
/// Bytes of one commit-pack line: a 4 MiB chunk is 5.6 MB of base64.
pub const PACK_LINE_BYTES_MAX: usize = 8 << 20;
/// Bytes of a commit pack's metadata line: its file list.
pub const PACK_METADATA_BYTES_MAX: usize = 16 << 20;
/// Files one commit pack may change.
pub const PACK_FILES_MAX: usize = 20_000;
/// Decoded bytes of one file in a commit pack.
pub const BLOB_BYTES_MAX: u64 = 1 << 30;
/// Decoded bytes of one commit pack, every file together.
pub const PACK_DECODED_BYTES_MAX: u64 = 2 << 30;
/// Bytes of one commit pack's body, as sent (base64 and JSON included).
pub const PACK_BODY_BYTES_MAX: u64 = 3 << 30;
/// Bytes of a `content_id`.
pub const CONTENT_ID_BYTES_MAX: usize = 256;

/// A file path's bytes.
pub const PATH_BYTES_MAX: usize = 4096;
/// A repository name's bytes.
pub const REPO_NAME_BYTES_MAX: usize = 128;
/// A branch name's bytes.
pub const BRANCH_NAME_BYTES_MAX: usize = 255;
/// An org name's bytes.
pub const ORG_NAME_BYTES_MAX: usize = 64;
/// A commit message's bytes.
pub const MESSAGE_BYTES_MAX: usize = 64 * 1024;

/// A JSON request body (create, branches/create, merge) or a restore's
/// NDJSON body.
pub const JSON_BODY_BYTES_MAX: usize = 1 << 20;
/// A query string's bytes, and its parameters.
pub const QUERY_BYTES_MAX: usize = 16 * 1024;
pub const QUERY_PARAMS_MAX: usize = 32;
/// A bearer token's bytes.
pub const TOKEN_BYTES_MAX: usize = 8 * 1024;
/// How far in the future a token may expire (the service's bound for
/// restricted keys, applied to every key).
pub const TOKEN_TTL_S_MAX: i64 = 3600;

/// `files/metadata`'s `limit`: the service's default and cap.
pub const FILES_LIMIT_DEFAULT: usize = 200;
pub const FILES_LIMIT_MAX: usize = 1000;
/// `GET /api/repos`'s `limit`.
pub const REPOS_LIMIT_DEFAULT: usize = 20;
pub const REPOS_LIMIT_MAX: usize = 100;
/// `commits`'s `limit`.
pub const COMMITS_LIMIT_DEFAULT: usize = 20;
pub const COMMITS_LIMIT_MAX: usize = 100;
/// History a `commits` cursor may skip.
pub const COMMITS_SKIP_MAX: usize = 1_000_000;

/// Repositories one org may hold, and all orgs together.
pub const REPOS_PER_ORG_MAX: usize = 100_000;
/// Orgs a config may name, and keys and webhooks per org.
pub const ORGS_MAX: usize = 64;
pub const KEYS_PER_ORG_MAX: usize = 16;
pub const WEBHOOKS_PER_ORG_MAX: usize = 8;
