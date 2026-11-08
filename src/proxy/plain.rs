//! The `:80` service (ADR-0015, ADR-0032, architecture §7.2): answers ACME HTTP-01 challenges
//! and sends everything else for a known host to HTTPS. It never proxies.

use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use bytes::Bytes;
use http::{StatusCode, header};
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::proxy::{ProxyHttp, Session};
use pingora::upstreams::peer::HttpPeer;
use pingora::{Error, ErrorType, Result};

use super::router::{ErrorPage, Forwarding};
use crate::acme::ChallengeMap;
use crate::routing::RouteTable;

/// Where HTTP-01 challenges are fetched from (RFC 8555 §8.3).
const CHALLENGE_PREFIX: &str = "/.well-known/acme-challenge/";

/// hoster's [`ProxyHttp`] for plain HTTP: every request is answered in `request_filter`.
pub struct ChallengeOrRedirect {
    routes: Arc<ArcSwap<RouteTable>>,
    challenges: ChallengeMap,
}

impl ChallengeOrRedirect {
    /// Redirects the hosts in `routes` and answers the tokens in `challenges`, both as they are
    /// at the time of each request.
    pub fn new(routes: Arc<ArcSwap<RouteTable>>, challenges: ChallengeMap) -> Self {
        Self { routes, challenges }
    }
}

#[async_trait]
impl ProxyHttp for ChallengeOrRedirect {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool> {
        let answer = PlainAnswer::new(session.req_header(), &self.routes.load(), &self.challenges);
        answer.send(session).await;
        Ok(true)
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        Error::e_explain(ErrorType::InternalError, "the :80 service never proxies")
    }
}

/// What the `:80` service answers to one request.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PlainAnswer {
    /// `200 text/plain` with the key authorization of a pending challenge.
    Challenge(Arc<str>),
    /// `308` to this `https://` URL.
    Redirect(String),
    /// `404`: an unknown challenge token, or a host no route serves (ADR-0032: :80 is not an
    /// open redirector).
    NotFound,
}

impl PlainAnswer {
    /// The answer to `req`. Challenges are answered whatever the host (ADR-0032); redirects
    /// only for hosts in `routes`.
    fn new(req: &RequestHeader, routes: &RouteTable, challenges: &ChallengeMap) -> Self {
        let path = req.uri.path();
        if let Some(token) = path.strip_prefix(CHALLENGE_PREFIX) {
            return match challenges.get(token) {
                Some(key_authorization) if !token.contains('/') => {
                    PlainAnswer::Challenge(key_authorization)
                }
                _ => PlainAnswer::NotFound,
            };
        }
        let Some(host) = Forwarding::request_host(req).and_then(|host| routes.known_host(host))
        else {
            return PlainAnswer::NotFound;
        };
        // `OPTIONS *` and the like have no path to keep.
        let target = req
            .uri
            .path_and_query()
            .map(|pq| pq.as_str())
            .filter(|target| target.starts_with('/'))
            .unwrap_or("/");
        PlainAnswer::Redirect(format!("https://{host}{target}"))
    }

    fn response(&self) -> Result<(ResponseHeader, Bytes)> {
        let (status, body, location) = match self {
            PlainAnswer::Challenge(key_authorization) => (
                StatusCode::OK,
                Bytes::copy_from_slice(key_authorization.as_bytes()),
                None,
            ),
            PlainAnswer::Redirect(url) => (StatusCode::PERMANENT_REDIRECT, Bytes::new(), Some(url)),
            PlainAnswer::NotFound => {
                let page = ErrorPage::from(StatusCode::NOT_FOUND);
                return Ok((page.header(page.body().len())?, page.body()));
            }
        };
        let mut resp = ResponseHeader::build(status, Some(4))?;
        resp.insert_header(header::CONTENT_TYPE, "text/plain")?;
        resp.insert_header(header::CONTENT_LENGTH, body.len())?;
        resp.insert_header(header::CACHE_CONTROL, "no-store")?;
        if let Some(url) = location {
            resp.insert_header(header::LOCATION, url.as_str())?;
        }
        Ok((resp, body))
    }

