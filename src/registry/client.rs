//! Generic OCI distribution client: tag → digest resolution (ADR-0005, ADR-0025).
//!
//! [`RegistryClient::resolve`] sends `HEAD /v2/<repo>/manifests/<tag>` and returns the
//! `Docker-Content-Digest`. Authentication is discovered, not configured: a `401` carries a
//! `WWW-Authenticate` challenge, and for `Bearer` the client fetches a pull token from the
//! challenge's realm (ghcr: `ghcr.io/token`; Docker Hub: `auth.docker.io/token`), anonymously or
//! with the registry host's [`RegistryCredential`] as basic auth. Tokens are cached per
//! (registry, repository) until they expire; a `401` on a cached token refreshes it once.
//! Everything is verified against ghcr in docs/architecture.md §1.4.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::{Response, StatusCode, Url};
use serde::Deserialize;
use tokio::time::Instant;

use super::{CredentialError, RegistryCredential, RegistryCredentials};
use crate::stack::{Digest, ImageRef, PinnedImage};

/// Manifest media types accepted by the `HEAD`: multi-arch indexes first, so a multi-arch tag
/// resolves to its index digest (docs/architecture.md §1.4, ADR-0005).
const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.v2+json";

/// The response header carrying the manifest digest.
const DIGEST_HEADER: &str = "docker-content-digest";

/// Docker Hub's registry API lives on a different host than its image names.
const DOCKER_HUB: (&str, &str) = ("docker.io", "https://registry-1.docker.io/");

/// Token lifetime when the token server does not say; the distribution token spec's minimum.
const MIN_TOKEN_LIFETIME: Duration = Duration::from_secs(60);

/// Longest a token is cached, whatever `expires_in` claims. This also keeps a bogus huge
/// `expires_in` from overflowing the expiry `Instant`, which would panic.
const MAX_TOKEN_LIFETIME: Duration = Duration::from_secs(24 * 3600);

/// Tokens are dropped this long before the server-side expiry, so a request in flight does not
/// carry a token that expires on arrival.
const TOKEN_EXPIRY_MARGIN: Duration = Duration::from_secs(10);

/// How long a "this registry wants basic auth" answer is reused.
const BASIC_AUTH_LIFETIME: Duration = Duration::from_secs(3600);

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a tag could not be resolved. Messages never contain a credential or token.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error(transparent)]
    Credentials(#[from] CredentialError),
    #[error("cannot build the registry HTTP client: {0}")]
    Client(#[source] reqwest::Error),
    #[error("`{image}` names neither a tag nor a digest; there is nothing to resolve")]
    NoTag { image: String },
    #[error("request to {registry} failed: {source}")]
    Http {
        registry: String,
        #[source]
        source: reqwest::Error,
    },
    #[error(
        "{registry} refused access to `{image}` ({status}); check that the image exists and \
         that the {registry} credential can read it"
    )]
    Unauthorized {
        registry: String,
        image: String,
        status: StatusCode,
    },
    #[error(
        "{registry} refused the token request ({status}); check the username and token of the \
         {registry} credential"
    )]
    Rejected {
        registry: String,
        status: StatusCode,
    },
    #[error("`{image}` was not found on its registry")]
    NotFound { image: String },
    #[error("{registry} is rate-limiting requests{}", retry_after_text(*.retry_after))]
    RateLimited {
        registry: String,
        retry_after: Option<Duration>,
    },
    #[error("{registry} answered {status} to {what}")]
    Status {
        registry: String,
        what: &'static str,
        status: StatusCode,
    },
    #[error("{registry} sent an unusable authentication challenge: {reason}")]
    Challenge { registry: String, reason: String },
    #[error("the token server for {registry} sent an unusable response: {reason}")]
    Token { registry: String, reason: String },
    #[error("{registry} sent no usable digest for `{image}`: {reason}")]
    Digest {
        registry: String,
        image: String,
        reason: String,
    },
}

fn retry_after_text(retry_after: Option<Duration>) -> String {
    retry_after.map_or_else(String::new, |d| {
        format!("; retry after {}", humantime::format_duration(d))
    })
}

