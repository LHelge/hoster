//! A stack's logs tab (ADR-0044, epic 8bf, task gn6): the log of one of the stack's containers
//! at a time, under the header and tab bar every stack page shares.
//!
//! The toolbar has a chip per service (`?service=`, the first by default) and streams the
//! container that runs it, or its newest one. When the service has more than one container (a
//! draining or starting generation during a deploy), a second row picks which (`?container=`,
//! as the overview's services table links them). Then come the log options of
//! [`LogView`]: *since* presets and free text, *tail* and the *follow* switch, all plain GET
//! parameters rendered on the server. Every link keeps the options, and the form keeps the
//! service and container as hidden fields. The log itself streams over SSE from
//! `/containers/{name}/logs/stream`, as it did before the tab existed.
//!
//! There is no merged all-services stream, search, level filter or download, and no access-log
//! or deployment source: those need backend hoster does not have yet (the stack-logs follow-up).

use askama::Template;
use axum::Extension;
use axum::extract::{Path, Query};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::Stacks;
use super::frame::{StackHeader, Tab};
use super::overview::{LogCandidate, LogPick, StackContainers, StackView};
use crate::web::containers::{Containers, LogQuery, LogView};
use crate::web::pages::{Flash, FlashKind, Layout, Page, Section, Shell};

impl Stacks {
    /// `GET /stacks/{name}/logs[?service=…&container=…&tail=…&since=…&follow=on]`: the logs
    /// tab; an unknown stack is a 404.
    pub async fn logs(
        Extension(stacks): Extension<Stacks>,
        Extension(containers): Extension<Containers>,
        shell: Shell,
        Path(name): Path<String>,
        Query(query): Query<LogsQuery>,
    ) -> Response {
        let mut layout = shell.layout(Section::Stacks);
        let status = match stacks.find(&name).await {
            Ok(status) => status,
            Err(outcome) => return outcome.page(layout),
        };
        let (listed, error) = match containers.of_stack(&name).await {
            Ok(list) => (StackContainers::from(list), None),
            Err(err) => {
                tracing::error!(stack = %name, error = %err, "listing the stack's containers failed");
                (
                    StackContainers::default(),
                    Some(format!("Cannot list the stack's containers: {err}")),
                )
            }
        };
        let pick = listed.pick_log(
            &status,
            query.service.as_deref(),
            query.container.as_deref(),
        );
        let mut tab = LogsTab::new(&name, pick, &query.log);
        tab.listing_failed = error.is_some();
        if let Some(message) = error.or_else(|| tab.log.error.clone()) {
            layout = layout.with_flash(Flash {
                kind: FlashKind::Error,
                message,
            });
        }
        Page(StackLogsPage {
            layout,
            header: stacks.header(&status, Tab::Logs).await,
            tab,
        })
        .into_response()
    }
}

/// The query of the logs tab: which log, and its options.
#[derive(Debug, Deserialize)]
pub struct LogsQuery {
    service: Option<String>,
    container: Option<String>,
    #[serde(flatten)]
    log: LogQuery,
}

/// `GET /stacks/{name}/logs`.
#[derive(Template)]
#[template(path = "stack_logs.html")]
struct StackLogsPage {
    layout: Layout,
    header: StackHeader,
    tab: LogsTab,
}

/// The logs tab of one stack.
#[derive(Debug)]
struct LogsTab {
    services: Vec<ChoiceLink>,
    /// The chosen service's containers, when it has more than one to choose from.
    containers: Vec<ChoiceLink>,
    /// The chosen service; `None` when the stack has none.
    service: Option<String>,
    /// Whether the stack's containers could not be listed, so there is no telling whether the
    /// service has one.
    listing_failed: bool,
    log: LogView,
}

/// A chip of the toolbar: a service, or one of its containers.
#[derive(Debug)]
struct ChoiceLink {
    label: String,
    /// What it is beyond its label: a container's generation and state, a service's absence
    /// from the active revision.
    detail: Option<String>,
    href: String,
    current: bool,
    /// The colour class of a service's dot.
    dot: &'static str,
}

impl LogsTab {
    /// The dot colours of the service chips, in turn.
    const DOTS: [&str; 4] = ["dot-s0", "dot-s1", "dot-s2", "dot-s3"];

