//! What can go wrong running git, typed; and what a refused ref update
//! means, read from git's stderr. The messages are git 2.55.0's (the pin);
//! `tests/conformance.rs` makes the engine produce each one and checks its
//! reading, so a new git that rewords one fails a test before it turns a
//! 409 into a 500.

use std::time::Duration;

use thiserror::Error;

use crate::oid::Oid;
use crate::parse::ParseError;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("git {op}: could not start: {source}")]
    Spawn { op: &'static str, source: std::io::Error },
    #[error("git {op}: no answer within {after:?}")]
    Timeout { op: &'static str, after: Duration },
    #[error("git {op}: its output passed {max} bytes")]
    OutputTooLarge { op: &'static str, max: usize },
    #[error("git {op}: exited {code}: {stderr}")]
    Failed { op: &'static str, code: i32, stderr: String },
    #[error("git {op}: killed by signal {signal}: {stderr}")]
    Killed { op: &'static str, signal: i32, stderr: String },
    #[error("git {op}: {source}")]
    Io { op: &'static str, source: std::io::Error },
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error("git {op}: {detail}")]
    Protocol { op: &'static str, detail: String },
    #[error("the jail: {0}")]
    Jail(String),
    #[error("the engine at {prefix}: {detail}")]
    Engine { prefix: String, detail: String },
}

impl GitError {
    /// The stderr of a git that exited with an error, if this is one.
    pub fn stderr(&self) -> Option<&str> {
        match self {
            GitError::Failed { stderr, .. } | GitError::Killed { stderr, .. } => Some(stderr),
            _ => None,
        }
    }
}

/// Bytes of a git's stderr kept in an error.
pub const STDERR_KEPT_BYTES: usize = 2048;

/// Why `update-ref --stdin` refused a transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefRefusal {
    /// `create` of a ref that exists: "reference already exists".
    Exists,
    /// `update` whose old value is not the ref's: "is at X but expected Y".
    Moved { current: Oid },
    /// `update` with an old value of a ref that does not exist: "unable to
    /// resolve reference".
    Missing,
    /// Another writer holds the ref's lock, or a crash left it: "Unable to
    /// create '….lock': File exists".
    Locked,
    /// A ref whose name is a directory of others, or the reverse:
    /// "'refs/heads/a' exists; cannot create 'refs/heads/a/b'".
    NameConflict,
    /// The new value is no object: "with nonexistent object".
    MissingObject,
}

/// Reads a refused transaction's stderr; `None` for anything else, which
/// the caller treats as a failure of the store, never as a refusal.
pub fn classify_update_ref(stderr: &str) -> Option<RefRefusal> {
    if stderr.contains("reference already exists") {
        return Some(RefRefusal::Exists);
    }
    if let Some(i) = stderr.find(" but expected ") {
        let before = &stderr[..i];
        let current = before.rsplit("is at ").next().and_then(|s| Oid::parse(s.trim()))?;
        return Some(RefRefusal::Moved { current });
    }
    if stderr.contains("unable to resolve reference") {
        return Some(RefRefusal::Missing);
    }
    if stderr.contains(".lock': File exists") {
        return Some(RefRefusal::Locked);
    }
    if stderr.contains("exists; cannot create") || (stderr.contains("cannot process") && stderr.contains("at the same time")) {
        return Some(RefRefusal::NameConflict);
    }
    if stderr.contains("with nonexistent object") {
        return Some(RefRefusal::MissingObject);
    }
    None
}

/// Whether a merge-base, rev-list, or commit-tree failed because a name
/// is no commit (a 404 or 412 to the caller, not a 500).
pub fn is_unknown_object(stderr: &str) -> bool {
    ["Not a valid commit name", "is not a valid object", "not a valid commit", "bad revision", "bad object", "unknown revision", "Not a valid object name", "not a tree object"]
        .iter()
        .any(|m| stderr.contains(m))
}

/// Whether merge-tree refused because the two have no common history.
pub fn is_unrelated(stderr: &str) -> bool {
    stderr.contains("refusing to merge unrelated histories")
}

#[cfg(test)]
mod tests {
    use super::*;

    const C1: &str = "3033acb8fd0d39124facfc02ff13d974de0e6568";

    // Goal: each refusal git 2.55.0 writes (captured from the engine) is
    // read as its kind, and anything else as none. Method: the messages.
    #[test]
    fn refusals_are_read() {
        let r = |s: &str| classify_update_ref(s);
        assert_eq!(r("fatal: cannot lock ref 'refs/heads/main': reference already exists\n"), Some(RefRefusal::Exists));
        assert_eq!(
            r(&format!("fatal: cannot lock ref 'refs/heads/main': is at {C1} but expected 0123456789012345678901234567890123456789\n")),
            Some(RefRefusal::Moved { current: Oid::parse(C1).unwrap() })
        );
        assert_eq!(r("fatal: cannot lock ref 'refs/heads/live': unable to resolve reference 'refs/heads/live'\n"), Some(RefRefusal::Missing));
        assert_eq!(
            r("fatal: cannot lock ref 'refs/heads/main': Unable to create '/tmp/mf/r/refs/heads/main.lock': File exists.\n\nAnother git process seems to be running in this repository"),
            Some(RefRefusal::Locked)
        );
        assert_eq!(r("fatal: cannot lock ref 'refs/heads/m/y': 'refs/heads/m' exists; cannot create 'refs/heads/m/y'\n"), Some(RefRefusal::NameConflict));
        assert_eq!(r("fatal: trying to write ref 'refs/heads/main' with nonexistent object 0123456789012345678901234567890123456789\n"), Some(RefRefusal::MissingObject));
        assert_eq!(r("fatal: out of memory\n"), None);
        assert_eq!(r("is at nothing but expected"), None, "a moved ref names its current id");
        assert!(is_unknown_object("fatal: Not a valid commit name 0123"));
        assert!(is_unknown_object("fatal: bad revision 'refs/heads/nope'"));
        assert!(!is_unknown_object("fatal: out of memory"));
        assert!(is_unrelated("fatal: refusing to merge unrelated histories"));
    }
}
