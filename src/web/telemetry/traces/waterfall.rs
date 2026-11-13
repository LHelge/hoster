//! The waterfall of the trace picked (epic qnh, `docs/design/Telemetry.dc.html`): a header of
//! the trace's figures, a legend of its services, a minimap and one row per span in the trace's
//! depth-first order, each with a bar on one time axis from the trace's start to its end.
//!
//! The CSP allows no inline styles and no script (ADR-0054, ADR-0060), so every position is an
//! SVG attribute computed here and every colour a class: a row's service class sets `--svc`,
//! which its name's border, its bar and the minimap's line take. A row's bar is its own `<svg>`
//! in user units (px) with `x` and `width` in percent, so the column scales with the page while
//! the duration label and the error ring keep their pixel sizes; the minimap is one `<svg>` with
//! a `viewBox` of 0–1000 that stretches. [`Bar`] and [`Minimap`] hold the numbers, tested
//! without templates.
//!
//! The controls are links too ([`WaterfallState`]): a span with children has a toggle that
//! collapses or expands it (`?collapsed=`, which the trace resolves, [`Trace::collapsed`]),
//! "expand all" and "collapse to services" set it, and the critical-path switch is a checkbox
//! that CSS `:has()` follows on the page, with a `?crit=on` link where `:has()` is missing.
//!
//! The span selected (`?span=`, a [`Selection`]; without one the trace's [focus](Trace::focus))
//! opens its [`SpanDetails`] (`traces/details.rs`) below its row, when its row is shown.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde_json::{Map, Value};

use super::super::Telemetry;
use super::details::SpanDetails;
use super::{Links, ServiceChip, ServicePalette, TracesView};
use crate::stats::Elapsed;
use crate::store::{Collapse, LogEntry, Span, SpanId, Trace, TraceId, TraceSpan, UnknownLogText};
use crate::web::charts::Num;
use crate::web::ui::{Count, Escaped};

/// The waterfall of one trace, ready for the template.
#[derive(Debug)]
pub struct Waterfall {
    pub id: TraceId,
    /// The root span's stack.
    pub stack: String,
    /// The root span's name.
    pub root: String,
    /// When the trace started: RFC 3339 in UTC, to the millisecond.
    pub time: String,
    /// The same as shown: `2026-11-12 19:58:41.081`.
    pub shown: String,
    pub duration: String,
    /// Every span of the trace, those not shown included.
    pub spans: Count,
    /// The levels of the tree: 1 for a root alone.
    pub depth: usize,
    /// The spans shown that ended in error.
    pub errors: Count,
    pub legend: Vec<LegendEntry>,
    /// The time axis: five labels from the trace's start to its end.
    pub ticks: Vec<String>,
    pub minimap: Minimap,
    /// The spans shown: none under a collapsed span.
    pub rows: Vec<SpanRow>,
    /// The critical path is highlighted (`?crit=on`): the switch starts on.
    pub critical: bool,
    /// The waterfall with the highlight flipped: the switch where CSS has no `:has()`.
    pub critical_href: String,
    /// The waterfall with nothing collapsed.
    pub expand_all: String,
    /// The waterfall with "collapse to services" ([`Trace::service_entries`]).
    pub to_services: String,
    /// The spans past the span cap ([`Trace::MAX_SPANS`]), when any were left out.
    pub more: Option<Count>,
    /// The log records made in the trace, those without a span included.
    pub logs: Count,
    /// "logs in trace": the Logs tab filtered to the trace, at every level; `None` when it has
    /// no records.
    pub logs_href: Option<String>,
}

/// A service in the legend: its name and colour, and how many spans it has.
#[derive(Debug, PartialEq, Eq)]
pub struct LegendEntry {
    pub service: ServiceChip,
    /// `1 span`, `9 spans`.
    pub spans: String,
}

/// One span's row.
#[derive(Debug)]
pub struct SpanRow {
    pub id: SpanId,
    pub name: String,
    pub service: ServiceChip,
    /// Selects this span, or for the span selected, clears the selection.
    pub href: String,
    /// The span `?span=` selects.
    pub current: bool,
    /// The indent level: the span's depth, at most [`Waterfall::DEPTH_CAP`].
    pub indent: usize,
    pub error: bool,
    /// What the span calls, from its attributes: `pg`, `redis`, `exec`.
    pub tag: Option<String>,
    /// Its parent is not in the trace: it hangs under the root.
    pub parent_missing: bool,
    pub duration: String,
    pub bar: Bar,
    /// On the critical path: not dimmed when it is highlighted.
    pub critical: bool,
    /// Collapses or expands the span; `None` for a span without children.
    pub toggle: Option<Toggle>,
    /// The span's details, below its row: for the span selected only.
    pub details: Option<Box<SpanDetails>>,
}

