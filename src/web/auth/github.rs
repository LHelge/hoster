//! The GitHub OAuth client (ADR-0021, ADR-0031): authorization code + `state` + PKCE, no
//! scopes, then one `GET /user` to learn who logged in.
//!
//! HTTP goes through reqwest on rustls with the ring provider (ADR-0037) and the platform's
//! roots. Tokens and codes are never logged or put into error messages.

use std::sync::Arc;
use std::time::Duration;

use oauth2::basic::{BasicClient, BasicErrorResponse};
use oauth2::url::Url;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet, EndpointSet,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, RequestTokenError, TokenResponse as _,
    TokenUrl,
};
use serde::Deserialize;

/// Where GitHub's OAuth and REST endpoints live; overridable for tests.
#[derive(Debug, Clone)]
pub struct GithubEndpoints {
    pub authorize: Url,
    pub token: Url,
    /// The REST API base; `GET <api>/user` names the logged-in account.
    pub api: Url,
}

impl Default for GithubEndpoints {
    fn default() -> Self {
        let url = |s: &str| Url::parse(s).expect("valid built-in URL");
        GithubEndpoints {
            authorize: url("https://github.com/login/oauth/authorize"),
            token: url("https://github.com/login/oauth/access_token"),
            api: url("https://api.github.com/"),
        }
    }
}

/// The GitHub OAuth App's credentials.
pub struct GithubApp {
    pub client_id: String,
    pub client_secret: String,
}

/// The account a login authenticated.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GithubUser {
    pub login: String,
    pub id: i64,
}

/// A started authorization: send the browser to `url`; keep `state` and `pkce_verifier` for
/// the callback.
pub struct Authorization {
    pub url: Url,
    pub state: String,
    pub pkce_verifier: String,
}

/// Why a login could not be completed with GitHub.
#[derive(Debug, thiserror::Error)]
pub enum GithubError {
    /// GitHub could not be reached or answered with something unexpected.
    #[error("GitHub is unavailable: {0}")]
    Unavailable(String),
    /// GitHub refused the authorization code (expired, already used, wrong client).
    #[error("GitHub refused the login: {0}")]
    Rejected(String),
}

type OAuthClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

/// The HTTP error of the client oauth2 drives.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
    #[error(transparent)]
    Http(#[from] http::Error),
}

/// The GitHub OAuth client. Cheap to share behind an `Arc`.
pub struct GithubOAuth {
    oauth: OAuthClient,
    http: reqwest::Client,
    api: Url,
}

impl GithubOAuth {
    /// Time allowed for each request to GitHub.
    const TIMEOUT: Duration = Duration::from_secs(15);
    const USER_AGENT: &str = concat!("hoster/", env!("CARGO_PKG_VERSION"));

    /// A client for `app`, redirecting back to `redirect` (`<origin>/auth/github/callback`).
    pub fn new(app: GithubApp, redirect: &str, endpoints: GithubEndpoints) -> anyhow::Result<Self> {
        let oauth = BasicClient::new(ClientId::new(app.client_id))
            .set_client_secret(ClientSecret::new(app.client_secret))
            .set_auth_uri(AuthUrl::from_url(endpoints.authorize))
            .set_token_uri(TokenUrl::from_url(endpoints.token))
            .set_redirect_uri(RedirectUrl::new(redirect.to_owned())?);
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?;
        let tls = rustls_platform_verifier::BuilderVerifierExt::with_platform_verifier(tls)?
            .with_no_client_auth();
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            // The token endpoint must not be followed elsewhere (RFC 6749 §10.6 spirit).
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Self::TIMEOUT)
            .user_agent(Self::USER_AGENT)
            .build()?;
        Ok(GithubOAuth {
            oauth,
            http,
            api: endpoints.api,
        })
    }

    /// Starts a login: a fresh `state`, a PKCE S256 challenge, no scopes.
    pub fn authorize(&self) -> Authorization {
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, state) = self
            .oauth
            .authorize_url(CsrfToken::new_random)
            .set_pkce_challenge(challenge)
            .url();
        Authorization {
            url,
            state: state.secret().clone(),
            pkce_verifier: verifier.secret().clone(),
        }
    }

    /// Exchanges the callback's `code` for a token and asks GitHub whose it is. The token is
    /// dropped afterwards; hoster needs nothing else from GitHub.
    pub async fn identify(
        &self,
        code: &str,
        pkce_verifier: &str,
    ) -> Result<GithubUser, GithubError> {
        // Each request future owns its client handle: a future borrowing `self` here is not
        // provably `Send` through oauth2's higher-ranked client bound, and axum needs that.
        let http = self.http.clone();
        let send = move |request| Self::send(http.clone(), request);
        let token = self
            .oauth
            .exchange_code(AuthorizationCode::new(code.to_owned()))
            .set_pkce_verifier(PkceCodeVerifier::new(pkce_verifier.to_owned()))
            .request_async(&send)
            .await
            .map_err(Self::token_error)?;

        let url = self
            .api
            .join("user")
            .map_err(|e| GithubError::Unavailable(e.to_string()))?;
        let response = self
            .http
            .get(url)
            .bearer_auth(token.access_token().secret())
            .header(http::header::ACCEPT, "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| GithubError::Unavailable(Self::describe(&e)))?;
        let status = response.status();
        if !status.is_success() {
            return Err(GithubError::Unavailable(format!(
                "GET /user answered {status}"
            )));
        }
        response.json::<GithubUser>().await.map_err(|e| {
            GithubError::Unavailable(format!("unexpected /user response: {}", Self::describe(&e)))
        })
    }

    /// Sends one oauth2 request through reqwest.
    async fn send(
        http: reqwest::Client,
        request: oauth2::HttpRequest,
    ) -> Result<oauth2::HttpResponse, HttpError> {
        let request = reqwest::Request::try_from(request)?;
        let response = http.execute(request).await?;
        let mut builder = http::Response::builder().status(response.status());
        for (name, value) in response.headers() {
            builder = builder.header(name, value);
        }
        let body = response.bytes().await?;
        Ok(builder.body(body.to_vec())?)
    }

    /// Classifies a failed token exchange. GitHub reports a bad code with `200 OK` and an error
    /// body, which oauth2 sees as a parse failure; that body is still a refusal.
    fn token_error(err: RequestTokenError<HttpError, BasicErrorResponse>) -> GithubError {
        match err {
            RequestTokenError::ServerResponse(response) => {
                GithubError::Rejected(response.error().to_string())
            }
            RequestTokenError::Parse(_, body) => {
                match serde_json::from_slice::<BasicErrorResponse>(&body) {
                    Ok(response) => GithubError::Rejected(response.error().to_string()),
                    Err(_) => GithubError::Unavailable(
                        "unexpected response from the token endpoint".to_owned(),
                    ),
                }
            }
            RequestTokenError::Request(HttpError::Reqwest(e)) => {
                GithubError::Unavailable(Self::describe(&e))
            }
            RequestTokenError::Request(e) => GithubError::Unavailable(e.to_string()),
            RequestTokenError::Other(reason) => GithubError::Unavailable(reason),
        }
    }

    /// A reqwest error without its URL (which, for the callback, could carry a code).
    fn describe(err: &reqwest::Error) -> String {
        let kind = if err.is_timeout() {
            "timed out"
        } else if err.is_connect() {
            "cannot connect"
        } else if err.is_decode() {
            "cannot decode the response"
        } else {
            "request failed"
        };
        match err.status() {
            Some(status) => format!("{kind} ({status})"),
            None => kind.to_owned(),
        }
    }
}
