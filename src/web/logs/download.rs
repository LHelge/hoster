//! A download of the stored logs view (task gz9): every record the view's options select over
//! its whole range, oldest first, not only the page shown, as a file ([`LogDownload`]).
//!
//! It plans the store query as the page does ([`LogsView::plan`]), with the same parameters,
//! and drops only the page cursor: it reads from the start of the range on, one page of
//! [`LogQuery::MAX_LIMIT`] records at a time, so the scan cap ([`LogQuery::SCAN_CAP`]) applies
//! per page and a filtered download still covers the whole range. Each page is written as one
//! chunk of the streamed body as soon as it is read, so memory does not grow with the size of
//! the download, and when the client goes away the body is dropped and nothing more is read.
//! A download holds at most [`LogDownload::CAP`] records; one that reaches it ends with a line
//! saying so and from which record time on the rest is left out.
//!
//! Two formats ([`LogFormat`]): `text`, one line per record (`<time> <service> <level or ->
//! <message>`, line breaks in the message written as `\n` and `\r`), and `jsonl`, one JSON
//! object per record. The file name is built from the stack's name and the resolved range
//! only, never from a parameter as given.
//!
//! The access log (`source=access`) downloads its request rows the same way, planned as its
//! page is ([`AccessPlan`]): `text` is then one line per request in a common-log-like format
//! (`<client> - - [<time>] "<method> <path>" <status> <bytes> <host> <service> <total ms>ms
//! <request id>`, the path's `"`, `\` and line breaks escaped), `jsonl` one object per row
//! with every column. The deploy source has no download.

use std::convert::Infallible;
use std::fmt;
use std::str::FromStr;
use std::time::SystemTime;

use axum::body::{Body, Bytes};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, Stream, StreamExt};
use serde::Deserialize;

use super::access::AccessPlan;
use super::{Links, LogParams, LogReader, LogScope, LogSource, LogsView, Plan};
use crate::store::{
    LogEntry, LogQuery, LogStore, PageAt, RecordKey, RequestEntry, RequestQuery, StoreError,
    Timestamp,
};
use crate::web::ui::Count;

/// The file format of a download (`format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// One line per record.
    #[default]
    Text,
    /// One JSON object per record.
    Jsonl,
}

impl LogFormat {
    /// Every format, as the button offers them.
    pub const ALL: [LogFormat; 2] = [LogFormat::Text, LogFormat::Jsonl];

