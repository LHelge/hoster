//! The System page (ADR-0052): a version card (the running version with its target, schema and
//! hoster's uptime, the installed current and previous versions), an update card for "check for
//! updates" and "Apply update", the host's facts ([`HostFacts`], read on every render) and the
//! log store's size, reach and losses ([`LogFacts`], ADR-0067, likewise).
//!
//! - Checking asks GitHub only when the button is clicked (`POST /system/check`); the release
//!   notes are shown as escaped preformatted text, never as HTML from the release body.
//! - Updating (`POST /system/update`) is refused with a 409 while any deployment is in
//!   progress, while another update runs, and once one was applied and hoster is restarting;
//!   the button is disabled with the reason, and the server checks again. After the
//!   updater switched `current`, hoster shuts down gracefully through the supervisor
//!   ([`ShutdownRequest`], the same path as SIGTERM) so systemd starts the new binary. The
//!   response polls `GET /system/version` until a different version answers, then reloads.
//! - The restart step list names only what the updater did (verified, installed, snapshotted,
//!   switched, pruned) and the restart that follows; it invents no health checks.
//! - Rollback is CLI-only (`hoster rollback`); the page says how.
//!
//! Every update error is shown inline; the updater applies nothing on error.

use std::sync::Arc;
use std::time::SystemTime;

use askama::Template;
use axum::Extension;
use axum::extract::{Form, Query};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use self::host::Fact;
use super::auth::Session;
use super::pages::{Layout, Page, Section, Shell};
use super::stacks::Outcome;
use super::ui::Uptime;
use crate::installation::Version;
use crate::orchestrator::Operator;
use crate::supervisor::ShutdownRequest;
use crate::update::{
    InstalledVersions, SourceError, UpdateCheck, UpdateError, UpdateOutcome, Updater,
};

/// The System page's handlers, sharing the updater and the shutdown request as an extension.
#[derive(Clone)]
pub struct System(Arc<Inner>);

struct Inner {
    updater: Updater,
    shutdown: ShutdownRequest,
    host: Arc<dyn HostFacts>,
    /// The log store's figures; `None` leaves its card out (tests that do not need it).
    logs: Option<Arc<dyn LogFacts>>,
    /// When this hoster started, for its uptime.
    started: SystemTime,
    /// Held while an update runs: a second one (a double submit, another tab) is refused
    /// instead of racing it over the same `versions/` directory and `current` link.
    updating: tokio::sync::Mutex<()>,
}

/// Why the Update button is disabled.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Blocked {
    Deploying(usize),
    NotInstalled,
    /// An update was applied and hoster is shutting down to restart into it.
    Restarting,
    /// Another update is running right now.
    Updating,
    Unknown(String),
}

impl std::fmt::Display for Blocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Blocked::Deploying(count) => write!(
                f,
                "{count} deployment(s) are in progress. Updating is refused until they finish \
                 (or are cancelled); check again then."
            ),
            Blocked::NotInstalled => f.write_str(
                "This hoster was not installed with install.sh: the binary layout has no \
                 current version, so an update would not be the binary systemd runs.",
            ),
            Blocked::Restarting => f.write_str(
                "An update was applied and hoster is restarting into it; reload this page once \
                 it is back.",
            ),
            Blocked::Updating => f.write_str(
                "Another update is running right now; wait for it to finish and reload this page.",
            ),
            Blocked::Unknown(reason) => write!(f, "Cannot tell whether updating is safe: {reason}"),
        }
    }
}

/// The form of the Update button.
#[derive(Deserialize)]
pub struct UpdateForm {
    #[serde(default)]
    version: String,
}

/// The restart poll's query: the version that was running when the update was applied.
#[derive(Deserialize)]
pub struct VersionQuery {
    #[serde(default)]
    from: String,
}

impl System {
    /// The page for `updater`, showing the facts of `host`; a successful update requests
    /// `shutdown`. hoster's uptime counts from now.
    pub fn new(updater: Updater, shutdown: ShutdownRequest, host: Arc<dyn HostFacts>) -> Self {
        Self(Arc::new(Inner {
            updater,
            shutdown,
            host,
            logs: None,
            started: SystemTime::now(),
            updating: tokio::sync::Mutex::new(()),
        }))
    }

    /// The same page with the log store card, showing `logs`.
    pub fn with_logs(mut self, logs: Arc<dyn LogFacts>) -> Self {
        Arc::get_mut(&mut self.0)
            .expect("set before the page is shared")
            .logs = Some(logs);
        self
    }

