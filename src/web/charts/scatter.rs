//! The duration scatter of the Telemetry page's Traces tab (epic qnh): [`DurationScatter`], one
//! dot per root span over the range, its start on the x axis and its duration on a log10 y
//! axis ([`LogScale`]) from 1 ms to the first power of ten at or above the slowest, at least
//! 1 s. The dots are what [`LogStore::trace_points`](crate::store::LogStore::trace_points)
//! read for the list's query: every matching trace, or [`TracePoints::CAP`] of them sampled
//! evenly by start, which the chart draws as given and says so; it never samples again. The
//! axis reaches the slowest matching root even when sampling left it out (the store reads its
//! duration for every root), but a trace's status is read for the points alone, so a sampled
//! chart names the slowest error *drawn*, not of the range.
//!
//! The background dots are muted for ok and red for error, decorative (`aria-hidden`). The
//! traces of the list's page are drawn on top, each a link that selects it as its row's does,
//! an accent ring that is hollow for ok and filled red for error, named for a screen reader
//! ("Open trace GET /a, 50.0 ms"); the selected one is larger, ringed in the text colour and
//! drawn last. Nothing
//! else is clickable: the background narrows nothing (no brushing), the list's duration filter
//! does.
//!
//! The CSP allows no inline styles (ADR-0054), so the plot is SVG whose geometry is attributes
//! (`cx`, `cy`, `r`) and whose colours are classes (`scatter-*` in `assets/hoster.css`). Unlike
//! the bars it has no viewBox: positions are in percent of the plot and radii in px
//! ([`Units::Percent`]), so the dots stay round as the plot stretches to its card. The x axis
//! is relative (`-15m -10m -5m now`), as the traffic charts', so no time zone is involved.

use std::fmt::{self, Write as _};
use std::time::Duration;

use super::{Frame, HEIGHT, KeyShape, Legend, LegendKey, LogScale, Num, Offset, Span, Units};
use crate::store::{MicroTimestamp, TraceId, TracePoint, TracePoints, TraceStatus, TraceSummary};
use crate::web::ui::{Count, Escaped, SpanTime};

/// The "Trace duration" chart above the trace list: the root spans of a range on a log scale,
/// the listed traces marked and linked. Built from the range's end (`now`) and length, the
/// store's [`TracePoints`] and the list's page ([`DurationScatter::list`]); rendered by
/// `Display`, with its [`legend`](Self::legend) and [`heading`](Self::heading) for the card
/// head. An empty range is an empty plot with its axes, 1 ms to 1 s.
#[derive(Debug, Clone, PartialEq)]
pub struct DurationScatter {
    now: MicroTimestamp,
    length: Duration,
    points: Vec<TracePoint>,
    /// How many traces match, which is more than the points when they were sampled.
    matching: u64,
    /// The slowest matching root, drawn or not.
    slowest: Duration,
    listed: Vec<Listed>,
}

/// A trace of the list's page, drawn on top.
#[derive(Debug, Clone, PartialEq)]
struct Listed {
    point: TracePoint,
    name: String,
    href: String,
    current: bool,
}

impl Listed {
    /// Its link's accessible name and tooltip: `Open trace GET /a, 50.0 ms`.
    fn label(&self) -> String {
        format!(
            "Open trace {}, {}",
            self.name,
            SpanTime(self.point.duration)
        )
    }
}

impl DurationScatter {
    /// The bottom of the y axis, in seconds: 1 ms. Faster roots lie on it.
    const BOTTOM: f64 = 1e-3;
    /// Powers of ten the y axis spans at least: 1 ms to 1 s.
    const MIN_DECADES: u32 = 3;
    /// Intervals on the x axis: four labels, the range's start to now.
    const X_INTERVALS: u32 = 3;
    /// The radius of a background dot, in px.
    const DOT: f64 = 3.0;
    /// The radius of a listed trace's mark inside its 2 px ring, in px: 10 px across.
    const MARK: f64 = 4.0;
    /// The radius of the selected trace's mark: 14 px across.
    const CURRENT: f64 = 6.0;
    /// The radius of a listed trace's transparent target around its mark, in px.
    const HIT: f64 = 10.0;

