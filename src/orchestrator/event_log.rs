//! The [`EventLog`] handle (ADR-0055): producers record an event through it, and it tells the
//! open UI pages once the event is stored. The log never depends on a page keeping up.

use std::fmt;
use std::net::IpAddr;
use std::time::{Duration, SystemTime};

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::{CANCELLED, STACK_REMOVED, UiEvent};
use crate::acme::DnsMismatch;
use crate::health::{HealthChange, HealthStatus};
use crate::installation::Version;
use crate::runtime::{ContainerInfo, ContainerState, ServiceGeneration};
use crate::stack::Name;
use crate::store::{
    Deployment, DeploymentId, DeploymentSource, DeploymentState, Event, EventKind, EventPage,
    EventQuery, NewEvent, Severity, ShortDigest, ShortSha, StackRevision, Store, StoreError,
    Timestamp,
};

/// Records events in the store and announces them on `ui_events`. Cheap to clone; every
/// producer (orchestrator, reconciler, ACME task, web handlers, update flow) holds one.
#[derive(Debug, Clone)]
pub struct EventLog {
    store: Store,
    ui_events: broadcast::Sender<UiEvent>,
}

impl EventLog {
    /// How long events are kept (ADR-0055 §4).
    pub const RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
    /// How often the retention sweep runs.
    const SWEEP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

    pub(super) fn new(store: Store, ui_events: broadcast::Sender<UiEvent>) -> Self {
        Self { store, ui_events }
    }

    /// Records `event` and tells the open pages. A failed write is logged at `warn` and
    /// otherwise ignored: the operation the event describes has happened regardless.
    pub async fn record(&self, event: NewEvent) {
        match self.store.record_event(&event).await {
            Ok(event) => self.announce(event),
            Err(err) => tracing::warn!(
                error = format!("{:#}", anyhow::Error::from(err)),
                kind = %event.kind,
                stack = event.stack,
                "cannot record event"
            ),
        }
    }

    /// Tells the open pages about `event`, which a store transaction already recorded.
    pub fn announce(&self, event: Event) {
        // No receiver is not an error: no UI page is open.
        let _ = self.ui_events.send(UiEvent::Event(event));
    }

    /// One page of the events `query` asks for, newest first (see [`Store::event_page`]).
    pub async fn page(&self, query: &EventQuery) -> Result<EventPage, StoreError> {
        self.store.event_page(query).await
    }

    /// Deletes the events older than [`Self::RETENTION`] once now and then daily, until
    /// `cancel` fires. A failed sweep is logged and tried again the next day.
    pub async fn run_retention(self, cancel: CancellationToken) {
        loop {
            let cutoff = Timestamp::now() - Self::RETENTION;
            match self.store.prune_events(cutoff).await {
                Ok(0) => {}
                Ok(deleted) => tracing::info!(deleted, "pruned old events"),
                Err(err) => tracing::warn!(
                    error = format!("{:#}", anyhow::Error::from(err)),
                    "cannot prune old events; trying again tomorrow"
                ),
            }
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(Self::SWEEP_INTERVAL) => {}
            }
        }
        tracing::debug!("event retention stopped");
    }
}

/// The deploy events of one deployment (ADR-0055 §2): a line when it is accepted and one for
/// its outcome, never one per transition (those stay in `deployment_events`). Each links to
/// the deployment's page and carries its source and, for a webhook deploy, the commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DeployEvents {
    id: DeploymentId,
    stack: String,
    source: DeploymentSource,
    /// The commit sha, when the trigger names one (the webhook's OIDC claims).
    sha: Option<String>,
    created_at: Timestamp,
    /// When the deployment was accepted (its move to `Validating`): what its duration is
    /// measured from, so time spent `Queued` behind another deployment does not count.
    accepted_at: Option<Timestamp>,
    /// The earlier deployment a rollback redeploys (ADR-0041).
    rollback_of: Option<DeploymentId>,
}

impl From<&Deployment> for DeployEvents {
    fn from(deployment: &Deployment) -> Self {
        Self {
            id: deployment.id,
            stack: deployment.stack.clone(),
            source: deployment.source,
            sha: deployment.provenance.as_ref().map(|p| p.sha.clone()),
            created_at: deployment.created_at,
            accepted_at: None,
            rollback_of: None,
        }
    }
}

