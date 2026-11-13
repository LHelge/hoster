//! The traffic charts (ADR-0062 §9, ADR-0054): [`StackedBars`] of responses by status class,
//! a [`LineChart`] of the p50 and p95 response times, and the [`Sparkline`] of a table row or
//! a stack card. They are drawn on the server as SVG: the CSP allows no inline styles and no
//! script, so every coordinate is an attribute computed here, and every colour and stroke
//! comes from a class in `assets/hoster.css` (`chart-*`, `spark-*`).
//!
//! A chart takes plain numbers, one entry per bar of a [`Timeline`], oldest first; it does not
//! read history itself. A page builds one from the bars of a
//! [`ScopeHistory`](crate::stats::ScopeHistory):
//!
//! ```ignore
//! let timeline = Timeline::from(range);
//! let statuses = StackedBars::new(timeline, scope.bars.iter().map(|b| b.traffic.statuses));
//! let latency = LineChart::new(timeline, scope.bars.iter().map(|b| Percentiles::from(&b.traffic.latency)));
//! let spark = Sparkline::new(scope.bars.iter().map(|b| b.traffic.requests()), tone);
//! ```
//!
//! and a template places them, the legends in the card head beside the heading:
//!
//! ```html
//! <div class="card-head"><h2>Responses by status</h2>{{ statuses.legend() }}</div>
//! <div class="card-body">{{ statuses }}</div>
//! ```
//!
//! Charts stretch to their card's width (a fixed viewBox with `preserveAspectRatio="none"`,
//! strokes kept by `vector-effect`); the axis labels are HTML beside the plot, so they do not
//! stretch with it. Each chart is one `role="img"` whose `aria-label` says what it shows in
//! words, and each bar carries a `<title>` as the tooltip that needs no script. The x axis is
//! relative (`-24h … now`), so no time zone is involved.

use std::fmt::{self, Write as _};
use std::time::Duration;

use super::ui::{Count, Escaped, Tone};
use crate::stats::{Elapsed, Histogram, Range, StatusClass, StatusCounts};

/// The plot's viewBox width; bars and points are laid out in these units.
const WIDTH: f64 = 600.0;
/// The plot's viewBox height, and its rendered height in px.
const HEIGHT: f64 = 180.0;
/// Intervals on the y axis: four labels, from 0 to the rounded maximum.
const Y_INTERVALS: u32 = 3;

/// The time a chart spans: `bars` bars of `bar` each, the newest ending now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeline {
    bars: usize,
    bar: Duration,
}

impl Timeline {
    /// `bars` bars of `bar` each (at least one).
    pub fn new(bars: usize, bar: Duration) -> Self {
        Self {
            bars: bars.max(1),
            bar,
        }
    }

    /// How far back the chart reaches.
    fn length(&self) -> Duration {
        self.bar * u32::try_from(self.bars).unwrap_or(u32::MAX)
    }

    /// The width of one bar's slot, in viewBox units.
    fn slot(&self) -> f64 {
        WIDTH / self.bars as f64
    }

    /// When bar `i` (0 the oldest) starts and ends: `-10h` to `-9h`.
    fn span_of(&self, i: usize) -> (Offset, Offset) {
        let back = |n: usize| Offset(self.bar * u32::try_from(n).unwrap_or(u32::MAX));
        (back(self.bars - i), back(self.bars - i - 1))
    }

    /// The x axis labels, oldest first and ending with `now`: the most evenly spaced set (of
    /// about four intervals) whose labels are each one round unit, `-24h -18h -12h -6h now`.
    fn ticks(&self) -> Vec<Offset> {
        let length = self.length();
        let labels = |k: u32| (0..=k).rev().map(move |i| Offset(length * i / k));
        [4, 3, 6, 5, 2, 7]
            .into_iter()
            .find(|&k| (length / k) * k == length && labels(k).all(|offset| offset.is_round()))
            .map_or_else(|| labels(1).collect(), |k| labels(k).collect())
    }

    /// [`Timeline::ticks`] as the x axis writes them.
    fn labels(&self) -> Vec<String> {
        self.ticks().iter().map(ToString::to_string).collect()
    }

    /// `items`, one per bar: a shorter series leaves the oldest bars empty, a longer one loses
    /// its oldest entries.
    fn align<T: Default>(&self, items: impl IntoIterator<Item = T>) -> Vec<T> {
        let mut items: Vec<T> = items.into_iter().collect();
        if items.len() > self.bars {
            items.drain(..items.len() - self.bars);
        }
        let mut aligned: Vec<T> = (items.len()..self.bars).map(|_| T::default()).collect();
        aligned.append(&mut items);
        aligned
    }
}

