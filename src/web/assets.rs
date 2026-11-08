//! Static assets embedded in the binary (ADR-0021): the vendored HTMX build, its SSE extension
//! and hoster's stylesheet. Each is served under a content-hashed name such as
//! `/assets/htmx.min.a0abb04f2f45.js`, so it can be cached for a year; a changed file gets a new
//! URL.

use std::fmt::Write as _;
use std::sync::LazyLock;

use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

/// One embedded file.
#[derive(Debug)]
pub struct Asset {
    content_type: &'static str,
    bytes: &'static [u8],
    /// The served name: `name` with the first 12 hex digits of its SHA-256 before the extension.
    hashed_name: String,
}

impl Asset {
    /// How long browsers may cache an asset: a year, never revalidated.
    const CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
    /// Hex digits of the SHA-256 put into the served name.
    const HASH_LEN: usize = 12;

    fn new(name: &'static str, content_type: &'static str, bytes: &'static [u8]) -> Self {
        let hash = Self::sha256_hex(bytes);
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
            self.bytes,
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
}

impl Assets {
    /// The assets compiled into this binary.
    pub fn get() -> &'static Assets {
        static ASSETS: LazyLock<Assets> = LazyLock::new(|| Assets {
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
            stylesheet: Asset::new(
                "hoster.css",
                "text/css; charset=utf-8",
                include_bytes!("../../assets/hoster.css"),
            ),
        });
        &ASSETS
    }

    fn all(&'static self) -> [&'static Asset; 3] {
        [&self.htmx, &self.htmx_sse, &self.stylesheet]
    }

    /// The asset served as `hashed_name`. Unhashed or stale names are not found, so a cached
    /// page never pairs with the wrong file.
    fn find(&'static self, hashed_name: &str) -> Option<&'static Asset> {
        self.all()
            .into_iter()
            .find(|a| a.hashed_name == hashed_name)
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
        for (file, asset) in [
            ("htmx.min.js", &assets.htmx),
            ("hx-sse.min.js", &assets.htmx_sse),
        ] {
            let checksum = Asset::sha256_hex(asset.bytes);
            let row = VENDORED
                .lines()
                .find(|line| line.starts_with(&format!("| `{file}`")))
                .unwrap_or_else(|| panic!("{file} is listed in assets/VENDORED.md"));
            assert!(
                row.contains(&checksum),
                "assets/VENDORED.md does not record the SHA-256 {checksum} of {file}"
            );
            assert!(row.contains("| 4.0."), "HTMX 4.0.x is vendored: {row}");
        }
    }

    #[test]
    fn only_hashed_names_are_found() {
        let assets = Assets::get();
        assert!(assets.find(&assets.stylesheet.hashed_name).is_some());
        assert!(assets.find("hoster.css").is_none());
        assert!(assets.find("htmx.min.js").is_none());
    }
}
