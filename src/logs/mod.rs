//! The log pipeline (ADR-0067, ADR-0068): what turns what containers print, and what hoster
//! logs itself, into records for the log store ([`crate::store::LogStore`]).
//!
//! [`ParsedLine`] is the first step: one line a container printed, mapped to a level, a message
//! and the remaining fields of a JSON line, or kept whole when it is not JSON (ADR-0067 §3).

#![allow(
    dead_code,
    reason = "the parser is used by the container log followers as they land (epic f8z)"
)]

mod line;

#[allow(
    unused_imports,
    reason = "read by the container log followers as they land (epic f8z)"
)]
pub use line::ParsedLine;