impl RegistryError {
    /// How long the registry asked us to wait, for a [`RateLimited`](Self::RateLimited) error.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// The kind of failure in a few words, for the UI's "test" result (`401 Unauthorized`,
    /// `429 Too Many Requests`, `network error`, …). Never contains a credential.
    pub fn class(&self) -> String {
        match self {
            Self::Unauthorized { status, .. }
            | Self::Rejected { status, .. }
            | Self::Status { status, .. } => status.to_string(),
            Self::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS.to_string(),
            Self::NotFound { .. } => StatusCode::NOT_FOUND.to_string(),
            Self::Http { .. } => "network error".to_owned(),
            Self::Credentials(_) => "credential store error".to_owned(),
            Self::Client(_) => "client error".to_owned(),
            Self::NoTag { .. } => "nothing to resolve".to_owned(),
            Self::Challenge { .. } | Self::Token { .. } | Self::Digest { .. } => {
                "unusable registry response".to_owned()
            }
        }
    }
}

/// What [`RegistryClient::check`] found out about a registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryCheck {
    /// The registry API answers without authentication.
    Open,
    /// The token request succeeded without a credential (none is stored for the registry).
    Anonymous,
    /// The token request succeeded with the registry's stored credential.
    Authenticated,
}

impl fmt::Display for RegistryCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Open => "the registry API answers without authentication",
            Self::Anonymous => "an anonymous token request succeeded (no credential is stored)",
            Self::Authenticated => "a token request with the stored credential succeeded",
        })
    }
}

/// A parsed `WWW-Authenticate` challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Challenge {
    /// Fetch a token from `realm` (distribution token authentication).
    Bearer {
        realm: String,
        service: Option<String>,
        scope: Option<String>,
    },
    /// Send the credential itself as basic auth.
    Basic,
}

/// Parses one challenge: `<scheme> key="value", key=token, …`. Quoted values may contain `,`
/// and backslash escapes.
impl FromStr for Challenge {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let (scheme, rest) = s.split_once(char::is_whitespace).unwrap_or((s, ""));
        let params = Self::params(rest)?;
        if scheme.eq_ignore_ascii_case("basic") {
            return Ok(Self::Basic);
        }
        if !scheme.eq_ignore_ascii_case("bearer") {
            return Err(format!("unsupported scheme `{scheme}`"));
        }
        let mut params = params;
        let realm = params
            .remove("realm")
            .ok_or_else(|| "the Bearer challenge has no realm".to_owned())?;
        Ok(Self::Bearer {
            realm,
            service: params.remove("service"),
            scope: params.remove("scope"),
        })
    }
}

impl Challenge {
    /// The auth-params after the scheme, with lowercased keys.
    fn params(mut rest: &str) -> Result<BTreeMap<String, String>, String> {
        let mut params = BTreeMap::new();
        loop {
            rest = rest.trim_start_matches([' ', '\t', ',']);
            if rest.is_empty() {
                return Ok(params);
            }
            let (key, after) = rest
                .split_once('=')
                .ok_or_else(|| format!("expected `key=value` at `{rest}`"))?;
            let key = key.trim().to_ascii_lowercase();
            let after = after.trim_start();
            let value;
            if let Some(quoted) = after.strip_prefix('"') {
                let mut out = String::new();
                let mut chars = quoted.char_indices();
                let end = loop {
                    match chars.next() {
                        Some((i, '"')) => break i,
                        Some((_, '\\')) => match chars.next() {
                            Some((_, c)) => out.push(c),
                            None => return Err("unterminated escape".to_owned()),
                        },
                        Some((_, c)) => out.push(c),
                        None => return Err(format!("unterminated quoted value for `{key}`")),
                    }
                };
                value = out;
                rest = &quoted[end + 1..];
            } else {
                let end = after.find(',').unwrap_or(after.len());
                value = after[..end].trim().to_owned();
                rest = &after[end..];
            }
            params.insert(key, value);
        }
    }

