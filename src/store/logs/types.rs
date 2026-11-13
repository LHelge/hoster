//! The rows of the log store: [`LogRecord`] (`log_records`), [`RequestRow`] (`requests`) and
//! [`Span`] (`spans`), and the text columns they hold.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::store::Timestamp;

/// Text that names no [`Level`] or [`Stream`], or is not a [`RequestId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown {what} {text:?}")]
pub struct UnknownLogText {
    what: &'static str,
    text: String,
}

impl UnknownLogText {
    pub(super) fn new(what: &'static str, text: &str) -> Self {
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

/// Where a record came from: a container's stdout or stderr, hoster itself (ADR-0068), or a
/// container's OpenTelemetry SDK over OTLP (ADR-0069 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stream {
    Stdout,
    Stderr,
    Hoster,
    Otlp,
}

impl Stream {
    /// Every stream.
    pub const ALL: [Stream; 4] = [Stream::Stdout, Stream::Stderr, Stream::Hoster, Stream::Otlp];

    /// The column text.
    pub fn as_str(self) -> &'static str {
        match self {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
            Stream::Hoster => "hoster",
            Stream::Otlp => "otlp",
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

/// One record of `log_records`: a line a container printed, an event hoster logged, a record a
/// container exported over OTLP, or a marker hoster wrote about lines it lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    /// When podman stamped the line, when hoster emitted the event, or the time an OTLP record
    /// gives (ADR-0069 §6).
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
    /// The trace the record was made in: an OTLP record's, or the request's for hoster's own
    /// records made inside one (ADR-0069 §8).
    pub trace_id: Option<TraceId>,
    /// The span the record was made in, with [`LogRecord::trace_id`].
    pub span_id: Option<SpanId>,
    /// When hoster accepted an OTLP record, whose [`LogRecord::time`] is the payload's; `None`
    /// for every other record.
    pub received: Option<Timestamp>,
}

impl LogRecord {
    /// The fields as `(dotted key, value)` pairs, sorted by key:
    /// nested objects join their keys with `.` and arrays use the index (`spans.0.name`).
    /// A string value is its text, `null` and other scalars their JSON. Empty without fields,
    /// or when they are not a JSON object.
    pub fn flat_fields(&self) -> Vec<(String, String)> {
        self.parsed_fields()
            .and_then(Result::ok)
            .unwrap_or_default()
    }

    /// The fields as [`LogRecord::flat_fields`] pairs, parsed once: `None` without fields, and
    /// their text as stored when it is not a JSON object (it was one when stored, so this is
    /// for showing it raw rather than losing it).
    pub fn parsed_fields(&self) -> Option<Result<Vec<(String, String)>, &str>> {
        let text = self.fields.as_deref()?;
        match serde_json::from_str(text) {
            Ok(serde_json::Value::Object(object)) => {
                let mut flat = Vec::new();
                for (key, value) in object {
                    Self::flatten(key, value, &mut flat);
                }
                Some(Ok(flat))
            }
            _ => Some(Err(text)),
        }
    }

    fn flatten(key: String, value: serde_json::Value, flat: &mut Vec<(String, String)>) {
        use serde_json::Value;
        match value {
            Value::Object(object) if !object.is_empty() => {
                for (inner, value) in object {
                    Self::flatten(format!("{key}.{inner}"), value, flat);
                }
            }
            Value::Array(items) if !items.is_empty() => {
                for (index, value) in items.into_iter().enumerate() {
                    Self::flatten(format!("{key}.{index}"), value, flat);
                }
            }
            Value::String(text) => flat.push((key, text)),
            other => flat.push((key, other.to_string())),
        }
    }
}

/// `N` bytes written as `2 * N` lowercase hex digits, as W3C trace context writes its ids:
/// [`TraceId`] (and [`RequestId`], the same shape) and [`SpanId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HexId<const N: usize>([u8; N]);

/// The id the proxy gives a request: 16 random bytes, the shape of a W3C trace id, and the
/// trace id of the request's trace (ADR-0069 §8).
pub type RequestId = HexId<16>;

/// A W3C trace id: 16 bytes, 32 hex digits.
pub type TraceId = HexId<16>;

/// A W3C span id: 8 bytes, 16 hex digits.
pub type SpanId = HexId<8>;

impl<const N: usize> HexId<N> {
    /// A new id from the operating system's random number generator, which is all zeros (an
    /// invalid trace or span id) only if that generator fails.
    pub fn random() -> Self {
        let mut bytes = [0; N];
        if getrandom::fill(&mut bytes).is_err() {
            bytes = [0; N];
        }
        Self(bytes)
    }