    /// The chart of `points` over the `length` before `now`.
    pub fn new(now: impl Into<MicroTimestamp>, length: Duration, points: TracePoints) -> Self {
        Self {
            now: now.into(),
            length,
            matching: points.matching,
            slowest: points.slowest,
            points: points.points,
            listed: Vec::new(),
        }
    }

    /// Marks `trace` of the list's page, linked to `href` (what its row's link selects), the
    /// selected trace when `current`.
    pub fn list(&mut self, trace: &TraceSummary, href: String, current: bool) {
        self.listed.push(Listed {
            point: TracePoint {
                trace_id: trace.trace_id,
                start: trace.start,
                duration: trace.duration,
                status: trace.status,
            },
            name: trace.name.clone(),
            href,
            current,
        });
    }

    /// The card head's words after the title: `root spans · last 15m · log scale`, and how
    /// many were drawn when the points were sampled (`· 1,000 of 18,204 drawn`).
    pub fn heading(&self) -> String {
        let mut heading = format!("root spans · last {} · log scale", Span(self.length));
        if self.sampled() {
            let _ = write!(
                heading,
                " · {} of {} drawn",
                Count(self.drawn()),
                Count(self.matching)
            );
        }
        heading
    }

    /// The legend: ok and error dots, and the ring of the listed traces.
    pub fn legend(&self) -> Legend {
        Legend(vec![
            LegendKey {
                class: "scatter-ok",
                shape: KeyShape::Dot,
                text: TraceStatus::Ok.to_string(),
            },
            LegendKey {
                class: "scatter-error",
                shape: KeyShape::Dot,
                text: TraceStatus::Error.to_string(),
            },
            LegendKey {
                class: "scatter-listed",
                shape: KeyShape::Ring,
                text: "listed below".to_owned(),
            },
        ])
    }

    /// The points the store returned.
    fn drawn(&self) -> u64 {
        u64::try_from(self.points.len()).unwrap_or(u64::MAX)
    }

    /// Whether the store sampled the points: more traces match than it returned.
    fn sampled(&self) -> bool {
        self.matching > self.drawn()
    }

    /// Every point drawn, the listed ones included.
    fn every_point(&self) -> impl Iterator<Item = &TracePoint> {
        self.points
            .iter()
            .chain(self.listed.iter().map(|listed| &listed.point))
    }

    fn scale(&self) -> LogScale {
        let slowest = self
            .every_point()
            .map(|point| point.duration)
            .chain([self.slowest])
            .max()
            .unwrap_or_default();
        LogScale::new(slowest.as_secs_f64(), Self::BOTTOM, Self::MIN_DECADES)
    }

    /// How long before now `start` is; none for a start past now.
    fn ago(&self, start: MicroTimestamp) -> Duration {
        let micros = self
            .now
            .as_micros()
            .saturating_sub(start.as_micros())
            .max(0);
        Duration::from_micros(u64::try_from(micros).unwrap_or(0))
    }

