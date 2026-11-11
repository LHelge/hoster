//! `GET /activity`: the Events page (ADR-0055 §5), every event of the log newest first, one
//! page of [`EventQuery::PAGE`] at a time.
//!
//! The filters are query parameters, so the page works without JavaScript (ADR-0054): `kind`
//! picks a group of kinds ([`EventGroup`]), `severity` the least severe event shown, `stack` one
//! stack's events, by name. The stack has no foreign key in the log, so a removed stack's events
//! stay filterable by the name it had. The kind and severity filters are rows of links, like the
//! status filter of `/stacks`; the stack is a small GET form, with the current stacks offered as
//! suggestions. As on `/stacks`, a filter value the page does not know is ignored and shows
//! every event it would have narrowed.
//!
//! Paging follows the store's cursor (`before`, the position of the last event shown), never an
//! offset, so new events arriving at the top do not shift the older pages. A malformed `before`
//! is a 400: the page then shows the newest events under a notice.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;
use std::time::SystemTime;

use askama::Template;
use axum::Extension;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::EventRow;
use crate::store::{EventCursor, EventKind, EventPage, EventQuery, Severity};
use crate::web::pages::{Flash, FlashKind, Layout, Page, Section, Shell};
use crate::web::stacks::Stacks;

/// A group of [`EventKind`]s, the Events page's kind filter (ADR-0055 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::web) enum EventGroup {
    Deploys,
    Containers,
    Certificates,
    /// Operator actions and hoster's own (started, updated, rolled back, a mail not delivered).
    Operator,
}

impl EventGroup {
    /// Every group, in the filter's order.
    const ALL: [Self; 4] = [
        Self::Deploys,
        Self::Containers,
        Self::Certificates,
        Self::Operator,
    ];

    /// The kinds of the group.
    fn kinds(self) -> BTreeSet<EventKind> {
        EventKind::ALL
            .into_iter()
            .filter(|kind| Self::from(*kind) == self)
            .collect()
    }

    /// How the filter link reads.
    fn label(self) -> &'static str {
        match self {
            Self::Deploys => "deploys",
            Self::Containers => "containers",
            Self::Certificates => "certificates",
            Self::Operator => "operator & system",
        }
    }
}

/// The group a kind belongs to: every kind is in exactly one.
impl From<EventKind> for EventGroup {
    fn from(kind: EventKind) -> Self {
        use EventKind as K;
        match kind {
            K::DeployStarted
            | K::DeploySucceeded
            | K::DeployFailed
            | K::DeployRolledBack
            | K::UpdateAvailable => Self::Deploys,
            K::ContainerRestarted | K::CrashLoopEntered | K::CrashLoopLeft | K::HealthChanged => {
                Self::Containers
            }
            K::CertificateIssued
            | K::CertificateRenewed
            | K::CertificateFailed
            | K::DnsMismatch => Self::Certificates,
            K::StackApplied
            | K::StackRemoved
            | K::SecretSet
            | K::SecretDeleted
            | K::HosterStarted
            | K::HosterUpdated
            | K::HosterRolledBack
            | K::NotificationFailed => Self::Operator,
        }
    }
}

/// The `kind` query value, e.g. `deploys`.
impl fmt::Display for EventGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Deploys => "deploys",
            Self::Containers => "containers",
            Self::Certificates => "certificates",
            Self::Operator => "operator",
        })
    }
}

/// As [`Display`](fmt::Display) writes it.
impl FromStr for EventGroup {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|group| group.to_string() == s)
            .ok_or(())
    }
}

/// The query of `GET /activity`, as typed: every field is optional and may be empty.
#[derive(Debug, Default, Deserialize)]
pub struct EventsPageQuery {
    kind: Option<String>,
    severity: Option<String>,
    stack: Option<String>,
    before: Option<String>,
}

/// The filters of the Events page, the unknown values of an [`EventsPageQuery`] dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Filters {
    group: Option<EventGroup>,
    min_severity: Severity,
    stack: Option<String>,
}

impl From<&EventsPageQuery> for Filters {
    fn from(query: &EventsPageQuery) -> Self {
        let given = |value: &Option<String>| {
            value
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        };
        Self {
            group: given(&query.kind).and_then(|kind| kind.parse().ok()),
            min_severity: given(&query.severity)
                .and_then(|severity| severity.parse().ok())
                .unwrap_or_default(),
            stack: given(&query.stack),
        }
    }
}

impl Filters {
    /// The store query of the page that starts at `before`.
    fn query(&self, before: Option<EventCursor>) -> EventQuery {
        EventQuery {
            stack: self.stack.clone(),
            kinds: self.group.map(EventGroup::kinds).unwrap_or_default(),
            min_severity: self.min_severity,
            before,
            limit: EventQuery::PAGE,
        }
    }