    /// The first usable challenge among a response's `WWW-Authenticate` headers, preferring
    /// `Bearer`.
    fn from_headers(registry: &str, headers: &HeaderMap) -> Result<Self, RegistryError> {
        let mut found = None;
        let mut last_error = "no WWW-Authenticate header".to_owned();
        for value in headers.get_all(header::WWW_AUTHENTICATE) {
            let parsed = value
                .to_str()
                .map_err(|_| "a WWW-Authenticate header is not ASCII".to_owned())
                .and_then(str::parse::<Self>);
            match parsed {
                Ok(bearer @ Self::Bearer { .. }) => return Ok(bearer),
                Ok(other) => found = Some(other),
                Err(e) => last_error = e,
            }
        }
        found.ok_or_else(|| RegistryError::Challenge {
            registry: registry.to_owned(),
            reason: last_error,
        })
    }
}

/// A token server's answer (distribution token spec; Docker Hub sends both token fields).
#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
    expires_in: Option<u64>,
}

impl TokenResponse {
    /// How long the token is valid: `expires_in`, but never less than the spec's minimum and
    /// never more than [`MAX_TOKEN_LIFETIME`].
    fn lifetime(&self) -> Duration {
        self.expires_in
            .map_or(MIN_TOKEN_LIFETIME, Duration::from_secs)
            .clamp(MIN_TOKEN_LIFETIME, MAX_TOKEN_LIFETIME)
    }

    /// The bearer token: `token`, or Docker Hub's `access_token`, whichever is non-empty.
    fn into_token(self) -> Option<String> {
        [self.token, self.access_token]
            .into_iter()
            .flatten()
            .find(|t| !t.is_empty())
    }
}

/// An `Authorization` header value and when it stops being usable.
#[derive(Debug, Clone)]
struct CachedAuth {
    header: HeaderValue,
    expires_at: Instant,
}

impl CachedAuth {
    fn new(mut header: HeaderValue, issued_at: Instant, lifetime: Duration) -> Self {
        header.set_sensitive(true);
        let usable = lifetime.saturating_sub(TOKEN_EXPIRY_MARGIN);
        Self {
            header,
            expires_at: issued_at + usable,
        }
    }

    fn is_fresh(&self, now: Instant) -> bool {
        now < self.expires_at
    }
}

/// The OCI distribution client. Cheap to share behind an `Arc`; the token cache is internal.
pub struct RegistryClient {
    http: reqwest::Client,
    credentials: Arc<dyn RegistryCredentials>,
    /// API base URLs that differ from `https://<registry>/`.
    endpoints: BTreeMap<String, Url>,
    /// Keyed by (registry, repository): tokens are scoped to `repository:<repo>:pull`.
    auth: Mutex<HashMap<(String, String), CachedAuth>>,
}

impl fmt::Debug for RegistryClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegistryClient")
            .field("credentials", &self.credentials)
            .field("endpoints", &self.endpoints)
            .finish_non_exhaustive()
    }
}

impl RegistryClient {
    /// A client using `credentials` for token requests (ADR-0025). TLS is rustls with ring
    /// (ADR-0037).
    pub fn new(credentials: Arc<dyn RegistryCredentials>) -> Result<Self, RegistryError> {
        // reqwest is built without a bundled provider; make ring the process default unless
        // Pingora already did.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .user_agent(concat!("hoster/", env!("CARGO_PKG_VERSION")))
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(RegistryError::Client)?;
        let (hub, hub_url) = DOCKER_HUB;
        Ok(Self {
            http,
            credentials,
            endpoints: BTreeMap::from([(hub.to_owned(), Url::parse(hub_url).expect("valid URL"))]),
            auth: Mutex::new(HashMap::new()),
        })
    }

    /// Talk to `registry` at `base` instead of `https://<registry>/` (tests, plain-HTTP local
    /// registries).
    pub fn with_endpoint(mut self, registry: impl Into<String>, mut base: Url) -> Self {
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        self.endpoints.insert(registry.into(), base);
        self
    }

    /// Resolves `image`'s tag to the digest the registry currently serves for it and returns
    /// the image pinned to that digest (any digest it already named is replaced). An image with
    /// a digest but no tag is returned as it is, without contacting the registry.
    pub async fn resolve(&self, image: &ImageRef) -> Result<PinnedImage, RegistryError> {
        let Some(tag) = image.tag() else {
            return PinnedImage::try_from(image.clone()).map_err(|_| RegistryError::NoTag {
                image: image.to_string(),
            });
        };
        let digest = self.manifest_digest(image, tag).await?;
        Ok(image.clone().pin(digest))
    }

