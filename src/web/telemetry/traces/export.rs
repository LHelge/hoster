//! The Traces tab's export (epic byh): the traces the tab lists for its options, as plain text
//! written for a model to read ([`TraceExport`]), served at [`TraceExport::PATH`] inline, or as
//! a file with `download=1`.
//!
//! It reads the list as the tab does, with the same [`TraceQuery`] ([`TraceParams::query`],
//! so the filters, sort and page are the tab's and a bad option falls back as it does there,
//! noted in the header), and then each listed trace with [`LogStore::load_trace`], one at a
//! time, its log record count ([`LogStore::trace_record_count`]) and, when it has records, each
//! written span's ([`LogStore::span_records`]). The selection options (`trace`, `span`,
//! `collapsed`, `crit`) are ignored.
//!
//! The text is a header (what this is, the resolved range in UTC, the stack, the filters and
//! sort in effect, which traces of how many, the fallbacks taken and a legend of the lines),
//! then each trace in the list's order: a summary line, its resource attributes once per
//! service, and its spans as a tree indented two spaces a level, each with its events and log
//! records on lines under it. Nothing is HTML-escaped; line breaks in a value are written as
//! `\n` and `\r`, as the log download does.
//!
//! Three caps, each leaving a `-- cut:` line (or, for a value, a marker) that says what was
//! left out: [`TraceExport::SPAN_BUDGET`] spans over the whole export (traces past it keep
//! their summary line), [`TraceExport::VALUE_CAP`] characters per value, and
//! [`TraceExport::RECORDS_PER_SPAN`] log records per span. The file name is built from the
//! stack and the resolved range only, never from a parameter as given.

use std::collections::HashSet;
use std::fmt;
use std::ops::Range;
use std::time::{Duration, SystemTime};

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Map, Value};

use super::{TraceParams, TracesView};
use crate::stack::Counted;
use crate::store::{
    LogEntry, LogQuery, LogStore, MicroTimestamp, Retention, Span, Timestamp, Trace, TraceList,
    TraceQuery, TraceSummary,
};
use crate::web::logs::LogReader;
use crate::web::ui::{Count, SpanTime};

/// The export's own parameter, beside the tab's [`TraceParams`].
#[derive(Debug, Default, Deserialize)]
pub struct ExportParam {
    download: Option<String>,
}

impl ExportParam {
    /// The value of `download` that asks for a file.
    pub const DOWNLOAD: &str = "1";

    /// Whether a file is asked for: `download=1`. Anything else is the text inline.
    pub fn download(&self) -> bool {
        self.download.as_deref().map(str::trim) == Some(Self::DOWNLOAD)
    }
}

/// The traces the tab lists for some options, as text: see the module.
#[derive(Debug)]
pub struct TraceExport {
    query: TraceQuery,
    /// The range preset in effect: `15m`.
    since: &'static str,
    /// The stack picked; every stack without one.
    stack: Option<String>,
    /// Why options were not taken, as the tab's flash says it.
    notes: Vec<String>,
    matching: u64,
    total: u64,
    traces: Vec<ExportedTrace>,
}

/// One listed trace, as read for the export.
#[derive(Debug)]
struct ExportedTrace {
    summary: TraceSummary,
    /// Its log records, those without a span included; `None` when they cannot be counted.
    records: Option<u64>,
    body: TraceBody,
}

/// What is written under a trace's summary line.
#[derive(Debug)]
enum TraceBody {
    /// Its spans, the first `shown` of them in depth-first order, with each shown span's log
    /// records, in the same order.
    Spans {
        trace: Trace,
        shown: usize,
        logs: Vec<SpanLogs>,
    },
    /// The export's span budget was spent before it.
    Skipped,
    /// Listed, but gone by the time it was read.
    Missing,
    /// It cannot be read.
    Unreadable,
}

/// The log records of one written span.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SpanLogs {
    /// Oldest first, up to one more than [`TraceExport::RECORDS_PER_SPAN`], so one past it
    /// says there are more.
    Read(Vec<LogEntry>),
    /// They cannot be read.
    Unreadable,
}

impl TraceExport {
    /// Where the export is.
    pub const PATH: &str = "/telemetry/traces/export";
    /// The most spans one export writes, over all its traces; fewer under test, so a test can
    /// reach it.
    pub const SPAN_BUDGET: usize = if cfg!(test) { 4 } else { 2_000 };
    /// The most characters of one value written; the rest is left out, said by a marker.
    pub const VALUE_CAP: usize = 200;
    /// The most log records written under one span; fewer under test.
    pub const RECORDS_PER_SPAN: u32 = if cfg!(test) { 2 } else { 20 };