/// A range's bar plan: `24h` is 24 bars of 1 h.
impl From<Range> for Timeline {
    fn from(range: Range) -> Self {
        Timeline::new(range.bars(), range.bar())
    }
}

/// A length of time in its two largest units, as the axes write it: `45m`, `1h 30m`, `24h`,
/// `7d`. Hours run up to two days, so a day reads `24h`, as in the mockups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span(Duration);

impl Span {
    /// Its units, largest first, without the zero ones after the first.
    fn units(&self) -> [(u64, &'static str); 2] {
        let secs = self.0.as_secs();
        if secs >= 2 * 86_400 {
            [(secs / 86_400, "d"), (secs % 86_400 / 3_600, "h")]
        } else if secs >= 3_600 {
            [(secs / 3_600, "h"), (secs % 3_600 / 60, "m")]
        } else if secs >= 60 {
            [(secs / 60, "m"), (secs % 60, "s")]
        } else {
            [(secs, "s"), (0, "")]
        }
    }

    /// Whether it is a whole number of its largest unit.
    fn is_round(&self) -> bool {
        self.units()[1].0 == 0
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [(big, big_unit), (small, small_unit)] = self.units();
        write!(f, "{big}{big_unit}")?;
        if small > 0 {
            write!(f, " {small}{small_unit}")?;
        }
        Ok(())
    }
}

/// A moment relative to now, as the x axis writes it: `-6h`, `-1h 30m`, `now`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Offset(Duration);

impl Offset {
    fn is_round(&self) -> bool {
        Span(self.0).is_round()
    }
}

impl fmt::Display for Offset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_zero() {
            f.write_str("now")
        } else {
            write!(f, "-{}", Span(self.0))
        }
    }
}

/// A coordinate to two decimals, without trailing zeros: `12.5`, `180`.
#[derive(Debug, Clone, Copy)]
pub(super) struct Num(pub(super) f64);

impl Num {
    /// The value as written: rounded to two decimals, so stacked segments meet exactly.
    fn rounded(self) -> f64 {
        (self.0 * 100.0).round() / 100.0
    }
}

impl fmt::Display for Num {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rounded = self.rounded();
        let text = format!("{:.2}", if rounded == 0.0 { 0.0 } else { rounded });
        f.write_str(text.trim_end_matches('0').trim_end_matches('.'))
    }
}

/// A y axis from 0 to a rounded maximum in [`Y_INTERVALS`] equal steps.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Scale {
    step: f64,
}

impl Scale {
    /// The multipliers of a power of ten a step may be.
    const NICE: [f64; 10] = [1.0, 1.5, 2.0, 2.5, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0];

    /// The smallest round scale that holds `max`, with steps of at least `unit` and, below ten
    /// units, whole multiples of it (no 2.5 requests). An empty or invalid `max` gets steps
    /// of one unit, so an empty chart keeps its axis.
    fn new(max: f64, unit: f64) -> Self {
        let raw = max / f64::from(Y_INTERVALS);
        let mut step = if raw.is_finite() && raw > 0.0 {
            let power = 10f64.powf(raw.log10().floor());
            let multiple = Self::NICE
                .into_iter()
                .find(|&m| m * power >= raw * (1.0 - 1e-9))
                .unwrap_or(10.0);
            multiple * power
        } else {
            unit
        };
        if step < 10.0 * unit {
            step = (step / unit - 1e-9).ceil().max(1.0) * unit;
        }
        Self { step }
    }

    fn top(&self) -> f64 {
        self.step * f64::from(Y_INTERVALS)
    }

    /// The y coordinate of `value`: 0 at the bottom of the plot, the maximum at its top.
    fn y(&self, value: f64) -> f64 {
        HEIGHT - (value / self.top()).clamp(0.0, 1.0) * HEIGHT
    }

    /// The tick values, top first.
    fn ticks(&self) -> impl Iterator<Item = f64> + '_ {
        (0..=Y_INTERVALS).rev().map(|i| self.step * f64::from(i))
    }
}

/// A count as the y axis writes it: `750`, `2.5k`, `90k`, `1.2M`.
#[derive(Debug, Clone, Copy)]
struct Compact(f64);