    /// Tests access to `registry` (the UI's "test" action, ADR-0025): `GET /v2/`, and when that
    /// asks for authentication, the token request the challenge names, with the registry's
    /// credential if one is stored and anonymously otherwise. No scope is requested, so this
    /// checks the credential itself, not access to any one repository. Nothing is cached.
    pub async fn check(&self, registry: &str) -> Result<RegistryCheck, RegistryError> {
        let base = self.endpoint(registry)?;
        let url = base.join("v2/").map_err(|e| RegistryError::Challenge {
            registry: registry.to_owned(),
            reason: format!("invalid API URL: {e}"),
        })?;
        let response = self
            .http
            .get(url.clone())
            .send()
            .await
            .map_err(|e| Self::http_error(registry, e))?;
        match response.status() {
            status if status.is_success() => return Ok(RegistryCheck::Open),
            StatusCode::UNAUTHORIZED => {}
            StatusCode::TOO_MANY_REQUESTS => return Err(Self::rate_limited(registry, &response)),
            status => {
                return Err(RegistryError::Status {
                    registry: registry.to_owned(),
                    what: "the API version check",
                    status,
                });
            }
        }
        let challenge = Challenge::from_headers(registry, response.headers())?;
        let credential = self.credentials.lookup(registry).await?;
        let outcome = if credential.is_some() {
            RegistryCheck::Authenticated
        } else {
            RegistryCheck::Anonymous
        };
        match challenge {
            Challenge::Basic => {
                let Some(credential) = credential else {
                    return Err(RegistryError::Rejected {
                        registry: registry.to_owned(),
                        status: StatusCode::UNAUTHORIZED,
                    });
                };
                let response = self
                    .http
                    .get(url)
                    .header(header::AUTHORIZATION, Self::basic_header(&credential))
                    .send()
                    .await
                    .map_err(|e| Self::http_error(registry, e))?;
                match response.status() {
                    status if status.is_success() => Ok(outcome),
                    status @ (StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) => {
                        Err(RegistryError::Rejected {
                            registry: registry.to_owned(),
                            status,
                        })
                    }
                    StatusCode::TOO_MANY_REQUESTS => Err(Self::rate_limited(registry, &response)),
                    status => Err(RegistryError::Status {
                        registry: registry.to_owned(),
                        what: "the API version check",
                        status,
                    }),
                }
            }
            Challenge::Bearer { realm, service, .. } => {
                let realm = self.realm_url(registry, &base, &realm)?;
                self.fetch_token(registry, realm, service, None, credential, Instant::now())
                    .await?;
                Ok(outcome)
            }
        }
    }

    /// Drops every cached token of `registry`, so the next request authenticates with its
    /// current credential (after the credential was replaced or deleted).
    pub fn forget_registry(&self, registry: &str) {
        self.auth_cache()
            .retain(|(cached, _), _| cached != registry);
    }

    /// Checks that `image`'s repository holds a manifest with `digest` (ADR-0042): a
    /// [`RegistryError::NotFound`] means it does not, so the digest belongs to some other
    /// repository or to none.
    pub async fn contains(&self, image: &ImageRef, digest: &Digest) -> Result<(), RegistryError> {
        let served = self.manifest_digest(image, digest.as_ref()).await?;
        if &served == digest {
            Ok(())
        } else {
            Err(RegistryError::Digest {
                registry: image.registry().to_owned(),
                image: format!("{}/{}@{digest}", image.registry(), image.repository()),
                reason: format!("the registry served {served} instead"),
            })
        }
    }

    fn endpoint(&self, registry: &str) -> Result<Url, RegistryError> {
        match self.endpoints.get(registry) {
            Some(url) => Ok(url.clone()),
            None => {
                Url::parse(&format!("https://{registry}/")).map_err(|e| RegistryError::Challenge {
                    registry: registry.to_owned(),
                    reason: format!("invalid registry host: {e}"),
                })
            }
        }
    }