    async fn send(&self, session: &mut Session) {
        let result = match self.response() {
            Ok((resp, body)) => match session.write_response_header(Box::new(resp), false).await {
                Ok(()) => session.write_response_body(Some(body), true).await,
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            tracing::debug!(answer = ?self, error = %e, "cannot answer on :80");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::{DesiredState, Reserved, UnixUpstream};

    fn routes() -> RouteTable {
        let reserved = Reserved::default().with_unix_host(
            "app.example.com".parse().unwrap(),
            "test",
            Arc::new(UnixUpstream::from(std::path::PathBuf::from("/run/t.sock"))),
        );
        RouteTable::build(&DesiredState::default(), &reserved).unwrap()
    }

    fn answer(target: &str, host: Option<&str>, challenges: &ChallengeMap) -> PlainAnswer {
        let mut req = RequestHeader::build("GET", target.as_bytes(), None).unwrap();
        if let Some(host) = host {
            req.insert_header("host", host).unwrap();
        }
        PlainAnswer::new(&req, &routes(), challenges)
    }

    #[test]
    fn known_hosts_are_redirected_with_path_and_query() {
        let none = ChallengeMap::default();
        for (host, target, expected) in [
            (
                "app.example.com",
                "/a/b?c=d&e",
                "https://app.example.com/a/b?c=d&e",
            ),
            ("APP.example.com.:80", "/", "https://app.example.com/"),
            ("app.example.com", "*", "https://app.example.com/"),
        ] {
            assert_eq!(
                answer(target, Some(host), &none),
                PlainAnswer::Redirect(expected.to_owned()),
                "{host} {target}"
            );
        }
        let absolute = answer("http://app.example.com/x?y", Some("app.example.com"), &none);
        assert_eq!(
            absolute,
            PlainAnswer::Redirect("https://app.example.com/x?y".to_owned())
        );
    }

    #[test]
    fn unknown_hosts_get_404_not_a_redirect() {
        let none = ChallengeMap::default();
        assert_eq!(
            answer("/", Some("evil.example.com"), &none),
            PlainAnswer::NotFound
        );
        assert_eq!(answer("/", None, &none), PlainAnswer::NotFound);
    }

    #[test]
    fn challenges_are_answered_for_any_host() {
        let challenges = ChallengeMap::default();
        challenges.insert("tok_-1", "tok_-1.thumb");
        for host in [Some("app.example.com"), Some("new.example.com"), None] {
            assert_eq!(
                answer("/.well-known/acme-challenge/tok_-1", host, &challenges),
                PlainAnswer::Challenge("tok_-1.thumb".into()),
                "{host:?}"
            );
        }
        for target in [
            "/.well-known/acme-challenge/other",
            "/.well-known/acme-challenge/",
            "/.well-known/acme-challenge/tok_-1/x",
        ] {
            assert_eq!(
                answer(target, Some("app.example.com"), &challenges),
                PlainAnswer::NotFound,
                "{target}"
            );
        }
    }

    #[test]
    fn responses_carry_status_type_and_location() {
        let (resp, body) = PlainAnswer::Challenge("k.a".into()).response().unwrap();
        assert_eq!(resp.status, StatusCode::OK);
        assert_eq!(resp.headers["content-type"], "text/plain");
        assert_eq!(body, Bytes::from("k.a"));

        let (resp, body) = PlainAnswer::Redirect("https://a.example/x".into())
            .response()
            .unwrap();
        assert_eq!(resp.status, StatusCode::PERMANENT_REDIRECT);
        assert_eq!(resp.headers["location"], "https://a.example/x");
        assert!(body.is_empty());

        let (resp, body) = PlainAnswer::NotFound.response().unwrap();
        assert_eq!(resp.status, StatusCode::NOT_FOUND);
        assert_eq!(body, Bytes::from("404 Not Found\n"));
    }
}
