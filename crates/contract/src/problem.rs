//! RFC 9457 problem details, as the service sends them (contract section
//! 4): `application/problem+json`, `{type, title, status, code, detail,
//! error}`, `error` mirroring `detail` for older clients. fragment's CLI
//! shows `detail`; its cell keeps the first 300 bytes.

use serde_json::{json, Value};

pub const CONTENT_TYPE: &str = "application/problem+json";

/// A refusal or a failure: its status, its stable code, and what happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub status: u16,
    pub code: &'static str,
    pub detail: String,
}

impl Problem {
    pub fn new(status: u16, code: &'static str, detail: impl Into<String>) -> Problem {
        assert!((400..600).contains(&status), "a problem is a 4xx or a 5xx");
        Problem { status, code, detail: detail.into() }
    }

    pub fn bad_request(code: &'static str, detail: impl Into<String>) -> Problem {
        Problem::new(400, code, detail)
    }

    pub fn not_found(code: &'static str, detail: impl Into<String>) -> Problem {
        Problem::new(404, code, detail)
    }

    pub fn conflict(code: &'static str, detail: impl Into<String>) -> Problem {
        Problem::new(409, code, detail)
    }

    /// A passing failure: never a 404, which clients read as "absent"
    /// (contract 5.4).
    pub fn unavailable(detail: impl Into<String>) -> Problem {
        Problem::new(503, "unavailable", detail)
    }

    pub fn internal(detail: impl Into<String>) -> Problem {
        Problem::new(500, "internal_error", detail)
    }

    pub fn body(&self) -> Value {
        json!({
            "type": "about:blank",
            "title": title(self.status),
            "status": self.status,
            "code": self.code,
            "detail": self.detail,
            "error": self.detail,
        })
    }
}

/// The status's reason phrase.
pub fn title(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Goal: the body carries every field the service's does, `error`
    // equal to `detail`. Method: e2e's exact branch answer (contract 5.4).
    #[test]
    fn a_problem_is_the_services_shape() {
        let b = Problem::not_found("branch_not_found", "branch not found").body();
        assert_eq!(b, json!({"type": "about:blank", "title": "Not Found", "status": 404, "code": "branch_not_found", "detail": "branch not found", "error": "branch not found"}));
        assert_eq!(Problem::unavailable("x").status, 503);
    }
}
