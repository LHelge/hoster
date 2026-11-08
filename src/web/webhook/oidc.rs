//! GitHub Actions OIDC token verification (ADR-0047).
//!
//! A token is a compact JWS: `base64url(header).base64url(claims).base64url(signature)`. Only
//! RS256 is accepted (the allow-list is that one algorithm; `none` and HS* never get near a
//! key). The key comes from GitHub's JWKS by `kid`. The key set is cached, refetched when a
//! token names a `kid` it lacks or the set is an hour old, and fetched at most once a minute
//! whatever happens. A failed fetch never lets a token through: with no usable key the token is
//! refused, and with no key set at all the verifier reports itself unavailable.
//!
//! The signature is checked with ring directly (ADR-0037). Then the claims: `iss`, `aud`, and
//! `exp`/`nbf`/`iat` with 60 s of skew. Single use (`jti`) is the caller's job, as it needs
//! the state store.

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Url;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::stack::DeployCaller;
use crate::store::{Provenance, Timestamp, WebhookRejection};

/// Why a token was refused. Logged for the admin; the caller only learns "invalid token".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    #[error("the token is malformed: {0}")]
    Malformed(&'static str),
    #[error("the token's algorithm {0:?} is not RS256")]
    Algorithm(String),
    #[error("the token's key {0:?} is not in GitHub's key set")]
    UnknownKey(String),
    #[error("the token's signature does not verify")]
    Signature,
    #[error("the token's claims are unusable: {0}")]
    Claims(String),
    #[error("the token's issuer {0:?} is not GitHub Actions")]
    Issuer(String),
    #[error("the token's audience {0:?} is not this hoster's")]
    Audience(Vec<String>),
    #[error("the token expired")]
    Expired,
    #[error("the token is not valid yet")]
    NotYetValid,
    #[error("the token was issued in the future")]
    IssuedInFuture,
    /// GitHub's key set could not be fetched, so nothing can be verified.
    #[error("GitHub's key set is unavailable: {0}")]
    Unavailable(String),
}

/// The verified claims of a GitHub Actions OIDC token that hoster uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claims {
    pub jti: String,
    /// Until when the `jti` must be remembered: as long as the token could still be accepted,
    /// its `exp` plus the skew.
    pub usable_until: Timestamp,
    pub repository: String,
    pub repository_id: u64,
    pub git_ref: String,
    pub sha: String,
    pub run_id: String,
    pub actor: String,
    pub environment: Option<String>,
    pub workflow_ref: Option<String>,
    pub job_workflow_ref: Option<String>,
}

impl Claims {
    /// Where a deployment this token triggers came from; `tag` is the body's.
    pub fn provenance(&self, tag: Option<String>) -> Provenance {
        Provenance {
            repository: self.repository.clone(),
            sha: self.sha.clone(),
            git_ref: self.git_ref.clone(),
            run_id: self.run_id.clone(),
            actor: self.actor.clone(),
            tag,
        }
    }

    /// The refused attempt on `stack` this token made, with its claims.
    pub fn rejection(&self, stack: &str, reason: impl Into<String>) -> WebhookRejection {
        WebhookRejection {
            stack: stack.to_owned(),
            reason: reason.into(),
            repository: self.repository.clone(),
            repository_id: self.repository_id.to_string(),
            git_ref: self.git_ref.clone(),
            environment: self.environment.clone(),
            workflow_ref: self.workflow_ref.clone(),
            job_workflow_ref: self.job_workflow_ref.clone(),
            actor: self.actor.clone(),
            run_id: self.run_id.clone(),
            at: Timestamp::now(),
        }
    }
}

impl From<&Claims> for DeployCaller {
    fn from(claims: &Claims) -> Self {
        DeployCaller {
            repository_id: claims.repository_id,
            git_ref: claims.git_ref.clone(),
            environment: claims.environment.clone(),
            job_workflow_ref: claims.job_workflow_ref.clone(),
        }
    }
}

/// The claims as GitHub writes them. Ids come as strings (`"repository_id": "74"`); numbers
/// are accepted too.
#[derive(Deserialize)]
struct RawClaims {
    jti: String,
    iss: String,
    aud: Audience,
    exp: u64,
    nbf: u64,
    iat: u64,
    repository: String,
    repository_id: StringOrNumber,
    #[serde(rename = "ref", default)]
    git_ref: String,
    sha: String,
    run_id: StringOrNumber,
    actor: String,
    environment: Option<String>,
    workflow_ref: Option<String>,
    job_workflow_ref: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    fn contains(&self, expected: &str) -> bool {
        match self {
            Audience::One(aud) => aud == expected,
            Audience::Many(auds) => auds.iter().any(|aud| aud == expected),
        }
    }

