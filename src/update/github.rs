//! Releases from GitHub's REST API (`api.github.com/repos/LHelge/hoster/releases`).
//!
//! Calls are unauthenticated and made only when the admin asks (ADR-0052). Every URL, including
//! each redirect, must be https on one of GitHub's release hosts ([`UrlPolicy`]); responses are
//! size-bounded and every request has a total time limit.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Url;
use reqwest::redirect::Policy;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use super::release::{Asset, Release, ReleaseSource, SourceError};
use crate::installation::Version;

/// Which URLs hoster fetches release data from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlPolicy {
    hosts: Vec<String>,
    https_only: bool,
}

impl UrlPolicy {
    /// The API, the release download links and the asset storage they redirect to. GitHub
    /// serves release downloads from `objects.githubusercontent.com` and, since 2025,
    /// `release-assets.githubusercontent.com`.
    const GITHUB_HOSTS: [&str; 4] = [
        "api.github.com",
        "github.com",
        "objects.githubusercontent.com",
        "release-assets.githubusercontent.com",
    ];

    /// GitHub's hosts, https only.
    pub fn github() -> Self {
        Self {
            hosts: Self::GITHUB_HOSTS.map(str::to_owned).to_vec(),
            https_only: true,
        }
    }

    /// Plain http on the given hosts (tests against a local server).
    #[cfg(test)]
    pub fn insecure(hosts: &[&str]) -> Self {
        Self {
            hosts: hosts.iter().map(|h| (*h).to_owned()).collect(),
            https_only: false,
        }
    }

    /// Whether `url` may be fetched.
    pub fn permits(&self, url: &Url) -> bool {
        let scheme_ok = url.scheme() == "https" || (!self.https_only && url.scheme() == "http");
        let host_ok = url
            .host_str()
            .is_some_and(|host| self.hosts.iter().any(|h| h.eq_ignore_ascii_case(host)));
        scheme_ok && host_ok && url.username().is_empty() && url.password().is_none()
    }

    fn check(&self, url: &Url) -> Result<(), SourceError> {
        if self.permits(url) {
            Ok(())
        } else {
            Err(SourceError::UntrustedUrl { url: url.clone() })
        }
    }
}

/// hoster's releases on GitHub.
#[derive(Debug, Clone)]
pub struct GithubReleases {
    http: reqwest::Client,
    /// `https://api.github.com/repos/LHelge/hoster/`.
    repo: Url,
    policy: Arc<UrlPolicy>,
    /// Total time allowed for one download, body included.
    download_timeout: Duration,
}

impl GithubReleases {
    const REPO_API: &str = "https://api.github.com/repos/LHelge/hoster/";
    /// Identifies the software only: no host name, account or other personal data.
    const USER_AGENT: &str = concat!(
        "hoster/",
        env!("CARGO_PKG_VERSION"),
        " (+https://github.com/LHelge/hoster)"
    );
    const API_TIMEOUT: Duration = Duration::from_secs(15);
    const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    /// A release JSON is a few KiB; its notes are generous at 1 MiB.
    const API_LIMIT: u64 = 1024 * 1024;
    const MAX_REDIRECTS: usize = 5;

    /// The production source. TLS is rustls with ring (ADR-0037).
    pub fn new() -> Result<Self, reqwest::Error> {
        Self::with(
            Url::parse(Self::REPO_API).expect("valid URL"),
            UrlPolicy::github(),
        )
    }

    /// A source with the repository API at `repo` and the given URL policy (tests).
    pub(super) fn with(mut repo: Url, policy: UrlPolicy) -> Result<Self, reqwest::Error> {
        // reqwest is built without a bundled provider; make ring the process default unless
        // Pingora already did.
        let _ = rustls::crypto::ring::default_provider().install_default();
        if !repo.path().ends_with('/') {
            repo.set_path(&format!("{}/", repo.path()));
        }
        let policy = Arc::new(policy);
        let redirects = Arc::clone(&policy);
        let http = reqwest::Client::builder()
            .user_agent(Self::USER_AGENT)
            .connect_timeout(Self::CONNECT_TIMEOUT)
            .redirect(Policy::custom(move |attempt| {
                if attempt.previous().len() >= Self::MAX_REDIRECTS {
                    attempt.error("too many redirects")
                } else if redirects.permits(attempt.url()) {
                    attempt.follow()
                } else {
                    let url = attempt.url().clone();
                    attempt.error(SourceError::UntrustedUrl { url })
                }
            }))
            .build()?;
        Ok(Self {
            http,
            repo,
            policy,
            download_timeout: Self::DOWNLOAD_TIMEOUT,
        })
    }

    /// Changes the time allowed for one download (tests).
    #[cfg(test)]
    pub(super) fn with_download_timeout(mut self, timeout: Duration) -> Self {
        self.download_timeout = timeout;
        self
    }

