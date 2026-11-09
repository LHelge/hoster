//! Static assets embedded in the binary (ADR-0021): the vendored HTMX build, its SSE extension,
//! the JetBrains Mono subset (ADR-0054), hoster's stylesheet and the icon sprite (ADR-0053). Each
//! is served under a content-hashed name such as `/assets/htmx.min.a0abb04f2f45.js`, so it can be
//! cached for a year; a changed file gets a new URL.
//!
//! The stylesheet names its fonts by their plain file names (`url("jetbrains-mono-400.woff2")`);
//! [`Assets::get`] rewrites those to the hashed names before hashing the stylesheet itself, so a
//! changed font also changes the stylesheet's URL.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::sync::LazyLock;

use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

use super::icons::Icon;

/// One embedded file.
#[derive(Debug)]
pub struct Asset {
    content_type: &'static str,
    /// Borrowed for files compiled in with `include_bytes!`, owned for generated ones.
    bytes: Cow<'static, [u8]>,
    /// The served name: `name` with the first 12 hex digits of its SHA-256 before the extension.
    hashed_name: String,
}

impl Asset {
    /// How long browsers may cache an asset: a year, never revalidated.
    const CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
    /// Hex digits of the SHA-256 put into the served name.
    const HASH_LEN: usize = 12;

    fn new(
        name: &'static str,
        content_type: &'static str,
        bytes: impl Into<Cow<'static, [u8]>>,
    ) -> Self {
        let bytes = bytes.into();
        let hash = Self::sha256_hex(&bytes);
        let (stem, ext) = name
            .rsplit_once('.')
            .expect("asset names have an extension");
        let hashed_name = format!("{stem}.{}.{ext}", &hash[..Self::HASH_LEN]);
        Asset {
            content_type,
            bytes,
            hashed_name,
        }
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .fold(String::with_capacity(64), |mut s, b| {
                let _ = write!(s, "{b:02x}");
                s
            })
    }

    /// `hoster.css` with every `url("<font name>")` pointing at that font's hashed name. The
    /// stylesheet sits next to the fonts under `/assets/`, so the URLs stay relative.
    fn stylesheet(css: &'static str, fonts: &[Asset]) -> Self {
        let css = fonts.iter().fold(css.to_owned(), |css, font| {
            let plain = format!(r#"url("{}")"#, font.name());
            assert!(css.contains(&plain), "hoster.css uses {plain}");
            css.replace(&plain, &format!(r#"url("{}")"#, font.hashed_name))
        });
        Asset::new("hoster.css", "text/css; charset=utf-8", css.into_bytes())
    }

    /// The file name without the hash.
    fn name(&self) -> String {
        let (stem, ext) = self.hashed_name.rsplit_once('.').expect("has an extension");
        let (stem, _) = stem.rsplit_once('.').expect("has a hash");
        format!("{stem}.{ext}")
    }

    /// The served file name, `<stem>.<hash>.<ext>`.
    #[cfg(test)]
    pub fn hashed_name(&self) -> &str {
        &self.hashed_name
    }

    /// The file's contents.
    #[cfg(test)]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The URL path the asset is served at.
    pub fn url(&self) -> String {
        format!("/assets/{}", self.hashed_name)
    }
}

impl IntoResponse for &'static Asset {
    fn into_response(self) -> Response {
        (
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static(self.content_type),
                ),
                (
                    header::CACHE_CONTROL,
                    HeaderValue::from_static(Asset::CACHE_CONTROL),
                ),
            ],
            &*self.bytes,
        )
            .into_response()
    }
}

/// Every embedded asset.
#[derive(Debug)]
pub struct Assets {
    pub htmx: Asset,
    /// HTMX's SSE extension, for live views such as container logs (ADR-0044).
    pub htmx_sse: Asset,
    pub stylesheet: Asset,
    /// Every [`Icon`] as one SVG sprite, generated at startup.
    pub icons: Asset,
    /// JetBrains Mono, Latin subset, at the weights the stylesheet uses (400, 500, 600).
    pub fonts: [Asset; 3],
}