impl fmt::Display for Compact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (value, suffix) = match self.0 {
            v if v >= 1e6 => (v / 1e6, "M"),
            v if v >= 1e3 => (v / 1e3, "k"),
            v => (v, ""),
        };
        let text = format!("{value:.1}");
        write!(
            f,
            "{}{suffix}",
            text.trim_end_matches('0').trim_end_matches('.')
        )
    }
}

/// The plot frame both charts share: the y labels, the grid and axis lines, and the x labels
/// around what `plot` draws inside the SVG.
struct Frame<'a> {
    class: &'a str,
    /// `img`, or `group` where the bars are links (an image's children are not reachable).
    role: &'a str,
    label: &'a str,
    y_labels: Vec<String>,
    /// The x axis labels, oldest first.
    x_labels: Vec<String>,
}

impl Frame<'_> {
    fn write(
        &self,
        f: &mut fmt::Formatter<'_>,
        plot: impl FnOnce(&mut fmt::Formatter<'_>) -> fmt::Result,
    ) -> fmt::Result {
        write!(
            f,
            r#"<div class="chart {}" role="{}" aria-label="{}"><div class="chart-y" aria-hidden="true">"#,
            self.class,
            self.role,
            Escaped(self.label)
        )?;
        for label in &self.y_labels {
            write!(f, "<span>{label}</span>")?;
        }
        let (w, h) = (Num(WIDTH), Num(HEIGHT));
        write!(
            f,
            r#"</div><svg class="chart-plot" width="{w}" height="{h}" viewBox="0 0 {w} {h}" preserveAspectRatio="none">"#
        )?;
        for i in 0..Y_INTERVALS {
            let y = Num(HEIGHT * f64::from(i) / f64::from(Y_INTERVALS));
            write!(
                f,
                r#"<line class="chart-grid" x1="0" y1="{y}" x2="{w}" y2="{y}" vector-effect="non-scaling-stroke"></line>"#
            )?;
        }
        plot(f)?;
        write!(
            f,
            r#"<line class="chart-axis" x1="0" y1="{h}" x2="{w}" y2="{h}" vector-effect="non-scaling-stroke"></line></svg><div class="chart-x" aria-hidden="true">"#
        )?;
        for label in &self.x_labels {
            write!(f, "<span>{}</span>", Escaped(label))?;
        }
        f.write_str("</div></div>")
    }
}

/// The status classes a bar stacks, bottom to top, with their CSS classes; 1xx and requests
/// without a response share the neutral `other` on top.
const SEGMENTS: [(&str, &[StatusClass]); 5] = [
    ("chart-2xx", &[StatusClass::Success]),
    ("chart-3xx", &[StatusClass::Redirection]),
    ("chart-4xx", &[StatusClass::ClientError]),
    ("chart-5xx", &[StatusClass::ServerError]),
    (
        "chart-other",
        &[StatusClass::Informational, StatusClass::NoResponse],
    ),
];

/// The legend names of [`SEGMENTS`].
const SEGMENT_NAMES: [&str; 5] = ["2xx", "3xx", "4xx", "5xx", "other"];

/// A stacked bar chart of responses by status class, one bar per bucket of its [`Timeline`]:
/// 2xx, 3xx, 4xx and 5xx from the bottom in the status colours, anything else (1xx, no
/// response) in a neutral tone on top. Its [`legend`](Self::legend) shows each class's total.
#[derive(Debug, Clone, PartialEq)]
pub struct StackedBars {
    timeline: Timeline,
    bars: Vec<StatusCounts>,
    subject: Option<String>,
}

impl StackedBars {
    /// The chart of `bars`, one per bar of `timeline`, oldest first.
    pub fn new(timeline: Timeline, bars: impl IntoIterator<Item = StatusCounts>) -> Self {
        Self {
            timeline,
            bars: timeline.align(bars),
            subject: None,
        }
    }

    /// The same chart, saying in its label whose traffic it is (`notes`, `all stacks`).
    pub fn of(mut self, subject: impl fmt::Display) -> Self {
        self.subject = Some(subject.to_string());
        self
    }

    /// The legend: each class's total over the chart, `other` only when there is some.
    pub fn legend(&self) -> Legend {
        let totals = self.segment_totals();
        Legend(
            SEGMENT_NAMES
                .into_iter()
                .zip(SEGMENTS)
                .zip(totals)
                .filter(|((name, _), total)| *name != "other" || *total > 0)
                .map(|((name, (class, _)), total)| LegendKey {
                    class,
                    line: false,
                    text: format!("{name} {}", Count(total)),
                })
                .collect(),
        )
    }