    /// The same page, with hoster started at `started` (the UI preview).
    #[cfg(test)]
    pub fn started_at(mut self, started: SystemTime) -> Self {
        Arc::get_mut(&mut self.0)
            .expect("set before the page is shared")
            .started = started;
        self
    }

    fn updater(&self) -> &Updater {
        &self.0.updater
    }

    fn running(&self) -> Version {
        self.updater().running().version.clone().into()
    }

    /// Why updating is not possible right now, if it is not.
    async fn blocked(&self) -> Option<Blocked> {
        if self.0.shutdown.is_requested() {
            return Some(Blocked::Restarting);
        }
        match self.updater().installed() {
            Ok(InstalledVersions { current: None, .. }) => return Some(Blocked::NotInstalled),
            Ok(_) => {}
            Err(err) => return Some(Blocked::Unknown(Self::describe(&err))),
        }
        match self.updater().deployments_in_progress().await {
            Ok(0) => None,
            Ok(count) => Some(Blocked::Deploying(count)),
            Err(err) => Some(Blocked::Unknown(Self::describe(&err))),
        }
    }

    /// `GET /system`.
    pub async fn page(Extension(system): Extension<System>, shell: Shell) -> Response {
        let info = system.updater().running();
        let (installed, problem) = match system.updater().installed() {
            Ok(installed) => (installed, None),
            Err(err) => (
                InstalledVersions::default(),
                Some(format!(
                    "Cannot read the installed versions: {}",
                    Self::describe(&err)
                )),
            ),
        };
        let up = SystemTime::now()
            .duration_since(system.0.started)
            .unwrap_or_default();
        Page(SystemPage {
            layout: shell.layout(Section::System),
            running: system.running().to_string(),
            target: info.target.clone(),
            schema: info.schema,
            up: Uptime(up),
            current: installed.current.map(|v| v.to_string()),
            previous: installed.previous.map(|v| v.to_string()),
            problem,
            host: system.0.host.read().await.rows(),
            logs: match &system.0.logs {
                Some(logs) => Some(LogCard::new(logs.read().await, SystemTime::now())),
                None => None,
            },
        })
        .into_response()
    }

    /// `POST /system/check`: the latest stable release against the running version. This is
    /// the only place hoster talks to GitHub's releases API on its own (ADR-0052).
    pub async fn check(Extension(system): Extension<System>) -> Response {
        let check: UpdateCheck = match system.updater().check().await {
            Ok(check) => check,
            Err(err) => return Self::failed(&err).into_response(),
        };
        if !check.update_available() {
            return Outcome::success(format!(
                "hoster {} is up to date (the latest stable release is {}).",
                check.running, check.latest
            ))
            .informing()
            .into_response();
        }
        Page(CheckResult {
            running: check.running.to_string(),
            latest: check.latest.to_string(),
            released: check.published.map(Self::date),
            page: check.page.map(String::from),
            notes: check.notes,
            blocked: system.blocked().await.map(|b| b.to_string()),
        })
        .into_response()
    }

