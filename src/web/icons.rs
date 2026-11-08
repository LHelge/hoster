//! Icons for the admin UI (ADR-0053): Lucide glyphs from the `icondata` crate, compiled into one
//! SVG sprite that is served like the other assets (`/assets/icons.<hash>.svg`, cached for a
//! year). Templates write `{{ crate::web::Icon::Stacks }}`, which renders an inline `<svg>` that
//! `<use>`s the sprite's symbol, so the glyph takes the surrounding text colour (`currentColor`)
//! in light and dark mode.
//!
//! Icons are named by what they mean in hoster, not by their glyph, so changing the look of
//! "redeploy" everywhere is one line in [`Icon::glyph`].

use std::fmt::{self, Write as _};

use icondata::Icon as Glyph;

use super::assets::Assets;

/// An icon the UI uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    /// hoster's mark, in the sidebar and on the login page.
    Brand,
    // Main navigation.
    Dashboard,
    Stacks,
    Containers,
    Certificates,
    Settings,
    System,
    // Status badges.
    Ok,
    Warning,
    Failed,
    Pending,
    // Actions.
    /// Create something: "New stack", "Add a stack".
    New,
    Apply,
    Deploy,
    Redeploy,
    Remove,
    CheckUpdates,
    Update,
    LogOut,
}

impl Icon {
    /// Every icon, in sprite order.
    pub const ALL: [Icon; 19] = [
        Icon::Brand,
        Icon::Dashboard,
        Icon::Stacks,
        Icon::Containers,
        Icon::Certificates,
        Icon::Settings,
        Icon::System,
        Icon::Ok,
        Icon::Warning,
        Icon::Failed,
        Icon::Pending,
        Icon::New,
        Icon::Apply,
        Icon::Deploy,
        Icon::Redeploy,
        Icon::Remove,
        Icon::CheckUpdates,
        Icon::Update,
        Icon::LogOut,
    ];

    /// The notice the ISC and MIT licences ask for in every copy (`assets/lucide.LICENSE`).
    const NOTICE: &str = "Lucide icons, https://lucide.dev. Copyright (c) Lucide Icons and \
                          Contributors (ISC); icons derived from Feather copyright (c) 2013-present \
                          Cole Bemis (MIT). Full text: assets/lucide.LICENSE in the hoster repository.";

    /// The Lucide glyph drawn for this icon.
    fn glyph(self) -> Glyph {
        match self {
            Icon::Brand => icondata::LuLayers,
            Icon::Dashboard => icondata::LuLayoutGrid,
            Icon::Stacks => icondata::LuLayers,
            Icon::Containers => icondata::LuContainer,
            Icon::Certificates => icondata::LuLock,
            Icon::Settings => icondata::LuSlidersVertical,
            Icon::System => icondata::LuServer,
            Icon::Ok => icondata::LuCircleCheck,
            Icon::Warning => icondata::LuTriangleAlert,
            Icon::Failed => icondata::LuCircleX,
            Icon::Pending => icondata::LuClock,
            Icon::New => icondata::LuPlus,
            Icon::Apply => icondata::LuCheck,
            Icon::Deploy => icondata::LuRocket,
            Icon::Redeploy => icondata::LuRotateCcw,
            Icon::Remove => icondata::LuTrash2,
            Icon::CheckUpdates => icondata::LuRefreshCw,
            Icon::Update => icondata::LuDownload,
            Icon::LogOut => icondata::LuLogOut,
        }
    }

    /// The symbol's id in the sprite, the URL fragment templates reference.
    fn id(self) -> &'static str {
        match self {
            Icon::Brand => "brand",
            Icon::Dashboard => "dashboard",
            Icon::Stacks => "stacks",
            Icon::Containers => "containers",
            Icon::Certificates => "certificates",
            Icon::Settings => "settings",
            Icon::System => "system",
            Icon::Ok => "ok",
            Icon::Warning => "warning",
            Icon::Failed => "failed",
            Icon::Pending => "pending",
            Icon::New => "new",
            Icon::Apply => "apply",
            Icon::Deploy => "deploy",
            Icon::Redeploy => "redeploy",
            Icon::Remove => "remove",
            Icon::CheckUpdates => "check-updates",
            Icon::Update => "update",
            Icon::LogOut => "log-out",
        }
    }

    /// The sprite: one `<symbol>` per icon, carrying the glyph's own presentation attributes
    /// (Lucide strokes with `currentColor` and fills nothing).
    pub(super) fn sprite() -> String {
        let mut svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\">\n<!-- {} -->\n",
            Self::NOTICE
        );
        for icon in Self::ALL {
            let glyph = icon.glyph();
            let _ = write!(svg, "<symbol id=\"{}\"", icon.id());
            for (name, value) in [
                ("viewBox", glyph.view_box),
                ("fill", glyph.fill),
                ("stroke", glyph.stroke),
                ("stroke-width", glyph.stroke_width),
                ("stroke-linecap", glyph.stroke_linecap),
                ("stroke-linejoin", glyph.stroke_linejoin),
            ] {
                if let Some(value) = value {
                    let _ = write!(svg, " {name}=\"{value}\"");
                }
            }
            let _ = writeln!(svg, ">{}</symbol>", glyph.data);
        }
        svg.push_str("</svg>\n");
        svg
    }
}

/// The inline markup: decorative, so hidden from assistive technology; the text next to it
/// carries the meaning.
impl fmt::Display for Icon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            r#"<svg class="icon" aria-hidden="true" focusable="false"><use href="{}#{}"></use></svg>"#,
            Assets::get().icons.url(),
            self.id()
        )
    }
}

/// The markup is fixed and contains no user input, so templates insert it unescaped.
impl askama::filters::HtmlSafe for Icon {}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn every_icon_is_a_symbol_with_a_unique_id() {
        let sprite = Icon::sprite();
        let ids: HashSet<_> = Icon::ALL.iter().map(|icon| icon.id()).collect();
        assert_eq!(ids.len(), Icon::ALL.len(), "ids are unique");
        for icon in Icon::ALL {
            assert_eq!(
                sprite
                    .matches(&format!("<symbol id=\"{}\" ", icon.id()))
                    .count(),
                1,
                "{icon:?}"
            );
        }
        assert_eq!(sprite.matches("<symbol ").count(), Icon::ALL.len());
    }

    #[test]
    fn symbols_follow_the_text_colour() {
        let sprite = Icon::sprite();
        assert!(sprite.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\">"));
        assert!(sprite.contains("Lucide"), "carries the licence notice");
        for line in sprite.lines().filter(|l| l.starts_with("<symbol ")) {
            assert!(line.contains(r#" viewBox="0 0 24 24""#), "{line}");
            assert!(line.contains(r#" stroke="currentColor""#), "{line}");
            assert!(line.contains(r#" fill="none""#), "{line}");
        }
    }

    #[test]
    fn markup_references_the_hashed_sprite() {
        let html = Icon::Redeploy.to_string();
        let url = Assets::get().icons.url();
        assert!(
            url.starts_with("/assets/icons.") && url.ends_with(".svg"),
            "{url}"
        );
        assert_eq!(
            html,
            format!(
                r#"<svg class="icon" aria-hidden="true" focusable="false"><use href="{url}#redeploy"></use></svg>"#
            )
        );
    }
}
