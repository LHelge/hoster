//! The CPU and Memory columns of the services table (ADR-0066 §3, §4, §9–§11), read from the
//! resource sampler's cache alone ([`ResourceUsage`]), never podman or the store.
//!
//! Each container of the stack gets a CPU cell and a Memory cell, with ids keyed by its
//! container id. CPU is its percent of one core with a sparkline of the sampler's hour; memory
//! is podman's `usage`. A figure is shown against a limit only for the container that runs its
//! service (the generation the service runs), with the limits in effect for it: the service's
//! `limits.cpus` and `limits.memory` in the revision the reconciler uses ([`Declarations`],
//! ADR-0059). Then it also shows `/ <limit>` and a [`Meter`] in the tone of its [`Load`]; CPU
//! keeps its sparkline. Without a limit (and on the muted rows of the other generations, which
//! may have been created with other limits) there is no meter: CPU and memory each show a
//! neutral sparkline. An unknown figure reads `—`, over an empty meter where there is a limit.
//!
//! The overview's live stream ([`UsageFeed`]) sends the cells again after every sampler tick
//! as `<hx-partial>`s that replace the content of each cell by its id. Rows that appear or go
//! away change only when the page is reloaded; a partial for a container the page does not
//! show finds no target and is dropped by htmx.

use std::collections::BTreeMap;

use askama::Template;

use super::services::Declarations;
use crate::machine::Memory;
use crate::orchestrator::{ServiceState, StackStatus};
use crate::runtime::ContainerId;
use crate::stack::{Limits, Stack};
use crate::stats::{ContainerSample, ContainerUsage, History, ResourceUsage};
use crate::web::charts::Sparkline;
use crate::web::ui::{Load, Meter, Percent, Tone};

/// The limits in effect for the container that runs each service: by service, the generation
/// it runs and its limits.
#[derive(Debug, Clone, Default)]
pub struct UsageLimits(BTreeMap<String, (u32, Limits)>);

impl UsageLimits {
    /// The limits of `status`'s services, as `declarations` has them in effect.
    pub(super) fn new(status: &StackStatus, declarations: &Declarations<'_>) -> Self {
        let limits = status
            .services
            .iter()
            .filter_map(|service| match service.state {
                ServiceState::Running { generation, .. } => {
                    let limits = declarations
                        .in_effect(&service.name)
                        .map(|declared| declared.limits)
                        .unwrap_or_default();
                    Some((service.name.clone(), (generation, limits)))
                }
                _ => None,
            })
            .collect();
        Self(limits)
    }

    /// The limits of `status`, its revisions parsed here.
    fn of(status: &StackStatus) -> Self {
        let active = status.revision.toml.parse::<Stack>().ok();
        Self::new(status, &Declarations::new(status, active.as_ref()))
    }

    /// The limits a container of `service` and `generation` is shown against: none unless it
    /// runs the service.
    fn of_container(&self, service: &str, generation: Option<u32>) -> Limits {
        match self.0.get(service) {
            Some((running, limits)) if generation == Some(*running) => *limits,
            _ => Limits::default(),
        }
    }
}

/// What the sampler has about a stack's containers, and the limits to show it against.
#[derive(Debug)]
pub struct StackUsage<'a> {
    limits: &'a UsageLimits,
    containers: BTreeMap<ContainerId, ContainerUsage>,
}

impl<'a> StackUsage<'a> {
    /// The `containers` (as [`ResourceUsage::stack`] reads them) against `limits`.
    pub fn new(limits: &'a UsageLimits, containers: BTreeMap<ContainerId, ContainerUsage>) -> Self {
        Self { limits, containers }
    }

    /// The cells of the container `id`, of `service` and `generation` by its labels. A
    /// container the sampler has not seen yet has every figure unknown.
    pub fn cells(&self, id: &ContainerId, service: &str, generation: Option<u32>) -> UsageCells {
        let history = self.containers.get(id).map(|usage| &usage.history);
        UsageCells::new(id, history, self.limits.of_container(service, generation))
    }

    /// Every container's cells as `<hx-partial>`s, each replacing a cell's content; empty when
    /// the sampler has no container of the stack.
    fn partials(&self) -> String {
        self.containers
            .iter()
            .map(|(id, usage)| {
                let limits = self.limits.of_container(&usage.service, usage.generation);
                UsageCells::new(id, Some(&usage.history), limits).partials()
            })
            .collect()
    }
}

/// The CPU and Memory cells of one container's row.
#[derive(Debug, Template)]
#[template(path = "usage_cells.html")]
pub struct UsageCells {
    /// The container id, as it goes into the cells' element ids.
    id: String,
    cpu: Figure,
    memory: Figure,
}

impl UsageCells {
    /// The prefixes of the cells' element ids.
    const CPU: &str = "cpu-";
    const MEMORY: &str = "mem-";

    /// The cells of container `id` from its `history` (`None`: not sampled yet), against the
    /// service's `limits` that apply to it.
    fn new(id: &ContainerId, history: Option<&History<ContainerSample>>, limits: Limits) -> Self {
        let samples = history.map_or(&[][..], History::samples);
        let latest = samples.last();
        Self {
            // Podman's ids are hex; anything else is left out so the id stays a plain CSS
            // selector for the partial's target.
            id: id
                .as_ref()
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
                .collect(),
            cpu: Figure::cpu(latest.and_then(|s| s.cpu), samples, limits),
            memory: Figure::memory(latest.and_then(|s| s.memory), samples, limits),
        }
    }