impl DeployEvents {
    /// The events of a deployment that rolls back to `earlier`, if it does.
    pub(super) fn rolling_back(mut self, earlier: Option<DeploymentId>) -> Self {
        self.rollback_of = earlier;
        self
    }

    /// The event a deployment's move to `to` writes, if it writes one: started on accepting
    /// it (`Validating`, with the `services` it deploys and their digests), failed on
    /// `Failed` or `Rejected` (with `reason`), and a warning on `LiveWithWarnings` (with the
    /// cleanup problem). Success is written at the commit point ([`Self::succeeded`]), not by
    /// a transition. The move to `Validating` also marks the moment the deployment was
    /// accepted, which [`Self::succeeded`] measures from.
    pub(super) fn on<'a>(
        &mut self,
        to: DeploymentState,
        reason: Option<&str>,
        services: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Option<NewEvent> {
        match to {
            DeploymentState::Validating => {
                self.accepted_at = Some(Timestamp::now());
                Some(self.started(services))
            }
            DeploymentState::Failed | DeploymentState::Rejected => Some(self.failed(reason)),
            DeploymentState::LiveWithWarnings => Some(self.live_with_warnings(reason)),
            _ => None,
        }
    }

    /// The deployment was accepted, to deploy `services` (name, digest).
    fn started<'a>(&self, services: impl IntoIterator<Item = (&'a str, &'a str)>) -> NewEvent {
        let message = match self.rollback_of {
            Some(earlier) => format!("Rolling back {} to deployment #{earlier}", self.stack),
            None => format!("Deploying {}", self.stack),
        };
        let services: Vec<String> = services
            .into_iter()
            .map(|(service, digest)| format!("{service} {}", ShortDigest(digest)))
            .collect();
        let meta = self.meta(Some(services.join(", ")).filter(|s| !s.is_empty()));
        self.event(EventKind::DeployStarted, Severity::Info, message)
            .meta(meta)
    }

    /// The deployment committed at `now`: a rollback reads as one, with its target. Its
    /// duration runs from when it was accepted, or from its creation if that is not known.
    pub(super) fn succeeded(&self, now: Timestamp) -> NewEvent {
        let since = self.accepted_at.unwrap_or(self.created_at);
        let millis = now.as_millis().saturating_sub(since.as_millis());
        let took = Took(Duration::from_millis(u64::try_from(millis).unwrap_or(0)));
        let (kind, message) = match self.rollback_of {
            Some(earlier) => (
                EventKind::DeployRolledBack,
                format!(
                    "Rolled back {} to deployment #{earlier} in {took}",
                    self.stack
                ),
            ),
            None => (
                EventKind::DeploySucceeded,
                format!("Deployed {} in {took}", self.stack),
            ),
        };
        self.event(kind, Severity::Success, message)
            .meta(self.meta(None))
    }

    /// The deployment is live, but cleaning up the generation it replaced failed because of
    /// `reason`: old containers or ports may be left behind for the operator to deal with.
    fn live_with_warnings(&self, reason: Option<&str>) -> NewEvent {
        let what = self.what();
        let message = match reason {
            Some(reason) => format!("{what} live with warnings: {reason}"),
            None => format!("{what} live with warnings"),
        };
        let kind = match self.rollback_of {
            Some(_) => EventKind::DeployRolledBack,
            None => EventKind::DeploySucceeded,
        };
        self.event(kind, Severity::Warning, message)
            .meta(self.meta(None))
    }

    /// The deployment failed or was rejected because of `reason`. Cancelling it and removing
    /// its stack are things an operator did, so they read as warnings, not errors.
    pub(super) fn failed(&self, reason: Option<&str>) -> NewEvent {
        let what = self.what();
        let (severity, message) = match reason {
            Some(CANCELLED) => (Severity::Warning, format!("{what} cancelled")),
            Some(STACK_REMOVED) => (Severity::Warning, format!("{what} ended: stack removed")),
            Some(reason) => (Severity::Error, format!("{what} failed: {reason}")),
            None => (Severity::Error, format!("{what} failed")),
        };
        self.event(EventKind::DeployFailed, severity, message)
            .meta(self.meta(None))
    }

    /// "Deployment of X", or "Rollback of X to deployment #N".
    fn what(&self) -> String {
        match self.rollback_of {
            Some(earlier) => format!("Rollback of {} to deployment #{earlier}", self.stack),
            None => format!("Deployment of {}", self.stack),
        }
    }

    fn event(&self, kind: EventKind, severity: Severity, message: String) -> NewEvent {
        NewEvent::new(kind, severity, message)
            .stack(&self.stack)
            .subject(format!("/stacks/{}/deployments/{}", self.stack, self.id))
    }

    /// The mono line: the source, the commit if known, then `extra`.
    fn meta(&self, extra: Option<String>) -> String {
        let sha = self.sha.as_deref().map(|sha| ShortSha(sha).to_string());
        [Some(self.source.as_ref().to_owned()), sha, extra]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// The container lifecycle events of one service generation (ADR-0055 §2): a reconciler
/// restart, the service entering or leaving crash loop (ADR-0028), and a health change of the
/// live service (ADR-0038). Each links to the service's stack.
#[derive(Debug, Clone, Copy)]
pub(super) struct ServiceEvents<'a>(&'a ServiceGeneration);

