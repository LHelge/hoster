//! A stack's logs tab (ADR-0067, ADR-0068, epic psq): the stack's records from the log store
//! under the header and tab bar every stack page shares. It mounts the stored logs view of
//! `web/logs.rs` with the stack fixed ([`LogScope::Stack`]): every service merged oldest to
//! newest with a coloured service column, hoster's own records about the stack under
//! `hoster`, a chip per service (the current revision's, and any with records in the range),
//! the *since* presets and free text, and "older"/"newer" paging by record key. Records of
//! replaced and removed containers are listed too, with their generation where a service has
//! more than one in the range.
//!
//! The newest page follows the stack's new records (`/stacks/{name}/logs/stream`, see
//! [`LogFollow`]) unless switched off with `follow=off`; paging back into history stops it,
//! and the "newer" and "newest" links back to the newest page start it again. The stream ends
//! when the stack is removed or the session ends.
//!
//! The toolbar also links each of the stack's containers to its live log
//! (`/containers/{name}/logs`, ADR-0044), which streams straight from podman: it shows a line
//! before the recorder has committed it, and it is what is left when the store cannot be read
//! (ADR-0067 §10). An unknown stack is a 404, even when the store still holds records under
//! its name.
//!
//! A source switch above the toolbar picks where the records come from (`source`,
//! [`LogSource`]): the containers (the default, everything above), the access log of the
//! stack's routes (the proxy's request rows in the log store, `web/logs/access.rs`, with
//! service, status, path and duration filters), or the stack's deployment events ([`deploy`]),
//! which take the range, grep, paging and following but no service, level or field filters.
//! Only the containers link live logs.
//!
//! The containers' and the access log's toolbars have a download button
//! (`/stacks/{name}/logs/download`, see [`LogDownload`]): every record or request the tab's
//! options select over the whole range, oldest first, as a text or JSON lines file, up to
//! [`LogDownload::CAP`] of them.

mod deploy;

use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use axum::Extension;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use tokio::sync::broadcast;

use super::Stacks;
use super::frame::{StackHeader, Tab};
use super::overview::{LogCandidate, StackContainers, StackView};
use crate::orchestrator::{StackStatus, UiEvent};
use crate::store::EventKind;
use crate::web::auth::{Auth, Session};
use crate::web::containers::Containers;
use crate::web::logs::{
    AccessTail, EmptyNote, FormatParam, LiveLink, LogDownload, LogFollow, LogMount, LogParams,
    LogReader, LogScope, LogSource, LogsView, Refused, SourceFollow,
};
use crate::web::pages::{Flash, FlashKind, Layout, Page, Section, Shell};
use deploy::DeployLog;

impl Stacks {
    /// `GET /stacks/{name}/logs[?service=…&since=…&before=…|after=…]`: the logs tab; an
    /// unknown stack is a 404.
    pub async fn logs(
        Extension(stacks): Extension<Stacks>,
        Extension(containers): Extension<Containers>,
        Extension(reader): Extension<LogReader>,
        shell: Shell,
        Path(name): Path<String>,
        Query(params): Query<LogParams>,
    ) -> Response {
        let mut layout = shell.layout(Section::Stacks);
        let status = match stacks.find(&name).await {
            Ok(status) => status,
            Err(outcome) => return outcome.page(layout),
        };
        let (source, _) = params.source(&LogScope::Stack(name.clone()));
        if source == LogSource::Access {
            let empty = EmptyNote {
                text: "No requests to the stack's routes in this range.".to_owned(),
                link: "overview".to_owned(),
                href: StackView::url(&name),
            };
            let mount = Self::log_mount(&status, Vec::new());
            let view = LogsView::access(&reader, mount, &params, empty).await;
            if let Some(message) = view.problem.clone() {
                layout = layout.with_flash(Flash {
                    kind: FlashKind::Error,
                    message,
                });
            }
            return Page(StackLogsPage {
                layout,
                header: stacks.header(&status, Tab::Logs).await,
                view,
            })
            .into_response();
        }
        if source == LogSource::Deploy {
            let mount = Self::log_mount(&status, Vec::new());
            let view = DeployLog::new(stacks.clone(), name)
                .view(mount, &params)
                .await;
            if let Some(message) = view.problem.clone().or(view.unreadable.clone()) {
                layout = layout.with_flash(Flash {
                    kind: FlashKind::Error,
                    message,
                });
            }
            return Page(StackLogsPage {
                layout,
                header: stacks.header(&status, Tab::Logs).await,
                view,
            })
            .into_response();
        }
        let (live, listing) = match containers.of_stack(&name).await {
            Ok(list) => (
                StackContainers::from(list)
                    .live_logs(&status)
                    .iter()
                    .map(LiveLink::from)
                    .collect(),
                None,
            ),
            Err(err) => {
                tracing::error!(stack = %name, error = %err, "listing the stack's containers failed");
                (
                    Vec::new(),
                    Some(format!("Cannot list the stack's containers: {err}")),
                )
            }
        };
        let view = LogsView::load(&reader, Self::log_mount(&status, live), &params).await;
        if let Some(message) = view.problem.clone().or(listing) {
            layout = layout.with_flash(Flash {
                kind: FlashKind::Error,
                message,
            });
        }
        Page(StackLogsPage {
            layout,
            header: stacks.header(&status, Tab::Logs).await,
            view,
        })
        .into_response()
    }