/// The toggle of a span with children.
#[derive(Debug, PartialEq, Eq)]
pub struct Toggle {
    /// The waterfall with the span flipped, the rest as they are.
    pub href: String,
    /// The span's children are shown.
    pub expanded: bool,
    /// `Collapse documents.info`, `Expand documents.info`.
    pub label: String,
    /// What collapsing it hides: every span under it.
    pub hidden: Count,
}

/// What the URL asks of the waterfall: the span selected, the spans collapsed and whether the
/// critical path is highlighted.
#[derive(Debug, Clone, Default)]
pub(super) struct WaterfallState {
    /// `None` without a `span`: the trace's [focus](Trace::focus) is selected.
    pub selected: Option<Selection>,
    pub collapse: Collapse,
    pub critical: bool,
}

impl WaterfallState {
    /// The id of the span selected in `trace`: the one given, or without one the trace's
    /// focus; `None` when the details are closed.
    fn selected(&self, trace: &Trace) -> Option<SpanId> {
        match self.selected {
            None => trace.focus().map(|s| s.span.span_id),
            Some(Selection::Closed) => None,
            Some(Selection::Span(id)) => Some(id),
        }
    }

    /// The span whose details open in `trace`: the one selected, when the trace holds it and
    /// its row is shown. A span inside a collapsed subtree stays selected (expanding it opens
    /// its details again) but shows none.
    pub fn detailed<'a>(&self, trace: &'a Trace) -> Option<&'a TraceSpan> {
        let id = self.selected(trace)?;
        let collapsed = trace.collapsed(&self.collapse);
        let shown = trace
            .shown(&collapsed)
            .any(|(span, _)| span.span.span_id == id);
        trace.span(id).filter(|_| shown)
    }
}

/// The `span` parameter: a span selected, or `none` for none, which a link writes when it
/// closes the details so the trace's [focus](Trace::focus), which a missing `span` selects,
/// does not open again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    /// `none`: no span selected.
    Closed,
    Span(SpanId),
}

impl Selection {
    /// The value of `span` that selects none.
    const CLOSED: &str = "none";
}

impl From<Option<SpanId>> for Selection {
    fn from(span: Option<SpanId>) -> Self {
        span.map_or(Selection::Closed, Selection::Span)
    }
}

impl FromStr for Selection {
    type Err = UnknownLogText;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == Self::CLOSED {
            Ok(Selection::Closed)
        } else {
            s.parse().map(Selection::Span)
        }
    }
}

impl fmt::Display for Selection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Selection::Closed => f.write_str(Self::CLOSED),
            Selection::Span(id) => id.fmt(f),
        }
    }
}

impl Waterfall {
    /// The deepest indent; deeper spans are indented as much (`wf-d0` … `wf-d10` in CSS).
    pub const DEPTH_CAP: usize = 10;

