//! The waterfall of the selected trace (ADR-0069 §6, §9; epic qnh): the layout of a [`Trace`]
//! for `telemetry_traces.html`, below the trace list.
//!
//! The tree, its order, depth, critical path and self times are the trace's own ([`Trace`]);
//! this module adds only where things are drawn. Every span has a row in depth-first order,
//! indented by depth (a class per level up to [`SpanRow::DEPTH_CAP`]), with a bar on one axis
//! from the trace's start to its end. The CSP allows no inline styles (ADR-0054), so a bar is
//! an SVG per row whose coordinates are attributes: positions are laid out in thousandths of
//! the axis ([`Bar::AXIS`]) and written as percentages of the row's width, so the column
//! scales with the page and the label, corner radius and error ring keep their size in px.
//! Colours are the service classes of the list's [`ServicePalette`], so a service has one
//! colour in the list, the legend, the minimap and the bars.
//!
//! Each row's name is a link that selects the span (`span=<id>`, kept with the trace by
//! [`TraceParams::select_span`]); the whole row is its target, by CSS alone. The selected span
//! ([`TraceParams::selected_span`]: the trace's default span until another is picked) has its
//! details under its row ([`SpanDetails`], `waterfall/details.rs`), and its row's link closes
//! them ([`TraceParams::close_span`]).
//!
//! The controls work as links, without script (ADR-0054, ADR-0060). A span with rows under it
//! has a toggle that collapses or expands it: the collapsed set is the URL's `collapsed`
//! ([`TraceParams::collapsed`], kept to the spans of this trace that have rows under them), and
//! the rows are what [`Trace::visible`] shows for it; "expand all" clears it and "collapse to
//! services" sets it to [`Trace::service_entries`]. The selected span's details are drawn only
//! while its row is shown: a collapse that hides it closes nothing, so expanding shows them
//! again. The critical path is marked on its rows ([`TraceSpan::on_critical_path`]) and the
//! others are dimmed by CSS while the switch's checkbox is checked (`:has()`), or where
//! `:has()` is not supported by the server while `crit=on` ([`TraceParams::critical`]), which
//! the switch's fallback link toggles. The checkbox is not in the URL, so it lasts for the page
//! view: every link loads the page again with the checkbox as `crit` has it (checked for
//! `crit=on`), and only `crit=on` lasts across links. The minimap always draws every span
//! read.

mod details;

use std::collections::HashSet;
use std::fmt;
use std::time::Duration;

use serde_json::{Map, Value};

use self::details::SpanDetails;
use super::{ServicePalette, TraceParams};
use crate::stack::Counted;
use crate::stats::Elapsed;
use crate::store::{LogStore, Span, SpanId, Timestamp, Trace, TraceSpan};
use crate::web::ui::{Count, Escaped};

/// A span's or the axis's time as the waterfall writes it, finer than [`Elapsed`] since spans
/// are short and sit side by side, as the mockup writes them: `900 µs`, `17.6 ms`, `478 ms`,
/// `1.91 s`, and from a minute as everywhere else in the UI (`3 min 5 s`). The unit is picked
/// after rounding to what it shows, so 99.96 ms reads `100 ms`, never `100.0 ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanTime(pub Duration);

impl fmt::Display for SpanTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0.as_secs_f64();
        let micros = (secs * 1e6).round();
        let tenth_ms = (secs * 1e4).round();
        let ms = (secs * 1e3).round();
        let hundredth_s = (secs * 1e2).round();
        if micros < 1e3 {
            write!(f, "{micros} µs")
        } else if tenth_ms < 1e3 {
            write!(f, "{:.1} ms", tenth_ms / 10.0)
        } else if ms < 1e3 {
            write!(f, "{ms} ms")
        } else if hundredth_s < 6e3 {
            write!(f, "{:.2} s", hundredth_s / 100.0)
        } else {
            // Whole seconds, which `Elapsed` shows from a minute on (it would cut, not round).
            write!(f, "{}", Elapsed(Duration::from_secs_f64(secs.round())))
        }
    }
}