    /// The `format` parameter.
    pub fn as_str(self) -> &'static str {
        match self {
            LogFormat::Text => "text",
            LogFormat::Jsonl => "jsonl",
        }
    }

    /// What the button says.
    pub fn label(self) -> &'static str {
        match self {
            LogFormat::Text => "text",
            LogFormat::Jsonl => "JSON lines",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            LogFormat::Text => "log",
            LogFormat::Jsonl => "jsonl",
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            LogFormat::Text => "text/plain; charset=utf-8",
            LogFormat::Jsonl => "application/x-ndjson",
        }
    }

    /// `entry` as one line of the file, its line break included.
    fn record(self, entry: &LogEntry) -> String {
        let record = &entry.record;
        let time = LogsView::utc(record.time);
        match self {
            LogFormat::Text => {
                let level = record.level.map_or("-", |level| level.as_str());
                let message = Self::one_line(&record.message);
                format!("{time} {} {level} {message}\n", record.service)
            }
            LogFormat::Jsonl => {
                // The fields as stored are a JSON object; kept as a string if they no longer
                // parse.
                let fields = record.fields.as_deref().map(|raw| {
                    serde_json::from_str::<serde_json::Value>(raw)
                        .unwrap_or_else(|_| serde_json::Value::String(raw.to_owned()))
                });
                let line = serde_json::json!({
                    "time": time,
                    "stack": record.stack,
                    "service": record.service,
                    "generation": record.generation,
                    "stream": record.stream.as_str(),
                    "level": record.level.map(|level| level.as_str()),
                    "message": record.message,
                    "fields": fields,
                });
                format!("{line}\n")
            }
        }
    }

    /// `entry`, a request row, as one line of the file, its line break included.
    fn request(self, entry: &RequestEntry) -> String {
        let request = &entry.request;
        let time = LogsView::utc(request.time);
        match self {
            LogFormat::Text => {
                let path = Self::one_line(&request.path.replace('\\', "\\\\").replace('"', "\\\""));
                let method = Self::one_line(&request.method);
                format!(
                    "{} - - [{time}] \"{method} {path}\" {} {} {} {} {}ms {}\n",
                    request.client,
                    request.status,
                    request.bytes,
                    Self::one_line(&request.host),
                    request.service,
                    request.total.as_millis(),
                    request.request_id,
                )
            }
            LogFormat::Jsonl => {
                let millis =
                    |d: std::time::Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
                let line = serde_json::json!({
                    "time": time,
                    "stack": request.stack,
                    "service": request.service,
                    "host": request.host,
                    "prefix": request.prefix,
                    "method": request.method,
                    "path": request.path,
                    "status": request.status,
                    "to_header_ms": request.to_header.map(millis),
                    "total_ms": millis(request.total),
                    "bytes": request.bytes,
                    "client": request.client.to_string(),
                    "upstream_failed": request.upstream_failed,
                    "request_id": request.request_id.to_string(),
                });
                format!("{line}\n")
            }
        }
    }

    /// The closing line of a download cut at [`LogDownload::CAP`]: the row at `from` and
    /// those after it are left out; `rows` names them (`records`, `requests`).
    fn cut(self, from: Timestamp, rows: &str) -> String {
        let from = LogsView::utc(from);
        let cap = LogDownload::CAP;
        match self {
            LogFormat::Text => format!(
                "-- cut: a download holds at most {} {rows}; those from {from} on are not \
                 included\n",
                Count(u64::from(cap))
            ),
            LogFormat::Jsonl => format!(
                "{}\n",
                serde_json::json!({ "cut": { "limit": cap, "from": from } })
            ),
        }
    }

    /// The closing line of a download the store stopped answering midway.
    fn failed(self, error: &str) -> String {
        match self {
            LogFormat::Text => format!("-- stopped: {}\n", Self::one_line(error)),
            LogFormat::Jsonl => format!("{}\n", serde_json::json!({ "error": error })),
        }
    }

    /// `text` on one line: its line breaks written as `\n` and `\r`.
    fn one_line(text: &str) -> String {
        text.replace('\r', "\\r").replace('\n', "\\n")
    }
}

impl fmt::Display for LogFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LogFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let lower = s.trim().to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|format| format.as_str() == lower)
            .ok_or_else(|| format!("`{s}` is not a download format: use text or jsonl."))
    }
}

/// The download's own parameter, beside the view's [`LogParams`].
#[derive(Debug, Default, Deserialize)]
pub struct FormatParam {
    format: Option<String>,
}

impl FormatParam {
    /// The format asked for; text when absent or empty.
    pub fn format(&self) -> Result<LogFormat, String> {
        match self.format.as_deref().map(str::trim) {
            None | Some("") => Ok(LogFormat::default()),
            Some(format) => format.parse(),
        }
    }
}

/// The button of the stack logs tab: each format's download with the view's options.
#[derive(Debug)]
pub struct Download {
    /// `(format, link)`, in [`LogFormat::ALL`] order.
    pub links: Vec<(LogFormat, String)>,
}

impl Download {
    /// The button of the download at `action`, keeping the options of the view's `links` but
    /// following, which a file does not do.
    pub(super) fn of(action: &str, links: &Links) -> Self {
        let links = Links {
            action: action.to_owned(),
            filters: links
                .filters
                .iter()
                .filter(|(key, _)| *key != "follow")
                .cloned()
                .collect(),
        };
        Self {
            links: LogFormat::ALL
                .into_iter()
                .map(|format| {
                    let value = (format != LogFormat::default()).then(|| format.as_str());
                    (format, links.to(&[("format", value)]))
                })
                .collect(),
        }
    }

    /// The most records one download holds, as the button's hint says it.
    pub fn cap(&self) -> Count {
        Count(u64::from(LogDownload::CAP))
    }
}

