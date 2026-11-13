//! The log pipeline (ADR-0067, ADR-0068): what turns what containers print, and what hoster
//! logs itself, into records for the log store ([`crate::store::LogStore`]).
//!
//! [`LogFollowers`] reads what every managed container hoster started prints (ADR-0067 §1),
//! under the [`RateCap`] (§7). [`ParsedLine`] maps one line to a level, a message and the
//! remaining fields of a JSON line, or keeps it whole when it is not JSON (§3). [`LogRecorder`]
//! is the last step: the one writer of `logs.db`, fed through [`LogSink`]s that never block
//! (§6).

mod cap;
mod follower;
mod line;
#[allow(dead_code, reason = "the head is used as the logs UI lands (epic psq)")]
mod recorder;

pub use cap::RateCap;
pub use follower::LogFollowers;
#[allow(
    unused_imports,
    reason = "the OTLP receiver (task m7x) maps severities with it"
)]
pub use line::OtlpSeverity;
pub use line::ParsedLine;
#[cfg(test)]
pub use recorder::Tap;
pub use recorder::{Drops, LogRecorder, LogSink};
