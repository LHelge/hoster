//! Server-rendered pages (ADR-0021). Templates live in `templates/` and are compiled into the
//! binary by Askama; every page extends `base.html`, which reads the page's [`Layout`].

use std::fmt;

use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

use super::assets::Assets;

/// A section of the admin UI, as listed in the navigation bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Dashboard,
}

impl Section {
    const ALL: [Section; 1] = [Section::Dashboard];

    fn href(self) -> &'static str {
        match self {
            Section::Dashboard => "/",
        }
    }
}

impl fmt::Display for Section {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Section::Dashboard => "Dashboard",
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
#[allow(dead_code, reason = "used by the pages that report outcomes")]
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

/// What `base.html` needs from every page.
pub struct Layout {
    pub section: Section,
    pub flash: Option<Flash>,
}

impl Layout {
    pub fn new(section: Section) -> Self {
        Layout {
            section,
            flash: None,
        }
    }

    #[allow(dead_code, reason = "used by the pages that report outcomes")]
    pub fn with_flash(mut self, flash: Flash) -> Self {
        self.flash = Some(flash);
        self
    }

    pub fn nav(&self) -> Vec<NavItem> {
        Section::ALL
            .into_iter()
            .map(|section| NavItem {
                href: section.href(),
                label: section,
                current: section == self.section,
            })
            .collect()
    }

    pub fn htmx(&self) -> String {
        Assets::get().htmx.url()
    }

    pub fn stylesheet(&self) -> String {
        Assets::get().stylesheet.url()
    }

    pub fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }
}

/// The placeholder dashboard at `/`.
#[derive(Template)]
#[template(path = "dashboard.html")]
pub struct Dashboard {
    pub layout: Layout,
}

impl Dashboard {
    /// The `GET /` handler.
    pub async fn show() -> Page<Dashboard> {
        Page(Dashboard {
            layout: Layout::new(Section::Dashboard),
        })
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

    #[test]
    fn base_layout_has_nav_flash_area_and_assets() {
        let html = Dashboard {
            layout: Layout::new(Section::Dashboard),
        }
        .render()
        .unwrap();
        assert!(html.contains(r#"<a href="/" aria-current="page">Dashboard</a>"#));
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
            layout: Layout::new(Section::Dashboard).with_flash(Flash {
                kind: FlashKind::Error,
                message: "<b>deploy failed</b>".into(),
            }),
        }
        .render()
        .unwrap();
        assert!(html.contains(r#"class="flash flash-error""#));
        assert!(
            html.contains("&#60;b&#62;deploy failed&#60;/b&#62;"),
            "{html}"
        );
    }
}