    /// `HEAD` the manifest of `reference` (a tag or a digest), authenticating on demand and
    /// refreshing a rejected token once.
    async fn manifest_digest(
        &self,
        image: &ImageRef,
        reference: &str,
    ) -> Result<Digest, RegistryError> {
        let registry = image.registry();
        let repository = image.repository();
        let separator = if reference.contains(':') { '@' } else { ':' };
        let named = format!("{registry}/{repository}{separator}{reference}");
        let base = self.endpoint(registry)?;
        let url = base
            .join(&format!("v2/{repository}/manifests/{reference}"))
            .map_err(|e| RegistryError::Challenge {
                registry: registry.to_owned(),
                reason: format!("invalid manifest URL: {e}"),
            })?;
        let key = (registry.to_owned(), repository.to_owned());

        let mut auth = self.cached_auth(&key);
        let mut refreshed = false;
        loop {
            let mut request = self
                .http
                .head(url.clone())
                .header(header::ACCEPT, MANIFEST_ACCEPT);
            if let Some(auth) = &auth {
                request = request.header(header::AUTHORIZATION, auth.clone());
            }
            let response = request
                .send()
                .await
                .map_err(|e| Self::http_error(registry, e))?;
            match response.status() {
                status if status.is_success() => {
                    return Self::digest_of(image, &response);
                }
                StatusCode::UNAUTHORIZED if !refreshed => {
                    refreshed = true;
                    self.forget_auth(&key);
                    let challenge = Challenge::from_headers(registry, response.headers())?;
                    auth = Some(self.authenticate(&key, &base, challenge).await?);
                }
                status @ (StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) => {
                    self.forget_auth(&key);
                    return Err(RegistryError::Unauthorized {
                        registry: registry.to_owned(),
                        image: named,
                        status,
                    });
                }
                StatusCode::NOT_FOUND => {
                    return Err(RegistryError::NotFound { image: named });
                }
                StatusCode::TOO_MANY_REQUESTS => {
                    return Err(Self::rate_limited(registry, &response));
                }
                status => {
                    return Err(RegistryError::Status {
                        registry: registry.to_owned(),
                        what: "the manifest request",
                        status,
                    });
                }
            }
        }
    }

    fn digest_of(image: &ImageRef, response: &Response) -> Result<Digest, RegistryError> {
        let error = |reason: String| RegistryError::Digest {
            registry: image.registry().to_owned(),
            image: image.to_string(),
            reason,
        };
        let value = response
            .headers()
            .get(DIGEST_HEADER)
            .ok_or_else(|| error("no Docker-Content-Digest header".to_owned()))?;
        value
            .to_str()
            .map_err(|_| error("the Docker-Content-Digest header is not ASCII".to_owned()))?
            .trim()
            .parse()
            .map_err(error)
    }

    /// Obtains an `Authorization` value for `key` from `challenge` and caches it.
    async fn authenticate(
        &self,
        key: &(String, String),
        base: &Url,
        challenge: Challenge,
    ) -> Result<HeaderValue, RegistryError> {
        let (registry, repository) = key;
        let credential = self.credentials.lookup(registry).await?;
        let issued_at = Instant::now();
        let cached = match challenge {
            Challenge::Basic => {
                let Some(credential) = credential else {
                    return Err(RegistryError::Unauthorized {
                        registry: registry.clone(),
                        image: format!("{registry}/{repository}"),
                        status: StatusCode::UNAUTHORIZED,
                    });
                };
                CachedAuth::new(
                    Self::basic_header(&credential),
                    issued_at,
                    BASIC_AUTH_LIFETIME,
                )
            }
            Challenge::Bearer { realm, service, .. } => {
                // Ask for exactly the scope this client needs, whatever the challenge named.
                let scope = Some(format!("repository:{repository}:pull"));
                let realm = self.realm_url(registry, base, &realm)?;
                self.fetch_token(registry, realm, service, scope, credential, issued_at)
                    .await?
            }
        };
        let header = cached.header.clone();
        self.auth_cache().insert(key.clone(), cached);
        Ok(header)
    }