    fn cpu_id(&self) -> String {
        format!("{}{}", Self::CPU, self.id)
    }

    fn memory_id(&self) -> String {
        format!("{}{}", Self::MEMORY, self.id)
    }

    /// Both cells' content, each wrapped to replace the content of its cell on the page.
    fn partials(&self) -> String {
        [(self.cpu_id(), &self.cpu), (self.memory_id(), &self.memory)]
            .into_iter()
            .map(|(id, figure)| {
                let html = figure.render().unwrap_or_else(|err| {
                    tracing::error!(error = %err, "rendering a container's usage failed");
                    String::new()
                });
                format!(
                    r##"<hx-partial hx-target="#{id}" hx-swap="innerHTML">{html}</hx-partial>"##
                )
            })
            .collect()
    }
}

/// One cell's content: the latest figure, the limit it is shown against, and its meter and
/// sparkline.
#[derive(Debug, Template)]
#[template(path = "usage_figure.html")]
struct Figure {
    /// `None` when unknown, shown as `—`.
    value: Option<String>,
    /// The limit, `150%` or `512 MiB`.
    limit: Option<String>,
    /// Only against a limit.
    meter: Option<Meter>,
    /// The meter's tone against a known figure, else neutral.
    tone: Tone,
    spark: Option<Sparkline>,
}

impl Figure {
    /// CPU, in percent of one core, against `limits.cpus` (in CPUs, so `1.5` is `150%`), with
    /// a sparkline in tenths of a percent (the sparkline takes whole numbers, ADR-0066 §11).
    fn cpu(latest: Option<f64>, samples: &[ContainerSample], limits: Limits) -> Self {
        let limit = limits.cpus.map(|cpus| f64::from(cpus) * 100.0);
        let figure = Self::new(
            "cpu",
            latest.map(|cpu| (Percent(cpu).to_string(), cpu)),
            limit.map(|limit| (Percent(limit).to_string(), limit)),
        );
        figure.with_spark(
            samples
                .iter()
                .filter_map(|s| s.cpu)
                .map(|cpu| (cpu * 10.0).round() as u64),
        )
    }

    /// Memory, in bytes, against `limits.memory`; a sparkline only without a limit.
    fn memory(latest: Option<u64>, samples: &[ContainerSample], limits: Limits) -> Self {
        let limit = limits.memory.map(u64::from);
        #[expect(clippy::cast_precision_loss, reason = "only the share matters")]
        let figure = Self::new(
            "memory",
            latest.map(|bytes| (Memory(bytes).to_string(), bytes as f64)),
            limit.map(|bytes| (Memory(bytes).to_string(), bytes as f64)),
        );
        match limit {
            Some(_) => figure,
            None => figure.with_spark(samples.iter().filter_map(|s| s.memory)),
        }
    }

    /// The figure `what` of `latest` (its text and value), against `limit` (its text and
    /// value) when one is set.
    fn new(what: &str, latest: Option<(String, f64)>, limit: Option<(String, f64)>) -> Self {
        let (meter, tone) = match (&latest, &limit) {
            (Some((text, value)), Some((max_text, max))) => {
                let load = Load::new(*value, *max);
                let meter = load.meter(format_args!("{what} {text} of its {max_text} limit"));
                (Some(meter), load.into())
            }
            (None, Some(_)) => (
                Some(Meter::new(
                    0.0,
                    0.0,
                    Tone::Neutral,
                    format_args!("{what} unknown"),
                )),
                Tone::Neutral,
            ),
            (_, None) => (None, Tone::Neutral),
        };
        Self {
            value: latest.map(|(text, _)| text),
            limit: limit.map(|(text, _)| text),
            meter,
            tone,
            spark: None,
        }
    }

    /// The figure with a sparkline of `values`, oldest first, in its tone; none when the hour
    /// has no known value.
    fn with_spark(mut self, values: impl IntoIterator<Item = u64>) -> Self {
        let values: Vec<u64> = values.into_iter().collect();
        if !values.is_empty() {
            self.spark = Some(Sparkline::new(values, self.tone));
        }
        self
    }
}

/// The overview's live usage: what the stack's stream sends after each sampler tick.
#[derive(Debug, Clone)]
pub(in crate::web) struct UsageFeed {
    stack: String,
    limits: UsageLimits,
    usage: ResourceUsage,
}

impl UsageFeed {
    /// The feed of `status`'s containers from the sampler's cache `usage`. The limits are
    /// those in effect now; the page's rows, which they match, change only on reload.
    pub(in crate::web) fn new(status: &StackStatus, usage: ResourceUsage) -> Self {
        Self {
            stack: status.name.clone(),
            limits: UsageLimits::of(status),
            usage,
        }
    }

    /// The sampler's tick counter, which changes after each tick.
    pub(in crate::web) fn changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.usage.changes()
    }

    /// The stack's containers' cells as `<hx-partial>`s, from the cache as it is now; `None`
    /// when the sampler has none of them.
    pub(in crate::web) fn partials(&self) -> Option<String> {
        let html = StackUsage::new(&self.limits, self.usage.stack(&self.stack)).partials();
        (!html.is_empty()).then_some(html)
    }
}

#[cfg(test)]
mod tests;