    /// Sends a GET for `url` and returns the successful response.
    async fn get(
        &self,
        url: &Url,
        accept: &str,
        timeout: Duration,
    ) -> Result<reqwest::Response, SourceError> {
        self.policy.check(url)?;
        let response = self
            .http
            .get(url.clone())
            .header(http::header::ACCEPT, accept)
            .header("X-GitHub-Api-Version", "2022-11-28")
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| Self::request_error(url, e))?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let rate_limited = status == http::StatusCode::TOO_MANY_REQUESTS
            || (status == http::StatusCode::FORBIDDEN
                && response
                    .headers()
                    .get("x-ratelimit-remaining")
                    .is_some_and(|v| v == "0"));
        Err(if rate_limited {
            SourceError::RateLimited { url: url.clone() }
        } else {
            SourceError::Status {
                url: url.clone(),
                status,
            }
        })
    }

    fn request_error(url: &Url, error: reqwest::Error) -> SourceError {
        // A redirect the policy refused carries our own error as its source.
        let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
        while let Some(err) = source {
            if let Some(SourceError::UntrustedUrl { url }) = err.downcast_ref::<SourceError>() {
                return SourceError::UntrustedUrl { url: url.clone() };
            }
            source = err.source();
        }
        if error.is_timeout() {
            SourceError::Timeout { url: url.clone() }
        } else {
            SourceError::Request {
                url: url.clone(),
                source: error,
            }
        }
    }

    async fn fetch_release(&self, path: &str) -> Result<GithubRelease, SourceError> {
        let url = self
            .repo
            .join(path)
            .map_err(|e| SourceError::Malformed(e.to_string()))?;
        let response = self
            .get(&url, "application/vnd.github+json", Self::API_TIMEOUT)
            .await?;
        let mut body = BoundedBody::new(url, response, Self::API_LIMIT)?;
        let mut json = Vec::new();
        while let Some(chunk) = body.next().await? {
            json.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&json).map_err(|e| SourceError::Malformed(e.to_string()))
    }

    fn is_not_found(error: &SourceError) -> bool {
        matches!(error, SourceError::Status { status, .. } if *status == http::StatusCode::NOT_FOUND)
    }
}

#[async_trait]
impl ReleaseSource for GithubReleases {
    async fn latest(&self) -> Result<Release, SourceError> {
        let release = match self.fetch_release("releases/latest").await {
            Err(e) if Self::is_not_found(&e) => return Err(SourceError::NoRelease),
            other => other?,
        };
        let stable = !release.prerelease && !release.draft;
        let release = Release::try_from(release)?;
        // GitHub's "latest" excludes pre-releases; check rather than trust it.
        if !stable || release.version.is_prerelease() {
            return Err(SourceError::NotStable(release.version));
        }
        Ok(release)
    }

    async fn release(&self, version: &Version) -> Result<Release, SourceError> {
        let release = match self
            .fetch_release(&format!("releases/tags/{version}"))
            .await
        {
            Err(e) if Self::is_not_found(&e) => {
                return Err(SourceError::UnknownRelease(version.clone()));
            }
            other => Release::try_from(other?)?,
        };
        if release.version != *version {
            return Err(SourceError::Malformed(format!(
                "asked for {version}, got {}",
                release.version
            )));
        }
        Ok(release)
    }

    async fn download(&self, asset: &Asset, limit: u64, dest: &Path) -> Result<(), SourceError> {
        let io_err = |source| SourceError::Io {
            path: dest.to_owned(),
            source,
        };
        if asset.size.is_some_and(|size| size > limit) {
            return Err(SourceError::TooLarge {
                url: asset.url.clone(),
                limit,
            });
        }
        let response = self
            .get(
                &asset.url,
                "application/octet-stream",
                self.download_timeout,
            )
            .await?;
        let mut body = BoundedBody::new(asset.url.clone(), response, limit)?;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dest)
            .await
            .map_err(io_err)?;
        while let Some(chunk) = body.next().await? {
            file.write_all(&chunk).await.map_err(io_err)?;
        }
        file.flush().await.map_err(io_err)?;
        file.sync_all().await.map_err(io_err)
    }
}

/// A response body read chunk by chunk that fails once it exceeds its limit.
struct BoundedBody {
    url: Url,
    response: reqwest::Response,
    limit: u64,
    read: u64,
}

impl BoundedBody {
    /// Fails at once when the announced length is over `limit`.
    fn new(url: Url, response: reqwest::Response, limit: u64) -> Result<Self, SourceError> {
        if response.content_length().is_some_and(|len| len > limit) {
            return Err(SourceError::TooLarge { url, limit });
        }
        Ok(Self {
            url,
            response,
            limit,
            read: 0,
        })
    }

    /// The next chunk; `None` at the end of the body.
    async fn next(&mut self) -> Result<Option<bytes::Bytes>, SourceError> {
        let chunk = self
            .response
            .chunk()
            .await
            .map_err(|e| GithubReleases::request_error(&self.url, e))?;
        if let Some(chunk) = &chunk {
            self.read += chunk.len() as u64;
            if self.read > self.limit {
                return Err(SourceError::TooLarge {
                    url: self.url.clone(),
                    limit: self.limit,
                });
            }
        }
        Ok(chunk)
    }
}

/// The parts of GitHub's release object hoster reads.
#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GithubAsset>,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    html_url: Option<Url>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: Url,
    #[serde(default)]
    size: Option<u64>,
}

impl TryFrom<GithubRelease> for Release {
    type Error = SourceError;

    fn try_from(release: GithubRelease) -> Result<Self, Self::Error> {
        let version = release.tag_name.parse().map_err(|e| {
            SourceError::Malformed(format!("tag {:?} is not a version: {e}", release.tag_name))
        })?;
        // Only shown as a link, never fetched; still only GitHub's own https pages.
        let page = release
            .html_url
            .filter(|url| url.scheme() == "https" && url.host_str() == Some("github.com"));
        Ok(Release {
            version,
            notes: release.body.unwrap_or_default(),
            published: release
                .published_at
                .and_then(|at| humantime::parse_rfc3339(&at).ok()),
            page,
            assets: release
                .assets
                .into_iter()
                .map(|a| Asset {
                    name: a.name,
                    url: a.browser_download_url,
                    size: a.size,
                })
                .collect(),
        })
    }
}