impl<'a> From<&'a ServiceGeneration> for ServiceEvents<'a> {
    fn from(generation: &'a ServiceGeneration) -> Self {
        Self(generation)
    }
}

impl ServiceEvents<'_> {
    /// The reconciler restarted `container`, which had exited (as inspect reported it), on its
    /// `attempt`th restart attempt since hoster started (a failed attempt counts but writes no
    /// event, so the numbers can skip).
    pub(super) fn restarted(&self, container: &ContainerInfo, attempt: u32) -> NewEvent {
        let exit = match container.state {
            ContainerState::Exited(Some(code)) => format!("exit {code}"),
            state => state.to_string(),
        };
        let oom = if container.oom_killed {
            ", OOM killed"
        } else {
            ""
        };
        self.event(
            EventKind::ContainerRestarted,
            Severity::Warning,
            format!("Restarted {} ({exit}{oom})", self.name()),
        )
        .meta(format!("{} · attempt {attempt}", self.generation()))
    }

    /// The service restarted `restarts` times within `window`: it is crash looping, and the
    /// reconciler retries it at the maximum backoff.
    pub(super) fn crash_loop_entered(&self, restarts: usize, window: Duration) -> NewEvent {
        self.event(
            EventKind::CrashLoopEntered,
            Severity::Error,
            format!("{} is crash looping", self.name()),
        )
        .meta(format!(
            "{} · {restarts} restarts in {}",
            self.generation(),
            humantime::format_duration(window)
        ))
    }

    /// The service stayed up for a whole crash-loop window: its restart history reset.
    pub(super) fn crash_loop_left(&self) -> NewEvent {
        self.event(
            EventKind::CrashLoopLeft,
            Severity::Success,
            format!("{} stopped crash looping", self.name()),
        )
        .meta(self.generation())
    }

    /// The event a health `change` writes, if it writes one: only a move between healthy and
    /// unhealthy. The first verdict on a healthy service (from `Unknown`) is no change, and
    /// crash looping is written by the reconciler ([`Self::crash_loop_entered`] and
    /// [`Self::crash_loop_left`]), so the moves into it, out of it to `Healthy`, and on to
    /// `Unhealthy` while it recovers are left out.
    pub(super) fn health_changed(&self, change: &HealthChange) -> Option<NewEvent> {
        let (severity, message) = match (change.from, change.to) {
            (HealthStatus::Unknown | HealthStatus::Healthy, HealthStatus::Unhealthy) => {
                (Severity::Warning, format!("{} is unhealthy", self.name()))
            }
            (HealthStatus::Unhealthy, HealthStatus::Healthy) => (
                Severity::Success,
                format!("{} is healthy again", self.name()),
            ),
            _ => return None,
        };
        Some(
            self.event(EventKind::HealthChanged, severity, message)
                .meta(format!("{} · {}", self.generation(), change.detail)),
        )
    }

    /// The first pass after a (re)start that interrupted an unhealthy spell (`change` is from
    /// `Unknown`, which [`Self::health_changed`] leaves out): the service is healthy again.
    pub(super) fn healthy_again(&self, change: &HealthChange) -> NewEvent {
        self.event(
            EventKind::HealthChanged,
            Severity::Success,
            format!("{} is healthy again", self.name()),
        )
        .meta(format!("{} · {}", self.generation(), change.detail))
    }

    /// The reconciler started the container although the `dependencies` it waited for, which
    /// run, have not passed their checks within their grace (ADR-0056 Decision 3): one event
    /// per hold, of the reconciler-restart kind.
    pub(super) fn started_past_hold(&self, dependencies: &[Name]) -> NewEvent {
        let names: Vec<&str> = dependencies.iter().map(AsRef::as_ref).collect();
        let (them, have) = match names.len() {
            1 => ("its check", "has"),
            _ => ("their checks", "have"),
        };
        self.event(
            EventKind::ContainerRestarted,
            Severity::Warning,
            format!(
                "Started {} although {} {have} not passed {them}",
                self.name(),
                names.join(", ")
            ),
        )
        .meta(self.generation())
    }

    fn event(&self, kind: EventKind, severity: Severity, message: String) -> NewEvent {
        let stack = self.0.stack.as_ref();
        NewEvent::new(kind, severity, message)
            .stack(stack)
            .subject(format!("/stacks/{stack}"))
    }

    /// `stack/service`, as the messages name it.
    fn name(&self) -> String {
        format!("{}/{}", self.0.stack, self.0.service)
    }

    fn generation(&self) -> String {
        format!("generation {}", self.0.generation)
    }
}