    /// The realm URL, refusing to send credentials over plain HTTP unless the registry itself
    /// is plain HTTP (a local test registry).
    fn realm_url(&self, registry: &str, base: &Url, realm: &str) -> Result<Url, RegistryError> {
        let challenge_error = |reason: String| RegistryError::Challenge {
            registry: registry.to_owned(),
            reason,
        };
        let url = Url::parse(realm).map_err(|e| challenge_error(format!("invalid realm: {e}")))?;
        match url.scheme() {
            "https" => Ok(url),
            "http" if base.scheme() == "http" => Ok(url),
            scheme => Err(challenge_error(format!(
                "the token realm uses `{scheme}`, not https"
            ))),
        }
    }

    async fn fetch_token(
        &self,
        registry: &str,
        realm: Url,
        service: Option<String>,
        scope: Option<String>,
        credential: Option<RegistryCredential>,
        issued_at: Instant,
    ) -> Result<CachedAuth, RegistryError> {
        let mut url = realm;
        {
            let mut query = url.query_pairs_mut();
            if let Some(service) = &service {
                query.append_pair("service", service);
            }
            if let Some(scope) = &scope {
                query.append_pair("scope", scope);
            }
        }
        let mut request = self.http.get(url);
        if let Some(credential) = &credential {
            request = request.basic_auth(credential.username(), Some(credential.secret()));
        }
        let response = request
            .send()
            .await
            .map_err(|e| Self::http_error(registry, e))?;
        match response.status() {
            status if status.is_success() => {}
            status @ (StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) => {
                return Err(match scope {
                    Some(scope) => RegistryError::Unauthorized {
                        registry: registry.to_owned(),
                        image: scope,
                        status,
                    },
                    None => RegistryError::Rejected {
                        registry: registry.to_owned(),
                        status,
                    },
                });
            }
            StatusCode::TOO_MANY_REQUESTS => return Err(Self::rate_limited(registry, &response)),
            status => {
                return Err(RegistryError::Status {
                    registry: registry.to_owned(),
                    what: "the token request",
                    status,
                });
            }
        }
        let token_error = |reason: String| RegistryError::Token {
            registry: registry.to_owned(),
            reason,
        };
        let body: TokenResponse = response
            .json()
            .await
            .map_err(|e| token_error(format!("invalid JSON: {}", e.without_url())))?;
        let lifetime = body.lifetime();
        let token = body
            .into_token()
            .ok_or_else(|| token_error("no token in the response".to_owned()))?;
        let header = HeaderValue::try_from(format!("Bearer {token}"))
            .map_err(|_| token_error("the token is not a valid header value".to_owned()))?;
        Ok(CachedAuth::new(header, issued_at, lifetime))
    }

    fn basic_header(credential: &RegistryCredential) -> HeaderValue {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(format!(
            "{}:{}",
            credential.username(),
            credential.secret()
        ));
        HeaderValue::try_from(format!("Basic {encoded}")).expect("base64 is a valid header value")
    }

    fn cached_auth(&self, key: &(String, String)) -> Option<HeaderValue> {
        let now = Instant::now();
        let mut cache = self.auth_cache();
        match cache.get(key) {
            Some(cached) if cached.is_fresh(now) => Some(cached.header.clone()),
            Some(_) => {
                cache.remove(key);
                None
            }
            None => None,
        }
    }

    fn forget_auth(&self, key: &(String, String)) {
        self.auth_cache().remove(key);
    }

    /// The token cache. Every access is a single map operation, so a panic elsewhere cannot
    /// leave it inconsistent and a poisoned lock is simply taken over.
    fn auth_cache(&self) -> MutexGuard<'_, HashMap<(String, String), CachedAuth>> {
        self.auth.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn rate_limited(registry: &str, response: &Response) -> RegistryError {
        let retry_after = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        RegistryError::RateLimited {
            registry: registry.to_owned(),
            retry_after,
        }
    }

    fn http_error(registry: &str, source: reqwest::Error) -> RegistryError {
        RegistryError::Http {
            registry: registry.to_owned(),
            source,
        }
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
