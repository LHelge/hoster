//! Continuous liveness of live services (ADR-0038, "after a deploy").
//!
//! Once a generation is live, hoster keeps probing it every 10–30 s with the same [`Check`] the
//! deploy used. The verdicts feed the UI status and in-process statistics only (ADR-0045):
//! failing probes never restart anything, that is the reconciler's job (ADR-0028), which also
//! decides when a service is *crash looping*.
//!
//! - A [`Prober`] carries out one probe; [`NetworkProber`] talks to the container's loopback
//!   port, tests use a fake.
//! - [`Liveness`] turns probe results (and the reconciler's crash-loop verdict) into a
//!   [`HealthStatus`] with its [`HealthStats`], reporting each [`HealthChange`].
//! - [`MonitorSettings`] says how often a service is probed.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use tokio::time::Instant;

use super::Check;
use crate::stack::Service;

/// The liveness of a live service.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HealthStatus {
    /// Not probed since it went live (or since hoster started).
    #[default]
    Unknown,
    Healthy,
    /// The last probe failed, or it has not passed `healthy_after` times in a row since.
    Unhealthy,
    /// The reconciler restarted it 5 times within 10 minutes (ADR-0028).
    CrashLooping,
}

impl fmt::Display for HealthStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unknown => "unknown",
            Self::Healthy => "healthy",
            Self::Unhealthy => "unhealthy",
            Self::CrashLooping => "crash looping",
        })
    }
}

/// In-process statistics of one live generation (ADR-0045); they reset when hoster restarts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HealthStats {
    pub status: HealthStatus,
    /// When the status last changed.
    pub since: Option<Instant>,
    /// Probes carried out.
    pub probes: u64,
    /// Probes that failed.
    pub failures: u64,
    /// Status changes.
    pub changes: u32,
    /// What the last probe (or the reconciler) said.
    pub last: Option<String>,
}

/// A status change, as it is logged and sent to the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthChange {
    pub from: HealthStatus,
    pub to: HealthStatus,
    /// Why: the probe result that caused it.
    pub detail: String,
}

/// The liveness state machine of one live generation.
///
/// - From `Unknown` or `Healthy`, a passing probe means `Healthy`.
/// - Any failing probe means `Unhealthy`.
/// - From `Unhealthy` (or `CrashLooping`), the service is `Healthy` again after `healthy_after`
///   passing probes in a row; until then it is `Unhealthy`.
/// - The reconciler's crash-loop verdict wins over the probes while it lasts.
#[derive(Debug, Clone)]
pub struct Liveness {
    stats: HealthStats,
    healthy_after: u32,
    /// Passing probes in a row.
    streak: u32,
}

impl Liveness {
    /// A generation not probed yet, healthy again after `healthy_after` passes in a row.
    pub fn new(healthy_after: u32) -> Self {
        Self {
            stats: HealthStats::default(),
            healthy_after: healthy_after.max(1),
            streak: 0,
        }
    }

    pub fn stats(&self) -> &HealthStats {
        &self.stats
    }

    pub fn status(&self) -> HealthStatus {
        self.stats.status
    }

    /// Records one probe result at `now`: `Ok` with what passed, `Err` with why it failed.
    pub fn observe(
        &mut self,
        now: Instant,
        result: Result<String, String>,
    ) -> Option<HealthChange> {
        self.stats.probes += 1;
        let (to, detail) = match result {
            Ok(detail) => {
                self.streak = self.streak.saturating_add(1);
                let healthy = matches!(
                    self.stats.status,
                    HealthStatus::Unknown | HealthStatus::Healthy
                ) || self.streak >= self.healthy_after;
                let to = if healthy {
                    HealthStatus::Healthy
                } else {
                    HealthStatus::Unhealthy
                };
                (to, detail)
            }
            Err(detail) => {
                self.stats.failures += 1;
                self.streak = 0;
                (HealthStatus::Unhealthy, detail)
            }
        };
        self.set(now, to, detail)
    }

    /// The reconciler marked the service crash looping (ADR-0028).
    pub fn crash_looping(&mut self, now: Instant, detail: String) -> Option<HealthChange> {
        self.streak = 0;
        self.set(now, HealthStatus::CrashLooping, detail)
    }

    fn set(&mut self, now: Instant, to: HealthStatus, detail: String) -> Option<HealthChange> {
        let from = self.stats.status;
        self.stats.last = Some(detail.clone());
        if from == to {
            return None;
        }
        self.stats.status = to;
        self.stats.since = Some(now);
        self.stats.changes += 1;
        Some(HealthChange { from, to, detail })
    }
}

/// Carries out one probe of a [`Check`].
#[async_trait]
pub trait Prober: Send + Sync {
    /// `Ok` with what passed, `Err` with why it failed; bounded by the check's timeout.
    async fn probe(&self, check: &Check) -> Result<String, String>;
}

