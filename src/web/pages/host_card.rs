//! The sidebar's HOST card (ADR-0066 §9–§11): the host's CPU, memory and disk, each with its
//! value, a [`Meter`] in the tone of its [`Load`], and for CPU and memory a [`Sparkline`] of the
//! sampler's last hour. Disk is the filesystem of podman's storage root.
//!
//! The card is read from the resource sampler's cache alone ([`ResourceUsage`]), never podman or
//! `/proc`, so it costs a page nothing and never fails one: a figure the sampler does not have
//! reads `—` over an empty meter. Every signed-in page renders it in the shell; it then polls
//! [`HostCard::PATH`] every sampler interval and swaps itself for the answer.

use askama::Template;
use axum::Extension;

use super::{Page, ShellSource};
use crate::instance::InstanceName;
use crate::stats::{History, HostSample, ResourceUsage, Usage};
use crate::web::charts::Sparkline;
use crate::web::ui::{ByteUsage, Load, Meter, Percent, Tone};

/// The HOST card, as the shell includes it and [`HostCard::PATH`] serves it.
#[derive(Debug, Template)]
#[template(path = "host_card.html")]
pub struct HostCard {
    /// The machine's hostname, read once at start (the instance name's default, ADR-0058 §2).
    hostname: InstanceName,
    rows: [HostMeter; 3],
}

impl HostCard {
    /// Where the card polls for itself.
    pub const PATH: &str = "/shell/host";
    /// The poll's `hx-trigger`: once per sampler tick (`ResourceSampler::INTERVAL`).
    const POLL: &str = "every 10s";
    /// The element the poll swaps.
    const ID: &str = "host-card";

    /// The card of the host `hostname` from the sampler's `history`.
    pub fn new(hostname: InstanceName, history: &History<HostSample>) -> Self {
        let latest = history.latest();
        let samples = history.samples();
        let cpu = latest.and_then(|sample| sample.cpu);
        let cpu = HostMeter::new(
            "cpu",
            cpu.map(|cpu| (Percent(cpu).to_string(), Load::new(cpu, 100.0))),
            "of all cores",
        )
        .with_spark(
            samples
                .iter()
                .filter_map(|sample| sample.cpu)
                // Tenths of a percent: the sparkline takes whole numbers (ADR-0066 §11).
                .map(|cpu| (cpu * 10.0).round() as u64),
        );
        let memory = latest.and_then(|sample| sample.memory);
        let memory = HostMeter::usage("mem", memory).with_spark(
            samples
                .iter()
                .filter_map(|sample| sample.memory)
                .map(|memory| memory.used),
        );
        let disk = HostMeter::usage("disk", latest.and_then(|sample| sample.disk));
        Self {
            hostname,
            rows: [cpu, memory, disk],
        }
    }

    /// The card from `usage`, the sampler's cache.
    pub fn of(hostname: InstanceName, usage: &ResourceUsage) -> Self {
        Self::new(hostname, &usage.host())
    }

    /// `GET /shell/host`: the card alone, for its poll. Behind the session guard like every
    /// admin route.
    pub async fn fragment(Extension(shell): Extension<ShellSource>) -> Page<HostCard> {
        Page(shell.host_card())
    }
}

/// One row of the card: what it measures, its value, its meter and maybe its sparkline.
#[derive(Debug)]
struct HostMeter {
    label: &'static str,
    /// `None` when unknown, shown as `—`.
    value: Option<String>,
    meter: Meter,
    tone: Tone,
    spark: Option<Sparkline>,
}

impl HostMeter {
    /// A row whose latest figure is `known` (its text and load), or unknown. `of` says what the
    /// value is out of, for the meter's accessible name.
    fn new(label: &'static str, known: Option<(String, Load)>, of: &str) -> Self {
        match known {
            Some((value, load)) => Self {
                label,
                meter: load.meter(format_args!("{label} {value} {of}")),
                tone: load.into(),
                value: Some(value),
                spark: None,
            },
            None => Self {
                label,
                value: None,
                meter: Meter::new(0.0, 0.0, Tone::Neutral, format_args!("{label} unknown")),
                tone: Tone::Neutral,
                spark: None,
            },
        }
    }

    /// A row of bytes used out of a total.
    fn usage(label: &'static str, usage: Option<Usage>) -> Self {
        Self::new(
            label,
            usage.map(|usage| (ByteUsage(usage).to_string(), usage.into())),
            "used",
        )
    }

    /// The row with a sparkline of `values`, oldest first, in the row's tone; none when there
    /// is no known value in the hour.
    fn with_spark(mut self, values: impl IntoIterator<Item = u64>) -> Self {
        let values: Vec<u64> = values.into_iter().collect();
        if !values.is_empty() {
            self.spark = Some(Sparkline::new(values, self.tone));
        }
        self
    }
}

#[cfg(test)]
mod tests;