/// Why a download was refused, before any of it was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// An option the download cannot take: a pattern that does not compile, an unknown format.
    Invalid(String),
    /// The log store cannot be read.
    Unavailable(String),
}

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Refused::Invalid(message) => (StatusCode::BAD_REQUEST, message),
            Refused::Unavailable(message) => (StatusCode::SERVICE_UNAVAILABLE, message),
        };
        (
            status,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            format!("{message}\n"),
        )
            .into_response()
    }
}

/// What a download reads, page by page.
#[derive(Debug)]
enum Rows {
    /// The containers' records.
    Records(LogQuery),
    /// The access log's request rows.
    Requests(RequestQuery),
}

/// One page of a download: each row's time and line, and the key to read on after when there
/// may be more.
type Lines = (Vec<(Timestamp, String)>, Option<RecordKey>);

impl Rows {
    /// What the rows are called in the cut line.
    fn noun(&self) -> &'static str {
        match self {
            Rows::Records(_) => "records",
            Rows::Requests(_) => "requests",
        }
    }

    fn range(&self) -> &std::ops::Range<Timestamp> {
        match self {
            Rows::Records(query) => &query.range,
            Rows::Requests(query) => &query.range,
        }
    }

    /// From the start of the range on, a full page at a time.
    fn rewind(&mut self) {
        let start = PageAt::After(RecordKey {
            time: self.range().start,
            id: i64::MIN,
        });
        match self {
            Rows::Records(query) => {
                query.page = start;
                query.limit = LogQuery::MAX_LIMIT;
            }
            Rows::Requests(query) => {
                query.page = start;
                query.limit = LogQuery::MAX_LIMIT;
            }
        }
    }

    /// The next page as `format` writes it; reading on from the last row read: the last row,
    /// or where a capped scan stopped.
    async fn next(&mut self, store: &LogStore, format: LogFormat) -> Result<Lines, StoreError> {
        match self {
            Rows::Records(query) => {
                let page = store.records(query).await?;
                let next = page
                    .scan_stopped
                    .or_else(|| page.entries.last().map(LogEntry::key))
                    .filter(|_| page.newer);
                if let Some(next) = next {
                    query.page = PageAt::After(next);
                }
                let lines = page
                    .entries
                    .iter()
                    .map(|e| (e.record.time, format.record(e)))
                    .collect();
                Ok((lines, next))
            }
            Rows::Requests(query) => {
                let page = store.requests(query).await?;
                let next = page
                    .scan_stopped
                    .or_else(|| page.entries.last().map(RequestEntry::key))
                    .filter(|_| page.newer);
                if let Some(next) = next {
                    query.page = PageAt::After(next);
                }
                let lines = page
                    .entries
                    .iter()
                    .map(|e| (e.request.time, format.request(e)))
                    .collect();
                Ok((lines, next))
            }
        }
    }
}

/// A download of one stack's records or request rows: see the module.
#[derive(Debug)]
pub struct LogDownload {
    store: LogStore,
    rows: Rows,
    format: LogFormat,
    file_name: String,
}

impl LogDownload {
    /// The most records one download holds; fewer under test, so a test can reach it.
    pub const CAP: u32 = if cfg!(test) { 1_500 } else { 100_000 };

    /// The download of `stack`'s records, or request rows for the access log, that the
    /// options of `params` select, in `format`.
    pub fn open(
        reader: &LogReader,
        stack: &str,
        params: &LogParams,
        format: LogFormat,
    ) -> Result<Self, Refused> {
        let scope = LogScope::Stack(stack.to_owned());
        let source = params.source(&scope).0;
        if source == LogSource::Deploy {
            return Err(Refused::Invalid(
                "The deployment events cannot be downloaded; the containers' records and the \
                 access log can."
                    .to_owned(),
            ));
        }
        let store = reader.store().map_err(Refused::Unavailable)?.clone();
        let now = SystemTime::now();
        let params = &params.clone().scoped(&scope);
        let (mut rows, invalid) = match source {
            LogSource::Access => {
                let AccessPlan { query, shared } = AccessPlan::new(&scope, params, now);
                (Rows::Requests(query), shared.invalid)
            }
            _ => {
                let Plan { query, invalid, .. } = LogsView::plan(&scope, params, now);
                (Rows::Records(query), invalid)
            }
        };
        if let Some(invalid) = invalid {
            return Err(Refused::Invalid(invalid));
        }
        // The whole range, oldest first, whatever page the view shows.
        rows.rewind();
        let name = match source {
            LogSource::Access => format!("{stack}-access"),
            _ => stack.to_owned(),
        };
        let file_name = Self::file_name(&name, rows.range(), format);
        Ok(Self {
            store,
            rows,
            format,
            file_name,
        })
    }