    /// The export of what the tab lists for `params` at `now`, read from the store behind
    /// `reader`; why not when the store or the list cannot be read.
    pub async fn load(
        reader: &LogReader,
        params: &TraceParams,
        now: SystemTime,
    ) -> Result<Self, String> {
        let store = reader.store()?;
        let (query, notes) = params.query(now);
        let list = store.traces(&query).await.map_err(|err| {
            tracing::warn!(error = %err, "reading the trace list for an export failed");
            "The trace list cannot be read right now.".to_owned()
        })?;
        let TraceList {
            traces,
            matching,
            total,
        } = list;
        let mut budget = Self::SPAN_BUDGET;
        let mut exported = Vec::with_capacity(traces.len());
        for summary in traces {
            exported.push(ExportedTrace::read(store, summary, &mut budget).await);
        }
        let ((since, _), _) = params.range();
        Ok(Self {
            query,
            since,
            stack: params.stack().map(str::to_owned),
            notes,
            matching,
            total,
            traces: exported,
        })
    }

    /// `traces-<stack or all>-<start>-<end>.txt`, e.g.
    /// `traces-notes-2026-11-13T11-45-00Z-2026-11-13T12-00-01Z.txt`: the stack with anything
    /// but letters, digits, `-` and `_` replaced, and the range's ends to the second in UTC,
    /// their colons replaced.
    fn file_name(&self) -> String {
        let stack: String = match self.stack.as_deref() {
            None => "all".to_owned(),
            Some(stack) => stack
                .chars()
                .map(|c| match c {
                    'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => c,
                    _ => '_',
                })
                .collect(),
        };
        let at = |time: Timestamp| {
            humantime::format_rfc3339_seconds(time.into())
                .to_string()
                .replace(':', "-")
        };
        let Range { start, end } = self.query.range;
        format!("traces-{stack}-{}-{}.txt", at(start), at(end))
    }

    /// The response: the text inline, or as a file when `download`.
    pub fn respond(loaded: Result<Self, String>, download: bool) -> Response {
        let export = match loaded {
            Ok(export) => export,
            Err(message) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    format!("{message}\n"),
                )
                    .into_response();
            }
        };
        let disposition = if download { "attachment" } else { "inline" };
        let disposition = format!("{disposition}; filename=\"{}\"", export.file_name());
        (
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8".to_owned()),
                (header::CONTENT_DISPOSITION, disposition),
                (header::CACHE_CONTROL, "no-store".to_owned()),
            ],
            export.to_string(),
        )
            .into_response()
    }

    /// The header's filters: `status=error, slower than 500ms, span name contains "GET"`.
    fn filters(&self) -> String {
        let query = &self.query;
        let mut filters = Vec::new();
        if let Some(status) = query.status {
            filters.push(format!("status={status}"));
        }
        if let Some(min) = query.min_duration {
            filters.push(format!("slower than {}", humantime::format_duration(min)));
        }
        if let Some(name) = query.name.as_deref().filter(|name| !name.is_empty()) {
            filters.push(format!("span name contains {}", Quoted(name)));
        }
        if filters.is_empty() {
            "none".to_owned()
        } else {
            filters.join(", ")
        }
    }

    /// The header's count: `1–3 of 3 matching (5 in range)`, `none on page 3 of …`.
    fn counted(&self) -> String {
        let limit = u64::from(self.query.limit.clamp(1, LogQuery::MAX_LIMIT));
        let page = u64::from(self.query.page);
        let first = page * limit + 1;
        let shown = u64::try_from(self.traces.len()).unwrap_or(u64::MAX);
        let of = format!(
            "of {} matching ({} in range)",
            Count(self.matching),
            Count(self.total)
        );
        if shown == 0 {
            format!("none on page {} {of}", page + 1)
        } else {
            format!("{}–{} {of}", Count(first), Count(first + shown - 1))
        }
    }

    fn utc(time: Timestamp) -> String {
        humantime::format_rfc3339_millis(time.into()).to_string()
    }
}