/// The certificate events of one host (ADR-0055 §2): issued (the host's first certificate),
/// renewed, failed (with the reason and the next retry) and DNS mismatch. Each links to the
/// certificates page and belongs to the stack that routes the host, if one does (the admin
/// host belongs to none).
#[derive(Debug, Clone, Copy)]
pub(super) struct CertEvents<'a> {
    host: &'a str,
    stack: Option<&'a str>,
}

impl<'a> CertEvents<'a> {
    /// The page the events link to.
    const SUBJECT: &'static str = "/certificates";

    /// The events of `host`, routed by `stack`.
    pub(super) fn new(host: &'a str, stack: Option<&'a str>) -> Self {
        Self { host, stack }
    }

    /// A certificate valid until `not_after` was installed; `renewed` when it replaced one.
    pub(super) fn issued(&self, not_after: SystemTime, renewed: bool) -> NewEvent {
        let (kind, message) = if renewed {
            (
                EventKind::CertificateRenewed,
                format!("Renewed the certificate for {}", self.host),
            )
        } else {
            (
                EventKind::CertificateIssued,
                format!("Issued a certificate for {}", self.host),
            )
        };
        self.event(kind, Severity::Success, message).meta(format!(
            "valid until {}",
            humantime::format_rfc3339_seconds(not_after)
        ))
    }

    /// An attempt failed with `error`; the next one is not before `retry_at`.
    pub(super) fn failed(&self, error: &str, retry_at: SystemTime) -> NewEvent {
        self.event(
            EventKind::CertificateFailed,
            Severity::Error,
            format!("Certificate for {} failed: {error}", self.host),
        )
        .meta(format!(
            "next retry {}",
            humantime::format_rfc3339_seconds(retry_at)
        ))
    }

    /// The host's A/AAAA records do not point at hoster; issuance goes ahead (ADR-0051 #2).
    pub(super) fn dns_mismatch(&self, mismatch: &DnsMismatch) -> NewEvent {
        let observed = match mismatch.observed.as_slice() {
            [] => "no A/AAAA records".to_owned(),
            observed => format!("resolves to {}", Addresses(observed)),
        };
        self.event(
            EventKind::DnsMismatch,
            Severity::Warning,
            format!("DNS for {} does not point at this server", self.host),
        )
        .meta(format!(
            "{observed} · expected {}",
            Addresses(&mismatch.expected)
        ))
    }