    /// The waterfall of `trace`, its services coloured by `palette`, its rows linking to the
    /// tab of `links` with their span selected; `state` says which is, which are collapsed and
    /// whether the critical path is highlighted. `records` are the log records of the span
    /// whose details open ([`WaterfallState::detailed`]), the first of them by time.
    pub(super) fn new(
        trace: &Trace,
        palette: &ServicePalette,
        links: &Links,
        state: &WaterfallState,
        records: Vec<LogEntry>,
    ) -> Self {
        let selected = state.selected(trace);
        let mut records = Some(records);
        // The links write the collapse as the trace resolves it.
        let collapse = trace.canonical(&state.collapse);
        let collapsed = trace.collapsed(&collapse);
        let links = &links.with(
            "collapsed",
            (!collapse.is_empty()).then(|| collapse.to_string()),
        );
        let axis = Axis::of(trace);
        let spans = trace.spans();
        let chip = |name: &str| ServiceChip {
            name: name.to_owned(),
            class: palette.class(name),
        };
        let root = spans.first().map(|s| &s.span);
        let time = TracesView::utc(trace.start().into());
        let legend = trace
            .services()
            .into_iter()
            .map(|service| {
                let n = spans.iter().filter(|s| s.span.service == service).count();
                LegendEntry {
                    service: chip(service),
                    spans: format!(
                        "{} {}",
                        Count(n as u64),
                        if n == 1 { "span" } else { "spans" }
                    ),
                }
            })
            .collect();
        let rows = trace
            .shown(&collapsed)
            .map(|(span, folded)| {
                let id = span.span.span_id;
                let current = selected == Some(id);
                let toggle = (span.children > 0).then(|| Toggle {
                    href: links.collapsing(&trace.canonical(&collapse.toggle(id))),
                    expanded: !folded,
                    label: format!(
                        "{} {}",
                        if folded { "Expand" } else { "Collapse" },
                        span.span.name
                    ),
                    hidden: Count(span.descendants as u64),
                });
                // Shown rows only: a selected span under a collapsed one has no details.
                let details = current.then(|| {
                    let records = records.take().unwrap_or_default();
                    let close = links.selecting(None);
                    Box::new(SpanDetails::new(
                        trace,
                        span,
                        chip(&span.span.service),
                        close,
                        records,
                    ))
                });
                SpanRow {
                    id,
                    name: span.span.name.clone(),
                    service: chip(&span.span.service),
                    href: links.selecting((!current).then_some(id)),
                    current,
                    indent: span.depth.min(Self::DEPTH_CAP),
                    error: span.span.is_error(),
                    tag: SpanRow::tag(&span.span),
                    parent_missing: span.parent_missing,
                    duration: Elapsed(span.span.duration).to_string(),
                    bar: Bar::new(&axis, span),
                    critical: span.critical,
                    toggle,
                    details,
                }
            })
            .collect();
        let shown = spans.len() as u64;
        Self {
            id: trace.id(),
            stack: root.map(|s| s.stack.clone()).unwrap_or_default(),
            root: root.map(|s| s.name.clone()).unwrap_or_default(),
            shown: time[..23].replacen('T', " ", 1),
            time,
            duration: Elapsed(trace.duration()).to_string(),
            spans: Count(trace.total_spans()),
            depth: spans.iter().map(|s| s.depth + 1).max().unwrap_or(0),
            errors: Count(spans.iter().filter(|s| s.span.is_error()).count() as u64),
            legend,
            ticks: axis.ticks(),
            minimap: Minimap::new(&axis, palette, spans),
            rows,
            critical: state.critical,
            critical_href: links.highlighting(!state.critical),
            expand_all: links.collapsing(&Collapse::default()),
            to_services: links.collapsing(&Collapse::services()),
            more: trace
                .is_truncated()
                .then(|| Count(trace.total_spans() - shown)),
            logs: Count(trace.log_records()),
            logs_href: (trace.log_records() > 0).then(|| Telemetry::trace_logs_href(trace.id())),
        }
    }

    /// Whether any span shown ended in error: the ERRORS figure is red.
    pub fn has_errors(&self) -> bool {
        self.errors.0 > 0
    }

    /// The most spans a trace is read with.
    pub fn cap(&self) -> Count {
        Count(Trace::MAX_SPANS as u64)
    }
}

impl SpanRow {
    /// The tag of `span`'s attributes: the database system (`db.system.name`, or `db.system`
    /// of semantic conventions before 1.30) with PostgreSQL as `pg`, or `exec` for a process.
    fn tag(span: &Span) -> Option<String> {
        let attributes: Map<String, Value> =
            serde_json::from_str(span.attributes.as_deref()?).ok()?;
        let system = ["db.system.name", "db.system"]
            .into_iter()
            .find_map(|key| attributes.get(key)?.as_str());
        match system {
            Some("postgresql") => Some("pg".to_owned()),
            Some(system) if !system.is_empty() => Some(system.to_owned()),
            _ => attributes
                .contains_key("process.command")
                .then(|| "exec".to_owned()),
        }
    }
}

/// The time axis of a trace: from its start to its end, in percent.
#[derive(Debug, Clone, Copy)]
struct Axis {
    start: crate::store::MicroTimestamp,
    total: Duration,
}

impl Axis {
    fn of(trace: &Trace) -> Self {
        Self {
            start: trace.start(),
            total: trace.duration(),
        }
    }

    /// `length` as a share of the axis, in percent; 0 on an axis of no length.
    fn share(&self, length: Duration) -> f64 {
        if self.total.is_zero() {
            0.0
        } else {
            length.as_secs_f64() / self.total.as_secs_f64() * 100.0
        }
    }

    /// Where `span` starts and how wide it is, in percent, at least `min` wide and within the
    /// axis.
    fn place(&self, span: &Span, min: f64) -> (f64, f64) {
        let after = span.start.as_micros() - self.start.as_micros();
        let after = Duration::from_micros(u64::try_from(after).unwrap_or(0));
        let width = self.share(span.duration).clamp(min, 100.0);
        let left = self.share(after).clamp(0.0, 100.0 - width);
        (left, width)
    }

