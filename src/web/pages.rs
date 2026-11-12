//! Server-rendered pages (ADR-0021). Templates live in `templates/` and are compiled into the
//! binary by Askama; every page extends `base.html`, which reads the page's [`Layout`].
//!
//! Signed-in pages sit in the shell of ADR-0054: a sidebar with the version and instance name, the
//! navigation with its counts and the session, and a page header with a `~/path` breadcrumb, the
//! h1 and an actions slot (the `crumb`, `heading` and `actions` blocks). The sidebar ends with
//! the [`HostCard`], the host's resource meters (ADR-0066). Handlers take a [`Shell`] extractor,
//! which gathers the counts and the card per request, and call [`Shell::layout`].

mod host_card;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{Html, IntoResponse, Response};

use super::assets::Assets;
use super::auth::Session;
use super::certificates::CertificateControl;
use super::icons::Icon;
use super::stacks::StackControl;
use crate::instance::{Instance, InstanceName};
use crate::stats::ResourceUsage;
use crate::store::Timestamp;

pub use self::host_card::HostCard;

/// A section of the admin UI, each but [`Section::Events`] in the sidebar's navigation. The
/// containers and their logs belong to the stacks (ADR-0054).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Dashboard,
    Stacks,
    Certificates,
    Settings,
    System,
    /// The Events page (ADR-0055): the design gives it no navigation entry, so it is reached
    /// from the Recent events cards of the dashboard and the stack pages.
    Events,
}

impl Section {
    /// The sections the navigation lists, in order.
    const NAV: [Section; 5] = [
        Section::Dashboard,
        Section::Stacks,
        Section::Certificates,
        Section::Settings,
        Section::System,
    ];

    fn href(self) -> &'static str {
        match self {
            Section::Dashboard => "/",
            Section::Stacks => "/stacks",
            Section::Certificates => "/certificates",
            Section::Settings => "/settings/general",
            Section::System => "/system",
            Section::Events => super::events::EventsPage::PATH,
        }
    }

    /// The page's default `~/path` breadcrumb, without the `~/`.
    fn path(self) -> &'static str {
        match self {
            Section::Dashboard => "dashboard",
            Section::Stacks => "stacks",
            Section::Certificates => "certificates",
            Section::Settings => "settings",
            Section::System => "system",
            Section::Events => "activity",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Section::Dashboard => Icon::Dashboard,
            Section::Stacks => Icon::Stacks,
            Section::Certificates => Icon::Certificates,
            Section::Settings => Icon::Settings,
            Section::System => Icon::System,
            // Not in the navigation; the dashboard's, which it belongs with.
            Section::Events => Icon::Dashboard,
        }
    }
}

impl fmt::Display for Section {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Section::Dashboard => "Dashboard",
            Section::Stacks => "Stacks",
            Section::Certificates => "Certificates",
            Section::Settings => "Settings",
            Section::System => "System",
            Section::Events => "Events",
        })
    }
}

/// One link in the navigation.
pub struct NavItem {
    pub href: &'static str,
    pub icon: Icon,
    pub label: Section,
    pub current: bool,
    pub count: Option<NavCount>,
}

/// The number next to a navigation item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavCount {
    pub value: usize,
    /// It counts failures (a red badge, with an `aria-label` saying so), not just items.
    pub alert: bool,
}

impl NavCount {
    /// The badge's accessible name, for an alert.
    pub fn label(&self) -> Option<String> {
        self.alert.then(|| format!("{} failing", self.value))
    }
}

/// The numbers the navigation shows; `None` where they could not be read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NavCounts {
    pub stacks: Option<usize>,
    pub failing_certificates: Option<usize>,
}

impl NavCounts {
    /// The count shown next to `section`, if any: the number of stacks, and the failing
    /// certificates when there are some.
    fn of(&self, section: Section) -> Option<NavCount> {
        match section {
            Section::Stacks => self.stacks.map(|value| NavCount {
                value,
                alert: false,
            }),
            Section::Certificates => self
                .failing_certificates
                .filter(|&value| value > 0)
                .map(|value| NavCount { value, alert: true }),
            _ => None,
        }
    }
}

/// Where the shell's data comes from: shared by every route as a request extension, so the
/// layout can gather the navigation's counts and the host card on every full page render.
#[derive(Clone)]
pub struct ShellSource {
    stacks: Arc<dyn StackControl>,
    certificates: Arc<dyn CertificateControl>,
    /// The instance name (ADR-0058 §2), cached in memory: no store read per page. Its default
    /// is the hostname, read once, which the host card is headed with.
    instance: Instance,
    /// The resource sampler's cache, the host card's only source (ADR-0066 §1).
    resources: ResourceUsage,
}