/// The selected trace laid out: its header figures, legend, minimap and rows.
#[derive(Debug)]
pub struct Waterfall {
    trace: Trace,
    /// The root's stack; empty for a trace without one.
    pub stack: String,
    /// The root span's name, or the trace id when the trace has no spans.
    pub title: String,
    /// When the earliest span started: RFC 3339 in UTC to the millisecond, and as shown
    /// (`2026-11-12 19:58:41.081`).
    pub started: Option<(String, String)>,
    /// From the earliest start to the latest end.
    pub duration: String,
    /// Every span the trace has, those not read included.
    pub spans: String,
    /// Levels of the tree.
    pub depth: usize,
    /// Spans read in error.
    pub errors: usize,
    pub legend: Vec<LegendEntry>,
    /// The axis labels at 0, ¼, ½, ¾ and the end.
    pub ticks: Vec<String>,
    pub minimap: Minimap,
    pub rows: Vec<SpanRow>,
    /// The selected span's details, drawn under its row; `None` when none is selected or a
    /// collapse hides its row.
    pub details: Option<SpanDetails>,
    /// The critical path is highlighted from the URL (`crit=on`).
    pub critical: bool,
    /// Where the critical-path switch's fallback link goes: the highlight switched.
    pub critical_href: String,
    /// "expand all": the trace with nothing collapsed; `None` when nothing is.
    pub expand_all: Option<String>,
    /// "collapse to services": the trace with [`Trace::service_entries`] collapsed; `None`
    /// when it has none or they are what is collapsed.
    pub collapse_services: Option<String>,
}

impl Waterfall {
    /// How many labels the axis has: at 0, ¼, ½, ¾ and the end.
    const TICKS: u32 = 5;

    /// `trace` laid out, its services in the colours of `palette`, its rows linking from the
    /// tab as `params` has it.
    pub fn new(trace: Trace, palette: &ServicePalette, params: &TraceParams) -> Self {
        let total = trace.duration();
        let current = params.selected_span(&trace);
        let mut collapsed = params.collapsed();
        collapsed.retain(|id| trace.span(*id).is_some_and(TraceSpan::has_children));
        let visible = trace.visible(&collapsed);
        let details = current
            .and_then(|id| visible.iter().find(|row| row.span().span_id == id))
            .map(|span| SpanDetails::new(&trace, span, params));
        let rows = visible
            .into_iter()
            .map(|span| SpanRow::new(&trace, span, total, palette, params, current, &collapsed))
            .collect();
        let entries = trace.service_entries();
        let collapse_services = (!entries.is_empty() && entries != collapsed)
            .then(|| params.collapse(&trace, &entries));
        let expand_all = (!collapsed.is_empty()).then(|| params.collapse(&trace, &HashSet::new()));
        let started = trace.start().map(|start| {
            let time = humantime::format_rfc3339_millis(Timestamp::from(start).into()).to_string();
            let shown = time.trim_end_matches('Z').replacen('T', " ", 1);
            (time, shown)
        });
        let legend = trace
            .services()
            .into_iter()
            .map(|(service, count)| LegendEntry {
                name: service.to_owned(),
                class: palette.class(service),
                count: Counted(count, "span").to_string(),
            })
            .collect();
        let ticks = (0..Self::TICKS)
            .map(|i| SpanTime(total * i / (Self::TICKS - 1)).to_string())
            .collect();
        Self {
            stack: trace
                .root()
                .map(|root| root.span().stack.clone())
                .unwrap_or_default(),
            title: trace
                .root()
                .map_or_else(|| trace.id().to_string(), |root| root.span().name.clone()),
            started,
            duration: SpanTime(total).to_string(),
            spans: Count(trace.total_spans()).to_string(),
            depth: trace.depth(),
            errors: trace.errors(),
            legend,
            ticks,
            minimap: Minimap::new(&trace, palette),
            rows,
            details,
            critical: params.critical(),
            critical_href: params.toggle_critical(),
            expand_all,
            collapse_services,
            trace,
        }
    }

    /// Reads the log records of the selected span from `store` into its details.
    pub async fn read_logs(&mut self, store: &LogStore) {
        if let Some(details) = &mut self.details {
            details.read_logs(store, self.trace.id()).await;
        }
    }

    /// The trace laid out.
    pub fn trace(&self) -> &Trace {
        &self.trace
    }

    /// `988 more not shown`, when the read stopped at [`Trace::SPAN_CAP`].
    pub fn more(&self) -> Option<String> {
        let read = u64::try_from(self.trace.spans().len()).unwrap_or(u64::MAX);
        self.trace.is_truncated().then(|| {
            format!(
                "The first {} spans by start: {} more not shown.",
                Count(read),
                Count(self.trace.total_spans() - read)
            )
        })
    }
}