    /// The five labels of the axis, at 0, ¼, ½, ¾ and the end.
    fn ticks(&self) -> Vec<String> {
        (0..=4u32)
            .map(|i| match i {
                0 => "0".to_owned(),
                i => Elapsed(self.total * i / 4).to_string(),
            })
            .collect()
    }
}

/// Where a bar's duration label goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelSide {
    /// Inside a wide bar, from its start.
    Inside,
    /// Right of the bar.
    Right,
    /// Left of a bar near the axis's end.
    Left,
}

impl LabelSide {
    fn as_str(self) -> &'static str {
        match self {
            LabelSide::Inside => "inside",
            LabelSide::Right => "right",
            LabelSide::Left => "left",
        }
    }
}

/// A span's bar on the axis, with its duration label: an SVG of the row's height whose
/// positions are percentages of its width.
#[derive(Debug, Clone, PartialEq)]
pub struct Bar {
    /// Where it starts, in percent of the axis.
    left: f64,
    /// How wide it is, in percent of the axis: at least [`Bar::MIN_WIDTH`].
    width: f64,
    side: LabelSide,
    label: String,
    error: bool,
}

impl Bar {
    /// The narrowest bar, in percent, so the shortest spans are seen.
    const MIN_WIDTH: f64 = 0.3;
    /// A bar wider than this (in percent) holds its label.
    const INSIDE_ABOVE: f64 = 55.0;
    /// A narrower bar ending past this (in percent) has its label on its left.
    const LEFT_PAST: f64 = 78.0;
    /// The row's height, and the bar's top and height, in px.
    const ROW: u32 = 30;
    const TOP: u32 = 8;
    const HEIGHT: u32 = 14;

    fn new(axis: &Axis, span: &TraceSpan) -> Self {
        let (left, width) = axis.place(&span.span, Self::MIN_WIDTH);
        let side = if width > Self::INSIDE_ABOVE {
            LabelSide::Inside
        } else if left + width > Self::LEFT_PAST {
            LabelSide::Left
        } else {
            LabelSide::Right
        };
        Self {
            left,
            width,
            side,
            label: Elapsed(span.span.duration).to_string(),
            error: span.span.is_error(),
        }
    }

    /// Where the label is anchored, in percent, and its offset from there in px.
    fn label_at(&self) -> (f64, i32) {
        match self.side {
            LabelSide::Inside => (self.left, 8),
            LabelSide::Right => (self.left + self.width, 6),
            LabelSide::Left => (self.left, -6),
        }
    }
}

impl fmt::Display for Bar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (row, top, height) = (Self::ROW, Self::TOP, Self::HEIGHT);
        write!(
            f,
            r#"<svg class="wf-bar" width="100%" height="{row}" aria-hidden="true" focusable="false">"#
        )?;
        for x in [25, 50, 75] {
            write!(
                f,
                r#"<line class="wf-grid" x1="{x}%" y1="0" x2="{x}%" y2="{row}"></line>"#
            )?;
        }
        let (x, w) = (Num(self.left), Num(self.width));
        if self.error {
            // A red ring round a gap of the row's background (the fill's stroke).
            write!(
                f,
                r#"<rect class="wf-ring" x="{x}%" y="{top}" width="{w}%" height="{height}" rx="3"></rect>"#
            )?;
        }
        write!(
            f,
            r#"<rect class="wf-fill" x="{x}%" y="{top}" width="{w}%" height="{height}" rx="3"></rect>"#
        )?;
        let (at, dx) = self.label_at();
        write!(
            f,
            r#"<text class="wf-label wf-label-{}" x="{}%" dx="{dx}" y="{}">{}</text></svg>"#,
            self.side.as_str(),
            Num(at),
            row / 2,
            Escaped(&self.label)
        )
    }
}

/// Numbers and escaped text only.
impl askama::filters::HtmlSafe for Bar {}

/// Every span of the trace as a line above the rows, in the order of the rows, coloured by its
/// service or red in error: a `viewBox` 0–1000 wide and 3 units per span that stretches to the
/// card's width and at most [`Minimap::MAX_HEIGHT`] px high.
#[derive(Debug, Clone, PartialEq)]
pub struct Minimap {
    lines: Vec<MiniLine>,
}

#[derive(Debug, Clone, PartialEq)]
struct MiniLine {
    /// Where it starts and how wide it is, in thousandths of the axis.
    left: f64,
    width: f64,
    class: &'static str,
    error: bool,
}