    fn into_vec(self) -> Vec<String> {
        match self {
            Audience::One(aud) => vec![aud],
            Audience::Many(auds) => auds,
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StringOrNumber {
    String(String),
    Number(u64),
}

impl fmt::Display for StringOrNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StringOrNumber::String(s) => f.write_str(s),
            StringOrNumber::Number(n) => n.fmt(f),
        }
    }
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    kid: Option<String>,
    crit: Option<serde::de::IgnoredAny>,
}

/// One RSA public key of the JWKS.
#[derive(Debug, Clone)]
struct RsaKey {
    n: Vec<u8>,
    e: Vec<u8>,
}

/// The JWKS document.
#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    kty: String,
    kid: Option<String>,
    alg: Option<String>,
    #[serde(rename = "use")]
    usage: Option<String>,
    n: Option<String>,
    e: Option<String>,
}

impl Jwks {
    /// The RS256 signing keys by `kid`; other keys are skipped.
    fn rsa_keys(self) -> HashMap<String, RsaKey> {
        self.keys
            .into_iter()
            .filter(|k| k.kty == "RSA")
            .filter(|k| k.alg.as_deref().is_none_or(|alg| alg == "RS256"))
            .filter(|k| k.usage.as_deref().is_none_or(|usage| usage == "sig"))
            .filter_map(|k| {
                let n = URL_SAFE_NO_PAD.decode(k.n?).ok()?;
                let e = URL_SAFE_NO_PAD.decode(k.e?).ok()?;
                Some((k.kid?, RsaKey { n, e }))
            })
            .collect()
    }
}

/// The cached key set.
#[derive(Default)]
struct KeyCache {
    keys: HashMap<String, RsaKey>,
    /// When the keys were last fetched successfully.
    fetched: Option<Instant>,
    /// When a fetch was last attempted, successful or not: the rate limit.
    attempted: Option<Instant>,
}

/// Verifies GitHub Actions OIDC tokens for one audience.
pub struct OidcVerifier {
    audience: String,
    jwks_url: Url,
    http: reqwest::Client,
    /// The least time between two key set fetches.
    refresh_interval: Duration,
    cache: Mutex<KeyCache>,
}

impl fmt::Debug for OidcVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OidcVerifier")
            .field("audience", &self.audience)
            .field("jwks_url", &self.jwks_url.as_str())
            .finish_non_exhaustive()
    }
}

impl OidcVerifier {
    /// The issuer of GitHub Actions OIDC tokens.
    pub const ISSUER: &str = "https://token.actions.githubusercontent.com";
    /// Where GitHub publishes the signing keys.
    pub const GITHUB_JWKS: &str = "https://token.actions.githubusercontent.com/.well-known/jwks";
    /// Clock skew allowed on `exp`, `nbf` and `iat`.
    pub const SKEW: Duration = Duration::from_secs(60);
    /// A key set older than this is refetched (keys GitHub withdrew stop working).
    const MAX_KEY_AGE: Duration = Duration::from_secs(3600);
    /// Tokens are a few KiB; anything much larger is not one.
    const MAX_TOKEN_LEN: usize = 16 * 1024;
    const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