/// A service of the legend, with its colour and span count (`3 spans`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegendEntry {
    pub name: String,
    pub class: &'static str,
    pub count: String,
}

/// One row of the waterfall: a span's name column and its bar.
#[derive(Debug, Clone, PartialEq)]
pub struct SpanRow {
    pub id: SpanId,
    pub name: String,
    pub service: String,
    /// The service's colour class.
    pub class: &'static str,
    pub depth: usize,
    /// The indent's class: `wf-depth-<depth>`, the cap's beyond it.
    pub depth_class: &'static str,
    pub error: bool,
    /// What the span calls, from its attributes: `pg`, `redis`, `exec`.
    pub tag: Option<String>,
    /// Its parent is not in the trace, so it hangs under the root.
    pub parent_missing: bool,
    /// Where selecting it goes; for the selected span, where closing its details goes.
    pub href: String,
    /// It is the selected span.
    pub current: bool,
    /// It is on the critical path, which the switch highlights.
    pub critical: bool,
    /// Its collapse toggle, when it has rows under it.
    pub toggle: Option<Toggle>,
    /// Its duration and where it starts on the axis (`+51.7 ms`), as text.
    pub took: String,
    pub offset: String,
    pub bar: Bar,
}

impl SpanRow {
    /// The deepest indent; deeper rows are drawn at this one.
    pub const DEPTH_CAP: usize = 10;
    const DEPTH_CLASSES: [&str; Self::DEPTH_CAP + 1] = [
        "wf-depth-0",
        "wf-depth-1",
        "wf-depth-2",
        "wf-depth-3",
        "wf-depth-4",
        "wf-depth-5",
        "wf-depth-6",
        "wf-depth-7",
        "wf-depth-8",
        "wf-depth-9",
        "wf-depth-10",
    ];
    /// The longest `db.system` shown as it is; a longer one is shown as `db`.
    const TAG_MAX: usize = 12;

    fn new(
        trace: &Trace,
        row: &TraceSpan,
        total: Duration,
        palette: &ServicePalette,
        params: &TraceParams,
        current: Option<SpanId>,
        collapsed: &HashSet<SpanId>,
    ) -> Self {
        let span = row.span();
        let class = palette.class(&span.service);
        let offset = trace.offset(row);
        let took = SpanTime(span.duration).to_string();
        let current = current == Some(span.span_id);
        Self {
            id: span.span_id,
            name: span.name.clone(),
            service: span.service.clone(),
            class,
            depth: row.depth(),
            depth_class: Self::DEPTH_CLASSES[row.depth().min(Self::DEPTH_CAP)],
            error: span.is_error(),
            tag: Self::tag(span),
            parent_missing: row.parent_missing(),
            href: if current {
                params.close_span()
            } else {
                params.select_span(span.span_id)
            },
            current,
            critical: row.on_critical_path(),
            toggle: Toggle::new(trace, row, params, collapsed),
            offset: format!("+{}", SpanTime(offset)),
            bar: Bar::new(offset, span.duration, total, class, span.is_error(), &took),
            took,
        }
    }

    /// What the span calls, where its attributes say so: the database (`db.system.name`, or
    /// the older `db.system`; PostgreSQL as `pg`) or a process (`process.command`: `exec`).
    fn tag(span: &Span) -> Option<String> {
        let attributes: Map<String, Value> =
            serde_json::from_str(span.attributes.as_deref()?).ok()?;
        let text = |key: &str| attributes.get(key).and_then(Value::as_str);
        if let Some(system) = text("db.system.name").or_else(|| text("db.system")) {
            let tag = match system {
                "postgresql" => "pg",
                short if !short.is_empty() && short.chars().count() <= Self::TAG_MAX => short,
                _ => "db",
            };
            return Some(tag.to_owned());
        }
        attributes
            .contains_key("process.command")
            .then(|| "exec".to_owned())
    }
}

/// The toggle of a span with rows under it: a link that collapses it when it is expanded and
/// expands it when it is collapsed, the other collapsed spans kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toggle {
    /// The trace with this span collapsed or expanded.
    pub href: String,
    /// Its rows are shown.
    pub expanded: bool,
    /// `Collapse documents.info`, `Expand documents.info`.
    pub label: String,
    /// How many rows collapsing it hides, all the way down: the `+N` of a collapsed span.
    pub hidden: usize,
}