    fn event(&self, kind: EventKind, severity: Severity, message: String) -> NewEvent {
        let event = NewEvent::new(kind, severity, message).subject(Self::SUBJECT);
        match self.stack {
            Some(stack) => event.stack(stack),
            None => event,
        }
    }
}

/// Who did an operator action, as the event log names them (ADR-0055): the GitHub handle the
/// admin signed in with (ADR-0031). An action without one (a test, a future CLI) is recorded
/// without a "by".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operator(String);

impl From<&str> for Operator {
    fn from(login: &str) -> Self {
        Self(login.to_owned())
    }
}

impl From<String> for Operator {
    fn from(login: String) -> Self {
        Self(login)
    }
}

/// `@login`, as GitHub writes a handle.
impl fmt::Display for Operator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "@{}", self.0)
    }
}

/// The operator events of one stack (ADR-0055 §2): a revision applied, the stack removed (with
/// whether its volumes went too), and a secret of it set or deleted, by name only. Each names
/// the operator who did it, if known, at the end of its meta line.
#[derive(Debug, Clone, Copy)]
pub(super) struct OperatorEvents<'a> {
    stack: &'a str,
    by: Option<&'a Operator>,
}

impl<'a> OperatorEvents<'a> {
    /// The events of `stack`, done by `by`.
    pub(super) fn new(stack: &'a str, by: Option<&'a Operator>) -> Self {
        Self { stack, by }
    }

    /// `revision` was applied as the stack's active one.
    pub(super) fn applied(&self, revision: &StackRevision) -> NewEvent {
        self.event(
            EventKind::StackApplied,
            Severity::Info,
            format!("Applied revision {} of {}", revision.revision, self.stack),
        )
        .subject(format!("/stacks/{}", self.stack))
        .meta(self.meta([
            format!("revision {}", revision.revision),
            ShortDigest(revision.hash.as_ref()).to_string(),
        ]))
    }

    /// The stack was removed; `volumes` are the named volumes deleted with it, if the removal
    /// asked for that. The stack's page is gone, so the event links to its event history.
    pub(super) fn removed(&self, delete_volumes: bool, volumes: &[String]) -> NewEvent {
        let (severity, message, detail) = match (delete_volumes, volumes) {
            (false, _) => (
                Severity::Info,
                format!("Removed {}; its volumes were kept", self.stack),
                "volumes kept".to_owned(),
            ),
            (true, []) => (
                Severity::Info,
                format!("Removed {}", self.stack),
                "no volumes".to_owned(),
            ),
            (true, volumes) => (
                Severity::Warning,
                format!("Removed {} and deleted its volumes", self.stack),
                format!("volumes deleted: {}", volumes.join(", ")),
            ),
        };
        self.event(EventKind::StackRemoved, severity, message)
            .subject(self.history())
            .meta(self.meta([detail]))
    }

    /// The kept volumes of the stack, removed earlier, were deleted (ADR-0030's separate
    /// action), or `None` when it had none left to delete. Like the removal, it links to the
    /// stack's event history.
    pub(super) fn volumes_deleted(&self, volumes: &[String]) -> Option<NewEvent> {
        (!volumes.is_empty()).then(|| {
            self.event(
                EventKind::StackRemoved,
                Severity::Warning,
                format!("Deleted the kept volumes of {}", self.stack),
            )
            .subject(self.history())
            .meta(self.meta([format!("volumes deleted: {}", volumes.join(", "))]))
        })
    }

    /// The stack's secret `name` was set; `replaced` when it existed before. Never the value.
    pub(super) fn secret_set(&self, name: &str, replaced: bool) -> NewEvent {
        let verb = if replaced { "Replaced" } else { "Set" };
        self.secret(EventKind::SecretSet, format!("{verb} the secret {name}"))
    }

    /// The stack's secret `name` was deleted.
    pub(super) fn secret_deleted(&self, name: &str) -> NewEvent {
        self.secret(
            EventKind::SecretDeleted,
            format!("Deleted the secret {name}"),
        )
    }

    fn secret(&self, kind: EventKind, what: String) -> NewEvent {
        self.event(kind, Severity::Info, format!("{what} of {}", self.stack))
            .subject(format!("/stacks/{}/environment", self.stack))
            .meta(self.meta([format!("scope {}", self.stack)]))
    }

