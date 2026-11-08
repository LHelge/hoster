//! Server-rendered pages (ADR-0021). Templates live in `templates/` and are compiled into the
//! binary by Askama; every page extends `base.html`, which reads the page's [`Layout`].

use std::fmt;
use std::time::Duration;

use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

use axum::Extension;

use super::assets::Assets;
use super::auth::Session;
use crate::routing::Owner;
use crate::stats::{Counts, Elapsed, ServiceRestarts, Snapshot, Stats, StatusClass};

/// A section of the admin UI, as listed in the navigation bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Dashboard,
    Stacks,
    Containers,
    Certificates,
    Settings,
}

impl Section {
    const ALL: [Section; 5] = [
        Section::Dashboard,
        Section::Stacks,
        Section::Containers,
        Section::Certificates,
        Section::Settings,
    ];

    fn href(self) -> &'static str {
        match self {
            Section::Dashboard => "/",
            Section::Stacks => "/stacks",
            Section::Containers => "/containers",
            Section::Certificates => "/certificates",
            Section::Settings => "/settings/registries",
        }
    }
}

impl fmt::Display for Section {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Section::Dashboard => "Dashboard",
            Section::Stacks => "Stacks",
            Section::Containers => "Containers",
            Section::Certificates => "Certificates",
            Section::Settings => "Settings",
        })
    }
}

/// One link in the navigation bar.
pub struct NavItem {
    pub href: &'static str,
    pub label: Section,
    pub current: bool,
}

/// How a flash message is styled; its `Display` is the CSS class suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashKind {
    Info,
    Success,
    Error,
}

impl fmt::Display for FlashKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FlashKind::Info => "info",
            FlashKind::Success => "success",
            FlashKind::Error => "error",
        })
    }
}

/// A one-off message shown above the page content, in the `#flash` area (which HTMX responses
/// can also target).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flash {
    pub kind: FlashKind,
    pub message: String,
}

/// The signed-in admin, as the header shows them.
pub struct SignedIn {
    pub login: String,
    csrf_token: String,
}

/// What `base.html` needs from every page.
pub struct Layout {
    /// The current section; `None` on public pages (the login page), which show no navigation.
    pub section: Option<Section>,
    pub flash: Option<Flash>,
    pub user: Option<SignedIn>,
}

impl Layout {
    /// The layout of an admin page in `section`.
    pub fn new(section: Section, session: &Session) -> Self {
        Layout {
            section: Some(section),
            flash: None,
            user: Some(SignedIn {
                login: session.login().to_owned(),
                csrf_token: session.csrf_token().to_owned(),
            }),
        }
    }

    /// The layout of a page shown without a session.
    pub fn public() -> Self {
        Layout {
            section: None,
            flash: None,
            user: None,
        }
    }

    pub fn with_flash(mut self, flash: Flash) -> Self {
        self.flash = Some(flash);
        self
    }

    pub fn nav(&self) -> Vec<NavItem> {
        let Some(current) = self.section else {
            return Vec::new();
        };
        Section::ALL
            .into_iter()
            .map(|section| NavItem {
                href: section.href(),
                label: section,
                current: section == current,
            })
            .collect()
    }

    /// The `hx-headers` JSON that makes every HTMX request carry the session's CSRF token
    /// (ADR-0031). The token is hex, so it needs no JSON escaping.
    pub fn hx_headers(&self) -> Option<String> {
        self.user
            .as_ref()
            .map(|user| format!(r#"{{"X-CSRF-Token": "{}"}}"#, user.csrf_token))
    }

    pub fn htmx(&self) -> String {
        Assets::get().htmx.url()
    }

    /// HTMX's SSE extension, loaded on every page so any of them can hold a live view.
    pub fn htmx_sse(&self) -> String {
        Assets::get().htmx_sse.url()
    }

    pub fn stylesheet(&self) -> String {
        Assets::get().stylesheet.url()
    }

    pub fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }
}

/// The dashboard at `/`: the in-process statistics of ADR-0045.
#[derive(Template)]
#[template(path = "dashboard.html")]
pub struct Dashboard {
    pub layout: Layout,
    pub stats: StatsView,
}

impl Dashboard {
    /// The `GET /` handler.
    pub async fn show(
        Extension(session): Extension<Session>,
        Extension(stats): Extension<Stats>,
    ) -> Page<Dashboard> {
        Page(Dashboard {
            layout: Layout::new(Section::Dashboard, &session),
            stats: StatsView::from(stats.snapshot().await),
        })
    }
}

/// The statistics as the dashboard shows them.
#[derive(Debug, Default)]
pub struct StatsView {
    pub routes: Vec<TrafficRow>,
    /// Requests that matched no route.
    pub unrouted: TrafficRow,
    pub deployments: Vec<DeployRow>,
    pub services: Vec<ServiceRestarts>,
    pub certificates: Vec<CertRow>,
    pub problems: Vec<String>,
}

/// One route's traffic.
#[derive(Debug, Default)]
pub struct TrafficRow {
    /// `host/prefix`.
    pub route: String,
    /// Who serves it: "web (blog)", "hoster (admin UI)".
    pub owner: String,
    pub statuses: Vec<(StatusClass, u64)>,
    /// Every finished request.
    pub requests: u64,
    pub upstream_errors: u64,
    pub inflight: u64,
    pub p50: String,
    pub p95: String,
}

/// One stack's deployments.
#[derive(Debug)]
pub struct DeployRow {
    pub stack: String,
    pub succeeded: u32,
    pub failed: u32,
    pub skipped: u32,
    pub mean: String,
    pub max: String,
}

/// One served certificate.
#[derive(Debug)]
pub struct CertRow {
    pub host: String,
    pub not_after: String,
    pub left: String,
    /// Expires within [`CertRow::WARN_WITHIN`].
    pub expiring: bool,
}

impl CertRow {
    /// Certificates closer to expiry than this are flagged (renewal starts well before).
    pub const WARN_WITHIN: Duration = Duration::from_secs(14 * 24 * 60 * 60);
}

impl StatsView {
    /// The status classes, in the order of [`TrafficRow::statuses`].
    pub fn classes(&self) -> [StatusClass; 6] {
        StatusClass::ALL
    }

