//! The duration scatter above the trace list (epic qnh, `docs/design/Telemetry.dc.html`): one
//! dot per trace root over the range, its x when it started and its y its duration on a log
//! scale ([`LogScale`]: 1 ms up to the slowest, at least 1 s). Every root of the range and stack
//! is a muted dot, or red in error, as the store samples them (`LogStore::trace_points`, at most
//! [`TraceQuery::MAX_POINTS`](crate::store::TraceQuery::MAX_POINTS)); the traces listed below are drawn on top in the accent, each a
//! link that picks it, the picked one larger. It follows the range and the stack, not the list's
//! filters, so the list's filters show where their traces sit among all of them; nothing is
//! brushed, the list's duration filter is what narrows.
//!
//! The CSP allows no inline styles and no script (ADR-0054, ADR-0060): the plot is an `<svg>`
//! in user units (px) without a `viewBox`, with `cx` in percent and `cy` and `r` in px, so it
//! scales with the card while dots stay round; the y labels are their own `<svg>` beside it,
//! since a log axis spaces them unevenly. [`Scatter`] holds the numbers, tested without
//! templates.

use std::fmt::{self, Write as _};
use std::ops::Range;
use std::time::{Duration, SystemTime};

use super::Links;
use crate::stats::Elapsed;
use crate::store::{MicroTimestamp, Timestamp, TraceId, TracePoints, TraceStatus, TraceSummary};
use crate::web::charts::{AxisDuration, LogScale, Num, Offset, Span};
use crate::web::ui::{Count, Escaped};

/// The scatter of one range, ready for the template.
#[derive(Debug)]
pub struct Scatter {
    /// How far back the x axis reaches: from the range's start to now.
    length: Duration,
    scale: LogScale,
    /// Every root the store gave, by start.
    background: Vec<Dot>,
    /// The traces listed, the picked one last so it is drawn on top.
    listed: Vec<Listed>,
    /// The roots of the range, sampled or not.
    matched: u64,
    /// The name and duration of the slowest root in error drawn.
    slowest_error: Option<(String, Duration)>,
}

/// Where a root sits, both as shares of the plot (0 to 1), and whether it failed.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Dot {
    /// From the range's start (0) to now (1).
    x: f64,
    /// From the scale's floor (0) to its top (1).
    y: f64,
    status: TraceStatus,
}

impl Dot {
    fn class(&self) -> &'static str {
        match self.status {
            TraceStatus::Ok => "ts-ok",
            TraceStatus::Error => "ts-error",
        }
    }

    /// The `cx` and `cy` attributes: x in percent of the plot, y in px from its top.
    fn centre(&self) -> (Num, Num) {
        (Num(self.x * 100.0), Num(Scatter::HEIGHT * (1.0 - self.y)))
    }
}

/// A trace of the list: its dot, and the link that picks it.
#[derive(Debug, PartialEq)]
struct Listed {
    dot: Dot,
    href: String,
    /// `Open trace GET /api/x, 1.9 s`.
    label: String,
    /// The trace picked.
    current: bool,
}

impl Scatter {
    /// The plot's height in px.
    const HEIGHT: f64 = 150.0;
    /// The y labels' width in px.
    const LABELS: f64 = 44.0;
    /// The radius of a root's dot, of a listed one and of the picked one, in px.
    const R: f64 = 3.0;
    const R_LISTED: f64 = 5.0;
    const R_CURRENT: f64 = 7.0;
    /// The x axis labels: four, from the range's start to now.
    const X_INTERVALS: u32 = 3;

    /// The scatter of `points` over `range` at `now`, with the traces of `listed` linked by
    /// `links` and `picked` marked; `None` when there is nothing to draw.
    pub(super) fn new(
        points: &TracePoints,
        listed: &[TraceSummary],
        links: &Links,
        picked: Option<TraceId>,
        range: &Range<Timestamp>,
        now: SystemTime,
    ) -> Option<Self> {
        if points.points.is_empty() && listed.is_empty() {
            return None;
        }
        let from = MicroTimestamp::from(range.start).as_micros();
        let to = MicroTimestamp::from(now).as_micros().max(from);
        let length = Duration::from_micros(u64::try_from(to - from).unwrap_or(0));
        let slowest = points
            .points
            .iter()
            .map(|p| p.duration)
            .chain(listed.iter().map(|t| t.duration))
            .max()
            .unwrap_or_default();
        let scale = LogScale::new(slowest);
        let dot = |start: MicroTimestamp, duration: Duration, status: TraceStatus| Dot {
            x: if to > from {
                ((start.as_micros() - from) as f64 / (to - from) as f64).clamp(0.0, 1.0)
            } else {
                1.0
            },
            y: scale.share(duration),
            status,
        };
        let background = points
            .points
            .iter()
            .map(|p| dot(p.start, p.duration, p.status))
            .collect();
        let mut listed_dots: Vec<Listed> = listed
            .iter()
            .map(|t| Listed {
                dot: dot(t.start, t.duration, t.status),
                href: links.picking(Some(t.trace_id)),
                label: format!("Open trace {}, {}", t.name, Elapsed(t.duration)),
                current: picked == Some(t.trace_id),
            })
            .collect();
        listed_dots.sort_by_key(|l| l.current);
        let slowest_error = points
            .points
            .iter()
            .filter(|p| p.status == TraceStatus::Error)
            .map(|p| (&p.name, p.duration))
            .chain(
                listed
                    .iter()
                    .filter(|t| t.status == TraceStatus::Error)
                    .map(|t| (&t.name, t.duration)),
            )
            .max_by_key(|&(_, duration)| duration)
            .map(|(name, duration)| (name.clone(), duration));
        Some(Self {
            length,
            scale,
            background,
            listed: listed_dots,
            matched: points.matched.max(listed.len() as u64),
            slowest_error,
        })
    }