    fn event(&self, kind: EventKind, severity: Severity, message: String) -> NewEvent {
        NewEvent::new(kind, severity, message).stack(self.stack)
    }

    /// The Events page filtered to the stack (the web layer's `EventsPage::url`): the one
    /// page about it that outlives its removal.
    fn history(&self) -> String {
        format!("/activity?stack={}", self.stack)
    }

    /// `parts`, then who did it, as one mono line.
    fn meta(&self, parts: impl IntoIterator<Item = String>) -> String {
        parts
            .into_iter()
            .chain(self.by.map(|by| format!("by {by}")))
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// The events of one registry credential of Settings → Registries (ADR-0025): set, replaced
/// or deleted. A credential is a secret in the same store as a stack's (ADR-0029), so these
/// are secret events, but of no stack; they name the host, never the username or token, and
/// link to the registries page.
#[derive(Debug, Clone, Copy)]
pub struct RegistryEvents<'a> {
    host: &'a str,
    by: Option<&'a Operator>,
}

impl<'a> RegistryEvents<'a> {
    /// The page the events link to.
    const SUBJECT: &'static str = "/settings/registries";

    /// The events of the credential for `host`, done by `by`.
    pub fn new(host: &'a str, by: Option<&'a Operator>) -> Self {
        Self { host, by }
    }

    /// The credential was stored; `replaced` when one existed before.
    pub fn set(&self, replaced: bool) -> NewEvent {
        let verb = if replaced { "Replaced" } else { "Set" };
        self.event(
            EventKind::SecretSet,
            format!("{verb} the registry credential for {}", self.host),
        )
    }

    /// The credential was deleted.
    pub fn deleted(&self) -> NewEvent {
        self.event(
            EventKind::SecretDeleted,
            format!("Deleted the registry credential for {}", self.host),
        )
    }

    fn event(&self, kind: EventKind, message: String) -> NewEvent {
        let by = self.by.map(|by| format!(" · by {by}")).unwrap_or_default();
        NewEvent::new(kind, Severity::Info, message)
            .subject(Self::SUBJECT)
            .meta(format!("scope registry{by}"))
    }
}

/// The events of hoster itself (ADR-0055 §2): it started, it updated itself, or it rolled
/// back to its previous version (ADR-0052). They belong to no stack and link to the System
/// page; the meta line holds the versions.
#[derive(Debug, Clone, Copy)]
pub struct HosterEvents;

impl HosterEvents {
    /// The page the events link to.
    const SUBJECT: &'static str = "/system";

    /// hoster `version` started (once per process start).
    pub fn started(version: &Version) -> NewEvent {
        NewEvent::new(
            EventKind::HosterStarted,
            Severity::Info,
            format!("hoster {version} started"),
        )
        .subject(Self::SUBJECT)
        .meta(version.to_string())
    }

    /// `current` switched from `from` to `to`, done `by` the operator who asked for it from the
    /// System page, if any; the restart that follows runs `to`.
    pub fn updated(from: &Version, to: &Version, by: Option<&Operator>) -> NewEvent {
        let by = by.map(|by| format!(" · by {by}")).unwrap_or_default();
        NewEvent::new(
            EventKind::HosterUpdated,
            Severity::Success,
            format!("Updated hoster to {to}"),
        )
        .subject(Self::SUBJECT)
        .meta(format!("{from} → {to}{by}"))
    }

    /// `hoster rollback` returns from `from` to the previous version `to`.
    pub fn rolled_back(from: &Version, to: &Version) -> NewEvent {
        NewEvent::new(
            EventKind::HosterRolledBack,
            Severity::Warning,
            format!("Rolled hoster back to {to}"),
        )
        .subject(Self::SUBJECT)
        .meta(format!("{from} → {to}"))
    }
}

/// The events of notifications (ADR-0058 §9): a mail that could not be delivered, with the
/// provider's reason. It belongs to no stack, links to Settings → Notifications and is never
/// mailed itself.
#[derive(Debug, Clone, Copy)]
pub struct NotificationEvents;

impl NotificationEvents {
    /// The page the events link to.
    const SUBJECT: &'static str = "/settings/notifications";

