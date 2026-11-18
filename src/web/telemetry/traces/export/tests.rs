//! The export's text on fixed traces, without a store: the format, each cap's cut line and the
//! file name.

use std::time::Duration;

use super::*;
use crate::store::{
    Level, LogRecord, LogScope, SpanId, SpanKind, SpanStatus, Stream, TraceId, TraceSort,
    TraceStatus,
};

/// 2026-11-13T12:00:00Z.
const NOW: i64 = 1_794_571_200_000;

fn trace_id(n: u8) -> TraceId {
    TraceId::from([n; 16])
}

fn sid(n: u8) -> SpanId {
    SpanId::from([0, 0, 0, 0, 0, 0, 0, n])
}

/// A minute before [`NOW`], plus `ms`.
fn at(ms: u64) -> MicroTimestamp {
    MicroTimestamp::from(Timestamp::from_millis(NOW) - Duration::from_secs(60))
        + Duration::from_millis(ms)
}

/// Span `id` of trace 1 under `parent`, `ms` after the trace's start.
fn span(id: u8, parent: Option<u8>, service: &str, name: &str, ms: u64, took_ms: u64) -> Span {
    Span {
        trace_id: trace_id(1),
        span_id: sid(id),
        parent: parent.map(sid),
        stack: "notes".into(),
        service: service.into(),
        name: name.into(),
        kind: SpanKind::Internal,
        start: at(ms),
        duration: Duration::from_millis(took_ms),
        status: SpanStatus::Unset,
        status_message: None,
        attributes: None,
        resource: Some(format!(r#"{{"service.name":"{service}"}}"#)),
        events: None,
        received: Timestamp::from_millis(NOW),
    }
}

/// The proxy's server span, the app's span with an event, its failing child and a sibling of
/// that child of the same service as its parent.
fn spans() -> Vec<Span> {
    let mut root = span(1, None, "hoster", "GET /a", 0, 50);
    root.kind = SpanKind::Server;
    root.attributes = Some(r#"{"http.request.method":"GET","url.path":"/a"}"#.into());
    let mut app = span(2, Some(1), "web", "GET /a", 2, 45);
    app.resource = Some(r#"{"service.name":"web","service.version":"1.2"}"#.into());
    app.attributes = Some(r#"{"note":"two\nlines","retries":2}"#.into());
    app.events = Some(format!(
        r#"[{{"name":"cache miss","time_us":{},"attributes":{{"key":"user 7"}}}},"not an event"]"#,
        at(12).as_micros()
    ));
    let mut lookup = span(3, Some(2), "api", "lookup", 10, 20);
    lookup.status = SpanStatus::Error;
    lookup.status_message = Some("no row".into());
    let render = span(4, Some(2), "web", "render", 30, 5);
    vec![root, app, lookup, render]
}

fn summary(n: u8, spans: u64) -> TraceSummary {
    TraceSummary {
        trace_id: trace_id(n),
        start: at(0),
        name: "GET /a".into(),
        stack: "notes".into(),
        services: vec!["hoster".into(), "web".into(), "api".into()],
        spans,
        duration: Duration::from_millis(50),
        status: TraceStatus::Error,
        proxy_only: false,
    }
}

fn record(message: &str) -> LogEntry {
    LogEntry {
        id: 7,
        record: LogRecord {
            time: Timestamp::from(at(3)),
            stack: Some("notes".into()),
            service: "web".into(),
            generation: Some(1),
            stream: Stream::Otlp,
            level: Some(Level::Info),
            message: message.into(),
            fields: None,
            cut: false,
            marker: false,
            trace_id: Some(trace_id(1)),
            span_id: Some(sid(2)),
            received: None,
        },
    }
}

/// The export of `traces`, over the 15 minutes before [`NOW`] (and its second of slack), of
/// the stack `notes`.
fn export(traces: Vec<ExportedTrace>) -> TraceExport {
    let end = Timestamp::from_millis(NOW) + Duration::from_secs(1);
    let start = Timestamp::from_millis(NOW) - Duration::from_secs(15 * 60);
    let mut query = TraceQuery::new(start..end, LogScope::Stack("notes".into()));
    query.status = Some(TraceStatus::Error);
    query.sort = TraceSort::Slowest;
    TraceExport {
        query,
        since: "15m",
        stack: Some("notes".into()),
        notes: vec!["`soon` is not a page; showing the first.".into()],
        matching: 1,
        total: 5,
        traces,
    }
}

/// Trace 1 with every span written, a record under the app's span.
fn exported() -> ExportedTrace {
    let trace = Trace::new(trace_id(1), spans(), 4);
    let mut budget = TraceExport::SPAN_BUDGET;
    let mut exported = ExportedTrace::new(summary(1, 4), Some(1), Ok(Some(trace)), &mut budget);
    assert_eq!(budget, TraceExport::SPAN_BUDGET - 4);
    if let TraceBody::Spans { logs, .. } = &mut exported.body {
        logs.extend([
            SpanLogs::Read(Vec::new()),
            SpanLogs::Read(vec![record("listening\non :8080")]),
            SpanLogs::Read(Vec::new()),
            SpanLogs::Unreadable,
        ]);
    }
    exported
}

#[test]
fn a_small_trace_is_written_as_its_summary_resources_and_span_tree() {
    let text = export(vec![exported()]).to_string();
    let expected = "\
hoster trace export: the traces hoster's Traces tab lists for these options, as text
range:   2026-11-13T11:45:00.000Z to 2026-11-13T12:00:01.000Z (UTC, the last 15m)
stack:   notes
filters: status=error
sort:    slowest first
traces:  1–1 of 1 matching (5 in range)
note:    `soon` is not a page; showing the first.
legend:  a trace is its summary line, its resource attributes once per service, then its spans as a tree, two spaces a level:
         <service>: <name> [<kind>, <duration>, +<start after the trace's>, <status>[: <message>]] <attribute>=<value> …
         under a span, its events (`· event +<at> <name> <attribute>=<value> …`) and log records (`· log <time> <level> <message>`)
         a value longer than 200 characters ends in `…(+N chars)`; line breaks are written as \\n

trace 01010101010101010101010101010101 at 2026-11-13T11:59:00.000Z: \"GET /a\" stack=notes 50.0 ms error, 4 spans, 1 log record
  resource hoster: service.name=hoster
  resource web: service.name=web service.version=1.2
  resource api: service.name=api
  hoster: GET /a [server, 50.0 ms, +0 µs, unset] http.request.method=GET url.path=/a
    web: GET /a [internal, 45.0 ms, +2.0 ms, unset] note=\"two\\nlines\" retries=2
      · event +12.0 ms cache miss key=\"user 7\"
      · log 2026-11-13T11:59:00.003Z info listening\\non :8080
      api: lookup [internal, 20.0 ms, +10.0 ms, error: no row]
      web: render [internal, 5.0 ms, +30.0 ms, unset]
        · -- its log records cannot be read right now
";
    assert_eq!(text, expected, "{text}");
    // Nothing is an HTML escape.
    assert!(!text.contains("&#"), "{text}");
}

#[test]
fn an_empty_list_says_so_after_the_header() {
    let mut empty = export(Vec::new());
    empty.query.page = 2;
    empty.notes.clear();
    let text = empty.to_string();
    assert!(
        text.contains("traces:  none on page 3 of 1 matching (5 in range)\n"),
        "{text}"
    );
    assert!(text.ends_with("\nno traces listed.\n"), "{text}");
    assert!(!text.contains("note:"), "{text}");
}

#[test]
fn filters_are_written_as_resolved_and_none_without_any() {
    let mut export = export(Vec::new());
    export.query.min_duration = Some(Duration::from_millis(500));
    export.query.name = Some("say \"hi\"".into());
    assert_eq!(
        export.filters(),
        r#"status=error, slower than 500ms, span name contains "say \"hi\"""#
    );
    export.query.status = None;
    export.query.min_duration = None;
    export.query.name = Some(String::new());
    assert_eq!(export.filters(), "none");
    export.stack = None;
    assert!(export.to_string().contains("\nstack:   every stack\n"));
    // A stack as given cannot add a line to the header.
    export.stack = Some("x\nnote:    forged\r".into());
    let text = export.to_string();
    assert!(
        text.contains("\nstack:   x\\nnote:    forged\\r\n"),
        "{text}"
    );
    assert!(!text.contains("\nnote:    forged"), "{text}");
}

#[test]
fn the_span_budget_cuts_a_trace_and_skips_the_spans_of_those_after_it() {
    let mut budget = 3;
    let first = ExportedTrace::new(
        summary(1, 4),
        Some(0),
        Ok(Some(Trace::new(trace_id(1), spans(), 4))),
        &mut budget,
    );
    assert_eq!(budget, 0);
    let second = ExportedTrace::new(
        summary(2, 4),
        None,
        Ok(Some(Trace::new(trace_id(1), spans(), 4))),
        &mut budget,
    );
    let text = export(vec![first, second]).to_string();
    // Three spans of the first, then the line saying what is left out.
    assert!(text.contains("      api: lookup ["), "{text}");
    assert!(!text.contains("web: render"), "{text}");
    assert!(
        text.contains(&format!(
            "\n  -- cut: an export holds at most {} spans; the other 1 of this trace are left \
             out\n",
            TraceExport::SPAN_BUDGET
        )),
        "{text}"
    );
    // The second keeps its summary line.
    let second = text
        .split("trace 02020202020202020202020202020202 at ")
        .nth(1)
        .expect(&text);
    assert_eq!(
        second,
        format!(
            "2026-11-13T11:59:00.000Z: \"GET /a\" stack=notes 50.0 ms error, 4 spans, log \
             records not counted\n  -- cut: an export holds at most {} spans; this trace's are \
             left out\n",
            TraceExport::SPAN_BUDGET
        )
    );
}

#[test]
fn a_trace_read_up_to_its_cap_says_how_many_spans_are_not_read() {
    let mut budget = TraceExport::SPAN_BUDGET;
    let mut exported = ExportedTrace::new(
        summary(1, 1_204),
        Some(0),
        Ok(Some(Trace::new(trace_id(1), spans(), 1_204))),
        &mut budget,
    );
    if let TraceBody::Spans { logs, .. } = &mut exported.body {
        logs.resize(4, SpanLogs::Read(Vec::new()));
    }
    let text = exported.to_string();
    assert!(
        text.ends_with(
            "  -- cut: a trace is read up to 1,000 spans; 1,200 more of this trace are not read\n"
        ),
        "{text}"
    );
}

#[test]
fn a_span_with_more_records_than_the_cap_says_its_later_ones_are_left_out() {
    let cap = usize::try_from(TraceExport::RECORDS_PER_SPAN).unwrap();
    let records = SpanLogs::Read((0..=cap).map(|n| record(&format!("r{n}"))).collect());
    struct Written<'a>(&'a SpanLogs);
    impl fmt::Display for Written<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.write(f, "  ")
        }
    }
    let text = Written(&records).to_string();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), cap + 1, "{text}");
    assert!(lines[0].ends_with(" info r0"), "{text}");
    assert!(!text.contains(&format!(" r{cap}\n")), "{text}");
    assert_eq!(
        lines[cap],
        format!(
            "    · -- cut: at most {} log records per span; this span's later ones are left out",
            TraceExport::RECORDS_PER_SPAN
        )
    );
}

#[test]
fn values_are_cut_at_the_cap_with_how_much_is_left_out() {
    let cap = TraceExport::VALUE_CAP;
    let long = "é".repeat(cap + 34);
    assert_eq!(
        Cut(&long).to_string(),
        format!("{}…(+34 chars)", "é".repeat(cap))
    );
    assert_eq!(Cut(&"a".repeat(cap)).to_string(), "a".repeat(cap));
    // In an attribute too, quoted when it holds a space.
    let attributes = format!(r#"{{"q":"{} {}"}}"#, "x".repeat(cap), "y");
    assert_eq!(
        Attributes(&attributes).to_string(),
        format!(" q=\"{}…(+2 chars)\"", "x".repeat(cap))
    );
    // Not an object: written as stored, on one line.
    assert_eq!(Attributes("[1,\n2]").to_string(), " [1,\\n2]");
    // A cut right before a quote leaves no lone backslash, and counts the value's characters.
    let quote_at_cap = format!("{} {}\"\"z", "a".repeat(cap - 2), "b");
    assert_eq!(quote_at_cap.chars().count(), cap + 3);
    assert_eq!(
        Quoted(&quote_at_cap).to_string(),
        format!("\"{} b…(+3 chars)\"", "a".repeat(cap - 2))
    );
    let quote_before_cap = format!("{} \"x", "a".repeat(cap - 2));
    assert_eq!(
        Quoted(&quote_before_cap).to_string(),
        format!("\"{} \\\"…(+1 chars)\"", "a".repeat(cap - 2))
    );
    assert_eq!(Quoted("").to_string(), "\"\"");
    assert_eq!(Quoted("a\\b c").to_string(), "\"a\\\\b c\"");
}

#[test]
fn missing_and_unreadable_traces_keep_their_summary_line() {
    let mut budget = 1;
    let missing = ExportedTrace::new(summary(1, 2), Some(0), Ok(None), &mut budget);
    let unreadable = ExportedTrace::new(summary(2, 2), Some(0), Err(()), &mut budget);
    assert_eq!(budget, 1);
    assert!(
        missing
            .to_string()
            .ends_with("\n  -- its spans are no longer in the log store (spans are kept 3 days)\n")
    );
    assert!(
        unreadable
            .to_string()
            .ends_with("\n  -- its spans cannot be read right now\n")
    );
}

#[test]
fn the_file_name_is_the_stack_and_the_resolved_range() {
    let mut export = export(Vec::new());
    assert_eq!(
        export.file_name(),
        "traces-notes-2026-11-13T11-45-00Z-2026-11-13T12-00-01Z.txt"
    );
    export.stack = Some("a\"b;c/\r\nd".into());
    assert_eq!(
        export.file_name(),
        "traces-a_b_c___d-2026-11-13T11-45-00Z-2026-11-13T12-00-01Z.txt"
    );
    export.stack = None;
    assert_eq!(
        export.file_name(),
        "traces-all-2026-11-13T11-45-00Z-2026-11-13T12-00-01Z.txt"
    );
}

#[test]
fn download_is_asked_for_by_download_1_only() {
    let param = |value: Option<&str>| ExportParam {
        download: value.map(str::to_owned),
    };
    assert!(param(Some("1")).download());
    assert!(param(Some(" 1 ")).download());
    assert!(!param(Some("yes")).download());
    assert!(!param(None).download());
}
