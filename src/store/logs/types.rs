//! The rows of the log store: [`LogRecord`] (`log_records`) and [`RequestRow`] (`requests`),
//! and the text columns they hold.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;
use std::time::Duration;

use crate::store::Timestamp;

/// Text that names no [`Level`] or [`Stream`], or is not a [`RequestId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown {what} {text:?}")]
pub struct UnknownLogText {
    what: &'static str,
    text: String,
}

impl UnknownLogText {
    fn new(what: &'static str, text: &str) -> Self {
        Self {
            what,
            text: text.to_owned(),
        }
    }
}

/// A record's level, normalised from whatever the line said (ADR-0067 §3). Ordered from the
/// least to the most severe, so `level >= Level::Warn` reads as "warn and above".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    /// Every level, least severe first.
    pub const ALL: [Level; 5] = [
        Level::Trace,
        Level::Debug,
        Level::Info,
        Level::Warn,
        Level::Error,
    ];

    /// The column text.
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Trace => "trace",
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// As stored and as [`Display`](fmt::Display) writes it. Mapping the many spellings loggers use
/// onto these is the line parser's job, not this one's.
impl FromStr for Level {
    type Err = UnknownLogText;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|level| level.as_str() == s)
            .ok_or_else(|| UnknownLogText::new("level", s))
    }
}

/// Where a record came from: a container's stdout or stderr, or hoster itself (ADR-0068).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stream {
    Stdout,
    Stderr,
    Hoster,
}

impl Stream {
    /// Every stream.
    pub const ALL: [Stream; 3] = [Stream::Stdout, Stream::Stderr, Stream::Hoster];

    /// The column text.
    pub fn as_str(self) -> &'static str {
        match self {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
            Stream::Hoster => "hoster",
        }
    }
}

impl fmt::Display for Stream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Stream {
    type Err = UnknownLogText;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|stream| stream.as_str() == s)
            .ok_or_else(|| UnknownLogText::new("stream", s))
    }
}

/// One record of `log_records`: a line a container printed, an event hoster logged, or a marker
/// hoster wrote about lines it lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    /// When podman stamped the line, or when hoster emitted the event.
    pub time: Timestamp,
    /// `None` only for hoster's own records that name no stack (ADR-0068).
    pub stack: Option<String>,
    /// The stack file's service name, or `hoster`.
    pub service: String,
    /// The container's deployment generation; `None` for hoster's own records without one.
    pub generation: Option<u32>,
    pub stream: Stream,
    /// `None` for text lines: levels are never guessed (ADR-0067 §3).
    pub level: Option<Level>,
    pub message: String,
    /// The JSON object of a JSON line's other keys, as text.
    pub fields: Option<String>,
    /// The line was longer than the cap and was cut.
    pub cut: bool,
    /// Written by hoster about lines it lost (the rate cap, a gap while it was down); its
    /// message says what.
    pub marker: bool,
}

/// The id the proxy gives a request: 16 random bytes, written as 32 lowercase hex digits, the
/// shape of a W3C trace id so it can become one (OQ-42).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId([u8; 16]);

impl From<[u8; 16]> for RequestId {
    fn from(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl FromStr for RequestId {
    type Err = UnknownLogText;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || UnknownLogText::new("request id", s);
        if s.len() != 32 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(invalid());
        }
        // ASCII only (checked above), so every two-character slice is on a char boundary.
        let mut bytes = [0; 16];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|_| invalid())?;
        }
        Ok(Self(bytes))
    }
}

/// One row of `requests`: a request the proxy routed to a stack (ADR-0067 §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRow {
    /// When the request arrived.
    pub time: Timestamp,
    pub host: String,
    /// The matched route's prefix.
    pub prefix: String,
    pub stack: String,
    pub service: String,
    pub method: String,
    /// Without the query string.
    pub path: String,
    /// As sent to the client.
    pub status: u16,
    /// Until the upstream's response header arrived; `None` when none did.
    pub to_header: Option<Duration>,
    /// Until the last byte was sent or the request ended.
    pub total: Duration,
    /// Response body bytes sent.
    pub bytes: u64,
    /// The address the proxy saw.
    pub client: IpAddr,
    /// The request failed because of its upstream.
    pub upstream_failed: bool,
    pub request_id: RequestId,
}