/// Probes over the network, on the container's `127.0.0.1` port.
#[derive(Debug, Clone, Copy, Default)]
pub struct NetworkProber;

#[async_trait]
impl Prober for NetworkProber {
    async fn probe(&self, check: &Check) -> Result<String, String> {
        check.attempt().await
    }
}

/// How often live services are probed: every 10–30 s (ADR-0038).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorSettings {
    /// The interval of a service whose health check sets none.
    pub interval: Duration,
    /// A declared `interval` is raised to at least this.
    pub min_interval: Duration,
    /// A declared `interval` is lowered to at most this.
    pub max_interval: Duration,
}

impl Default for MonitorSettings {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(15),
            min_interval: Duration::from_secs(10),
            max_interval: Duration::from_secs(30),
        }
    }
}

impl MonitorSettings {
    /// The time between two liveness probes of `service`: its declared `health.interval`,
    /// clamped to the monitor's range (a deploy-time interval of 1 s would be too chatty), or
    /// the default.
    pub fn interval_for(&self, service: &Service) -> Duration {
        let max = self.max_interval.max(self.min_interval);
        service
            .health
            .as_ref()
            .and_then(|h| h.interval)
            .map_or(self.interval, |declared| {
                declared.clamp(self.min_interval, max)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use HealthStatus as H;

    fn ok() -> Result<String, String> {
        Ok("HTTP 200".into())
    }

    fn fail() -> Result<String, String> {
        Err("HTTP 503, expected 2xx-3xx".into())
    }

    #[tokio::test(start_paused = true)]
    async fn first_probe_decides_and_failures_flip_at_once() {
        let mut live = Liveness::new(3);
        assert_eq!(live.status(), H::Unknown);
        let start = Instant::now();
        let change = live.observe(start, ok()).unwrap();
        assert_eq!((change.from, change.to), (H::Unknown, H::Healthy));
        assert_eq!(live.observe(start, ok()), None);

        tokio::time::advance(Duration::from_secs(15)).await;
        let change = live.observe(Instant::now(), fail()).unwrap();
        assert_eq!((change.from, change.to), (H::Healthy, H::Unhealthy));
        assert_eq!(change.detail, "HTTP 503, expected 2xx-3xx");
        assert_eq!(live.stats().since, Some(start + Duration::from_secs(15)));
    }

    #[tokio::test(start_paused = true)]
    async fn recovery_needs_healthy_after_passes_in_a_row() {
        let mut live = Liveness::new(3);
        let now = Instant::now();
        live.observe(now, fail());
        assert_eq!(live.observe(now, ok()), None);
        assert_eq!(live.observe(now, ok()), None);
        live.observe(now, fail());
        assert_eq!(live.observe(now, ok()), None);
        assert_eq!(live.observe(now, ok()), None);
        let change = live.observe(now, ok()).unwrap();
        assert_eq!((change.from, change.to), (H::Unhealthy, H::Healthy));
        assert_eq!(
            live.stats(),
            &HealthStats {
                status: H::Healthy,
                since: Some(now),
                probes: 7,
                failures: 2,
                changes: 2,
                last: Some("HTTP 200".into()),
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn crash_looping_wins_until_probes_pass_again() {
        let mut live = Liveness::new(2);
        let now = Instant::now();
        live.observe(now, ok());
        let change = live.crash_looping(now, "restarted 5 times".into()).unwrap();
        assert_eq!((change.from, change.to), (H::Healthy, H::CrashLooping));
        assert_eq!(live.crash_looping(now, "restarted 6 times".into()), None);
        // The reconciler cleared it: one pass is not enough yet.
        let change = live.observe(now, ok()).unwrap();
        assert_eq!((change.from, change.to), (H::CrashLooping, H::Unhealthy));
        let change = live.observe(now, ok()).unwrap();
        assert_eq!(change.to, H::Healthy);
    }

    #[test]
    fn declared_intervals_are_clamped_to_the_monitor_range() {
        let settings = MonitorSettings::default();
        let stack = |health: &str| -> crate::stack::Stack {
            format!(
                "version = 1\nname = \"s\"\n[services.a]\nimage = \"docker.io/library/a:1\"\n\
                 [[services.a.routes]]\nhost = \"a.example.com\"\nport = 80\n{health}"
            )
            .parse()
            .unwrap()
        };
        let interval = |health: &str| {
            let stack = stack(health);
            settings.interval_for(stack.services.values().next().unwrap())
        };
        assert_eq!(interval(""), Duration::from_secs(15));
        let declared =
            |i: &str| format!("[services.a.health]\ninterval = \"{i}\"\nhttp = {{ port = 80 }}");
        assert_eq!(interval(&declared("1s")), Duration::from_secs(10));
        assert_eq!(interval(&declared("20s")), Duration::from_secs(20));
        assert_eq!(interval(&declared("5m")), Duration::from_secs(30));
    }
}