    /// How far back the latency percentiles look, e.g. `5 min`.
    pub fn latency_window(&self) -> String {
        Elapsed(Stats::LATENCY_WINDOW).to_string()
    }

    /// How far back the deployment statistics look, e.g. `30 d`.
    pub fn deploy_history(&self) -> String {
        Elapsed(Stats::DEPLOY_HISTORY).to_string()
    }

    /// The rows of the traffic table: every route, then the unrouted requests.
    pub fn traffic(&self) -> impl Iterator<Item = &TrafficRow> {
        self.routes.iter().chain([&self.unrouted])
    }
}

impl TrafficRow {
    fn new(route: String, owner: String, counts: &Counts) -> Self {
        TrafficRow {
            route,
            owner,
            statuses: counts.statuses.iter().collect(),
            requests: counts.statuses.total(),
            upstream_errors: counts.upstream_errors,
            inflight: counts.inflight,
            p50: Elapsed::cell(counts.latency.p50),
            p95: Elapsed::cell(counts.latency.p95),
        }
    }
}

impl From<Snapshot> for StatsView {
    fn from(snapshot: Snapshot) -> Self {
        let routes = snapshot
            .traffic
            .routes
            .iter()
            .map(|route| {
                let owner = match &route.owner {
                    Some(Owner::Route(owner)) => format!("{} ({})", owner.service, owner.stack),
                    Some(Owner::Reserved(purpose)) => format!("hoster ({purpose})"),
                    None => String::new(),
                };
                let name = format!("{}{}", route.host, route.prefix);
                TrafficRow::new(name, owner, &route.counts)
            })
            .collect();
        let unrouted = TrafficRow::new(
            "unrouted".to_owned(),
            String::new(),
            &snapshot.traffic.unrouted,
        );
        let deployments = snapshot
            .deployments
            .iter()
            .map(|stack| DeployRow {
                stack: stack.stack.clone(),
                succeeded: stack.succeeded(),
                failed: stack.failed(),
                skipped: stack.skipped(),
                mean: Elapsed::cell(stack.mean_live_duration()),
                max: Elapsed::cell(stack.max_live_duration()),
            })
            .collect();
        let certificates = snapshot
            .certificates
            .iter()
            .map(|cert| {
                let left = cert.remaining(snapshot.taken_at);
                CertRow {
                    host: cert.host.clone(),
                    not_after: humantime::format_rfc3339_seconds(cert.not_after).to_string(),
                    left: if left.is_zero() {
                        "expired".to_owned()
                    } else {
                        Elapsed(left).to_string()
                    },
                    expiring: left < CertRow::WARN_WITHIN,
                }
            })
            .collect();
        StatsView {
            routes,
            unrouted,
            deployments,
            services: snapshot.services,
            certificates,
            problems: snapshot.problems,
        }
    }
}

/// A rendered template as an HTML response; a rendering failure becomes a logged 500.
pub struct Page<T>(pub T);

impl<T: Template> IntoResponse for Page<T> {
    fn into_response(self) -> Response {
        match self.0.render() {
            Ok(html) => Html(html).into_response(),
            Err(err) => {
                tracing::error!(error = %err, "rendering a template failed");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Layout {
        Layout::new(Section::Dashboard, &Session::for_tests("octocat", "c0ffee"))
    }

    #[test]
    fn base_layout_has_nav_flash_area_and_assets() {
        let html = Dashboard {
            layout: layout(),
            stats: StatsView::default(),
        }
        .render()
        .unwrap();
        assert!(html.contains(r#"<a href="/" aria-current="page">Dashboard</a>"#));
        assert!(
            html.contains(
                r#"<body hx-headers:inherited="{&#34;X-CSRF-Token&#34;: &#34;c0ffee&#34;}">"#
            ),
            "{html}"
        );
        assert!(html.contains(r#"hx-post="/auth/logout""#), "{html}");
        assert!(
            html.contains(r#"<div id="flash" aria-live="polite"></div>"#),
            "{html}"
        );
        assert!(html.contains(&Assets::get().htmx.url()));
        assert!(html.contains(&Assets::get().stylesheet.url()));
        assert!(
            !html.contains("<script>"),
            "no inline scripts under the CSP"
        );
    }

    #[test]
    fn flash_messages_are_escaped() {
        let html = Dashboard {
            layout: layout().with_flash(Flash {
                kind: FlashKind::Error,
                message: "<b>deploy failed</b>".into(),
            }),
            stats: StatsView::default(),
        }
        .render()
        .unwrap();
        assert!(html.contains(r#"class="flash flash-error""#));
        assert!(
            html.contains("&#60;b&#62;deploy failed&#60;/b&#62;"),
            "{html}"
        );
    }

    #[test]
    fn public_pages_have_no_navigation_or_csrf_headers() {
        let layout = Layout::public();
        assert!(layout.nav().is_empty());
        assert_eq!(layout.hx_headers(), None);
    }
}