    /// The kind and severity filters as query pairs, each left out at its default.
    fn pairs(&self) -> Vec<(&'static str, String)> {
        let mut pairs = Vec::new();
        if let Some(group) = self.group {
            pairs.push(("kind", group.to_string()));
        }
        if self.min_severity != Severity::default() {
            pairs.push(("severity", self.min_severity.to_string()));
        }
        pairs
    }

    /// The URL of the page of these filters that starts at `before`. A filter at its default
    /// is left out, so the plain page is `/activity`.
    fn href(&self, before: Option<EventCursor>) -> String {
        let mut pairs = form_urlencoded::Serializer::new(String::new());
        pairs.extend_pairs(self.pairs());
        if let Some(stack) = &self.stack {
            pairs.append_pair("stack", stack);
        }
        if let Some(before) = before {
            pairs.append_pair("before", &before.to_string());
        }
        let query = pairs.finish();
        if query.is_empty() {
            EventsPage::PATH.to_owned()
        } else {
            format!("{}?{query}", EventsPage::PATH)
        }
    }

    /// The links of the kind filter, `all` first.
    fn group_links(&self) -> Vec<FilterLink> {
        std::iter::once(None)
            .chain(EventGroup::ALL.into_iter().map(Some))
            .map(|group| FilterLink {
                label: group.map_or("all", EventGroup::label),
                href: Self {
                    group,
                    ..self.clone()
                }
                .href(None),
                current: group == self.group,
            })
            .collect()
    }

    /// The links of the severity filter, from every severity to errors only.
    fn severity_links(&self) -> Vec<FilterLink> {
        Severity::ALL
            .into_iter()
            .map(|min_severity| FilterLink {
                label: match min_severity {
                    Severity::Info => "all",
                    Severity::Success => "success+",
                    Severity::Warning => "warning+",
                    Severity::Error => "error",
                },
                href: Self {
                    min_severity,
                    ..self.clone()
                }
                .href(None),
                current: min_severity == self.min_severity,
            })
            .collect()
    }

    /// Whether any filter narrows the page.
    fn narrowed(&self) -> bool {
        *self != Self::default()
    }
}

/// A link of a filter row.
#[derive(Debug)]
struct FilterLink {
    label: &'static str,
    href: String,
    current: bool,
}

/// `GET /activity`.
#[derive(Template)]
#[template(path = "events.html")]
pub struct EventsPage {
    layout: Layout,
    filters: Filters,
    groups: Vec<FilterLink>,
    severities: Vec<FilterLink>,
    /// The current stacks' names, offered by the stack filter.
    stacks: Vec<String>,
    /// The page's events; `None` when the log cannot be read.
    rows: Option<Vec<EventRow>>,
    /// The next, older page.
    older: Option<String>,
    /// The first page, when this is a later one.
    newest: Option<String>,
}

impl EventsPage {
    /// The page's path. `/events` is the dashboard's live stream.
    pub const PATH: &str = "/activity";

    /// The Events page of `stack`'s events, or of every event.
    pub(in crate::web) fn url(stack: Option<&str>) -> String {
        Filters {
            stack: stack.map(str::to_owned),
            ..Filters::default()
        }
        .href(None)
    }