impl ShellSource {
    pub fn new(
        stacks: Arc<dyn StackControl>,
        certificates: Arc<dyn CertificateControl>,
        instance: Instance,
        resources: ResourceUsage,
    ) -> Self {
        Self {
            stacks,
            certificates,
            instance,
            resources,
        }
    }

    /// The host card as the sampler's cache has it now.
    fn host_card(&self) -> HostCard {
        HostCard::of(self.instance.default_name().clone(), &self.resources)
    }

    /// The navigation's counts: one stack query and the in-memory certificate status. A count
    /// that cannot be read is left out rather than failing the page.
    async fn counts(&self) -> NavCounts {
        let (stacks, failing) = tokio::join!(self.stacks.count(), self.certificates.failing());
        NavCounts {
            stacks: stacks
                .inspect_err(|err| tracing::warn!(error = %err, "counting the stacks failed"))
                .ok(),
            failing_certificates: Some(failing),
        }
    }
}

/// The shell of a signed-in page, extracted per request: the session, the navigation's counts,
/// the instance name and the host card. A handler turns it into the page's [`Layout`] with
/// [`Shell::layout`].
pub struct Shell {
    session: Session,
    counts: NavCounts,
    instance: InstanceName,
    host: HostCard,
}

impl Shell {
    /// The layout of an admin page in `section`.
    pub fn layout(self, section: Section) -> Layout {
        Layout {
            counts: self.counts,
            instance: Some(self.instance),
            host: Some(self.host),
            ..Layout::new(section, &self.session)
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Shell {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        // Both are set for every route behind the guard; their absence is a wiring bug.
        let missing = || {
            tracing::error!("a page asked for the shell outside the admin routes");
            StatusCode::INTERNAL_SERVER_ERROR
        };
        let session = parts
            .extensions
            .get::<Session>()
            .cloned()
            .ok_or_else(missing)?;
        let source = parts
            .extensions
            .get::<ShellSource>()
            .cloned()
            .ok_or_else(missing)?;
        Ok(Shell {
            session,
            counts: source.counts().await,
            instance: source.instance.name(),
            host: source.host_card(),
        })
    }
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

/// The signed-in admin, as the sidebar shows them.
pub struct SignedIn {
    pub login: String,
    csrf_token: String,
    expires_at: Timestamp,
}

impl SignedIn {
    /// The avatar's letter.
    pub fn initial(&self) -> String {
        self.login
            .chars()
            .next()
            .map_or_else(|| "?".to_owned(), |c| c.to_uppercase().collect())
    }

    /// How long the session has left before its absolute expiry (`6h`, `25m`).
    pub fn left(&self) -> Remaining {
        let millis = self.expires_at.as_millis() - Timestamp::now().as_millis();
        Remaining(Duration::from_millis(u64::try_from(millis).unwrap_or(0)))
    }
}

/// A time left, coarsely: whole hours, then whole minutes, then `<1m`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Remaining(pub Duration);

impl fmt::Display for Remaining {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let minutes = self.0.as_secs() / 60;
        match minutes {
            0 => f.write_str("<1m"),
            1..60 => write!(f, "{minutes}m"),
            _ => write!(f, "{}h", minutes / 60),
        }
    }
}

/// What `base.html` needs from every page.
pub struct Layout {
    /// The current section; `None` on public pages (the login page), which show no shell.
    pub section: Option<Section>,
    pub flash: Option<Flash>,
    pub user: Option<SignedIn>,
    counts: NavCounts,
    /// The instance name, next to the version in the sidebar.
    pub instance: Option<InstanceName>,
    /// The sidebar's host card; `None` on public pages and in layouts built without a
    /// [`Shell`].
    pub host: Option<HostCard>,
}

impl Layout {
    /// The layout of an admin page in `section`, without the navigation's counts; handlers get
    /// theirs from [`Shell::layout`].
    pub fn new(section: Section, session: &Session) -> Self {
        Layout {
            section: Some(section),
            flash: None,
            user: Some(SignedIn {
                login: session.login().to_owned(),
                csrf_token: session.csrf_token().to_owned(),
                expires_at: session.expires_at(),
            }),
            counts: NavCounts::default(),
            instance: None,
            host: None,
        }
    }

    /// The layout of a page shown without a session.
    pub fn public() -> Self {
        Layout {
            section: None,
            flash: None,
            user: None,
            counts: NavCounts::default(),
            instance: None,
            host: None,
        }
    }

    pub fn with_flash(mut self, flash: Flash) -> Self {
        self.flash = Some(flash);
        self
    }

    /// The same layout with the navigation showing `counts`.
    #[cfg(test)]
    pub fn with_counts(mut self, counts: NavCounts) -> Self {
        self.counts = counts;
        self
    }

    pub fn nav(&self) -> Vec<NavItem> {
        let Some(current) = self.section else {
            return Vec::new();
        };
        Section::NAV
            .into_iter()
            .map(|section| NavItem {
                href: section.href(),
                icon: section.icon(),
                label: section,
                current: section == current,
                count: self.counts.of(section),
            })
            .collect()
    }

    /// The page's default breadcrumb, after `~/`.
    pub fn path(&self) -> &'static str {
        self.section.map_or("", Section::path)
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

    /// hoster's stack editor script (ADR-0060), loaded only by pages with an editor.
    pub fn editor(&self) -> String {
        Assets::get().editor.url()
    }

    pub fn stylesheet(&self) -> String {
        Assets::get().stylesheet.url()
    }

    pub fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
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
    use crate::web::dashboard::Dashboard;

    fn layout() -> Layout {
        Layout::new(Section::Dashboard, &Session::for_tests("octocat", "c0ffee"))
    }

    #[test]
    fn base_layout_has_sidebar_header_flash_area_and_assets() {
        let html = Dashboard::empty(layout()).render().unwrap();
        assert!(html.contains(r#"<nav class="sidebar" aria-label="Main">"#));
        assert!(html.contains(&format!(
            r#"<a href="/" aria-current="page">{}Dashboard</a>"#,
            crate::web::Icon::Dashboard
        )));
        assert!(
            !html.contains(r#"href="/containers""#),
            "Containers is not in the navigation"
        );
        assert!(
            html.contains(r#"<nav class="crumb" aria-label="Breadcrumb">~/dashboard</nav>"#),
            "{html}"
        );
        assert!(html.contains("<h1>Dashboard</h1>"), "{html}");
        assert!(
            html.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))),
            "the version is in the sidebar"
        );
        assert!(!html.contains("<footer"), "{html}");
        assert!(
            html.contains(
                r#"<body hx-headers:inherited="{&#34;X-CSRF-Token&#34;: &#34;c0ffee&#34;}">"#
            ),
            "{html}"
        );
        assert!(
            html.contains(r#"hx-post="/auth/logout" aria-label="Log out""#),
            "{html}"
        );
        assert!(
            html.contains(r#"hx-post="/auth/logout-everywhere">Log out everywhere</button>"#),
            "{html}"
        );
        assert!(html.contains(r#"<span class="account-login">octocat</span>"#));
        assert!(html.contains(r#"<span class="avatar" aria-hidden="true">O</span>"#));
        assert!(
            html.contains("session · 11h left") || html.contains("session · 12h left"),
            "{html}"
        );
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
        assert!(!html.contains("style="), "no inline styles under the CSP");
    }

    #[test]
    fn the_navigation_counts_stacks_and_failing_certificates() {
        let counts = |stacks, failing| NavCounts {
            stacks,
            failing_certificates: failing,
        };
        let html = Dashboard::empty(layout().with_counts(counts(Some(3), Some(2))))
            .render()
            .unwrap();
        assert!(
            html.contains(r#"Stacks <span class="nav-count">3</span></a>"#),
            "{html}"
        );
        assert!(
            html.contains(
                r#"Certificates <span class="nav-count nav-count-alert" role="img" aria-label="2 failing">2</span></a>"#
            ),
            "{html}"
        );

        // No badge without failures, nor for a count that could not be read.
        let html = Dashboard::empty(layout().with_counts(counts(None, Some(0))))
            .render()
            .unwrap();
        assert!(!html.contains("nav-count"), "{html}");
    }

    #[test]
    fn time_left_is_coarse() {
        let left = |secs| Remaining(Duration::from_secs(secs)).to_string();
        assert_eq!(left(0), "<1m");
        assert_eq!(left(59), "<1m");
        assert_eq!(left(60), "1m");
        assert_eq!(left(59 * 60 + 59), "59m");
        assert_eq!(left(3600), "1h");
        assert_eq!(left(6 * 3600 + 3599), "6h");
    }

    #[test]
    fn the_avatar_shows_the_logins_first_letter() {
        let signed_in = |login: &str| SignedIn {
            login: login.to_owned(),
            csrf_token: String::new(),
            expires_at: Timestamp::now(),
        };
        assert_eq!(signed_in("octocat").initial(), "O");
        assert_eq!(signed_in("").initial(), "?");
        assert_eq!(signed_in("octocat").left(), Remaining(Duration::ZERO));
    }

    #[test]
    fn flash_messages_are_escaped() {
        let html = Dashboard::empty(layout().with_flash(Flash {
            kind: FlashKind::Error,
            message: "<b>deploy failed</b>".into(),
        }))
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