    /// The response for `opened`: the file, streamed, or why it was refused.
    pub fn respond(opened: Result<Self, Refused>) -> Response {
        match opened {
            Ok(download) => download.into_response(),
            Err(refused) => refused.into_response(),
        }
    }

    /// `<stack>-<start>-<span>.<ext>`, e.g. `blog-2026-11-12T10-00Z-1h.log`: the stack's name
    /// with anything but letters, digits, `-` and `_` replaced, the start to the minute in UTC
    /// and the range's length.
    fn file_name(stack: &str, range: &std::ops::Range<Timestamp>, format: LogFormat) -> String {
        let stack: String = stack
            .chars()
            .map(|c| match c {
                'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => c,
                _ => '_',
            })
            .collect();
        let start = LogsView::utc(range.start);
        // `2026-11-12T10:00`, its colon replaced: file systems do not all take one.
        let start = start[..16].replace(':', "-");
        let span = Self::span(range.end.as_millis() - range.start.as_millis());
        format!("{stack}-{start}Z-{span}.{}", format.extension())
    }

    /// `millis` as the largest whole unit to the minute: `7d`, `1h`, `90m`.
    fn span(millis: i64) -> String {
        let minutes = millis.max(0) / 60_000;
        match minutes {
            0 => format!("{}s", millis.max(0) / 1000),
            m if m % 1440 == 0 => format!("{}d", m / 1440),
            m if m % 60 == 0 => format!("{}h", m / 60),
            m => format!("{m}m"),
        }
    }

    /// The file, page by page.
    fn body(self) -> impl Stream<Item = Result<Bytes, Infallible>> + Send + 'static {
        /// Where the download is: the next page to read, and how many records were sent.
        struct State {
            download: LogDownload,
            sent: u32,
        }
        let state = State {
            download: self,
            sent: 0,
        };
        stream::unfold(Some(state), |state| async move {
            let mut state = state?;
            loop {
                let download = &mut state.download;
                let format = download.format;
                let (lines, next) = match download.rows.next(&download.store, format).await {
                    Ok(lines) => lines,
                    Err(err) => {
                        tracing::error!(error = %err, "downloading from the log store failed");
                        let error =
                            format!("Cannot read the log store: {:#}", anyhow::Error::from(err));
                        return Some((Bytes::from(format.failed(&error)), None));
                    }
                };
                let mut chunk = String::new();
                for (time, line) in lines {
                    if state.sent == Self::CAP {
                        chunk.push_str(&format.cut(time, download.rows.noun()));
                        return Some((Bytes::from(chunk), None));
                    }
                    chunk.push_str(&line);
                    state.sent += 1;
                }
                if next.is_none() {
                    return (!chunk.is_empty()).then(|| (Bytes::from(chunk), None));
                }
                if !chunk.is_empty() {
                    return Some((Bytes::from(chunk), Some(state)));
                }
            }
        })
        .map(Ok)
    }
}