impl Minimap {
    /// The viewBox's width.
    const WIDTH: u32 = 1000;
    /// The narrowest line, in percent.
    const MIN_WIDTH: f64 = 0.4;
    /// Units per span, a line's thickness and the margin above and below.
    const PITCH: u32 = 3;
    const LINE: u32 = 2;
    const MARGIN: u32 = 3;
    /// The highest the minimap is drawn, in px: a larger trace's lines are squeezed.
    const MAX_HEIGHT: u32 = 120;

    fn new(axis: &Axis, palette: &ServicePalette, spans: &[TraceSpan]) -> Self {
        let scale = f64::from(Self::WIDTH) / 100.0;
        let lines = spans
            .iter()
            .map(|span| {
                let (left, width) = axis.place(&span.span, Self::MIN_WIDTH);
                MiniLine {
                    left: left * scale,
                    width: width * scale,
                    class: palette.class(&span.span.service),
                    error: span.span.is_error(),
                }
            })
            .collect();
        Self { lines }
    }

    /// The viewBox's height.
    fn height(&self) -> u32 {
        let n = u32::try_from(self.lines.len()).unwrap_or(u32::MAX / Self::PITCH);
        n.saturating_mul(Self::PITCH) + 2 * Self::MARGIN
    }
}

impl fmt::Display for Minimap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (w, h) = (Self::WIDTH, self.height());
        write!(
            f,
            r#"<svg class="wf-minimap" width="100%" height="{}" viewBox="0 0 {w} {h}" preserveAspectRatio="none" aria-hidden="true" focusable="false">"#,
            h.min(Self::MAX_HEIGHT)
        )?;
        for (i, line) in (0u32..).zip(&self.lines) {
            let error = if line.error { " wf-mini-error" } else { "" };
            write!(
                f,
                r#"<rect class="wf-mini {}{error}" x="{}" y="{}" width="{}" height="{}"></rect>"#,
                line.class,
                Num(line.left),
                Self::MARGIN + i * Self::PITCH,
                Num(line.width),
                Self::LINE
            )?;
        }
        f.write_str("</svg>")
    }
}

/// Numbers and class names only.
impl askama::filters::HtmlSafe for Minimap {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{MicroTimestamp, SpanKind, SpanStatus, Timestamp};

    fn id(name: &str) -> SpanId {
        let mut bytes = [0; 8];
        bytes[..name.len()].copy_from_slice(name.as_bytes());
        SpanId::from(bytes)
    }

    const TRACE: [u8; 16] = [0x4b; 16];

    /// A span of the mockup's sample traces, its start and duration in milliseconds after the
    /// trace's start, with its attributes as JSON.
    fn span(
        name: &str,
        parent: Option<&str>,
        service: &str,
        start: f64,
        ms: f64,
        attributes: Option<&str>,
    ) -> Span {
        Span {
            trace_id: TraceId::from(TRACE),
            span_id: id(name),
            parent: parent.map(id),
            stack: "outline".into(),
            service: service.into(),
            name: name.into(),
            kind: SpanKind::Internal,
            start: MicroTimestamp::from_micros(
                1_762_977_521_081_000 + (start * 1000.0).round() as i64,
            ),
            duration: Duration::from_micros((ms * 1000.0).round() as u64),
            status: SpanStatus::Unset,
            status_message: None,
            attributes: attributes.map(str::to_owned),
            resource: None,
            events: None,
            received: Timestamp::from_millis(0),
        }
    }