    /// A bar's count of each segment, in [`SEGMENTS`] order.
    fn segments(counts: &StatusCounts) -> [u64; 5] {
        SEGMENTS.map(|(_, classes)| classes.iter().map(|&class| counts.get(class)).sum())
    }

    fn segment_totals(&self) -> [u64; 5] {
        self.bars.iter().fold([0; 5], |mut totals, bar| {
            for (total, n) in totals.iter_mut().zip(Self::segments(bar)) {
                *total += n;
            }
            totals
        })
    }

    /// The bar with the most of `count`, if any has some.
    fn peak(&self, count: impl Fn(&StatusCounts) -> u64) -> Option<(usize, u64)> {
        self.bars
            .iter()
            .map(count)
            .enumerate()
            .filter(|&(_, n)| n > 0)
            .rev()
            .max_by_key(|&(_, n)| n)
    }

    /// The `aria-label`: what, over how long, the totals and where the peaks were.
    fn label(&self) -> String {
        let mut label = String::from("Responses by status class");
        if let Some(subject) = &self.subject {
            let _ = write!(label, " for {subject}");
        }
        let _ = write!(label, " over the last {}: ", Span(self.timeline.length()));
        let Some((busiest, most)) = self.peak(StatusCounts::total) else {
            label.push_str("no requests");
            return label;
        };
        let totals = self.segment_totals();
        let _ = write!(label, "{} requests", Count(totals.iter().sum()));
        for (name, total) in SEGMENT_NAMES.into_iter().zip(totals) {
            if name != "other" || total > 0 {
                let _ = write!(label, ", {name} {}", Count(total));
            }
        }
        let (from, to) = self.timeline.span_of(busiest);
        let _ = write!(label, "; busiest {from} to {to} with {}", Count(most));
        if let Some((bar, n)) = self.peak(|c| c.get(StatusClass::ServerError)) {
            let (from, to) = self.timeline.span_of(bar);
            let _ = write!(label, "; most 5xx {from} to {to} with {}", Count(n));
        }
        label
    }
}

impl fmt::Display for StackedBars {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let max = self.bars.iter().map(StatusCounts::total).max().unwrap_or(0);
        let scale = Scale::new(max as f64, 1.0);
        let frame = Frame {
            class: "chart-bars",
            role: "img",
            label: &self.label(),
            y_labels: scale
                .ticks()
                .map(|v| {
                    if v == 0.0 {
                        "0".to_owned()
                    } else {
                        Compact(v).to_string()
                    }
                })
                .collect(),
            x_labels: self.timeline.labels(),
        };
        let slot = self.timeline.slot();
        let gap = slot * 0.2;
        frame.write(f, |f| {
            for (i, bar) in self.bars.iter().enumerate() {
                let (from, to) = self.timeline.span_of(i);
                let segments = Self::segments(bar);
                write!(f, r#"<g class="chart-bar"><title>{from} to {to}: "#)?;
                match bar.total() {
                    0 => f.write_str("no requests")?,
                    total => write!(f, "{} requests", Count(total))?,
                }
                for (name, n) in SEGMENT_NAMES.into_iter().zip(segments) {
                    if n > 0 {
                        write!(f, ", {name} {}", Count(n))?;
                    }
                }
                write!(
                    f,
                    r#"</title><rect class="chart-hit" x="{}" y="0" width="{}" height="{}"></rect>"#,
                    Num(slot * i as f64),
                    Num(slot),
                    Num(HEIGHT)
                )?;
                let mut below = 0u64;
                for ((class, _), n) in SEGMENTS.into_iter().zip(segments) {
                    if n == 0 {
                        continue;
                    }
                    let bottom = scale.y(below as f64);
                    below += n;
                    let top = scale.y(below as f64);
                    write!(
                        f,
                        r#"<rect class="chart-seg {class}" x="{}" y="{}" width="{}" height="{}" vector-effect="non-scaling-stroke"></rect>"#,
                        Num(slot * i as f64 + gap / 2.0),
                        Num(top),
                        Num(slot - gap),
                        Num(Num(bottom).rounded() - Num(top).rounded()),
                    )?;
                }
                f.write_str("</g>")?;
            }
            Ok(())
        })
    }
}

/// Labels and numbers only; the subject is escaped by `Display`.
impl askama::filters::HtmlSafe for StackedBars {}

/// The p50 and p95 response times of one bucket; `None` where it has no samples.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Percentiles {
    pub p50: Option<Duration>,
    pub p95: Option<Duration>,
}

