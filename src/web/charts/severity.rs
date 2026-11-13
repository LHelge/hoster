//! The severity histogram of the Telemetry page's Logs tab (epic psq): [`SeverityBars`], log
//! records per time bucket stacked by level, over [`Buckets`] that are aligned to the epoch as
//! the log store counts them (`LogStore::count_by_level`). Unlike the traffic charts, whose x
//! axis is relative, its axis and tooltips are UTC clock times, as the log rows write them, and
//! each bar can be a link that narrows the view to its bucket.

use std::fmt::{self, Write as _};
use std::ops::Range;
use std::time::Duration;

use super::{Frame, HEIGHT, Legend, LegendKey, Num, Scale, Span, WIDTH};
use crate::store::{Level, LevelCounts, Timestamp};
use crate::web::ui::{Count, Escaped};

/// The time buckets of a histogram: `bars` bars of `bucket` each, the first holding the start
/// of the range and the last its end, each starting at a multiple of `bucket` since the epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Buckets {
    first: Timestamp,
    bucket: Duration,
    bars: usize,
    /// The range's length, to whole buckets, as the label says it.
    length: Duration,
}

impl Buckets {
    /// The bucket of each *since* preset, about 30 bars each: the shortest bucket of these that
    /// gives a range at most [`Buckets::MAX_BARS`] bars is the one used, which for a preset is
    /// its own.
    pub const PLANS: [(Duration, Duration); 6] = [
        (Duration::from_secs(5 * 60), Duration::from_secs(10)),
        (Duration::from_secs(15 * 60), Duration::from_secs(30)),
        (Duration::from_secs(3_600), Duration::from_secs(2 * 60)),
        (Duration::from_secs(6 * 3_600), Duration::from_secs(15 * 60)),
        (Duration::from_secs(24 * 3_600), Duration::from_secs(3_600)),
        (
            Duration::from_secs(7 * 86_400),
            Duration::from_secs(6 * 3_600),
        ),
    ];
    /// The most bars a plan's bucket may give; a longer range gets whole multiples of the
    /// longest bucket, about [`Buckets::TARGET_BARS`] of them.
    const MAX_BARS: u32 = 36;
    const TARGET_BARS: u32 = 30;

    /// The buckets of `range`.
    pub fn covering(range: Range<Timestamp>) -> Self {
        let millis = (range.end.as_millis() - range.start.as_millis()).max(0);
        let length = Duration::from_millis(u64::try_from(millis).unwrap_or(0));
        let bucket = Self::PLANS
            .iter()
            .map(|&(_, bucket)| bucket)
            .find(|&bucket| length <= bucket * Self::MAX_BARS)
            .unwrap_or_else(|| {
                let longest = Self::PLANS[Self::PLANS.len() - 1].1;
                let per_bar = longest * Self::TARGET_BARS;
                longest * u32::try_from(length.as_millis() / per_bar.as_millis() + 1).unwrap_or(1)
            });
        let first = range.start.floor(bucket);
        let last = Timestamp::from_millis(range.end.as_millis() - 1)
            .max(range.start)
            .floor(bucket);
        let step = i64::try_from(bucket.as_millis()).unwrap_or(i64::MAX).max(1);
        let bars = usize::try_from((last.as_millis() - first.as_millis()) / step + 1).unwrap_or(1);
        let whole = (length.as_millis() + bucket.as_millis() / 2) / bucket.as_millis().max(1);
        Self {
            first,
            bucket,
            bars,
            length: bucket * u32::try_from(whole).unwrap_or(u32::MAX),
        }
    }

    /// The length of one bar.
    pub fn bucket(&self) -> Duration {
        self.bucket
    }

    /// When bar `i` (0 the oldest) starts.
    fn start_of(&self, i: usize) -> Timestamp {
        self.first + self.bucket * u32::try_from(i).unwrap_or(u32::MAX)
    }

    /// The bar a bucket starting at `start` is, if it is one of them.
    fn index(&self, start: Timestamp) -> Option<usize> {
        let offset = start.as_millis() - self.first.as_millis();
        let step = i64::try_from(self.bucket.as_millis()).ok()?.max(1);
        let i = usize::try_from(offset / step).ok()?;
        (offset % step == 0 && i < self.bars).then_some(i)
    }

    /// `time` as the axis and tooltips write it: `19:45`, with seconds for buckets under a
    /// minute and the date for ranges of a day or more (`11-13 06:00`).
    fn clock(&self, time: Timestamp) -> String {
        let utc = humantime::format_rfc3339_seconds(time.into()).to_string();
        let end = if self.bucket < Duration::from_secs(60) {
            19
        } else {
            16
        };
        let start = if self.length >= Duration::from_secs(86_400) {
            5
        } else {
            11
        };
        utc[start..end].replacen('T', " ", 1)
    }