    /// Whether the background is a sample of the range's roots.
    fn is_sampled(&self) -> bool {
        self.matched > self.background.len() as u64
    }

    /// The heading's note: `root spans · last 15m · log scale`, and how many are drawn of a
    /// sample.
    pub fn note(&self) -> String {
        let mut note = format!("root spans · last {} · log scale", Span(self.length));
        if self.is_sampled() {
            let _ = write!(
                note,
                " · {} of {} sampled",
                Count(self.background.len() as u64),
                Count(self.matched)
            );
        }
        note
    }

    /// The chart's accessible name: the range, the count and the slowest error.
    fn label(&self) -> String {
        let mut label = format!(
            "Trace durations of root spans over the last {}, on a log scale up to {}: {} {}",
            Span(self.length),
            AxisDuration(self.scale.top()),
            Count(self.matched),
            if self.matched == 1 { "trace" } else { "traces" }
        );
        if self.is_sampled() {
            let _ = write!(
                label,
                ", a sample of {} drawn",
                Count(self.background.len() as u64)
            );
        }
        match &self.slowest_error {
            Some((name, duration)) => {
                let _ = write!(label, "; slowest error {name}, {}", Elapsed(*duration));
            }
            None => label.push_str("; no errors"),
        }
        label
    }

    /// The y of `share` of the scale, in px from the plot's top.
    fn y(share: f64) -> Num {
        Num(Self::HEIGHT * (1.0 - share))
    }
}