impl From<&Histogram> for Percentiles {
    fn from(latency: &Histogram) -> Self {
        Self {
            p50: latency.p50(),
            p95: latency.p95(),
        }
    }
}

/// A line chart of the p95 (yellow) and p50 (teal) response times over a [`Timeline`], one
/// point per bucket at the middle of its bar. A bucket without samples is drawn at zero, so
/// the lines run the whole timeline and quiet stretches lie on the axis; its tooltip says "no
/// samples". A series without any samples draws no line.
#[derive(Debug, Clone, PartialEq)]
pub struct LineChart {
    timeline: Timeline,
    points: Vec<Percentiles>,
    subject: Option<String>,
}

impl LineChart {
    /// The smallest y axis step: 0.1 ms, the precision [`Elapsed`] writes.
    const UNIT: f64 = 0.000_1;

    /// The chart of `points`, one per bar of `timeline`, oldest first.
    pub fn new(timeline: Timeline, points: impl IntoIterator<Item = Percentiles>) -> Self {
        Self {
            timeline,
            points: timeline.align(points),
            subject: None,
        }
    }

    /// The same chart, saying in its label whose traffic it is.
    pub fn of(mut self, subject: impl fmt::Display) -> Self {
        self.subject = Some(subject.to_string());
        self
    }

    /// The legend: a line key for p50 and p95.
    pub fn legend(&self) -> Legend {
        Legend(
            [("chart-p50", "p50"), ("chart-p95", "p95")]
                .map(|(class, text)| LegendKey {
                    class,
                    line: true,
                    text: text.to_owned(),
                })
                .into(),
        )
    }

    /// The highest value of one percentile and the bar it is in.
    fn peak(&self, pick: impl Fn(&Percentiles) -> Option<Duration>) -> Option<(usize, Duration)> {
        self.points
            .iter()
            .enumerate()
            .filter_map(|(i, p)| pick(p).map(|d| (i, d)))
            .rev()
            .max_by_key(|&(_, d)| d)
    }

    fn label(&self) -> String {
        let mut label = String::from("Response time");
        if let Some(subject) = &self.subject {
            let _ = write!(label, " for {subject}");
        }
        let _ = write!(label, " over the last {}: ", Span(self.timeline.length()));
        let p50 = self.peak(|p| p.p50);
        let p95 = self.peak(|p| p.p95);
        if p50.is_none() && p95.is_none() {
            label.push_str("no samples");
            return label;
        }
        let mut parts = Vec::new();
        for (name, peak) in [("p50", p50), ("p95", p95)] {
            if let Some((bar, d)) = peak {
                let (from, to) = self.timeline.span_of(bar);
                parts.push(format!("{name} up to {} ({from} to {to})", Elapsed(d)));
            }
        }
        label.push_str(&parts.join(", "));
        label
    }