    /// Bar `i`'s span as a tooltip says it: `19:56 to 19:58`, the newest `19:58 to now`.
    fn span_of(&self, i: usize) -> String {
        let to = match i + 1 == self.bars {
            true => "now".to_owned(),
            false => self.clock(self.start_of(i + 1)),
        };
        format!("{} to {to}", self.clock(self.start_of(i)))
    }

    /// The x axis labels: four, the starts of the bars at a third and two thirds, then `now`.
    fn labels(&self) -> Vec<String> {
        let mut labels: Vec<String> = (0..3)
            .map(|k| self.clock(self.start_of((k * self.bars + 1) / 3)))
            .collect();
        labels.push("now".to_owned());
        labels
    }
}

/// The records of one bar, by the histogram's segments: debug and info (trace too) together,
/// warn, error, and those without a level.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeverityCounts {
    pub info: u64,
    pub warn: u64,
    pub error: u64,
    pub unleveled: u64,
}

impl SeverityCounts {
    /// The counts of `counts` that a level filter of `levels` keeps: every record for none,
    /// otherwise only those levels (records without a level match no level filter).
    pub fn kept(counts: &LevelCounts, levels: &[Level]) -> Self {
        let n = |level: Level| match levels.is_empty() || levels.contains(&level) {
            true => counts.count(Some(level)),
            false => 0,
        };
        Self {
            info: n(Level::Trace) + n(Level::Debug) + n(Level::Info),
            warn: n(Level::Warn),
            error: n(Level::Error),
            unleveled: match levels.is_empty() {
                true => counts.count(None),
                false => 0,
            },
        }
    }

    /// Its segments, bottom to top, in [`SEGMENTS`] order.
    fn segments(&self) -> [u64; 4] {
        [self.unleveled, self.info, self.warn, self.error]
    }

    fn total(&self) -> u64 {
        self.segments().iter().sum()
    }
}

/// The segments of a bar, bottom to top: their CSS classes and legend names.
const SEGMENTS: [(&str, &str); 4] = [
    ("chart-lvl-none", "no level"),
    ("chart-lvl-info", "debug/info"),
    ("chart-lvl-warn", "warn"),
    ("chart-lvl-error", "error"),
];

/// A stacked bar chart of log records per bucket of its [`Buckets`] by severity: records
/// without a level in a muted tone at the bottom, then debug/info, warn and error, as in the
/// Telemetry mockup. Each bar links to the view of its bucket when [`linked`](Self::linked).
#[derive(Debug, Clone, PartialEq)]
pub struct SeverityBars {
    buckets: Buckets,
    bars: Vec<SeverityCounts>,
    /// The segments drawn and in the legend: those the level filter keeps.
    shown: [bool; 4],
    links: Vec<String>,
    subject: Option<String>,
    unfiltered: bool,
}

impl SeverityBars {
    /// The chart of `counts`, each the bucket starting at its time; those outside `buckets`
    /// are left out and missing buckets are empty. `levels` is the level filter in effect
    /// (none for every record): it picks the segments the legend shows.
    pub fn new(
        buckets: Buckets,
        counts: impl IntoIterator<Item = (Timestamp, SeverityCounts)>,
        levels: &[Level],
    ) -> Self {
        let mut bars = vec![SeverityCounts::default(); buckets.bars];
        for (start, n) in counts {
            if let Some(i) = buckets.index(start) {
                bars[i] = n;
            }
        }
        let any = |of: &[Level]| levels.is_empty() || of.iter().any(|l| levels.contains(l));
        Self {
            buckets,
            bars,
            shown: [
                levels.is_empty(),
                any(&[Level::Trace, Level::Debug, Level::Info]),
                any(&[Level::Warn]),
                any(&[Level::Error]),
            ],
            links: Vec::new(),
            subject: None,
            unfiltered: false,
        }
    }

    /// The same chart, saying in its label whose records it counts (`notes`, `all stacks`).
    pub fn of(mut self, subject: impl fmt::Display) -> Self {
        self.subject = Some(subject.to_string());
        self
    }

    /// The same chart with each bar a link: `href` of the bucket's range, and whether it is
    /// the newest (whose range runs on past now).
    pub fn linked(mut self, href: impl Fn(Range<Timestamp>, bool) -> String) -> Self {
        let b = self.buckets;
        self.links = (0..b.bars)
            .map(|i| href(b.start_of(i)..b.start_of(i + 1), i + 1 == b.bars))
            .collect();
        self
    }

    /// The same chart, marked as counting records a grep or field filter of the page would
    /// hide: it counts on the index only.
    pub fn unfiltered(mut self) -> Self {
        self.unfiltered = true;
        self
    }

    /// Whether it counts records the page's grep or field filters hide.
    pub fn is_unfiltered(&self) -> bool {
        self.unfiltered
    }

