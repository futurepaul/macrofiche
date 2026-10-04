//! code.storage's API as fragment uses it (docs/contract.md), as pure data:
//! the limits, the names and their checks, query and segment decoding,
//! cursors, times, and problem bodies. No I/O; everything here is tested
//! on any host.

pub mod cursor;
pub mod limits;
pub mod names;
pub mod problem;
pub mod query;
pub mod time;

pub use cursor::Cursor;
pub use names::Expected;
pub use problem::Problem;
pub use query::Query;