    /// The polylines of one percentile: one per run of buckets with samples.
    fn lines(
        &self,
        f: &mut fmt::Formatter<'_>,
        class: &str,
        scale: &Scale,
        pick: impl Fn(&Percentiles) -> Option<Duration>,
    ) -> fmt::Result {
        if self.points.iter().all(|point| pick(point).is_none()) {
            return Ok(());
        }
        let slot = self.timeline.slot();
        write!(f, r#"<polyline class="chart-line {class}" points=""#)?;
        for (i, point) in self.points.iter().enumerate() {
            // A bucket without samples sits on the axis.
            let value = pick(point).unwrap_or_default().as_secs_f64();
            let sep = if i == 0 { "" } else { " " };
            write!(
                f,
                "{sep}{},{}",
                Num(slot * (i as f64 + 0.5)),
                Num(scale.y(value))
            )?;
        }
        f.write_str(r#"" vector-effect="non-scaling-stroke"></polyline>"#)
    }
}

impl fmt::Display for LineChart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let max = self
            .points
            .iter()
            .flat_map(|p| [p.p50, p.p95])
            .flatten()
            .max()
            .unwrap_or_default();
        let scale = Scale::new(max.as_secs_f64(), Self::UNIT);
        let frame = Frame {
            class: "chart-lines",
            role: "img",
            label: &self.label(),
            y_labels: scale
                .ticks()
                .map(|v| {
                    if v == 0.0 {
                        "0".to_owned()
                    } else {
                        Elapsed(Duration::from_secs_f64(v)).to_string()
                    }
                })
                .collect(),
            x_labels: self.timeline.labels(),
        };
        let slot = self.timeline.slot();
        frame.write(f, |f| {
            for (i, point) in self.points.iter().enumerate() {
                let (from, to) = self.timeline.span_of(i);
                write!(f, r#"<g class="chart-bar"><title>{from} to {to}: "#)?;
                match (point.p50, point.p95) {
                    (None, None) => f.write_str("no samples")?,
                    (p50, p95) => write!(
                        f,
                        "p50 {}, p95 {}",
                        Elapsed::cell(p50),
                        Elapsed::cell(p95)
                    )?,
                }
                write!(
                    f,
                    r#"</title><rect class="chart-hit" x="{}" y="0" width="{}" height="{}"></rect></g>"#,
                    Num(slot * i as f64),
                    Num(slot),
                    Num(HEIGHT)
                )?;
            }
            self.lines(f, "chart-p95", &scale, |p| p.p95)?;
            self.lines(f, "chart-p50", &scale, |p| p.p50)
        })
    }
}

/// Labels and numbers only; the subject is escaped by `Display`.
impl askama::filters::HtmlSafe for LineChart {}

/// A chart's legend: a coloured key and a text per series, for the card head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Legend(Vec<LegendKey>);

#[derive(Debug, Clone, PartialEq, Eq)]
struct LegendKey {
    class: &'static str,
    /// A line key (the line chart's) rather than a square swatch.
    line: bool,
    text: String,
}

impl fmt::Display for Legend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(r#"<ul class="chart-legend">"#)?;
        for key in &self.0 {
            let shape = if key.line {
                "chart-key-line"
            } else {
                "chart-key-box"
            };
            write!(
                f,
                r#"<li><span class="chart-key {shape} {}" aria-hidden="true"></span>{}</li>"#,
                key.class,
                Escaped(&key.text)
            )?;
        }
        f.write_str("</ul>")
    }
}

impl askama::filters::HtmlSafe for Legend {}

/// A small line of requests over time for a table row or a stack card: 96 x 28, from zero at
/// the bottom to the series' maximum, decorative (`aria-hidden`; the row says the figures).
/// Its tone follows the stack: teal by default, peach when degraded, red when failed, muted
/// when stopped or not deployed ([`Tone::Neutral`]). No data, or only zeros, is a flat line
/// along the bottom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sparkline {
    values: Vec<u64>,
    tone: Tone,
}

impl Sparkline {
    const WIDTH: f64 = 96.0;
    const HEIGHT: f64 = 28.0;
    /// Room above and below for the stroke.
    const PAD: f64 = 1.5;

    /// The sparkline of `values`, oldest first.
    pub fn new(values: impl IntoIterator<Item = u64>, tone: Tone) -> Self {
        Self {
            values: values.into_iter().collect(),
            tone,
        }
    }

    fn points(&self) -> Vec<(f64, f64)> {
        let max = self.values.iter().copied().max().unwrap_or(0);
        let y = |v: u64| {
            let share = if max == 0 { 0.0 } else { v as f64 / max as f64 };
            Self::HEIGHT - Self::PAD - share * (Self::HEIGHT - 2.0 * Self::PAD)
        };
        match self.values.as_slice() {
            [] => vec![(0.0, y(0)), (Self::WIDTH, y(0))],
            [only] => vec![(0.0, y(*only)), (Self::WIDTH, y(*only))],
            values => {
                let step = Self::WIDTH / (values.len() - 1) as f64;
                values
                    .iter()
                    .enumerate()
                    .map(|(i, &v)| (step * i as f64, y(v)))
                    .collect()
            }
        }
    }
}

impl fmt::Display for Sparkline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (w, h) = (Num(Self::WIDTH), Num(Self::HEIGHT));
        write!(
            f,
            r#"<svg class="spark spark-{}" aria-hidden="true" width="{w}" height="{h}" viewBox="0 0 {w} {h}" preserveAspectRatio="none"><polyline points=""#,
            self.tone
        )?;
        for (n, (x, y)) in self.points().into_iter().enumerate() {
            let sep = if n == 0 { "" } else { " " };
            write!(f, "{sep}{},{}", Num(x), Num(y))?;
        }
        f.write_str(r#"" vector-effect="non-scaling-stroke"></polyline></svg>"#)
    }
}

/// Numbers only.
impl askama::filters::HtmlSafe for Sparkline {}

mod severity;

pub use severity::{Buckets, SeverityBars, SeverityCounts};

#[cfg(test)]
mod tests;