    /// A notification mail was dropped because of `reason`, the delivery error as
    /// `notify::DeliveryError` writes it (it never holds the key).
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "recorded by the notifier as it lands")
    )]
    pub fn failed(reason: &str) -> NewEvent {
        NewEvent::new(
            EventKind::NotificationFailed,
            Severity::Warning,
            "A notification mail could not be delivered",
        )
        .subject(Self::SUBJECT)
        .meta(reason)
    }
}

/// Addresses as a comma-separated list, or "none".
struct Addresses<'a>(&'a [IpAddr]);

impl fmt::Display for Addresses<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("none");
        }
        for (i, address) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            address.fmt(f)?;
        }
        Ok(())
    }
}

/// How long a deployment took: whole seconds, or milliseconds under one.
struct Took(Duration);

impl fmt::Display for Took {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rounded = if self.0 < Duration::from_secs(1) {
            Duration::from_millis(self.0.subsec_millis().into())
        } else {
            Duration::from_secs(self.0.as_secs())
        };
        humantime::format_duration(rounded).fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::store::{EventKind, Severity};

    async fn log() -> (TempDir, EventLog, broadcast::Receiver<UiEvent>) {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let (tx, rx) = broadcast::channel(16);
        (dir, EventLog::new(store, tx), rx)
    }

    fn webhook_deploy(created_at: Timestamp) -> DeployEvents {
        DeployEvents {
            id: DeploymentId::from(7),
            stack: "notes".into(),
            source: DeploymentSource::Webhook,
            sha: Some("0f3c5e1a9d2b7c4e8f6a1b3d5c7e9f0a2b4c6d8e".into()),
            created_at,
            accepted_at: None,
            rollback_of: None,
        }
    }

    #[test]
    fn the_meta_holds_the_source_and_the_short_sha() {
        let event = webhook_deploy(Timestamp::now()).failed(None);
        assert_eq!(event.meta.as_deref(), Some("webhook · 0f3c5e1"));
    }

    #[test]
    fn success_is_timed_from_acceptance_not_from_queueing() {
        let now = Timestamp::now();
        let mut events = webhook_deploy(now - Duration::from_secs(600));
        events.on(DeploymentState::Validating, None, []);
        let succeeded = events.succeeded(events.accepted_at.unwrap() + Duration::from_secs(20));
        assert_eq!(succeeded.message, "Deployed notes in 20s");
    }

    #[test]
    fn success_without_acceptance_is_timed_from_creation() {
        let now = Timestamp::now();
        let events = webhook_deploy(now);
        let succeeded = events.succeeded(now + Duration::from_secs(90));
        assert_eq!(succeeded.message, "Deployed notes in 1m 30s");
    }

    #[test]
    fn hoster_events_name_their_versions_and_link_to_the_system_page() {
        let started = HosterEvents::started(&"0.3.0".parse().unwrap());
        assert_eq!(started.kind, EventKind::HosterStarted);
        assert_eq!(started.message, "hoster v0.3.0 started");
        assert_eq!(started.meta.as_deref(), Some("v0.3.0"));
        assert_eq!(started.stack, None);
        assert_eq!(started.subject.as_deref(), Some("/system"));
        let (from, to) = ("0.2.0".parse().unwrap(), "0.3.0".parse().unwrap());
        let updated = HosterEvents::updated(&from, &to, None);
        assert_eq!(updated.message, "Updated hoster to v0.3.0");
        assert_eq!(updated.meta.as_deref(), Some("v0.2.0 → v0.3.0"));
        let by = Operator::from("octocat");
        let updated = HosterEvents::updated(&from, &to, Some(&by));
        assert_eq!(
            updated.meta.as_deref(),
            Some("v0.2.0 → v0.3.0 · by @octocat")
        );
        let rolled_back = HosterEvents::rolled_back(&to, &from);
        assert_eq!(rolled_back.severity, Severity::Warning);
        assert_eq!(rolled_back.message, "Rolled hoster back to v0.2.0");
        assert_eq!(rolled_back.meta.as_deref(), Some("v0.3.0 → v0.2.0"));
    }