impl fmt::Display for TraceExport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let range = &self.query.range;
        writeln!(
            f,
            "hoster trace export: the traces hoster's Traces tab lists for these options, as text"
        )?;
        writeln!(
            f,
            "range:   {} to {} (UTC, the last {})",
            Self::utc(range.start),
            Self::utc(range.end),
            self.since
        )?;
        writeln!(
            f,
            "stack:   {}",
            OneLine(self.stack.as_deref().unwrap_or("every stack"))
        )?;
        writeln!(f, "filters: {}", self.filters())?;
        writeln!(f, "sort:    {}", TracesView::order_of(self.query.sort))?;
        writeln!(f, "traces:  {}", self.counted())?;
        for note in &self.notes {
            writeln!(f, "note:    {}", OneLine(note))?;
        }
        writeln!(
            f,
            "legend:  a trace is its summary line, its resource attributes once per service, then \
             its spans as a tree, two spaces a level:"
        )?;
        writeln!(
            f,
            "         <service>: <name> [<kind>, <duration>, +<start after the trace's>, \
             <status>[: <message>]] <attribute>=<value> …"
        )?;
        writeln!(
            f,
            "         under a span, its events (`· event +<at> <name> <attribute>=<value> …`) and \
             log records (`· log <time> <level> <message>`)"
        )?;
        writeln!(
            f,
            "         a value longer than {} characters ends in `…(+N chars)`; line breaks are \
             written as \\n",
            TraceExport::VALUE_CAP
        )?;
        if self.traces.is_empty() {
            return writeln!(f, "\nno traces listed.");
        }
        for trace in &self.traces {
            writeln!(f)?;
            write!(f, "{trace}")?;
        }
        Ok(())
    }
}

impl ExportedTrace {
    /// `summary`'s trace read from `store`, writing at most `budget` of its spans and taking
    /// those from it.
    async fn read(store: &LogStore, summary: TraceSummary, budget: &mut usize) -> Self {
        let id = summary.trace_id;
        let records = store
            .trace_record_count(id)
            .await
            .inspect_err(|err| tracing::warn!(error = %err, "counting a trace's records failed"))
            .ok();
        let mut exported = if *budget == 0 {
            Self {
                summary,
                records,
                body: TraceBody::Skipped,
            }
        } else {
            let loaded = store
                .load_trace(id)
                .await
                .inspect_err(|err| tracing::warn!(error = %err, "reading a trace failed"));
            Self::new(summary, records, loaded.map_err(drop), budget)
        };
        if let TraceBody::Spans { trace, shown, logs } = &mut exported.body {
            for row in &trace.spans()[..*shown] {
                // No records in the trace: none to read for any span.
                let read = if records == Some(0) {
                    Ok(Vec::new())
                } else {
                    store
                        .span_records(id, row.span().span_id, TraceExport::RECORDS_PER_SPAN + 1)
                        .await
                        .inspect_err(
                            |err| tracing::warn!(error = %err, "reading a span's records failed"),
                        )
                };
                logs.push(read.map_or(SpanLogs::Unreadable, SpanLogs::Read));
            }
        }
        exported
    }

    /// The trace of `summary` as `loaded`, writing at most `budget` of its spans and taking
    /// those from it; no log records read yet.
    fn new(
        summary: TraceSummary,
        records: Option<u64>,
        loaded: Result<Option<Trace>, ()>,
        budget: &mut usize,
    ) -> Self {
        let body = match loaded {
            Err(()) => TraceBody::Unreadable,
            Ok(None) => TraceBody::Missing,
            Ok(Some(_)) if *budget == 0 => TraceBody::Skipped,
            Ok(Some(trace)) => {
                let shown = trace.spans().len().min(*budget);
                *budget -= shown;
                TraceBody::Spans {
                    trace,
                    shown,
                    logs: Vec::with_capacity(shown),
                }
            }
        };
        Self {
            summary,
            records,
            body,
        }
    }

    /// The summary line, its line break included.
    fn summary(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = &self.summary;
        write!(
            f,
            "trace {} at {}: {} stack={} {} {}, {}, ",
            s.trace_id,
            TraceExport::utc(Timestamp::from(s.start)),
            Quoted(&s.name),
            Quoted(&s.stack),
            SpanTime(s.duration),
            s.status,
            Counted(usize::try_from(s.spans).unwrap_or(usize::MAX), "span"),
        )?;
        match self.records {
            Some(records) => write!(
                f,
                "{}",
                Counted(usize::try_from(records).unwrap_or(usize::MAX), "log record")
            )?,
            None => write!(f, "log records not counted")?,
        }
        if s.proxy_only && s.spans > 1 {
            write!(f, ", app not instrumented")?;
        }
        writeln!(f)
    }
}