    /// `POST /system/update`: installs and switches to the version the button named, then shuts
    /// hoster down gracefully so systemd starts it.
    pub async fn update(
        Extension(system): Extension<System>,
        Extension(session): Extension<Session>,
        Form(form): Form<UpdateForm>,
    ) -> Response {
        let version: Version = match form.version.trim().parse() {
            Ok(version) => version,
            Err(_) => {
                return Outcome::error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("{:?} is not a hoster version.", form.version),
                )
                .into_response();
            }
        };
        let Ok(_updating) = system.0.updating.try_lock() else {
            return Outcome::error(StatusCode::CONFLICT, Blocked::Updating.to_string())
                .into_response();
        };
        match system.blocked().await {
            None => {}
            Some(Blocked::Deploying(count)) => {
                return Self::failed(&UpdateError::Deploying { count }).into_response();
            }
            Some(blocked) => {
                return Outcome::error(StatusCode::CONFLICT, blocked.to_string()).into_response();
            }
        }
        let by = Operator::from(session.login());
        match system.updater().update_by(Some(&version), Some(&by)).await {
            Ok(UpdateOutcome::UpToDate { running }) => Outcome::success(format!(
                "hoster {running} is already running; nothing to do."
            ))
            .informing()
            .into_response(),
            Ok(UpdateOutcome::Switched {
                from,
                to,
                snapshot,
                pruned,
            }) => {
                // The response still reaches the browser: a graceful shutdown lets in-flight
                // admin requests finish before the proxy stops.
                system.0.shutdown.request();
                Page(Restarting {
                    from: from.to_string(),
                    to: to.to_string(),
                    binary: crate::update::Release::binary_name(&system.updater().running().target),
                    snapshot: snapshot.map(|path| path.display().to_string()),
                    pruned: pruned.iter().map(ToString::to_string).collect(),
                })
                .into_response()
            }
            Err(err) => Self::failed(&err).into_response(),
        }
    }

    /// `GET /system/version?from=vX.Y.Z`: polled while hoster restarts. Answers `HX-Refresh`
    /// (reload the page) once the running version is no longer `from`, and 204 before that.
    pub async fn version(
        Extension(system): Extension<System>,
        Query(query): Query<VersionQuery>,
    ) -> Response {
        let running = system.running();
        let restarted = match query.from.parse::<Version>() {
            Ok(from) => from != running,
            Err(_) => true,
        };
        if restarted {
            (
                [(
                    HeaderName::from_static("hx-refresh"),
                    HeaderValue::from_static("true"),
                )],
                running.to_string(),
            )
                .into_response()
        } else {
            StatusCode::NO_CONTENT.into_response()
        }
    }

    /// The day of `time` (`2026-11-06`, UTC).
    fn date(time: SystemTime) -> String {
        let mut text = humantime::format_rfc3339_seconds(time).to_string();
        text.truncate(10);
        text
    }

    /// An error with its sources, for showing inline.
    fn describe(err: &dyn std::error::Error) -> String {
        let mut text = err.to_string();
        let mut source = err.source();
        while let Some(cause) = source {
            let message = cause.to_string();
            if !text.contains(&message) {
                text = format!("{text}: {message}");
            }
            source = cause.source();
        }
        text
    }

    /// A failed check or update, shown inline. Nothing was applied.
    fn failed(err: &UpdateError) -> Outcome {
        use UpdateError as E;
        let status = match err {
            E::Deploying { .. } => StatusCode::CONFLICT,
            E::Downgrade { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            E::Source(SourceError::UnknownRelease(_)) => StatusCode::NOT_FOUND,
            E::Source(_)
            | E::MissingAsset { .. }
            | E::MissingChecksum { .. }
            | E::Checksums(_)
            | E::ChecksumMismatch { .. } => StatusCode::BAD_GATEWAY,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let message = Self::describe(err);
        if status.is_server_error() {
            tracing::warn!(error = %message, "a self-update failed");
        }
        Outcome::error(status, Outcome::sentence(&message))
    }
}

/// `GET /system`.
#[derive(Template)]
#[template(path = "system.html")]
pub struct SystemPage {
    layout: Layout,
    running: String,
    target: String,
    schema: i64,
    /// How long this hoster has been running.
    up: Uptime,
    current: Option<String>,
    previous: Option<String>,
    /// Shown when the installed versions cannot be read.
    problem: Option<String>,
    host: Vec<Fact>,
    logs: Option<LogCard>,
}

impl SystemPage {
    /// The manual recovery guide, for when `hoster rollback` cannot run.
    const RECOVERY: &str = "https://docs.hoster.lhelge.se/wiki/install/#manual-recovery";
}

/// `POST /system/check` when a newer release exists.
#[derive(Template)]
#[template(path = "system_check.html")]
pub struct CheckResult {
    running: String,
    latest: String,
    /// The day it was published, if known.
    released: Option<String>,
    /// Its page on GitHub with the full notes, if known.
    page: Option<String>,
    /// The release notes, rendered escaped as preformatted text.
    notes: String,
    /// Why the Update button is disabled.
    blocked: Option<String>,
}

/// `POST /system/update` after `current` was switched.
#[derive(Template)]
#[template(path = "system_restarting.html")]
pub struct Restarting {
    from: String,
    to: String,
    /// The binary that was downloaded and verified (`hoster-<target>`).
    binary: String,
    snapshot: Option<String>,
    /// The old versions removed after the switch.
    pruned: Vec<String>,
}

mod host;
mod logs;

#[cfg(test)]
pub use host::Facts;
pub use host::{HostFacts, LocalHost};
use logs::LogCard;
pub use logs::{LocalLogs, LogFacts, LogStoreState};
#[cfg(test)]
pub use logs::{LogFigures, LogHealth};

#[cfg(test)]
mod tests;
