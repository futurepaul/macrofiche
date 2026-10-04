//! Query strings and path segments, decoded once (contract section 4):
//! a query as `application/x-www-form-urlencoded` (`+` is a space, as
//! the cell's `form_urlencoded` writes it), a path segment by percent
//! escapes alone.

use thiserror::Error;

use crate::limits::{QUERY_BYTES_MAX, QUERY_PARAMS_MAX};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum QueryError {
    #[error("a query longer than {QUERY_BYTES_MAX} bytes or with more than {QUERY_PARAMS_MAX} parameters")]
    TooLarge,
    #[error("a malformed percent escape")]
    Escape,
    #[error("a query or path that is not UTF-8 once decoded")]
    NotUtf8,
    #[error("{0} must be {1}")]
    Value(&'static str, &'static str),
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn decode(s: &str, plus_is_space: bool) -> Result<String, QueryError> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    // Bounded by the input's length: each pass takes one or three bytes.
    while i < b.len() {
        match b[i] {
            b'%' => {
                let (Some(h), Some(l)) = (b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) else { return Err(QueryError::Escape) };
                out.push(h << 4 | l);
                i += 3;
            }
            b'+' if plus_is_space => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| QueryError::NotUtf8)
}

/// A path segment (a repository's name in `/api/repos/{repo}/…`), decoded
/// once: `%2F` becomes `/`, `+` stays `+`.
pub fn decode_segment(s: &str) -> Result<String, QueryError> {
    decode(s, false)
}

/// A decoded query: its parameters in order. The first of a repeated
/// name is the one read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query(Vec<(String, String)>);

impl Query {
    pub fn parse(raw: Option<&str>) -> Result<Query, QueryError> {
        let Some(raw) = raw.filter(|r| !r.is_empty()) else { return Ok(Query::default()) };
        if raw.len() > QUERY_BYTES_MAX {
            return Err(QueryError::TooLarge);
        }
        let mut params = Vec::new();
        for pair in raw.split('&').filter(|p| !p.is_empty()) {
            if params.len() == QUERY_PARAMS_MAX {
                return Err(QueryError::TooLarge);
            }
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            params.push((decode(k, true)?, decode(v, true)?));
        }
        Ok(Query(params))
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// A boolean: `true` or `1`, `false` or `0`; absent is `false`.
    pub fn flag(&self, name: &'static str) -> Result<bool, QueryError> {
        match self.get(name) {
            None | Some("false") | Some("0") | Some("") => Ok(false),
            Some("true") | Some("1") => Ok(true),
            Some(_) => Err(QueryError::Value(name, "true or false")),
        }
    }

    /// A count: absent is `default`; otherwise a decimal, kept within
    /// `1..=max` (the service caps a large one rather than refusing it).
    pub fn limit(&self, name: &'static str, default: usize, max: usize) -> Result<usize, QueryError> {
        match self.get(name) {
            None | Some("") => Ok(default),
            Some(v) => v.parse::<usize>().map(|n| n.clamp(1, max)).map_err(|_| QueryError::Value(name, "a whole number")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Goal: the cell's and the CLI's encodings decode to what they meant:
    // `+` and `%20` are spaces in a query, `%2F` a slash in a segment.
    // Method: their encodings of paths with spaces, slashes, and
    // non-ASCII.
    #[test]
    fn encodings_decode() {
        let q = Query::parse(Some("path=notes%2Fa+b.md&ref=main&path=second")).unwrap();
        assert_eq!(q.get("path"), Some("notes/a b.md"), "the first of a repeated name");
        assert_eq!(q.get("ref"), Some("main"));
        assert_eq!(Query::parse(Some("path=a%20b%2Bc")).unwrap().get("path"), Some("a b+c"));
        assert_eq!(Query::parse(Some("path=caf%C3%A9.md")).unwrap().get("path"), Some("café.md"));
        assert_eq!(decode_segment("main--todo--paul").unwrap(), "main--todo--paul");
        assert_eq!(decode_segment("acme%2Fweb").unwrap(), "acme/web");
        assert_eq!(decode_segment("a+b").unwrap(), "a+b");
        assert_eq!(Query::parse(None).unwrap(), Query::default());
    }

    // Goal: malformed queries are refused, not guessed at.
    #[test]
    fn malformed_is_refused() {
        assert_eq!(Query::parse(Some("a=%zz")), Err(QueryError::Escape));
        assert_eq!(Query::parse(Some("a=%4")), Err(QueryError::Escape));
        assert_eq!(Query::parse(Some("a=%ff")), Err(QueryError::NotUtf8));
        assert_eq!(Query::parse(Some(&"a=1&".repeat(QUERY_PARAMS_MAX + 1))), Err(QueryError::TooLarge));
        assert_eq!(Query::parse(Some(&format!("a={}", "x".repeat(QUERY_BYTES_MAX)))), Err(QueryError::TooLarge));
    }

    // Goal: limits default, clamp, and refuse what is no number; flags
    // read the forms clients send.
    #[test]
    fn limits_and_flags() {
        let q = Query::parse(Some("limit=5000&small=0&bad=x&e=true")).unwrap();
        assert_eq!(q.limit("limit", 200, 1000), Ok(1000));
        assert_eq!(q.limit("small", 200, 1000), Ok(1));
        assert_eq!(q.limit("none", 200, 1000), Ok(200));
        assert!(q.limit("bad", 200, 1000).is_err());
        assert_eq!(q.flag("e"), Ok(true));
        assert_eq!(q.flag("none"), Ok(false));
        assert!(q.flag("bad").is_err());
    }
}