    /// The length of a bar as the card head says it: `30s`, `2m`, `6h`.
    pub fn bucket_label(&self) -> String {
        Span(self.buckets.bucket).to_string()
    }

    /// The legend: each segment shown and its total.
    pub fn legend(&self) -> Legend {
        let totals = self.totals();
        Legend(
            SEGMENTS
                .into_iter()
                .zip(totals)
                .zip(self.shown)
                .rev()
                .filter(|(_, shown)| *shown)
                .map(|(((class, name), total), _)| LegendKey {
                    class,
                    line: false,
                    text: format!("{name} {}", Count(total)),
                })
                .collect(),
        )
    }

    fn totals(&self) -> [u64; 4] {
        self.bars.iter().fold([0; 4], |mut totals, bar| {
            for (total, n) in totals.iter_mut().zip(bar.segments()) {
                *total += n;
            }
            totals
        })
    }

    /// The bar with the most of `count`, the newest of equals, if any has some.
    fn peak(&self, count: impl Fn(&SeverityCounts) -> u64) -> Option<(usize, u64)> {
        self.bars
            .iter()
            .map(count)
            .enumerate()
            .filter(|&(_, n)| n > 0)
            .max_by_key(|&(_, n)| n)
    }

    /// The `aria-label`: what, over how long, the totals and where the errors peaked.
    fn label(&self) -> String {
        let mut label = String::from("Log records by severity");
        if let Some(subject) = &self.subject {
            let _ = write!(label, " for {subject}");
        }
        let _ = write!(
            label,
            " over the last {} in {} buckets",
            Span(self.buckets.length),
            Span(self.buckets.bucket)
        );
        if self.unfiltered {
            label.push_str(", not narrowed by grep or field filters");
        }
        label.push_str(": ");
        let totals = self.totals();
        let total: u64 = totals.iter().sum();
        if total == 0 {
            label.push_str("no records");
            return label;
        }
        label.push_str(&Records(total).to_string());
        for (((_, name), n), shown) in SEGMENTS.into_iter().zip(totals).zip(self.shown).rev() {
            if shown {
                let _ = write!(label, ", {name} {}", Count(n));
            }
        }
        match self.peak(|c| c.error) {
            Some((bar, n)) => {
                let _ = write!(
                    label,
                    "; most errors {} with {}",
                    self.buckets.span_of(bar),
                    Count(n)
                );
            }
            None if self.shown[3] => label.push_str("; no errors"),
            None => {}
        }
        label
    }
}

impl fmt::Display for SeverityBars {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let max = self
            .bars
            .iter()
            .map(SeverityCounts::total)
            .max()
            .unwrap_or(0);
        let scale = Scale::new(max as f64, 1.0);
        let linked = self.links.len() == self.bars.len();
        let frame = Frame {
            class: "chart-bars chart-levels",
            role: if linked { "group" } else { "img" },
            label: &self.label(),
            y_labels: scale
                .ticks()
                .map(|v| {
                    if v == 0.0 {
                        "0".to_owned()
                    } else {
                        super::Compact(v).to_string()
                    }
                })
                .collect(),
            x_labels: self.buckets.labels(),
        };
        let slot = WIDTH / self.bars.len().max(1) as f64;
        let gap = slot * 0.2;
        frame.write(f, |f| {
            for (i, bar) in self.bars.iter().enumerate() {
                match linked {
                    true => write!(
                        f,
                        r#"<a class="chart-bar" href="{}">"#,
                        Escaped(&self.links[i])
                    )?,
                    false => f.write_str(r#"<g class="chart-bar">"#)?,
                }
                write!(f, "<title>{}: ", self.buckets.span_of(i))?;
                write!(f, "{}", Records(bar.total()))?;
                for ((_, name), n) in SEGMENTS.into_iter().zip(bar.segments()).rev() {
                    if n > 0 {
                        write!(f, ", {name} {}", Count(n))?;
                    }
                }
                write!(
                    f,
                    r#"</title><rect class="chart-hit" x="{}" y="0" width="{}" height="{}" vector-effect="non-scaling-stroke"></rect>"#,
                    Num(slot * i as f64),
                    Num(slot),
                    Num(HEIGHT)
                )?;
                let mut below = 0u64;
                for ((class, _), n) in SEGMENTS.into_iter().zip(bar.segments()) {
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
                f.write_str(if linked { "</a>" } else { "</g>" })?;
            }
            Ok(())
        })
    }
}

/// A number of records in words: `no records`, `1 record`, `2,500 records`.
struct Records(u64);

impl fmt::Display for Records {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            0 => f.write_str("no records"),
            1 => f.write_str("1 record"),
            n => write!(f, "{} records", Count(n)),
        }
    }
}

/// Labels, numbers and escaped links only; the subject is escaped by `Display`.
impl askama::filters::HtmlSafe for SeverityBars {}

#[cfg(test)]
mod tests;