impl fmt::Display for Scatter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (w, h) = (Num(Self::LABELS), Num(Self::HEIGHT));
        write!(
            f,
            r#"<div class="chart trace-scatter" role="group" aria-label="{}"><svg class="ts-y" width="{w}" height="{h}" aria-hidden="true" focusable="false">"#,
            Escaped(&self.label())
        )?;
        let ticks: Vec<_> = self.scale.ticks().collect();
        for tick in &ticks {
            let y = Self::y(self.scale.share(tick.0));
            write!(f, r#"<text x="{w}" y="{y}">{tick}</text>"#)?;
        }
        write!(
            f,
            r#"</svg><svg class="chart-plot ts-plot" width="100%" height="{h}">"#
        )?;
        for tick in ticks.iter().skip(1) {
            let y = Self::y(self.scale.share(tick.0));
            write!(
                f,
                r#"<line class="chart-grid" x1="0" y1="{y}" x2="100%" y2="{y}"></line>"#
            )?;
        }
        write!(
            f,
            r#"<line class="chart-axis" x1="0" y1="0" x2="0" y2="{h}"></line><line class="chart-axis" x1="0" y1="{h}" x2="100%" y2="{h}"></line><g aria-hidden="true">"#
        )?;
        for dot in &self.background {
            let (cx, cy) = dot.centre();
            write!(
                f,
                r#"<circle class="ts-dot {}" cx="{cx}%" cy="{cy}" r="{}"></circle>"#,
                dot.class(),
                Num(Self::R)
            )?;
        }
        f.write_str("</g>")?;
        for listed in &self.listed {
            let (cx, cy) = listed.dot.centre();
            let label = Escaped(&listed.label);
            let (current, r) = if listed.current {
                (r#" aria-current="true""#, Self::R_CURRENT)
            } else {
                ("", Self::R_LISTED)
            };
            write!(
                f,
                r#"<a class="ts-link" href="{}" aria-label="{label}"{current}><title>{label}</title><circle class="ts-listed {}" cx="{cx}%" cy="{cy}" r="{}"></circle></a>"#,
                Escaped(&listed.href),
                listed.dot.class(),
                Num(r)
            )?;
        }
        f.write_str(r#"</svg><div class="chart-x" aria-hidden="true">"#)?;
        for i in (0..=Self::X_INTERVALS).rev() {
            write!(
                f,
                "<span>{}</span>",
                Offset(self.length * i / Self::X_INTERVALS)
            )?;
        }
        f.write_str("</div></div>")
    }
}

/// Numbers, and labels and links escaped by `Display`.
impl askama::filters::HtmlSafe for Scatter {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{TracePoint, TraceQuery};

    const MINUTE: Duration = Duration::from_secs(60);

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    /// The last 15 minutes, as the tab's range ends: a second past now.
    fn range() -> Range<Timestamp> {
        let now = Timestamp::from(now());
        (now - 15 * MINUTE)..(now + Duration::from_secs(1))
    }

    fn at(ago: Duration) -> MicroTimestamp {
        MicroTimestamp::from(now() - ago)
    }

    fn id(n: u8) -> TraceId {
        TraceId::from([n; 16])
    }

    fn point(n: u8, ago: Duration, millis: u64, status: TraceStatus) -> TracePoint {
        TracePoint {
            trace_id: id(n),
            name: format!("GET /{n}"),
            start: at(ago),
            duration: Duration::from_millis(millis),
            status,
        }
    }

    fn summary(n: u8, ago: Duration, millis: u64, status: TraceStatus) -> TraceSummary {
        TraceSummary {
            trace_id: id(n),
            start: at(ago),
            name: format!("POST /api/{n}"),
            stack: "notes".into(),
            services: vec!["hoster".into()],
            spans: 2,
            duration: Duration::from_millis(millis),
            status,
            proxy_only: true,
        }
    }

    fn links() -> Links {
        Links {
            action: "/telemetry/traces",
            pairs: vec![("status", "error".to_owned()), ("slower", "1s".to_owned())],
            page: 0,
        }
    }

    fn scatter(points: Vec<TracePoint>, matched: u64, listed: &[TraceSummary]) -> Scatter {
        let points = TracePoints { points, matched };
        Scatter::new(&points, listed, &links(), Some(id(9)), &range(), now()).unwrap()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn the_log_scale_reaches_one_second_and_rounds_up_the_slowest() {
        let ms = Duration::from_millis;
        let top = |slowest| LogScale::new(slowest).top();
        assert_eq!(top(ms(5)), ms(1_000));
        assert_eq!(top(ms(1_000)), ms(1_000));
        assert_eq!(top(ms(1_910)), ms(2_000));
        assert_eq!(top(ms(2_100)), ms(5_000));
        assert_eq!(top(ms(7_000)), ms(10_000));
        let scale = LogScale::new(ms(1_000));
        assert!(close(scale.share(ms(1)), 0.0));
        assert!(close(scale.share(Duration::from_micros(200)), 0.0));
        assert!(close(scale.share(ms(10)), 1.0 / 3.0));
        assert!(close(scale.share(ms(100)), 2.0 / 3.0));
        assert!(close(scale.share(ms(1_000)), 1.0));
        assert!(close(scale.share(ms(9_000)), 1.0));
        let labels: Vec<String> = LogScale::new(ms(1_910))
            .ticks()
            .map(|t| t.to_string())
            .collect();
        assert_eq!(labels, ["1 ms", "10 ms", "100 ms", "1 s"]);
        let labels: Vec<String> = LogScale::new(ms(42_000))
            .ticks()
            .map(|t| t.to_string())
            .collect();
        assert_eq!(labels, ["1 ms", "10 ms", "100 ms", "1 s", "10 s"]);
    }

    #[test]
    fn roots_sit_by_start_and_log_duration() {
        let s = scatter(
            vec![
                point(1, 15 * MINUTE, 1, TraceStatus::Ok),
                point(2, 5 * MINUTE, 10, TraceStatus::Error),
                point(3, Duration::ZERO, 1_000, TraceStatus::Ok),
            ],
            3,
            &[],
        );
        let at: Vec<(f64, f64)> = s.background.iter().map(|d| (d.x, d.y)).collect();
        assert!(close(at[0].0, 0.0) && close(at[0].1, 0.0), "{at:?}");
        assert!(
            close(at[1].0, 2.0 / 3.0) && close(at[1].1, 1.0 / 3.0),
            "{at:?}"
        );
        assert!(close(at[2].0, 1.0) && close(at[2].1, 1.0), "{at:?}");
        let html = s.to_string();
        assert!(html.contains(r#"<circle class="ts-dot ts-ok" cx="0%" cy="150" r="3"></circle>"#));
        assert!(
            html.contains(
                r#"<circle class="ts-dot ts-error" cx="66.67%" cy="100" r="3"></circle>"#
            ),
            "{html}"
        );
        assert!(html.contains(r#"<circle class="ts-dot ts-ok" cx="100%" cy="0" r="3"></circle>"#));
        assert!(!html.contains("style"), "{html}");
    }

    #[test]
    fn the_axes_label_decades_and_four_times() {
        let html = scatter(vec![point(1, MINUTE, 1_910, TraceStatus::Error)], 1, &[]).to_string();
        // Up to 2 s: 1 ms at the bottom, 1 s at 3 / log10(2000) of the height from it.
        assert!(
            html.contains(r#"<svg class="ts-y" width="44" height="150" aria-hidden="true" focusable="false"><text x="44" y="150">1 ms</text><text x="44" y="104.56">10 ms</text><text x="44" y="59.12">100 ms</text><text x="44" y="13.68">1 s</text></svg>"#),
            "{html}"
        );
        assert_eq!(html.matches(r#"<line class="chart-grid""#).count(), 3);
        assert!(html.contains(
            r#"<line class="chart-grid" x1="0" y1="13.68" x2="100%" y2="13.68"></line>"#
        ));
        assert!(html.contains(
            r#"<div class="chart-x" aria-hidden="true"><span>-15m</span><span>-10m</span><span>-5m</span><span>now</span></div>"#
        ));
    }

    #[test]
    fn listed_traces_are_links_on_top_and_the_picked_one_is_larger() {
        let listed = [
            summary(9, 2 * MINUTE, 1_910, TraceStatus::Error),
            summary(4, MINUTE, 120, TraceStatus::Ok),
        ];
        let s = scatter(vec![point(1, MINUTE, 5, TraceStatus::Ok)], 3, &listed);
        // The picked one is drawn last, so nothing covers it.
        let order: Vec<bool> = s.listed.iter().map(|l| l.current).collect();
        assert_eq!(order, [false, true]);
        let html = s.to_string();
        let ok = html.find("Open trace POST /api/4, 120 ms").unwrap();
        let picked = html.find("Open trace POST /api/9, 1.9 s").unwrap();
        let background = html.find("ts-dot").unwrap();
        assert!(background < ok && ok < picked);
        let href = format!(
            "/telemetry/traces?status=error&#38;slower=1s&#38;trace={}",
            id(4)
        );
        assert!(
            html.contains(&format!(
                r#"<a class="ts-link" href="{href}" aria-label="Open trace POST /api/4, 120 ms"><title>Open trace POST /api/4, 120 ms</title><circle class="ts-listed ts-ok" cx="93.33%" cy=""#
            )),
            "{html}"
        );
        assert!(html.contains(r#"r="5"></circle></a>"#));
        assert!(html.contains(
            r#"aria-label="Open trace POST /api/9, 1.9 s" aria-current="true"><title>Open trace POST /api/9, 1.9 s</title><circle class="ts-listed ts-error" cx="86.67%" cy="0.91" r="7"></circle></a>"#
        ), "{html}");
        // The chart is a group, so its links are reachable; the background is hidden.
        assert!(html.starts_with(r#"<div class="chart trace-scatter" role="group" aria-label=""#));
        assert!(html.contains(r#"<g aria-hidden="true"><circle class="ts-dot"#));
    }

    #[test]
    fn the_label_states_range_count_and_slowest_error() {
        let listed = [summary(9, 2 * MINUTE, 2_400, TraceStatus::Error)];
        let s = scatter(
            vec![
                point(1, MINUTE, 1_910, TraceStatus::Error),
                point(2, MINUTE, 3_000, TraceStatus::Ok),
            ],
            2,
            &listed,
        );
        assert_eq!(
            s.label(),
            "Trace durations of root spans over the last 15m, on a log scale up to 5 s: 2 traces; slowest error POST /api/9, 2.4 s"
        );
        assert_eq!(s.note(), "root spans · last 15m · log scale");
        let s = scatter(vec![point(1, MINUTE, 20, TraceStatus::Ok)], 1, &[]);
        assert_eq!(
            s.label(),
            "Trace durations of root spans over the last 15m, on a log scale up to 1 s: 1 trace; no errors"
        );
    }

    #[test]
    fn a_sample_says_how_many_it_draws() {
        let points = (0..TraceQuery::MAX_POINTS)
            .map(|i| point(1, MINUTE * (i % 15), 10, TraceStatus::Ok))
            .collect();
        let s = scatter(points, 2_500, &[]);
        assert_eq!(s.background.len(), TraceQuery::MAX_POINTS as usize);
        assert_eq!(
            s.note(),
            "root spans · last 15m · log scale · 1,000 of 2,500 sampled"
        );
        assert!(
            s.label()
                .contains(": 2,500 traces, a sample of 1,000 drawn; no errors")
        );
        assert_eq!(s.to_string().matches("<circle ").count(), 1_000);
    }

    #[test]
    fn nothing_to_draw_is_no_chart() {
        let points = TracePoints::default();
        assert!(Scatter::new(&points, &[], &links(), None, &range(), now()).is_none());
    }
}