    /// The text a [`FromStr`] error names, by length.
    fn what() -> &'static str {
        match N {
            8 => "span id",
            16 => "trace id",
            _ => "id",
        }
    }
}

impl<const N: usize> From<[u8; N]> for HexId<N> {
    fn from(bytes: [u8; N]) -> Self {
        Self(bytes)
    }
}

/// The bytes as OTLP carries them, which must be exactly `N` long.
impl<const N: usize> TryFrom<&[u8]> for HexId<N> {
    type Error = std::array::TryFromSliceError;

    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        <[u8; N]>::try_from(bytes).map(Self)
    }
}

impl<const N: usize> AsRef<[u8]> for HexId<N> {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl<const N: usize> fmt::Display for HexId<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl<const N: usize> FromStr for HexId<N> {
    type Err = UnknownLogText;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || UnknownLogText::new(Self::what(), s);
        if s.len() != 2 * N || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(invalid());
        }
        // ASCII only (checked above), so every two-character slice is on a char boundary.
        let mut bytes = [0; N];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|_| invalid())?;
        }
        Ok(Self(bytes))
    }
}

/// A point in time in microseconds since the Unix epoch (UTC): the times of spans, which can
/// be shorter than a millisecond, where [`Timestamp`] has milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, sqlx::Type)]
#[sqlx(transparent)]
pub struct MicroTimestamp(i64);

impl MicroTimestamp {
    /// Microseconds since the Unix epoch.
    pub fn as_micros(self) -> i64 {
        self.0
    }

    /// The time `micros` microseconds after the Unix epoch.
    pub const fn from_micros(micros: i64) -> Self {
        Self(micros)
    }

    /// The time OTLP gives as nanoseconds since the Unix epoch, cut to microseconds.
    pub fn from_unix_nanos(nanos: u64) -> Self {
        Self(i64::try_from(nanos / 1_000).unwrap_or(i64::MAX))
    }
}

impl From<Timestamp> for MicroTimestamp {
    fn from(time: Timestamp) -> Self {
        Self(time.as_millis().saturating_mul(1_000))
    }
}

/// Cut to the millisecond, towards the past.
impl From<MicroTimestamp> for Timestamp {
    fn from(time: MicroTimestamp) -> Self {
        Timestamp::from_millis(time.0.div_euclid(1_000))
    }
}

impl From<SystemTime> for MicroTimestamp {
    fn from(time: SystemTime) -> Self {
        let micros = match time.duration_since(UNIX_EPOCH) {
            Ok(after) => i64::try_from(after.as_micros()).unwrap_or(i64::MAX),
            Err(before) => i64::try_from(before.duration().as_micros())
                .map(|m| -m)
                .unwrap_or(i64::MIN),
        };
        Self(micros)
    }
}

impl std::ops::Add<Duration> for MicroTimestamp {
    type Output = MicroTimestamp;

    fn add(self, rhs: Duration) -> MicroTimestamp {
        let micros = i64::try_from(rhs.as_micros()).unwrap_or(i64::MAX);
        MicroTimestamp(self.0.saturating_add(micros))
    }
}

/// What a span stands for, as OTLP's `SpanKind` says (ADR-0069 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SpanKind {
    /// The payload did not say.
    #[default]
    Unspecified,
    Internal,
    Server,
    Client,
    Producer,
    Consumer,
}

impl SpanKind {
    /// Every kind.
    pub const ALL: [SpanKind; 6] = [
        SpanKind::Unspecified,
        SpanKind::Internal,
        SpanKind::Server,
        SpanKind::Client,
        SpanKind::Producer,
        SpanKind::Consumer,
    ];