impl Assets {
    /// The assets compiled into this binary.
    pub fn get() -> &'static Assets {
        static ASSETS: LazyLock<Assets> = LazyLock::new(|| {
            let fonts = [
                Asset::new(
                    "jetbrains-mono-400.woff2",
                    "font/woff2",
                    include_bytes!("../../assets/jetbrains-mono-400.woff2"),
                ),
                Asset::new(
                    "jetbrains-mono-500.woff2",
                    "font/woff2",
                    include_bytes!("../../assets/jetbrains-mono-500.woff2"),
                ),
                Asset::new(
                    "jetbrains-mono-600.woff2",
                    "font/woff2",
                    include_bytes!("../../assets/jetbrains-mono-600.woff2"),
                ),
            ];
            let stylesheet = Asset::stylesheet(include_str!("../../assets/hoster.css"), &fonts);
            Assets {
                htmx: Asset::new(
                    "htmx.min.js",
                    "text/javascript; charset=utf-8",
                    include_bytes!("../../assets/htmx.min.js"),
                ),
                htmx_sse: Asset::new(
                    "hx-sse.min.js",
                    "text/javascript; charset=utf-8",
                    include_bytes!("../../assets/hx-sse.min.js"),
                ),
                stylesheet,
                icons: Asset::new(
                    "icons.svg",
                    "image/svg+xml; charset=utf-8",
                    Icon::sprite().into_bytes(),
                ),
                fonts,
            }
        });
        &ASSETS
    }

    /// Every asset, in no particular order.
    pub fn all(&'static self) -> impl Iterator<Item = &'static Asset> {
        [&self.htmx, &self.htmx_sse, &self.stylesheet, &self.icons]
            .into_iter()
            .chain(&self.fonts)
    }

    /// The asset served as `hashed_name`. Unhashed or stale names are not found, so a cached
    /// page never pairs with the wrong file.
    fn find(&'static self, hashed_name: &str) -> Option<&'static Asset> {
        self.all().find(|a| a.hashed_name == hashed_name)
    }

    /// The `GET /assets/{file}` handler.
    pub async fn serve(Path(file): Path<String>) -> Response {
        match Assets::get().find(&file) {
            Some(asset) => asset.into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VENDORED: &str = include_str!("../../assets/VENDORED.md");

    #[test]
    fn hashed_names_keep_the_extension() {
        let htmx = &Assets::get().htmx;
        assert!(htmx.hashed_name.starts_with("htmx.min."));
        assert!(htmx.hashed_name.ends_with(".js"));
        assert_eq!(
            htmx.hashed_name.len(),
            "htmx.min..js".len() + Asset::HASH_LEN
        );
        assert_eq!(htmx.url(), format!("/assets/{}", htmx.hashed_name));
    }

    #[test]
    fn vendored_files_match_their_recorded_checksums() {
        let assets = Assets::get();
        for (file, asset, version) in [
            ("htmx.min.js", &assets.htmx, "| 4.0."),
            ("hx-sse.min.js", &assets.htmx_sse, "| 4.0."),
            ("jetbrains-mono-400.woff2", &assets.fonts[0], "| 2.304"),
            ("jetbrains-mono-500.woff2", &assets.fonts[1], "| 2.304"),
            ("jetbrains-mono-600.woff2", &assets.fonts[2], "| 2.304"),
        ] {
            let checksum = Asset::sha256_hex(&asset.bytes);
            let row = VENDORED
                .lines()
                .find(|line| line.starts_with(&format!("| `{file}`")))
                .unwrap_or_else(|| panic!("{file} is listed in assets/VENDORED.md"));
            assert!(
                row.contains(&checksum),
                "assets/VENDORED.md does not record the SHA-256 {checksum} of {file}"
            );
            assert!(row.contains(version), "{file} is version {version}: {row}");
        }
    }

    #[test]
    fn the_stylesheet_loads_the_hashed_fonts() {
        let assets = Assets::get();
        let css = std::str::from_utf8(&assets.stylesheet.bytes).unwrap();
        for font in &assets.fonts {
            assert_eq!(font.content_type, "font/woff2");
            assert!(font.bytes.starts_with(b"wOF2"), "{}", font.hashed_name);
            assert!(
                css.contains(&format!(r#"url("{}") format("woff2")"#, font.hashed_name)),
                "{css}"
            );
            assert!(!css.contains(&format!(r#"url("{}")"#, font.name())));
            assert!(assets.find(&font.hashed_name).is_some());
        }
        assert!(css.contains("font-display: swap"), "{css}");
        assert!(!css.contains("http"), "no external URLs in the stylesheet");
    }

    /// The colour tokens of one flavour in `hoster.css`: every `--name: #rrggbb;` in `block`.
    fn tokens(block: &str) -> std::collections::HashMap<&str, [f64; 3]> {
        block
            .lines()
            .filter_map(|line| {
                let (name, rest) = line.trim().strip_prefix("--")?.split_once(':')?;
                let hex = rest.trim().strip_prefix('#')?.get(..6)?;
                let channel = |i: usize| f64::from(u8::from_str_radix(&hex[i..i + 2], 16).unwrap());
                Some((name, [channel(0), channel(2), channel(4)]))
            })
            .collect()
    }

    /// WCAG 2 contrast ratio of two sRGB colours.
    fn contrast(a: [f64; 3], b: [f64; 3]) -> f64 {
        let luminance = |c: [f64; 3]| {
            let [r, g, b] = c.map(|v| {
                let v = v / 255.0;
                if v <= 0.03928 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            });
            0.2126 * r + 0.7152 * g + 0.0722 * b
        };
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    /// ADR-0054: every text and status colour reaches 4.5:1 on the page, card and input
    /// backgrounds, and on its own tint over them (pills, tags: 12% for the accent, 10% for the
    /// rest), in Latte and Mocha. Hovered rows, nav items and ghost buttons (`--hover`) carry
    /// only `--text`, `--text-2` and the accent. Input borders reach 3:1 on the card and the field.
    #[test]
    fn text_colours_meet_the_contrast_rule_in_both_themes() {
        let css = include_str!("../../assets/hoster.css");
        let dark_at = css.find("@media (prefers-color-scheme: dark)").unwrap();
        let latte = &css[css.find("/* Catppuccin Latte. */").unwrap()..dark_at];
        let mocha = &css[dark_at..];
        let mocha = &mocha[..mocha.find("\n}\n").unwrap()];
        for (flavour, block) in [("Latte", latte), ("Mocha", mocha)] {
            let tokens = tokens(block);
            let token = |name: &str| *tokens.get(name).unwrap_or_else(|| panic!("--{name}"));
            let backgrounds = ["bg", "surface", "inset"].map(token);
            for text in [
                "text", "text-2", "muted", "accent", "link", "ok", "progress", "warn", "fail",
                "p50", "p95",
            ] {
                let colour = token(text);
                let share = if text == "accent" { 0.12 } else { 0.1 };
                for background in backgrounds {
                    let tint =
                        std::array::from_fn(|i| share * colour[i] + (1.0 - share) * background[i]);
                    for (on, behind) in [("background", background), ("tint", tint)] {
                        let ratio = contrast(colour, behind);
                        assert!(
                            ratio >= 4.5,
                            "{flavour}: --{text} on a {on} {behind:?} is only {ratio:.2}:1"
                        );
                    }
                }
            }
            for text in ["text", "text-2", "accent"] {
                let ratio = contrast(token(text), token("hover"));
                assert!(
                    ratio >= 4.5,
                    "{flavour}: --{text} on --hover is only {ratio:.2}:1"
                );
            }
            // WCAG 1.4.11: the border is a text field's only edge, so it needs 3:1 against the
            // field and the card around it.
            for background in ["surface", "inset"] {
                let ratio = contrast(token("input-border"), token(background));
                assert!(
                    ratio >= 3.0,
                    "{flavour}: --input-border on --{background} is only {ratio:.2}:1"
                );
            }
            let ratio = contrast(token("on-accent"), token("accent"));
            assert!(
                ratio >= 4.5,
                "{flavour}: --on-accent on --accent is {ratio:.2}:1"
            );
        }
    }

    #[test]
    fn only_hashed_names_are_found() {
        let assets = Assets::get();
        assert!(assets.find(&assets.stylesheet.hashed_name).is_some());
        assert!(assets.find(&assets.icons.hashed_name).is_some());
        assert!(assets.find("hoster.css").is_none());
        assert!(assets.find("icons.svg").is_none());
        assert!(assets.find("htmx.min.js").is_none());
    }
}
