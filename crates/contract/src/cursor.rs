//! Opaque cursors: base64url of a tagged text, checked when read back. A
//! cursor pins what its first page saw (a listing's commit, a history's
//! tip), so a branch that moves between pages never mixes two snapshots.

use base64::Engine as _;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;
/// Bytes of a cursor as sent.
pub const CURSOR_BYTES_MAX: usize = 512;

/// What a cursor names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cursor {
    /// `files/metadata`: the commit listed and the next entry's index.
    Files { commit: String, offset: usize },
    /// `GET /api/repos`: the last repository of the page before, newest
    /// first by creation, then id.
    Repos { created_ms: i64, repo_id: String },
    /// `commits`: the tip walked and the commits already shown.
    Commits { tip: String, offset: usize },
}

impl Cursor {
    pub fn encode(&self) -> String {
        let text = match self {
            Cursor::Files { commit, offset } => format!("f1:{commit}:{offset}"),
            Cursor::Repos { created_ms, repo_id } => format!("r1:{created_ms}:{repo_id}"),
            Cursor::Commits { tip, offset } => format!("c1:{tip}:{offset}"),
        };
        B64.encode(text)
    }

    /// The cursor, or `None` for one this server did not write (400).
    pub fn decode(s: &str) -> Option<Cursor> {
        if s.is_empty() || s.len() > CURSOR_BYTES_MAX {
            return None;
        }
        let text = String::from_utf8(B64.decode(s).ok()?).ok()?;
        let mut f = text.splitn(3, ':');
        let (kind, a, b) = (f.next()?, f.next()?, f.next()?);
        match kind {
            "f1" if crate::names::is_sha(a) => Some(Cursor::Files { commit: a.into(), offset: b.parse().ok()? }),
            "r1" if !b.is_empty() && b.bytes().all(|c| c.is_ascii_alphanumeric()) => Some(Cursor::Repos { created_ms: a.parse().ok()?, repo_id: b.into() }),
            "c1" if crate::names::is_sha(a) => Some(Cursor::Commits { tip: a.into(), offset: b.parse().ok()? }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Goal: every cursor round-trips, and one this server did not write
    // is refused. Method: each kind, then forgeries.
    #[test]
    fn cursors_round_trip_and_forgeries_fail() {
        let sha = "3033acb8fd0d39124facfc02ff13d974de0e6568".to_string();
        for c in [
            Cursor::Files { commit: sha.clone(), offset: 1000 },
            Cursor::Repos { created_ms: 1_790_000_000_123, repo_id: "a1b2".into() },
            Cursor::Commits { tip: sha.clone(), offset: 20 },
        ] {
            assert_eq!(Cursor::decode(&c.encode()), Some(c));
        }
        for bad in ["", "page-2", "!!!", &B64.encode("f1:short:1"), &B64.encode(format!("f1:{sha}:-1")), &B64.encode("r1:x:id"), &B64.encode("r1:1:a/b"), &B64.encode("z1:a:b"), &"A".repeat(600)] {
            assert_eq!(Cursor::decode(bad), None, "{bad}");
        }
    }
}
