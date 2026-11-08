//! The System page (ADR-0052): the running version, target and schema, the installed current
//! and previous versions, "Check for updates" and "Update to vX.Y.Z".
//!
//! - Checking asks GitHub only when the button is clicked (`POST /system/check`); the release
//!   notes are shown as escaped preformatted text, never as HTML from the release body.
//! - Updating (`POST /system/update`) is refused with a 409 while any deployment is in
//!   progress, while another update runs, and once one was applied and hoster is restarting;
//!   the button is disabled with the reason, and the server checks again. After the
//!   updater switched `current`, hoster shuts down gracefully through the supervisor
//!   ([`ShutdownRequest`], the same path as SIGTERM) so systemd starts the new binary. The
//!   response polls `GET /system/version` until a different version answers, then reloads.
//! - Rollback is CLI-only (`hoster rollback`); the page says how.
//!
//! Every update error is shown inline; the updater applies nothing on error.

use std::sync::Arc;

use askama::Template;
use axum::Extension;
use axum::extract::{Form, Query};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::pages::{Layout, Page, Section, Shell};
use super::stacks::Outcome;
use crate::installation::Version;
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
    /// The page for `updater`; a successful update requests `shutdown`.
    pub fn new(updater: Updater, shutdown: ShutdownRequest) -> Self {
        Self(Arc::new(Inner {
            updater,
            shutdown,
            updating: tokio::sync::Mutex::new(()),
        }))
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
        Page(SystemPage {
            layout: shell.layout(Section::System),
            running: system.running().to_string(),
            target: info.target.clone(),
            schema: info.schema,
            current: installed.current.map(|v| v.to_string()),
            previous: installed.previous.map(|v| v.to_string()),
            problem,
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
            notes: check.notes,
            blocked: system.blocked().await.map(|b| b.to_string()),
        })
        .into_response()
    }

    /// `POST /system/update`: installs and switches to the version the button named, then shuts
    /// hoster down gracefully so systemd starts it.
    pub async fn update(
        Extension(system): Extension<System>,
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
        match system.updater().update(Some(&version)).await {
            Ok(UpdateOutcome::UpToDate { running }) => Outcome::success(format!(
                "hoster {running} is already running; nothing to do."
            ))
            .informing()
            .into_response(),
            Ok(UpdateOutcome::Switched {
                from, to, snapshot, ..
            }) => {
                // The response still reaches the browser: a graceful shutdown lets in-flight
                // admin requests finish before the proxy stops.
                system.0.shutdown.request();
                Page(Restarting {
                    from: from.to_string(),
                    to: to.to_string(),
                    snapshot: snapshot.map(|path| path.display().to_string()),
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
    current: Option<String>,
    previous: Option<String>,
    /// Shown when the installed versions cannot be read.
    problem: Option<String>,
}

/// `POST /system/check` when a newer release exists.
#[derive(Template)]
#[template(path = "system_check.html")]
pub struct CheckResult {
    running: String,
    latest: String,
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
    snapshot: Option<String>,
}

#[cfg(test)]
mod tests;