    /// A verifier accepting tokens for `audience`, with keys from `jwks_url` (GitHub's,
    /// [`Self::GITHUB_JWKS`], in production). TLS is rustls with ring (ADR-0037).
    pub fn new(audience: impl Into<String>, jwks_url: Url) -> anyhow::Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .user_agent(concat!("hoster/", env!("CARGO_PKG_VERSION")))
            .timeout(Self::FETCH_TIMEOUT)
            .build()?;
        Ok(Self {
            audience: audience.into(),
            jwks_url,
            http,
            refresh_interval: Duration::from_secs(60),
            cache: Mutex::default(),
        })
    }

    /// A verifier for GitHub's production key set.
    pub fn github(audience: impl Into<String>) -> anyhow::Result<Self> {
        Self::new(audience, Url::parse(Self::GITHUB_JWKS)?)
    }

    /// Changes the least time between key set fetches (tests).
    #[cfg(test)]
    pub fn with_refresh_interval(mut self, interval: Duration) -> Self {
        self.refresh_interval = interval;
        self
    }

    /// Verifies `token` now: signature, issuer, audience and lifetime.
    pub async fn verify(&self, token: &str) -> Result<Claims, TokenError> {
        self.verify_at(token, SystemTime::now()).await
    }

    async fn verify_at(&self, token: &str, now: SystemTime) -> Result<Claims, TokenError> {
        if token.len() > Self::MAX_TOKEN_LEN {
            return Err(TokenError::Malformed("too long"));
        }
        let malformed = TokenError::Malformed("not three dot-separated parts");
        let (signed, signature) = token.rsplit_once('.').ok_or(malformed.clone())?;
        let (header, claims) = signed.split_once('.').ok_or(malformed.clone())?;
        if claims.contains('.') {
            return Err(malformed);
        }
        let decode = |part: &str, what: &'static str| {
            URL_SAFE_NO_PAD
                .decode(part)
                .map_err(|_| TokenError::Malformed(what))
        };
        let header: Header = serde_json::from_slice(&decode(header, "header is not base64url")?)
            .map_err(|_| TokenError::Malformed("header is not a JWS header"))?;
        if header.alg != "RS256" {
            return Err(TokenError::Algorithm(header.alg));
        }
        if header.crit.is_some() {
            return Err(TokenError::Malformed("critical header extensions"));
        }
        let kid = header.kid.ok_or(TokenError::Malformed("no key id"))?;
        let signature = decode(signature, "signature is not base64url")?;
        let key = self.key(&kid).await?;
        RsaPublicKeyComponents {
            n: &key.n,
            e: &key.e,
        }
        .verify(&RSA_PKCS1_2048_8192_SHA256, signed.as_bytes(), &signature)
        .map_err(|_| TokenError::Signature)?;

        let raw: RawClaims = serde_json::from_slice(&decode(claims, "claims are not base64url")?)
            .map_err(|err| TokenError::Claims(err.to_string()))?;
        self.check(raw, now)
    }

    /// The claim checks after the signature verified.
    fn check(&self, raw: RawClaims, now: SystemTime) -> Result<Claims, TokenError> {
        if raw.iss != Self::ISSUER {
            return Err(TokenError::Issuer(raw.iss));
        }
        if !raw.aud.contains(&self.audience) {
            return Err(TokenError::Audience(raw.aud.into_vec()));
        }
        let now = now
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let skew = Self::SKEW.as_secs();
        if now > raw.exp.saturating_add(skew) {
            return Err(TokenError::Expired);
        }
        if raw.nbf > now.saturating_add(skew) {
            return Err(TokenError::NotYetValid);
        }
        if raw.iat > now.saturating_add(skew) {
            return Err(TokenError::IssuedInFuture);
        }
        if raw.jti.is_empty() {
            return Err(TokenError::Claims("empty jti".into()));
        }
        // `SystemTime + Duration` panics on overflow; GitHub never signs such an `exp`.
        let usable_until = UNIX_EPOCH
            .checked_add(Duration::from_secs(raw.exp.saturating_add(skew)))
            .map(Timestamp::from)
            .ok_or_else(|| TokenError::Claims("exp is out of range".into()))?;
        let repository_id = raw
            .repository_id
            .to_string()
            .parse()
            .map_err(|_| TokenError::Claims("repository_id is not a number".into()))?;
        Ok(Claims {
            jti: raw.jti,
            usable_until,
            repository: raw.repository,
            repository_id,
            git_ref: raw.git_ref,
            sha: raw.sha,
            run_id: raw.run_id.to_string(),
            actor: raw.actor,
            environment: raw.environment,
            workflow_ref: raw.workflow_ref,
            job_workflow_ref: raw.job_workflow_ref,
        })
    }

    /// The key `kid`, refetching the key set when it lacks the key or is old, at most once per
    /// `refresh_interval`.
    async fn key(&self, kid: &str) -> Result<RsaKey, TokenError> {
        let mut cache = self.cache.lock().await;
        let stale = cache
            .fetched
            .is_none_or(|at| at.elapsed() >= Self::MAX_KEY_AGE);
        let may_fetch = cache
            .attempted
            .is_none_or(|at| at.elapsed() >= self.refresh_interval);
        if (stale || !cache.keys.contains_key(kid)) && may_fetch {
            cache.attempted = Some(Instant::now());
            match self.fetch().await {
                Ok(keys) => {
                    tracing::info!(keys = keys.len(), "fetched GitHub's OIDC key set");
                    cache.keys = keys;
                    cache.fetched = Some(Instant::now());
                }
                Err(err) => {
                    // Keep the cached keys: they cover a short outage of GitHub's endpoint.
                    tracing::warn!(error = %err, "cannot fetch GitHub's OIDC key set");
                    if cache.fetched.is_none() {
                        return Err(TokenError::Unavailable(err));
                    }
                }
            }
        }
        match cache.keys.get(kid) {
            Some(key) => Ok(key.clone()),
            None if cache.fetched.is_none() => {
                Err(TokenError::Unavailable("no key set fetched yet".to_owned()))
            }
            None => Err(TokenError::UnknownKey(kid.to_owned())),
        }
    }

    async fn fetch(&self) -> Result<HashMap<String, RsaKey>, String> {
        let response = self
            .http
            .get(self.jwks_url.clone())
            .send()
            .await
            .map_err(|err| err.to_string())?;
        if !response.status().is_success() {
            return Err(format!("{} answered {}", self.jwks_url, response.status()));
        }
        let jwks: Jwks = response.json().await.map_err(|err| err.to_string())?;
        let keys = jwks.rsa_keys();
        if keys.is_empty() {
            return Err("the key set has no RS256 keys".to_owned());
        }
        Ok(keys)
    }
}