impl Toggle {
    /// The toggle of `row` of `trace` with `collapsed` collapsed; `None` when nothing is under
    /// it.
    fn new(
        trace: &Trace,
        row: &TraceSpan,
        params: &TraceParams,
        collapsed: &HashSet<SpanId>,
    ) -> Option<Self> {
        if !row.has_children() {
            return None;
        }
        let id = row.span().span_id;
        let expanded = !collapsed.contains(&id);
        let mut toggled = collapsed.clone();
        if expanded {
            toggled.insert(id);
        } else {
            toggled.remove(&id);
        }
        Some(Self {
            href: params.collapse(trace, &toggled),
            expanded,
            label: format!(
                "{} {}",
                if expanded { "Collapse" } else { "Expand" },
                row.span().name
            ),
            hidden: row.descendants(),
        })
    }

    /// `9 spans`: what is hidden, as said to a screen reader.
    pub fn hidden_spans(&self) -> String {
        Counted(self.hidden, "span").to_string()
    }
}

/// Where a bar's duration label goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelSide {
    /// Inside the bar, from its start: the bar is wide.
    Inside,
    /// Before the bar, ending at its start: the bar ends near the right edge.
    Left,
    /// After the bar.
    Right,
}

impl LabelSide {
    fn as_str(self) -> &'static str {
        match self {
            LabelSide::Inside => "inside",
            LabelSide::Left => "left",
            LabelSide::Right => "right",
        }
    }
}

/// A span's bar on the trace's axis, in thousandths of it ([`Bar::AXIS`]), with its duration
/// label; drawn as an SVG the row's width wide.
#[derive(Debug, Clone, PartialEq)]
pub struct Bar {
    left: f64,
    width: f64,
    side: LabelSide,
    label: String,
    class: &'static str,
    error: bool,
}

impl Bar {
    /// The axis, from the trace's start to its end.
    pub const AXIS: f64 = 1000.0;
    /// The narrowest bar, so a span of no visible length still has one.
    const MIN_WIDTH: f64 = 3.0;
    /// Wider than this, the label goes inside the bar.
    const INSIDE_OVER: f64 = 550.0;
    /// A bar ending past this has its label before it.
    const LEFT_PAST: f64 = 780.0;
    /// The row's and bar's height in px, and where the bar starts.
    const HEIGHT: u32 = 30;
    const BAR_Y: u32 = 8;
    const BAR_HEIGHT: u32 = 14;

    /// The bar of a span starting `offset` into a trace of `total` and taking `duration`.
    fn new(
        offset: Duration,
        duration: Duration,
        total: Duration,
        class: &'static str,
        error: bool,
        label: &str,
    ) -> Self {
        let (left, width) = Self::place(offset, duration, total, Self::MIN_WIDTH);
        let side = if width > Self::INSIDE_OVER {
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
            label: label.to_owned(),
            class,
            error,
        }
    }

    /// Where a span starting `offset` into a trace of `total` and taking `duration` is drawn:
    /// its start and width in thousandths of the axis, to a tenth, the width at least
    /// `min_width` and the whole inside the axis. Everything starts at 0 on an axis of no
    /// length.
    fn place(offset: Duration, duration: Duration, total: Duration, min_width: f64) -> (f64, f64) {
        let share = |d: Duration| {
            if total.is_zero() {
                0.0
            } else {
                d.as_secs_f64() / total.as_secs_f64() * Self::AXIS
            }
        };
        let width = share(duration).clamp(min_width, Self::AXIS);
        let left = share(offset).clamp(0.0, Self::AXIS - width);
        let round = |x: f64| (x * 10.0).round() / 10.0;
        (round(left), round(width))
    }

    /// Thousandths of the axis as a percentage of the row.
    fn percent(x: f64) -> Percent {
        Percent(x / Self::AXIS * 100.0)
    }

    /// Where the label is anchored, and its offset from there in px.
    fn label_at(&self) -> (Percent, i32) {
        match self.side {
            LabelSide::Inside => (Self::percent(self.left), 8),
            LabelSide::Left => (Self::percent(self.left), -6),
            LabelSide::Right => (Self::percent(self.left + self.width), 6),
        }
    }
}

/// A percentage as an SVG attribute: `27.03%`.
struct Percent(f64);