    const PG: Option<&str> = Some(r#"{"db.system":"postgresql"}"#);
    const REDIS: Option<&str> = Some(r#"{"db.system":"redis"}"#);
    const EXEC: Option<&str> = Some(r#"{"process.command":"git","process.exit.code":0}"#);

    /// `t1` of `docs/design/Telemetry.dc.html`.
    fn t1() -> Trace {
        let app = "outline-app";
        let mut spans = vec![
            span("s0", None, "hoster", 0.0, 1912.4, None),
            span("s1", Some("s0"), "hoster", 0.6, 1910.9, None),
            span("s2", Some("s1"), app, 3.8, 1905.2, None),
            span("s3", Some("s2"), app, 5.1, 17.6, None),
            span("s4", Some("s3"), app, 6.9, 1.8, REDIS),
            span("s5", Some("s3"), app, 10.4, 9.2, PG),
            span("s6", Some("s2"), app, 23.9, 1880.3, None),
            span("s12", Some("s6"), app, 24.6, 0.9, REDIS),
            span("s7", Some("s6"), app, 26.2, 14.1, PG),
            span("s8", Some("s6"), app, 41.0, 8.3, PG),
            span("s9", Some("s6"), app, 51.7, 1800.4, PG),
            span("s10", Some("s6"), app, 1853.6, 2.7, PG),
        ];
        for error in [0, 1, 2, 6, 10] {
            spans[error].status = SpanStatus::Error;
        }
        spans[0].name = "POST /api/documents.info".into();
        Trace::new(TraceId::from(TRACE), spans, 12)
    }

    /// `t2` of the mockup: gitea, with `exec` spans.
    fn t2() -> Trace {
        let gitea = "gitea";
        let spans = vec![
            span("g0", None, "hoster", 0.0, 142.3, None),
            span("g1", Some("g0"), "hoster", 0.5, 141.2, None),
            span("g2", Some("g1"), gitea, 2.9, 137.6, None),
            span("g3", Some("g2"), gitea, 3.6, 2.8, None),
            span("g4", Some("g3"), gitea, 4.0, 1.6, PG),
            span("g5", Some("g2"), gitea, 6.9, 21.8, None),
            span("g6", Some("g5"), gitea, 7.3, 2.1, PG),
            span("g7", Some("g5"), gitea, 10.2, 15.9, EXEC),
            span("g8", Some("g2"), gitea, 29.1, 84.6, None),
            span("g9", Some("g8"), gitea, 29.8, 40.7, EXEC),
            span("g10", Some("g8"), gitea, 71.2, 41.9, EXEC),
            span("g11", Some("g2"), gitea, 114.3, 23.8, None),
        ];
        Trace::new(TraceId::from(TRACE), spans, 12)
    }

    fn links() -> Links {
        Links {
            action: "/telemetry/traces",
            pairs: vec![
                ("sort", "slowest".to_owned()),
                ("trace", TraceId::from(TRACE).to_string()),
            ],
            page: 0,
        }
    }

    fn waterfall(trace: &Trace, selected: Option<&str>) -> Waterfall {
        let state = WaterfallState {
            selected: selected.map(|name| Selection::Span(id(name))),
            ..WaterfallState::default()
        };
        shown(trace, &state)
    }

    fn shown(trace: &Trace, state: &WaterfallState) -> Waterfall {
        let palette = ServicePalette::of(trace.services());
        Waterfall::new(trace, &palette, &links(), state, Vec::new())
    }

    fn row<'a>(w: &'a Waterfall, name: &str) -> &'a SpanRow {
        w.rows.iter().find(|r| r.name == name).unwrap()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 0.01
    }

    #[test]
    fn the_header_legend_and_axis_of_the_sample_trace() {
        let w = waterfall(&t1(), None);
        assert_eq!(w.stack, "outline");
        assert_eq!(w.root, "POST /api/documents.info");
        assert_eq!(w.time, "2025-11-12T19:58:41.081Z");
        assert_eq!(w.shown, "2025-11-12 19:58:41.081");
        assert_eq!(w.duration, "1.9 s");
        assert_eq!((w.spans, w.depth), (Count(12), 5));
        assert_eq!(w.errors, Count(5));
        assert!(w.has_errors() && w.more.is_none());
        assert_eq!(
            w.legend,
            [
                LegendEntry {
                    service: ServiceChip {
                        name: "hoster".into(),
                        class: "svc-hoster"
                    },
                    spans: "2 spans".into()
                },
                LegendEntry {
                    service: ServiceChip {
                        name: "outline-app".into(),
                        class: "svc-0"
                    },
                    spans: "10 spans".into()
                },
            ]
        );
        assert_eq!(w.ticks, ["0", "478 ms", "956 ms", "1.4 s", "1.9 s"]);
    }

    #[test]
    fn rows_are_in_depth_first_order_with_indents_and_tags() {
        let w = waterfall(&t1(), None);
        let names: Vec<&str> = w.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "POST /api/documents.info",
                "s1",
                "s2",
                "s3",
                "s4",
                "s5",
                "s6",
                "s12",
                "s7",
                "s8",
                "s9",
                "s10"
            ]
        );
        let indents: Vec<usize> = w.rows.iter().map(|r| r.indent).collect();
        assert_eq!(indents, [0, 1, 2, 3, 4, 4, 3, 4, 4, 4, 4, 4]);
        let errors: Vec<&str> = w
            .rows
            .iter()
            .filter(|r| r.error)
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(errors, ["POST /api/documents.info", "s1", "s2", "s6", "s9"]);
        assert_eq!(row(&w, "s4").tag.as_deref(), Some("redis"));
        assert_eq!(row(&w, "s9").tag.as_deref(), Some("pg"));
        assert_eq!(row(&w, "s6").tag, None);
        assert_eq!(row(&w, "s9").duration, "1.8 s");
        assert_eq!(row(&w, "s1").service.class, "svc-hoster");