    /// `GET /activity[?kind=…&severity=…&stack=…&before=…]`.
    pub async fn page(
        Extension(stacks): Extension<Stacks>,
        shell: Shell,
        Query(query): Query<EventsPageQuery>,
    ) -> Response {
        let mut layout = shell.layout(Section::Events);
        let filters = Filters::from(&query);
        let mut status = StatusCode::OK;
        let before = match query.before.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(before) => match before.parse::<EventCursor>() {
                Ok(cursor) => Some(cursor),
                Err(_) => {
                    status = StatusCode::BAD_REQUEST;
                    layout = layout.with_flash(Flash {
                        kind: FlashKind::Error,
                        message: "That link to older events is not valid, so the newest are shown."
                            .into(),
                    });
                    None
                }
            },
        };
        let store_query = filters.query(before);
        let (page, names) = tokio::join!(stacks.event_page(&store_query), stacks.status());
        let page = page
            .inspect_err(|err| tracing::warn!(error = %err, "cannot read the event log"))
            .ok();
        if page.is_none() {
            status = StatusCode::INTERNAL_SERVER_ERROR;
        }
        let names = names
            .inspect_err(|err| tracing::warn!(error = %err, "listing the stacks failed"))
            .map(|list| list.into_iter().map(|s| s.name).collect())
            .unwrap_or_default();
        let log = Self::new(
            layout,
            filters,
            before.is_some(),
            page,
            names,
            SystemTime::now(),
        );
        (status, Page(log)).into_response()
    }

    /// The page of `filters` showing `page` (`None` when it cannot be read) as of `now`;
    /// `paged` when it is not the first. `stacks` are the current stacks' names.
    fn new(
        layout: Layout,
        filters: Filters,
        paged: bool,
        page: Option<EventPage>,
        stacks: Vec<String>,
        now: SystemTime,
    ) -> Self {
        Self {
            layout,
            groups: filters.group_links(),
            severities: filters.severity_links(),
            older: page
                .as_ref()
                .and_then(|page| page.next)
                .map(|next| filters.href(Some(next))),
            newest: paged.then(|| filters.href(None)),
            rows: page.map(|page| {
                page.events
                    .iter()
                    .map(|event| EventRow::new(event, now))
                    .collect()
            }),
            stacks,
            filters,
        }
    }

    /// The stack filtered on.
    fn stack(&self) -> &str {
        self.filters.stack.as_deref().unwrap_or_default()
    }

    /// The page without its stack filter, when it has one.
    fn all_stacks(&self) -> Option<String> {
        self.filters.stack.as_ref().map(|_| {
            Filters {
                stack: None,
                ..self.filters.clone()
            }
            .href(None)
        })
    }

    /// The page with no filter, when one narrows it.
    fn unfiltered(&self) -> Option<String> {
        self.filters
            .narrowed()
            .then(|| Filters::default().href(None))
    }

    /// The hidden fields that keep the kind and severity filters when the stack form is sent.
    fn kept(&self) -> Vec<(&'static str, String)> {
        self.filters.pairs()
    }

    /// The page's path, where the stack form is sent.
    fn path(&self) -> &'static str {
        Self::PATH
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_is_in_the_group_that_names_it() {
        let grouped: Vec<_> = EventGroup::ALL
            .into_iter()
            .flat_map(EventGroup::kinds)
            .collect();
        assert_eq!(
            grouped.len(),
            EventKind::ALL.len(),
            "each kind in one group"
        );
        assert!(
            EventGroup::Deploys
                .kinds()
                .contains(&EventKind::DeployRolledBack)
        );
        assert!(
            EventGroup::Containers
                .kinds()
                .contains(&EventKind::HealthChanged)
        );
        assert!(
            EventGroup::Certificates
                .kinds()
                .contains(&EventKind::DnsMismatch)
        );
        assert!(EventGroup::Operator.kinds().contains(&EventKind::SecretSet));
        assert!(
            EventGroup::Operator
                .kinds()
                .contains(&EventKind::HosterUpdated)
        );
        assert!(
            EventGroup::Operator
                .kinds()
                .contains(&EventKind::NotificationFailed)
        );
        for group in EventGroup::ALL {
            assert_eq!(group.to_string().parse(), Ok(group));
        }
        assert!("deploy".parse::<EventGroup>().is_err());
    }

    #[test]
    fn unknown_filter_values_are_ignored() {
        let query = EventsPageQuery {
            kind: Some("nonsense".into()),
            severity: Some("fatal".into()),
            stack: Some("  ".into()),
            before: None,
        };
        assert_eq!(Filters::from(&query), Filters::default());

        let query = EventsPageQuery {
            kind: Some("certificates".into()),
            severity: Some("warning".into()),
            stack: Some(" notes ".into()),
            before: None,
        };
        let filters = Filters::from(&query);
        assert_eq!(
            filters,
            Filters {
                group: Some(EventGroup::Certificates),
                min_severity: Severity::Warning,
                stack: Some("notes".into()),
            }
        );
        let store = filters.query(None);
        assert_eq!(store.kinds, EventGroup::Certificates.kinds());
        assert_eq!(store.min_severity, Severity::Warning);
        assert_eq!(store.stack.as_deref(), Some("notes"));
        assert_eq!(store.limit, EventQuery::PAGE);
    }

    #[test]
    fn links_leave_out_the_defaults_and_encode_the_rest() {
        assert_eq!(EventsPage::url(None), "/activity");
        assert_eq!(EventsPage::url(Some("notes")), "/activity?stack=notes");
        let filters = Filters {
            group: Some(EventGroup::Operator),
            min_severity: Severity::Error,
            stack: Some("a b&c".into()),
        };
        let before: EventCursor = "1800000000000.42".parse().unwrap();
        assert_eq!(
            filters.href(Some(before)),
            "/activity?kind=operator&severity=error&stack=a+b%26c&before=1800000000000.42"
        );
    }
}
