//! git 2.55.0 as macrofiche's engine (docs/library.md, docs/design.md): the
//! pinned build (`pin`), every process jailed (`jail`, `linux::landlock`,
//! `linux::seccomp`), the plumbing as typed calls (`linux::plumbing`), a
//! pool of `cat-file` readers (`linux::catfile`), and errors classified
//! from exit statuses and stderr (`error`). The parsers, the jail's plan,
//! the fast-import stream, and the classification are pure and tested on
//! any host; the processes need Linux and the built engine.

pub mod error;
pub mod fastimport;
pub mod jail;
pub mod oid;
pub mod parse;
pub mod pin;

#[cfg(target_os = "linux")]
pub mod linux;

pub use error::{GitError, RefRefusal};
pub use fastimport::Ident;
pub use oid::Oid;

#[cfg(target_os = "linux")]
pub use linux::{
    catfile::{BlobReader, CatFile, ObjInfo, Pool},
    plumbing::{Change, Merged, RefOp, Source},
    process::{Engine, Output, Repo, RunOpts},
};