    #[test]
    fn a_failed_notification_is_a_warning_with_the_reason_linking_to_its_settings() {
        let reason = "Resend answered 403 Forbidden (validation_error): domain not verified";
        let failed = NotificationEvents::failed(reason);
        assert_eq!(failed.kind, EventKind::NotificationFailed);
        assert_eq!(failed.severity, Severity::Warning);
        assert_eq!(failed.stack, None);
        assert_eq!(failed.subject.as_deref(), Some("/settings/notifications"));
        assert_eq!(failed.meta.as_deref(), Some(reason));
    }

    #[test]
    fn removal_events_link_to_the_stack_history_not_its_gone_page() {
        let by = Operator::from("octocat");
        let events = OperatorEvents::new("notes", Some(&by));
        for event in [
            events.removed(false, &[]),
            events.removed(true, &["notes_data".into()]),
            events.volumes_deleted(&["notes_data".into()]).unwrap(),
        ] {
            assert_eq!(event.kind, EventKind::StackRemoved);
            assert_eq!(event.subject.as_deref(), Some("/activity?stack=notes"));
        }
    }

    #[test]
    fn an_operator_reads_as_a_github_handle() {
        assert_eq!(Operator::from("octocat").to_string(), "@octocat");
    }

    #[test]
    fn registry_events_name_the_host_and_belong_to_no_stack() {
        let by = Operator::from("octocat");
        let set = RegistryEvents::new("ghcr.io", Some(&by)).set(false);
        assert_eq!(set.kind, EventKind::SecretSet);
        assert_eq!(set.message, "Set the registry credential for ghcr.io");
        assert_eq!(set.stack, None);
        assert_eq!(set.subject.as_deref(), Some("/settings/registries"));
        assert_eq!(set.meta.as_deref(), Some("scope registry · by @octocat"));
        let replaced = RegistryEvents::new("ghcr.io", None).set(true);
        assert_eq!(
            replaced.message,
            "Replaced the registry credential for ghcr.io"
        );
        assert_eq!(replaced.meta.as_deref(), Some("scope registry"));
        let deleted = RegistryEvents::new("ghcr.io", Some(&by)).deleted();
        assert_eq!(deleted.kind, EventKind::SecretDeleted);
        assert_eq!(
            deleted.message,
            "Deleted the registry credential for ghcr.io"
        );
    }

    #[tokio::test]
    async fn recorded_events_are_stored_then_announced() {
        let (_dir, log, mut rx) = log().await;
        log.record(
            NewEvent::new(EventKind::StackApplied, Severity::Info, "applied notes").stack("notes"),
        )
        .await;
        let UiEvent::Event(announced) = rx.try_recv().unwrap() else {
            panic!("not an event");
        };
        assert_eq!(announced.kind, EventKind::StackApplied);
        let page = log.page(&EventQuery::default()).await.unwrap();
        assert_eq!(page.events, [announced]);
    }

    #[tokio::test]
    async fn a_failed_write_is_not_announced_and_does_not_fail_the_caller() {
        let (_dir, log, mut rx) = log().await;
        log.store.close().await;
        log.record(NewEvent::new(
            EventKind::HosterStarted,
            Severity::Info,
            "started",
        ))
        .await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn retention_prunes_at_startup_and_stops_on_cancel() {
        let (_dir, log, _rx) = log().await;
        let old = NewEvent::new(EventKind::HosterStarted, Severity::Info, "long ago");
        let day = Duration::from_secs(24 * 60 * 60);
        let now = Timestamp::now();
        log.store
            .record_event_at(&old, now - EventLog::RETENTION - day)
            .await
            .unwrap();
        let kept = log
            .store
            .record_event_at(&old, now - EventLog::RETENTION + day)
            .await
            .unwrap();

        let cancel = CancellationToken::new();
        let task = tokio::spawn(log.clone().run_retention(cancel.clone()));
        let pruned = async {
            loop {
                let page = log.page(&EventQuery::default()).await.unwrap();
                if page.events.len() == 1 {
                    return page.events;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let events = tokio::time::timeout(Duration::from_secs(5), pruned)
            .await
            .expect("the startup sweep prunes the old event");
        assert_eq!(events, [kept]);
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("cancel stops the retention task")
            .unwrap();
    }
}