    /// Where `point` is, in percent of the plot from its left and its top.
    fn position(&self, scale: &LogScale, point: &TracePoint) -> (Num, Num) {
        let length = self.length.as_secs_f64();
        let share = if length > 0.0 {
            1.0 - (self.ago(point.start).as_secs_f64() / length).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let y = scale.y(point.duration.as_secs_f64()) / HEIGHT;
        (Num(share * 100.0), Num(y * 100.0))
    }

    /// The x axis labels, oldest first: the range's start, two between and `now`.
    fn x_labels(&self) -> Vec<String> {
        (0..=Self::X_INTERVALS)
            .rev()
            .map(|i| Offset(self.length * i / Self::X_INTERVALS).to_string())
            .collect()
    }

    /// The slowest trace in error drawn, with its name when it is listed: of the range unless
    /// the points were sampled, since only theirs and the listed traces' statuses are known.
    fn slowest_error(&self) -> Option<(&TracePoint, Option<&str>)> {
        let point = self
            .every_point()
            .filter(|point| point.status == TraceStatus::Error)
            .max_by_key(|point| (point.duration, point.start))?;
        let name = self
            .listed
            .iter()
            .find(|listed| listed.point.trace_id == point.trace_id)
            .map(|listed| listed.name.as_str());
        Some((point, name))
    }

    /// The `aria-label`: what, over how long, how many and the slowest error.
    fn label(&self) -> String {
        let mut label = format!(
            "Trace duration of root spans over the last {}, log scale: ",
            Span(self.length)
        );
        let listed = u64::try_from(self.listed.len()).unwrap_or(u64::MAX);
        let traces = self.matching.max(self.drawn()).max(listed);
        match traces {
            0 => {
                label.push_str("no traces");
                return label;
            }
            1 => label.push_str("1 trace"),
            n => {
                let _ = write!(label, "{} traces", Count(n));
            }
        }
        if self.sampled() {
            let _ = write!(label, ", {} of them drawn", Count(self.drawn()));
        }
        if listed > 0 {
            let _ = write!(label, ", {} listed below", Count(listed));
        }
        let drawn = if self.sampled() { " drawn" } else { "" };
        match self.slowest_error() {
            None => {
                let _ = write!(label, "; no errors{drawn}");
            }
            Some((point, name)) => {
                let _ = write!(label, "; slowest error{drawn} ");
                if let Some(name) = name {
                    let _ = write!(label, "{name}, ");
                }
                let _ = write!(
                    label,
                    "{}, {} ago",
                    SpanTime(point.duration),
                    Span(self.ago(point.start))
                );
            }
        }
        label
    }

    /// The y axis label of a power of ten of seconds: `1 ms`, `100 ms`, `1 s`, `1,000 s`.
    fn decade(seconds: f64) -> String {
        let ms = (seconds * 1e3).round();
        if ms < 1e3 {
            format!("{ms} ms")
        } else {
            format!("{} s", Count((ms / 1e3).round() as u64))
        }
    }
}

impl fmt::Display for DurationScatter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scale = self.scale();
        let frame = Frame {
            class: "chart-scatter",
            role: if self.listed.is_empty() {
                "img"
            } else {
                "group"
            },
            label: &self.label(),
            y_labels: scale.ticks().map(Self::decade).collect(),
            x_labels: self.x_labels(),
            units: Units::Percent,
        };
        let listed: Vec<TraceId> = self.listed.iter().map(|l| l.point.trace_id).collect();
        frame.write(f, |f| {
            f.write_str(r#"<g class="scatter-points" aria-hidden="true">"#)?;
            // Errors after the ok ones, so none is hidden under them; a listed trace only once.
            for status in TraceStatus::ALL {
                let class = match status {
                    TraceStatus::Ok => "scatter-ok",
                    TraceStatus::Error => "scatter-error",
                };
                for point in self
                    .points
                    .iter()
                    .filter(|p| p.status == status && !listed.contains(&p.trace_id))
                {
                    let (cx, cy) = self.position(&scale, point);
                    write!(
                        f,
                        r#"<circle class="scatter-dot {class}" cx="{cx}%" cy="{cy}%" r="{}"></circle>"#,
                        Num(Self::DOT)
                    )?;
                }
            }
            f.write_str("</g>")?;
            // The selected one last, on top of the rest.
            let (current, others): (Vec<&Listed>, Vec<&Listed>) =
                self.listed.iter().partition(|listed| listed.current);
            for listed in others.into_iter().chain(current) {
                let (cx, cy) = self.position(&scale, &listed.point);
                let label = listed.label();
                write!(
                    f,
                    r#"<a class="scatter-link{}" href="{}" aria-label="{}"{}><title>{}</title>"#,
                    if listed.current {
                        " scatter-current"
                    } else {
                        ""
                    },
                    Escaped(&listed.href),
                    Escaped(&label),
                    if listed.current {
                        r#" aria-current="true""#
                    } else {
                        ""
                    },
                    Escaped(&label),
                )?;
                write!(
                    f,
                    r#"<circle class="scatter-hit" cx="{cx}%" cy="{cy}%" r="{}"></circle>"#,
                    Num(Self::HIT)
                )?;
                write!(
                    f,
                    r#"<circle class="scatter-mark{}" cx="{cx}%" cy="{cy}%" r="{}"></circle></a>"#,
                    match listed.point.status {
                        TraceStatus::Ok => "",
                        TraceStatus::Error => " scatter-error",
                    },
                    Num(if listed.current {
                        Self::CURRENT
                    } else {
                        Self::MARK
                    })
                )?;
            }
            Ok(())
        })
    }
}

/// Numbers and escaped names and links only.
impl askama::filters::HtmlSafe for DurationScatter {}

#[cfg(test)]
mod tests;