    fn new(stack: &str, pick: LogPick, query: &LogQuery) -> Self {
        let mut keep = Vec::new();
        if let Some(service) = pick.service.as_ref().filter(|s| !s.is_empty()) {
            keep.push(("service", service.clone()));
        }
        if pick.named
            && let Some(container) = &pick.container
        {
            keep.push(("container", container.clone()));
        }
        let mut log = LogView::new(
            pick.container.as_deref(),
            format!("{}/logs", StackView::url(stack)),
            keep,
            query,
        );
        log.detail = pick
            .containers
            .iter()
            .find(|candidate| pick.container.as_ref() == Some(&candidate.name))
            .map(Self::detail);
        let services = pick
            .services
            .iter()
            .enumerate()
            .map(|(i, name)| ChoiceLink {
                label: name.clone(),
                detail: None,
                href: log.link(&[("service", name)]),
                current: pick.service.as_ref() == Some(name),
                dot: Self::DOTS[i % Self::DOTS.len()],
            })
            .collect();
        let containers = match pick.containers.len() {
            0 | 1 => Vec::new(),
            _ => {
                let service = pick.service.as_deref().unwrap_or_default();
                pick.containers
                    .iter()
                    .map(|candidate| ChoiceLink {
                        label: candidate.name.clone(),
                        detail: Some(Self::detail(candidate)),
                        href: log.link(&[("service", service), ("container", &candidate.name)]),
                        current: pick.container.as_ref() == Some(&candidate.name),
                        dot: "",
                    })
                    .collect()
            }
        };
        Self {
            services,
            containers,
            service: pick.service,
            listing_failed: false,
            log,
        }
    }

    /// `g2 · running`, `g1 · previous generation · running`.
    fn detail(candidate: &LogCandidate) -> String {
        let mut parts = Vec::new();
        if let Some(generation) = candidate.generation {
            parts.push(format!("g{generation}"));
        }
        if let Some(role) = candidate.role {
            parts.push(role.to_string());
        }
        parts.push(candidate.state.clone());
        parts.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::stacks::overview::Role;

    fn candidate(name: &str, generation: u32, role: Option<Role>) -> LogCandidate {
        LogCandidate {
            name: name.into(),
            generation: Some(generation),
            role,
            state: "running".into(),
        }
    }

    #[test]
    fn links_keep_the_options_and_the_form_keeps_the_choice() {
        let pick = LogPick {
            services: vec!["api".into(), "storage".into()],
            service: Some("api".into()),
            containers: vec![
                candidate("hoster-notes-api-g2", 2, None),
                candidate("hoster-notes-api-g1", 1, Some(Role::Previous)),
            ],
            container: Some("hoster-notes-api-g1".into()),
            named: true,
        };
        let query: LogQuery = serde_urlencoded_query("tail=50&since=2026-11-08T12:00:00Z");
        let tab = LogsTab::new("notes", pick, &query);
        assert_eq!(
            tab.services[1].href,
            "/stacks/notes/logs?service=storage&tail=50&since=2026-11-08T12%3A00%3A00Z"
        );
        assert!(tab.services[0].current);
        assert_eq!(tab.services[1].dot, "dot-s1");
        assert_eq!(tab.containers.len(), 2);
        assert!(tab.containers[1].current);
        assert_eq!(
            tab.containers[1].detail.as_deref(),
            Some("g1 · previous generation · running")
        );
        assert_eq!(
            tab.containers[0].href,
            "/stacks/notes/logs?service=api&container=hoster-notes-api-g2&tail=50&since=2026-11-08T12%3A00%3A00Z"
        );
        assert_eq!(
            tab.log.keep,
            [
                ("service", "api".to_owned()),
                ("container", "hoster-notes-api-g1".to_owned())
            ]
        );
        assert_eq!(
            tab.log.stream_url.as_deref(),
            Some(
                "/containers/hoster-notes-api-g1/logs/stream?tail=50&since=2026-11-08T12%3A00%3A00Z"
            )
        );
        let presets: Vec<_> = tab
            .log
            .presets
            .iter()
            .map(|p| (p.label, p.current))
            .collect();
        assert_eq!(
            presets,
            [
                ("15m", false),
                ("1h", false),
                ("6h", false),
                ("24h", false),
                ("all", false)
            ]
        );
        assert_eq!(
            tab.log.presets[1].href,
            "/stacks/notes/logs?service=api&container=hoster-notes-api-g1&tail=50&since=1h"
        );
    }

    #[test]
    fn a_first_visit_follows_and_one_container_needs_no_choice() {
        let pick = LogPick {
            services: vec!["api".into()],
            service: Some("api".into()),
            containers: vec![candidate("hoster-notes-api-g2", 2, None)],
            container: Some("hoster-notes-api-g2".into()),
            named: false,
        };
        let tab = LogsTab::new("notes", pick, &LogQuery::default());
        assert!(tab.containers.is_empty());
        assert_eq!(tab.log.detail.as_deref(), Some("g2 · running"));
        assert_eq!(tab.log.keep, [("service", "api".to_owned())]);
        assert!(tab.log.form.follow);
        assert!(tab.log.presets[4].current, "no start: all");
        assert_eq!(
            tab.services[0].href,
            "/stacks/notes/logs?service=api&tail=200&since=&follow=on"
        );
    }

    fn serde_urlencoded_query(query: &str) -> LogQuery {
        let uri: axum::http::Uri = format!("/?{query}").parse().unwrap();
        Query::<LogQuery>::try_from_uri(&uri).unwrap().0
    }
}
