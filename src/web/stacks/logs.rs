//! A stack's logs tab (ADR-0067, ADR-0068, epic psq): the stack's records from the log store
//! under the header and tab bar every stack page shares. It mounts the stored logs view of
//! `web/logs.rs` with the stack fixed ([`LogScope::Stack`]): every service merged oldest to
//! newest with a coloured service column, hoster's own records about the stack under
//! `hoster`, a chip per service (the current revision's, and any with records in the range),
//! the *since* presets and free text, and "older"/"newer" paging by record key. Records of
//! replaced and removed containers are listed too, with their generation where a service has
//! more than one in the range.
//!
//! The toolbar also links each of the stack's containers to its live log
//! (`/containers/{name}/logs`, ADR-0044), which streams straight from podman: it shows a line
//! before the recorder has committed it, and it is what is left when the store cannot be read
//! (ADR-0067 §10). An unknown stack is a 404, even when the store still holds records under
//! its name.

use askama::Template;
use axum::Extension;
use axum::extract::{Path, Query};
use axum::response::{IntoResponse, Response};

use super::Stacks;
use super::frame::{StackHeader, Tab};
use super::overview::{LogCandidate, StackContainers, StackView};
use crate::web::containers::Containers;
use crate::web::logs::{LiveLink, LogMount, LogParams, LogReader, LogScope, LogsView};
use crate::web::pages::{Flash, FlashKind, Layout, Page, Section, Shell};

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
        let mount = LogMount {
            scope: LogScope::Stack(name.clone()),
            action: format!("{}/logs", StackView::url(&name)),
            services: status.services.iter().map(|s| s.name.clone()).collect(),
            live,
        };
        let view = LogsView::load(&reader, mount, &params).await;
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