impl IntoResponse for LogDownload {
    fn into_response(self) -> Response {
        let disposition = format!("attachment; filename=\"{}\"", self.file_name);
        let content_type = self.format.content_type();
        (
            [
                (header::CONTENT_TYPE, content_type.to_owned()),
                (header::CONTENT_DISPOSITION, disposition),
                (header::CACHE_CONTROL, "no-store".to_owned()),
            ],
            Body::from_stream(self.body()),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::store::{Level, LogRecord, Stream};

    fn entry(message: &str) -> LogEntry {
        LogEntry {
            id: 7,
            record: LogRecord {
                // 2026-11-13T12:00:00Z.
                time: Timestamp::from_millis(1_794_571_200_000),
                stack: Some("notes".into()),
                service: "api".into(),
                generation: Some(2),
                stream: Stream::Stderr,
                level: None,
                message: message.into(),
                fields: None,
                cut: false,
                marker: false,
                trace_id: None,
                span_id: None,
                received: None,
            },
        }
    }

    #[test]
    fn text_is_one_line_per_record_with_line_breaks_escaped() {
        let mut first = entry("panicked at\nsrc/main.rs:3\r\n");
        assert_eq!(
            LogFormat::Text.record(&first),
            "2026-11-13T12:00:00.000Z api - panicked at\\nsrc/main.rs:3\\r\\n\n"
        );
        first.record.level = Some(Level::Warn);
        assert!(
            LogFormat::Text
                .record(&first)
                .contains(" api warn panicked")
        );
    }

    #[test]
    fn json_lines_hold_every_column_and_the_fields_as_json() {
        let mut record = entry("GET /\n");
        record.record.level = Some(Level::Info);
        record.record.fields = Some(r#"{"span":{"id":1}}"#.into());
        let line = LogFormat::Jsonl.record(&record);
        assert!(
            line.ends_with("}\n") && line.matches('\n').count() == 1,
            "{line}"
        );
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "time": "2026-11-13T12:00:00.000Z",
                "stack": "notes",
                "service": "api",
                "generation": 2,
                "stream": "stderr",
                "level": "info",
                "message": "GET /\n",
                "fields": { "span": { "id": 1 } },
            })
        );
        // Fields that no longer parse are kept as text.
        record.record.fields = Some("{cut".into());
        let value: serde_json::Value =
            serde_json::from_str(&LogFormat::Jsonl.record(&record)).unwrap();
        assert_eq!(value["fields"], "{cut");
    }

    #[test]
    fn formats_parse_and_default_to_text() {
        assert_eq!(FormatParam::default().format(), Ok(LogFormat::Text));
        assert_eq!("JSONL".parse(), Ok(LogFormat::Jsonl));
        assert_eq!("text".parse(), Ok(LogFormat::Text));
        assert!("csv".parse::<LogFormat>().is_err());
    }

    #[test]
    fn the_file_name_is_the_stack_the_start_and_the_span() {
        let start = Timestamp::from_millis(1_794_571_200_000);
        let hour = start..start + Duration::from_secs(3_601);
        assert_eq!(
            LogDownload::file_name("blog", &hour, LogFormat::Text),
            "blog-2026-11-13T12-00Z-1h.log"
        );
        let week = start..start + Duration::from_secs(7 * 86_400);
        assert_eq!(
            LogDownload::file_name("blog", &week, LogFormat::Jsonl),
            "blog-2026-11-13T12-00Z-7d.jsonl"
        );
        let odd = start..start + Duration::from_secs(90 * 60);
        assert_eq!(
            LogDownload::file_name("a\"b;c/\r\nd", &odd, LogFormat::Text),
            "a_b_c___d-2026-11-13T12-00Z-90m.log"
        );
        assert_eq!(LogDownload::span(30_000), "30s");
    }

    #[test]
    fn the_cut_line_says_the_cap_and_from_when() {
        let from = Timestamp::from_millis(1_794_571_200_000);
        assert_eq!(
            LogFormat::Text.cut(from, "records"),
            format!(
                "-- cut: a download holds at most {} records; those from \
                 2026-11-13T12:00:00.000Z on are not included\n",
                Count(u64::from(LogDownload::CAP))
            )
        );
        let value: serde_json::Value =
            serde_json::from_str(&LogFormat::Jsonl.cut(from, "records")).unwrap();
        assert_eq!(value["cut"]["limit"], LogDownload::CAP);
        assert_eq!(value["cut"]["from"], "2026-11-13T12:00:00.000Z");
    }
}