impl fmt::Display for Percent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.2}%", self.0)
    }
}

/// The bar, its axis's quarter lines and its label; an error span's bar is ringed in red with a
/// gap of the row's colour, as the mockup draws it: two strokes under the fill.
impl fmt::Display for Bar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let h = Self::HEIGHT;
        write!(
            f,
            r#"<svg class="wf-bar" width="100%" height="{h}" aria-hidden="true" focusable="false">"#
        )?;
        for quarter in [25, 50, 75] {
            write!(
                f,
                r#"<line class="wf-grid" x1="{quarter}%" y1="0" x2="{quarter}%" y2="{h}"></line>"#
            )?;
        }
        let rect = |f: &mut fmt::Formatter<'_>, class: &str| {
            write!(
                f,
                r#"<rect class="{class}" x="{x}" y="{y}" width="{w}" height="{bh}" rx="3"></rect>"#,
                x = Self::percent(self.left),
                y = Self::BAR_Y,
                w = Self::percent(self.width),
                bh = Self::BAR_HEIGHT,
            )
        };
        if self.error {
            rect(f, "wf-ring")?;
            rect(f, "wf-gap")?;
        }
        rect(f, &format!("wf-fill {}", self.class))?;
        let (x, dx) = self.label_at();
        write!(
            f,
            r#"<text class="wf-took wf-took-{side}" x="{x}" dx="{dx}" y="50%">{label}</text></svg>"#,
            side = self.side.as_str(),
            label = Escaped(&self.label),
        )
    }
}

/// The label is escaped by `Display`; everything else is numbers and fixed classes.
impl askama::filters::HtmlSafe for Bar {}

/// Every span read as one short line on the trace's axis, in row order, above the rows.
#[derive(Debug, Clone, PartialEq)]
pub struct Minimap {
    lines: Vec<MiniLine>,
}

/// One span's line on the minimap, in thousandths of the axis.
#[derive(Debug, Clone, PartialEq)]
struct MiniLine {
    left: f64,
    width: f64,
    /// The service's colour class, or red for a span in error.
    class: &'static str,
}

impl Minimap {
    /// Each span's slot, px: a 2 px line and a 1 px gap.
    const PITCH: u32 = 3;
    const LINE: u32 = 2;
    /// Above the first line and below the last.
    const PAD: u32 = 3;
    /// The tallest it is drawn; a longer trace's lines are squeezed into it.
    const MAX_HEIGHT: u32 = 120;
    /// The narrowest line.
    const MIN_WIDTH: f64 = 4.0;

    fn new(trace: &Trace, palette: &ServicePalette) -> Self {
        let total = trace.duration();
        let lines = trace
            .spans()
            .iter()
            .map(|row| {
                let span = row.span();
                let (left, width) =
                    Bar::place(trace.offset(row), span.duration, total, Self::MIN_WIDTH);
                MiniLine {
                    left,
                    width,
                    class: if span.is_error() {
                        "wf-mini-error"
                    } else {
                        palette.class(&span.service)
                    },
                }
            })
            .collect();
        Self { lines }
    }

    /// The height of its viewBox: a slot per span and the padding.
    fn view_height(&self) -> u32 {
        let n = u32::try_from(self.lines.len()).unwrap_or(u32::MAX);
        n.saturating_mul(Self::PITCH).saturating_add(2 * Self::PAD)
    }
}

/// One SVG stretched to the card's width, its lines as rects in viewBox units.
impl fmt::Display for Minimap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let view = self.view_height();
        write!(
            f,
            r#"<svg class="wf-minimap" viewBox="0 0 {axis} {view}" preserveAspectRatio="none" width="100%" height="{h}" aria-hidden="true" focusable="false">"#,
            axis = Bar::AXIS,
            h = view.min(Self::MAX_HEIGHT),
        )?;
        for (i, line) in (0u32..).zip(&self.lines) {
            write!(
                f,
                r#"<rect class="wf-mini {class}" x="{x}" y="{y}" width="{w}" height="{lh}"></rect>"#,
                class = line.class,
                x = line.left,
                y = Self::PAD + i * Self::PITCH,
                w = line.width,
                lh = Self::LINE,
            )?;
        }
        f.write_str("</svg>")
    }
}

/// Numbers and fixed classes only.
impl askama::filters::HtmlSafe for Minimap {}

#[cfg(test)]
mod tests;