impl fmt::Display for ExportedTrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.summary(f)?;
        match &self.body {
            TraceBody::Skipped => writeln!(
                f,
                "  -- cut: an export holds at most {} spans; this trace's are left out",
                Spans(TraceExport::SPAN_BUDGET)
            ),
            TraceBody::Missing => writeln!(
                f,
                "  -- its spans are no longer in the log store (spans are kept {} days)",
                Retention::SPAN_MAX_AGE.as_secs() / 86_400
            ),
            TraceBody::Unreadable => writeln!(f, "  -- its spans cannot be read right now"),
            TraceBody::Spans { trace, shown, logs } => {
                Self::spans(f, trace, *shown, logs)?;
                let read = trace.spans().len();
                if *shown < read {
                    writeln!(
                        f,
                        "  -- cut: an export holds at most {} spans; the other {} of this trace \
                         are left out",
                        Spans(TraceExport::SPAN_BUDGET),
                        Spans(read - shown)
                    )?;
                }
                if trace.is_truncated() {
                    writeln!(
                        f,
                        "  -- cut: a trace is read up to {} spans; {} more of this trace are not \
                         read",
                        Spans(Trace::SPAN_CAP),
                        Count(
                            trace
                                .total_spans()
                                .saturating_sub(u64::try_from(read).unwrap_or(u64::MAX))
                        )
                    )?;
                }
                Ok(())
            }
        }
    }
}

impl ExportedTrace {
    /// The resources of the first `shown` spans of `trace`, once per service, then those
    /// spans with their events and `logs`.
    fn spans(
        f: &mut fmt::Formatter<'_>,
        trace: &Trace,
        shown: usize,
        logs: &[SpanLogs],
    ) -> fmt::Result {
        let rows = &trace.spans()[..shown];
        let mut services = HashSet::new();
        for row in rows {
            let span = row.span();
            let Some(resource) = span.resource.as_deref() else {
                continue;
            };
            if services.insert(span.service.as_str()) {
                writeln!(
                    f,
                    "  resource {}:{}",
                    OneLine(&span.service),
                    Attributes(resource)
                )?;
            }
        }
        let trace_start = trace.start().unwrap_or(MicroTimestamp::from_micros(0));
        for (n, row) in rows.iter().enumerate() {
            let span = row.span();
            let indent = "  ".repeat(row.depth() + 1);
            write!(
                f,
                "{indent}{}: {} [{}, {}, +{}, {}",
                OneLine(&span.service),
                Cut(&span.name),
                span.kind,
                SpanTime(span.duration),
                SpanTime(trace.offset(row)),
                span.status,
            )?;
            if let Some(message) = span.status_message.as_deref().filter(|m| !m.is_empty()) {
                write!(f, ": {}", Cut(message))?;
            }
            if row.parent_missing() {
                write!(f, ", parent missing")?;
            }
            write!(f, "]")?;
            match span.attributes.as_deref() {
                Some(attributes) => writeln!(f, "{}", Attributes(attributes))?,
                None => writeln!(f)?,
            }
            Self::events(f, &indent, trace_start, span)?;
            if let Some(logs) = logs.get(n) {
                logs.write(f, &indent)?;
            }
        }
        Ok(())
    }

    /// The events of `span`, each `· event +<at> <name> <attributes>` under it; none when the
    /// column is absent or not a JSON array, and an entry that is not an object is skipped.
    fn events(
        f: &mut fmt::Formatter<'_>,
        indent: &str,
        trace_start: MicroTimestamp,
        span: &Span,
    ) -> fmt::Result {
        let Some(Ok(Value::Array(events))) = span.events.as_deref().map(serde_json::from_str)
        else {
            return Ok(());
        };
        for event in events.iter().filter_map(Value::as_object) {
            let name = event.get("name").and_then(Value::as_str).unwrap_or("");
            let time = event
                .get("time_us")
                .and_then(Value::as_i64)
                .map_or(span.start, MicroTimestamp::from_micros);
            let at = Duration::from_micros(
                u64::try_from(time.as_micros() - trace_start.as_micros()).unwrap_or(0),
            );
            write!(f, "{indent}  · event +{} {}", SpanTime(at), Cut(name))?;
            if let Some(attributes) = event.get("attributes").and_then(Value::as_object) {
                write!(f, "{}", Pairs(attributes))?;
            }
            writeln!(f)?;
        }
        Ok(())
    }
}

