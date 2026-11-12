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
#[allow(
    dead_code,
    reason = "request rows, drop counts and the head are used as the proxy, the System page and \
              the logs UI land (epics f8z, psq)"
)]
mod recorder;

pub use cap::RateCap;
pub use follower::LogFollowers;
pub use line::ParsedLine;
#[cfg(test)]
pub use recorder::Tap;
#[allow(
    unused_imports,
    reason = "the drop counts are shown on the System page as it lands"
)]
pub use recorder::{Drops, LogRecorder, LogSink};