    /// The column text.
    pub fn as_str(self) -> &'static str {
        match self {
            SpanKind::Unspecified => "unspecified",
            SpanKind::Internal => "internal",
            SpanKind::Server => "server",
            SpanKind::Client => "client",
            SpanKind::Producer => "producer",
            SpanKind::Consumer => "consumer",
        }
    }
}

impl fmt::Display for SpanKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SpanKind {
    type Err = UnknownLogText;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == s)
            .ok_or_else(|| UnknownLogText::new("span kind", s))
    }
}

/// How a span ended, as OTLP's `Status.code` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SpanStatus {
    /// The payload did not say.
    #[default]
    Unset,
    Ok,
    Error,
}

impl SpanStatus {
    /// Every status.
    pub const ALL: [SpanStatus; 3] = [SpanStatus::Unset, SpanStatus::Ok, SpanStatus::Error];

    /// The column text.
    pub fn as_str(self) -> &'static str {
        match self {
            SpanStatus::Unset => "unset",
            SpanStatus::Ok => "ok",
            SpanStatus::Error => "error",
        }
    }
}

impl fmt::Display for SpanStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SpanStatus {
    type Err = UnknownLogText;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == s)
            .ok_or_else(|| UnknownLogText::new("span status", s))
    }
}

/// One row of `spans`: a span a container exported over OTLP (ADR-0069 §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    /// `None` for a root span.
    pub parent: Option<SpanId>,
    /// The stack and service the exporter's token names, never what the payload claims.
    pub stack: String,
    pub service: String,
    pub name: String,
    pub kind: SpanKind,
    /// As the payload gives it.
    pub start: MicroTimestamp,
    /// Stored in whole microseconds.
    pub duration: Duration,
    pub status: SpanStatus,
    /// The status's message; `None` when empty.
    pub status_message: Option<String>,
    /// The span's attributes as a JSON object, as text; `None` without any.
    pub attributes: Option<String>,
    /// The resource's attributes as a JSON object, as text; `None` without any.
    pub resource: Option<String>,
    /// The span's events as a JSON array, as text; `None` without any.
    pub events: Option<String>,
    /// When hoster accepted the span.
    pub received: Timestamp,
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
    /// Also the id of the request's trace (ADR-0069 §8).
    pub request_id: RequestId,
    /// The proxy's server span for the request, from which it is built when a trace is read
    /// (ADR-0069 §9); `None` for rows written before it was stored.
    pub server_span: Option<SpanId>,
    /// The proxy's client span for the upstream request, the parent it sends upstream.
    pub client_span: Option<SpanId>,
    /// When the upstream request started; `None` when it never did, or the row is older.
    pub upstream_start: Option<MicroTimestamp>,
}

/// Where a container's follower got to (ADR-0067 §1): the last line it handed to the store, by
/// podman's timestamp of it in nanoseconds since the epoch and its place among the lines with
/// that stamp, so two lines stamped alike are told apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogCursor {
    /// The podman container id.
    pub container: String,
    pub time_ns: i64,
    /// The line's place in the run of consecutive lines stamped `time_ns` in the container's
    /// log: 1 for the first.
    pub nth: i64,
}

/// What one commit writes: records, request rows, spans and the cursors of the lines among the
/// records, in one transaction, so a cursor never runs ahead of the lines it covers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogBatch {
    pub records: Vec<LogRecord>,
    pub requests: Vec<RequestRow>,
    pub spans: Vec<Span>,
    /// At most one per container: the newest.
    pub cursors: Vec<LogCursor>,
    /// Containers whose cursors are deleted, after the cursors above are written: containers
    /// that no longer exist.
    pub forgotten: Vec<String>,
}

impl LogBatch {
    /// Records plus request rows plus spans.
    pub fn len(&self) -> usize {
        self.records.len() + self.requests.len() + self.spans.len()
    }

    /// Nothing to write.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
            && self.requests.is_empty()
            && self.spans.is_empty()
            && self.cursors.is_empty()
            && self.forgotten.is_empty()
    }
}

/// The newest committed ids: what a page or a follow stream has seen up to. `0` for an empty
/// table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct LogHead {
    pub record: i64,
    pub request: i64,
}
