//! What a store call answers when it does not do what was asked, as the
//! API's problem: absent things are 404, malformed asks 400, collisions
//! 409, and a failure to reach git 503, never a 404 (contract 5.4).

use macrofiche_contract::Problem;
use macrofiche_git::GitError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("{detail}")]
    NotFound { code: &'static str, detail: String },
    #[error("{detail}")]
    Invalid { code: &'static str, detail: String },
    #[error("{detail}")]
    Conflict { code: &'static str, detail: String },
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Internal(String),
}

impl StoreError {
    pub fn not_found(code: &'static str, detail: impl Into<String>) -> StoreError {
        StoreError::NotFound { code, detail: detail.into() }
    }

    pub fn invalid(code: &'static str, detail: impl Into<String>) -> StoreError {
        StoreError::Invalid { code, detail: detail.into() }
    }

    pub fn conflict(code: &'static str, detail: impl Into<String>) -> StoreError {
        StoreError::Conflict { code, detail: detail.into() }
    }

    pub fn problem(&self) -> Problem {
        match self {
            StoreError::NotFound { code, detail } => Problem::not_found(code, detail.clone()),
            StoreError::Invalid { code, detail } => Problem::bad_request(code, detail.clone()),
            StoreError::Conflict { code, detail } => Problem::conflict(code, detail.clone()),
            StoreError::Unavailable(d) => Problem::unavailable(d.clone()),
            StoreError::Internal(d) => Problem::internal(d.clone()),
        }
    }
}

impl From<GitError> for StoreError {
    fn from(e: GitError) -> StoreError {
        match e {
            GitError::Parse(_) | GitError::Protocol { .. } => StoreError::Internal(e.to_string()),
            _ => StoreError::Unavailable(e.to_string()),
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> StoreError {
        StoreError::Unavailable(format!("the store's disk: {e}"))
    }
}