    /// `GET /stacks/{name}/logs/stream?from=…&…`: the newest page's follow stream, with the
    /// options of the page; a `Last-Event-ID` header (sent by the SSE extension on reconnect)
    /// resumes after that record. An unknown stack is a 404.
    pub async fn logs_stream(
        State(auth): State<Arc<Auth>>,
        Extension(stacks): Extension<Stacks>,
        Extension(reader): Extension<LogReader>,
        Extension(session): Extension<Session>,
        Path(name): Path<String>,
        Query(params): Query<LogParams>,
        headers: HeaderMap,
    ) -> Response {
        // Subscribed before the stack is looked up, so a removal in between is seen.
        let events = stacks.subscribe();
        let status = match stacks.find(&name).await {
            Ok(status) => status,
            Err(outcome) => return outcome.into_response(),
        };
        let last = headers.get("last-event-id").and_then(|id| id.to_str().ok());
        let mount = Self::log_mount(&status, Vec::new());
        let (source, _) = params.source(&mount.scope);
        let tail = DeployLog::new(stacks.clone(), status.name.clone());
        let deploy_events = stacks.subscribe();
        let ends = async move {
            tokio::select! {
                ended = auth.session_ended(&session, Self::SESSION_CHECK) => match ended {
                    Ok(()) => "Signed out: sign in again to follow the log.".to_owned(),
                    Err(err) => {
                        tracing::error!(error = %err, "checking the session of a log stream failed");
                        "Cannot check the session: reload the page to follow the log.".to_owned()
                    }
                },
                () = stacks.removal(events, name) => "The stack was removed.".to_owned(),
            }
        };
        match source {
            LogSource::Containers => {
                let opened = LogFollow::open(&reader, &mount, &params, last).await;
                LogFollow::respond(opened, ends)
            }
            LogSource::Access => {
                let opened = AccessTail::open(&reader, &mount, &params).and_then(|tail| {
                    SourceFollow::open(&reader, &mount, &params, source, last, tail)
                });
                SourceFollow::respond(opened, ends)
            }
            LogSource::Deploy => {
                let tail = tail.tail(&params, deploy_events);
                let opened = SourceFollow::open(&reader, &mount, &params, source, last, tail);
                SourceFollow::respond(opened, ends)
            }
        }
    }

    /// `GET /stacks/{name}/logs/download?format=text|jsonl&…`: the records the tab's options
    /// select over the whole range, as a file, streamed. An unknown stack is a 404, an option
    /// the download cannot take (a pattern that does not compile, an unknown format, the
    /// deploy source) a 400, an unreadable store a 503.
    pub async fn logs_download(
        Extension(stacks): Extension<Stacks>,
        Extension(reader): Extension<LogReader>,
        Path(name): Path<String>,
        Query(params): Query<LogParams>,
        Query(format): Query<FormatParam>,
    ) -> Response {
        let status = match stacks.find(&name).await {
            Ok(status) => status,
            Err(outcome) => return outcome.into_response(),
        };
        let opened = format
            .format()
            .map_err(Refused::Invalid)
            .and_then(|format| LogDownload::open(&reader, &status.name, &params, format));
        LogDownload::respond(opened)
    }

    /// How often a follow stream checks that its session is still signed in.
    const SESSION_CHECK: Duration = if cfg!(test) {
        Duration::from_millis(20)
    } else {
        Duration::from_secs(30)
    };

    /// The stored logs view of the stack in `status`, linking the live logs of `live`.
    fn log_mount(status: &StackStatus, live: Vec<LiveLink>) -> LogMount {
        let action = format!("{}/logs", StackView::url(&status.name));
        LogMount {
            scope: LogScope::Stack(status.name.clone()),
            stream: Some(format!("{action}/stream")),
            download: Some(format!("{action}/download")),
            action,
            services: status.services.iter().map(|s| s.name.clone()).collect(),
            live,
            stacks: Vec::new(),
        }
    }

    /// Resolves once the stack called `name` is removed, as `events` (subscribed while it
    /// still existed) tell; never when the orchestrator is gone.
    async fn removal(&self, mut events: broadcast::Receiver<UiEvent>, name: String) {
        loop {
            let check = match events.recv().await {
                Ok(UiEvent::Event(event)) => {
                    event.kind == EventKind::StackRemoved
                        && event.stack.as_deref() == Some(name.as_str())
                }
                Ok(_) => false,
                // It may have missed the removal.
                Err(broadcast::error::RecvError::Lagged(_)) => true,
                Err(broadcast::error::RecvError::Closed) => return std::future::pending().await,
            };
            if check && self.find(&name).await.is_err() {
                return;
            }
        }
    }
}

/// `GET /stacks/{name}/logs`.
#[derive(Template)]
#[template(path = "stack_logs.html")]
struct StackLogsPage {
    layout: Layout,
    header: StackHeader,
    view: LogsView,
}

impl From<&LogCandidate> for LiveLink {
    /// `api · g1 · previous generation · running`.
    fn from(candidate: &LogCandidate) -> Self {
        let mut parts = Vec::new();
        if !candidate.service.is_empty() {
            parts.push(candidate.service.clone());
        }
        if let Some(generation) = candidate.generation {
            parts.push(format!("g{generation}"));
        }
        if let Some(role) = candidate.role {
            parts.push(role.to_string());
        }
        parts.push(candidate.state.clone());
        LiveLink {
            name: candidate.name.clone(),
            detail: parts.join(" · "),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::stacks::overview::Role;

    #[test]
    fn live_links_say_what_the_container_is() {
        let candidate = LogCandidate {
            name: "hoster-notes-api-g1".into(),
            service: "api".into(),
            generation: Some(1),
            role: Some(Role::Previous),
            state: "running".into(),
        };
        let link = LiveLink::from(&candidate);
        assert_eq!(link.detail, "api · g1 · previous generation · running");
        assert_eq!(link.href(), "/containers/hoster-notes-api-g1/logs");
    }
}