impl SpanLogs {
    /// The records, each `· log <time> <level> <message>` under the span at `indent`, and the
    /// cut line past [`TraceExport::RECORDS_PER_SPAN`].
    fn write(&self, f: &mut fmt::Formatter<'_>, indent: &str) -> fmt::Result {
        let entries = match self {
            SpanLogs::Unreadable => {
                return writeln!(f, "{indent}  · -- its log records cannot be read right now");
            }
            SpanLogs::Read(entries) => entries,
        };
        let cap = usize::try_from(TraceExport::RECORDS_PER_SPAN).unwrap_or(usize::MAX);
        for entry in entries.iter().take(cap) {
            let record = &entry.record;
            writeln!(
                f,
                "{indent}  · log {} {} {}",
                TraceExport::utc(record.time),
                record.level.map_or("-", |level| level.as_str()),
                Cut(&record.message)
            )?;
        }
        if entries.len() > cap {
            writeln!(
                f,
                "{indent}  · -- cut: at most {} log records per span; this span's later ones are \
                 left out",
                TraceExport::RECORDS_PER_SPAN
            )?;
        }
        Ok(())
    }
}

/// A number of spans, as [`Count`] writes it.
struct Spans(usize);

impl fmt::Display for Spans {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", Count(u64::try_from(self.0).unwrap_or(u64::MAX)))
    }
}

/// Text on one line: its line breaks written as `\n` and `\r`.
struct OneLine<'a>(&'a str);

impl fmt::Display for OneLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.replace('\r', "\\r").replace('\n', "\\n"))
    }
}

/// Text on one line, cut at [`TraceExport::VALUE_CAP`] characters with `…(+N chars)` for the
/// rest.
struct Cut<'a>(&'a str);

impl<'a> Cut<'a> {
    /// The text kept, and how many characters are left out past the cap, if any.
    fn parts(&self) -> (&'a str, Option<usize>) {
        match self.0.char_indices().nth(TraceExport::VALUE_CAP) {
            None => (self.0, None),
            Some((at, _)) => (&self.0[..at], Some(self.0[at..].chars().count())),
        }
    }

    /// The marker of `rest` characters left out: `…(+N chars)`.
    fn marker(f: &mut fmt::Formatter<'_>, rest: Option<usize>) -> fmt::Result {
        match rest {
            None => Ok(()),
            Some(rest) => write!(f, "…(+{rest} chars)"),
        }
    }
}

impl fmt::Display for Cut<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kept, rest) = self.parts();
        write!(f, "{}", OneLine(kept))?;
        Self::marker(f, rest)
    }
}

/// A value of a `key=value` pair: cut as [`Cut`] does, and in double quotes, with `"` and `\`
/// escaped, when it is empty or holds whitespace or a quote. The value is cut before it is
/// escaped, so the marker counts the value's own characters.
struct Quoted<'a>(&'a str);

impl fmt::Display for Quoted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = self.0;
        if !text.is_empty() && !text.chars().any(|c| c.is_whitespace() || c == '"') {
            return write!(f, "{}", Cut(text));
        }
        // Cut first, so the cut cannot split an escape and counts the value's own characters.
        let (kept, rest) = Cut(text).parts();
        let escaped = kept.replace('\\', "\\\\").replace('"', "\\\"");
        write!(f, "\"{}", OneLine(&escaped))?;
        Cut::marker(f, rest)?;
        f.write_str("\"")
    }
}

/// A JSON object column (attributes, resource) as ` key=value` pairs, in order: a string value
/// as its text, anything else as JSON; the column as stored when it is not an object.
struct Attributes<'a>(&'a str);

impl fmt::Display for Attributes<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match serde_json::from_str::<Map<String, Value>>(self.0) {
            Ok(map) => write!(f, "{}", Pairs(&map)),
            Err(_) => write!(f, " {}", Cut(self.0)),
        }
    }
}

/// The pairs of a JSON object, each ` key=value`.
struct Pairs<'a>(&'a Map<String, Value>);

impl fmt::Display for Pairs<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (key, value) in self.0 {
            let text = match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            write!(f, " {}={}", OneLine(key), Quoted(&text))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