        let w = waterfall(&t2(), None);
        assert_eq!(row(&w, "g7").tag.as_deref(), Some("exec"));
        assert_eq!(row(&w, "g4").tag.as_deref(), Some("pg"));
        assert_eq!(row(&w, "g11").tag, None);
        assert_eq!(w.depth, 5);
        assert_eq!(w.errors, Count(0));
        assert!(!w.has_errors());
    }

    #[test]
    fn bars_sit_on_the_trace_axis_with_their_labels_beside_or_inside() {
        let w = waterfall(&t1(), None);
        let bar = |name| &row(&w, name).bar;
        // The root fills the axis and holds its label; so does s9 (94 %).
        assert!(close(bar("POST /api/documents.info").left, 0.0));
        assert!(close(bar("POST /api/documents.info").width, 100.0));
        assert_eq!(bar("POST /api/documents.info").side, LabelSide::Inside);
        assert!(close(bar("s9").left, 2.70) && close(bar("s9").width, 94.14));
        assert_eq!(bar("s9").side, LabelSide::Inside);
        // Short spans early on: the minimum width, the label right of the bar.
        assert!(close(bar("s12").left, 1.29) && close(bar("s12").width, Bar::MIN_WIDTH));
        assert_eq!(bar("s12").side, LabelSide::Right);
        assert!(close(bar("s3").left, 0.27) && close(bar("s3").width, 0.92));
        assert_eq!(bar("s3").side, LabelSide::Right);
        // Near the end: the label left of the bar.
        assert!(close(bar("s10").left, 96.93));
        assert_eq!(bar("s10").side, LabelSide::Left);
        assert!(bar("s9").error && !bar("s10").error);

        let w = waterfall(&t2(), None);
        let bar = |name| &row(&w, name).bar;
        assert!(close(bar("g8").left, 20.45) && close(bar("g8").width, 59.45));
        assert_eq!(bar("g8").side, LabelSide::Inside);
        // 50.04 % + 29.44 % ends past 78 %.
        assert_eq!(bar("g10").side, LabelSide::Left);
        assert_eq!(bar("g9").side, LabelSide::Right);
        assert_eq!(bar("g11").side, LabelSide::Left);
    }

    #[test]
    fn a_bar_is_svg_with_percentages_and_an_error_ring() {
        let w = waterfall(&t1(), None);
        let svg = row(&w, "s10").bar.to_string();
        assert!(
            svg.starts_with(r#"<svg class="wf-bar" width="100%" height="30" aria-hidden="true" focusable="false">"#),
            "{svg}"
        );
        assert!(
            svg.contains(
                r#"<rect class="wf-fill" x="96.93%" y="8" width="0.3%" height="14" rx="3"></rect>"#
            ),
            "{svg}"
        );
        assert!(!svg.contains("wf-ring"), "{svg}");
        assert!(
            svg.contains(
                r#"<text class="wf-label wf-label-left" x="96.93%" dx="-6" y="15">2.7 ms</text>"#
            ),
            "{svg}"
        );
        let svg = row(&w, "s9").bar.to_string();
        assert!(
            svg.contains(r#"<rect class="wf-ring" x="2.7%" y="8" width="94.14%" height="14" rx="3"></rect><rect class="wf-fill""#),
            "{svg}"
        );
        assert!(svg.contains(r#"class="wf-label wf-label-inside" x="2.7%" dx="8""#));
        assert!(!svg.contains("style"), "{svg}");
    }

    #[test]
    fn the_minimap_draws_every_span_in_row_order() {
        let w = waterfall(&t1(), None);
        let svg = w.minimap.to_string();
        assert!(
            svg.starts_with(r#"<svg class="wf-minimap" width="100%" height="42" viewBox="0 0 1000 42" preserveAspectRatio="none""#),
            "{svg}"
        );
        assert_eq!(svg.matches("<rect ").count(), 12);
        assert!(
            svg.contains(r#"<rect class="wf-mini svc-hoster wf-mini-error" x="0" y="3" width="1000" height="2"></rect>"#),
            "{svg}"
        );
        // s4, the fifth row: 6.9 ms in, at least 0.4 % wide.
        assert!(
            svg.contains(
                r#"<rect class="wf-mini svc-0" x="3.61" y="15" width="4" height="2"></rect>"#
            ),
            "{svg}"
        );
    }

    #[test]
    fn a_large_minimap_is_squeezed() {
        let spans = (0..100)
            .map(|i| span(&format!("x{i}"), None, "a", f64::from(i), 1.0, None))
            .collect();
        let w = waterfall(&Trace::new(TraceId::from(TRACE), spans, 100), None);
        assert!(
            w.minimap
                .to_string()
                .contains(r#"height="120" viewBox="0 0 1000 306""#)
        );
    }

    #[test]
    fn rows_select_their_span_and_the_selected_one_clears_it() {
        let w = waterfall(&t1(), Some("s9"));
        let trace = TraceId::from(TRACE);
        let s9 = row(&w, "s9");
        assert!(s9.current);
        assert_eq!(
            s9.href,
            format!("/telemetry/traces?sort=slowest&trace={trace}&span=none")
        );
        let s4 = row(&w, "s4");
        assert!(!s4.current);
        assert_eq!(
            s4.href,
            format!(
                "/telemetry/traces?sort=slowest&trace={trace}&span={}",
                id("s4")
            )
        );
        assert_eq!(w.rows.iter().filter(|r| r.current).count(), 1);
    }

    #[test]
    fn without_a_span_the_focus_is_selected_and_none_selects_none() {
        let trace = t1();
        let w = shown(&trace, &WaterfallState::default());
        let current: Vec<&str> = w
            .rows
            .iter()
            .filter(|r| r.current)
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(current, ["s9"]);
        assert!(row(&w, "s9").details.is_some());
        let closed = WaterfallState {
            selected: Some(Selection::Closed),
            ..WaterfallState::default()
        };
        let w = shown(&trace, &closed);
        assert!(w.rows.iter().all(|r| !r.current && r.details.is_none()));
        // A span hidden by a collapse stays selected, without details.
        let hidden = WaterfallState {
            collapse: Collapse::default().toggle(id("s6")),
            ..WaterfallState::default()
        };
        assert!(hidden.detailed(&trace).is_none());
        assert!(
            shown(&trace, &hidden)
                .rows
                .iter()
                .all(|r| r.details.is_none())
        );
        assert_eq!(
            WaterfallState::default()
                .detailed(&trace)
                .map(|s| s.span.span_id),
            Some(id("s9"))
        );
    }

    #[test]
    fn a_selection_is_a_span_id_or_none() {
        assert_eq!("none".parse::<Selection>().unwrap(), Selection::Closed);
        let s9 = id("s9");
        assert_eq!(
            s9.to_string().parse::<Selection>().unwrap(),
            Selection::Span(s9)
        );
        assert!("nope".parse::<Selection>().is_err());
        assert_eq!(Selection::from(None).to_string(), "none");
        assert_eq!(Selection::from(Some(s9)).to_string(), s9.to_string());
    }

    #[test]
    fn a_partial_trace_hangs_lost_spans_under_the_root_and_counts_what_is_cut() {
        let spans = vec![
            span("root", None, "hoster", 0.0, 100.0, None),
            span("late", Some("lost"), "app", 50.0, 10.0, None),
            span("child", Some("root"), "app", 10.0, 20.0, None),
        ];
        let w = waterfall(&Trace::new(TraceId::from(TRACE), spans, 2_345), None);
        let names: Vec<&str> = w.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["root", "child", "late"]);
        let late = row(&w, "late");
        assert!(late.parent_missing && late.indent == 1);
        assert!(close(late.bar.left, 50.0) && close(late.bar.width, 10.0));
        assert!(!row(&w, "child").parent_missing);
        assert_eq!(w.spans, Count(2_345));
        assert_eq!(w.more, Some(Count(2_342)));
    }

    #[test]
    fn deep_spans_indent_to_the_cap() {
        let mut spans = vec![span("d0", None, "a", 0.0, 50.0, None)];
        for d in 1..14 {
            let parent = format!("d{}", d - 1);
            spans.push(span(
                &format!("d{d}"),
                Some(&parent),
                "a",
                f64::from(d),
                1.0,
                None,
            ));
        }
        let w = waterfall(&Trace::new(TraceId::from(TRACE), spans, 14), None);
        assert_eq!(w.depth, 14);
        assert_eq!(row(&w, "d13").indent, Waterfall::DEPTH_CAP);
        assert_eq!(row(&w, "d9").indent, 9);
    }

    #[test]
    fn a_trace_of_no_length_draws_minimal_bars() {
        let spans = vec![span("only", None, "a", 0.0, 0.0, None)];
        let w = waterfall(&Trace::new(TraceId::from(TRACE), spans, 1), None);
        let bar = &w.rows[0].bar;
        assert!(close(bar.left, 0.0) && close(bar.width, Bar::MIN_WIDTH));
        assert_eq!(w.ticks[4], "0.0 ms");
    }
}
