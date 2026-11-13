//! The `:80` service (ADR-0015, ADR-0032, architecture §7.2): answers ACME HTTP-01 challenges,
//! hands requests for the OTLP receiver from local peers to its socket (ADR-0069 §3), and sends
//! everything else for a known host to HTTPS. The receiver is the only thing it proxies.

use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use bytes::Bytes;
use http::{StatusCode, header};
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::proxy::{FailToProxy, ProxyHttp, Session};
use pingora::upstreams::peer::HttpPeer;
use pingora::{Error, OkOrErr, Result};

use super::DownstreamLimits;
use super::receiver::ReceiverRoute;
use super::router::{AccessRecord, ErrorPage, Forwarding, RoutingProxy};
use crate::acme::ChallengeMap;
use crate::config::Timeouts;
use crate::routing::{RouteEntry, RouteTable};

/// Where HTTP-01 challenges are fetched from (RFC 8555 §8.3).
const CHALLENGE_PREFIX: &str = "/.well-known/acme-challenge/";

/// hoster's [`ProxyHttp`] for plain HTTP: every request but the receiver's is answered in
/// `request_filter`.
pub struct ChallengeOrRedirect {
    routes: Arc<ArcSwap<RouteTable>>,
    challenges: ChallengeMap,
    /// The OTLP receiver's route, for local peers only.
    receiver: ReceiverRoute,
    /// How long a client may stall reading an answer, and the receiver's upstream timeouts.
    timeouts: Timeouts,
    limits: DownstreamLimits,
}

impl ChallengeOrRedirect {
    /// Redirects the hosts in `routes` and answers the tokens in `challenges`, both as they are
    /// at the time of each request, and hands `receiver`'s requests to it, to clients held to
    /// `limits` and `timeouts`.
    pub fn new(
        routes: Arc<ArcSwap<RouteTable>>,
        challenges: ChallengeMap,
        receiver: ReceiverRoute,
        timeouts: Timeouts,
        limits: DownstreamLimits,
    ) -> Self {
        Self {
            routes,
            challenges,
            receiver,
            timeouts,
            limits,
        }
    }
}

/// Per-request state of the `:80` service.
pub struct PlainCtx {
    /// When the request header had been read, for the access log's duration.
    started: Instant,
    /// The request was an ACME challenge that got its answer.
    challenge: bool,
    /// The request host as sent, for the receiver's forwarding headers.
    host: String,
    /// The receiver's route, for a request handed to it.
    receiver: Option<RouteEntry>,
    /// How long after `started` the receiver's response header was ready.
    header_after: Option<Duration>,
}

#[async_trait]
impl ProxyHttp for ChallengeOrRedirect {
    type CTX = PlainCtx;

    fn new_ctx(&self) -> Self::CTX {
        PlainCtx {
            started: Instant::now(),
            challenge: false,
            host: String::new(),
            receiver: None,
            header_after: None,
        }
    }

    /// The receiver's route is matched first, before challenges and the redirect: its host is
    /// reserved, so no route or certificate ever names it, and a request for it from a peer
    /// that is not local goes on to the unknown host's 404.
    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        self.limits.admit(session, self.timeouts);
        let host = Forwarding::request_host(session.req_header()).unwrap_or_default();
        let local = session
            .server_addr()
            .and_then(|addr| addr.as_inet())
            .map(|addr| addr.ip());
        if let Some(route) = self
            .receiver
            .route(host, Forwarding::client_ip(session), local)
        {
            ctx.host = host.to_owned();
            ctx.receiver = Some(route.clone());
            return Ok(false);
        }
        let answer = PlainAnswer::new(session.req_header(), &self.routes.load(), &self.challenges);
        ctx.challenge = matches!(answer, PlainAnswer::Challenge(_));
        answer.send(session).await;
        Ok(true)
    }

    /// The receiver's Unix socket; nothing else is proxied.
    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let route = ctx.receiver.as_ref().or_err(
            pingora::ErrorType::InternalError,
            "the :80 service proxies only to the OTLP receiver",
        )?;
        RoutingProxy::peer(route, self.timeouts)
    }

    /// The forwarding headers of ADR-0010, with an origin-form target, as for any route.
    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let route = ctx.receiver.as_ref().or_err(
            pingora::ErrorType::InternalError,
            "request has no receiver route",
        )?;
        Forwarding::new(session, &ctx.host).apply(route, upstream_request)
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        _upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        ctx.header_after
            .get_or_insert_with(|| ctx.started.elapsed());
        Ok(())
    }

    /// An export is a `POST` with a body: never retried, whether the connection to the socket
    /// failed fresh or pooled.
    fn fail_to_connect(
        &self,
        _session: &mut Session,
        _peer: &HttpPeer,
        _ctx: &mut Self::CTX,
        mut e: Box<Error>,
    ) -> Box<Error> {
        e.set_retry(false);
        e
    }

    fn error_while_proxy(
        &self,
        peer: &HttpPeer,
        _session: &mut Session,
        e: Box<Error>,
        _ctx: &mut Self::CTX,
        _client_reused: bool,
    ) -> Box<Error> {
        let mut e = e.more_context(format!("Peer: {peer}"));
        e.set_retry(false);
        e
    }

    /// A 502 or 504 page when the receiver is not there or too slow, as for any route.
    async fn fail_to_proxy(
        &self,
        session: &mut Session,
        e: &Error,
        _ctx: &mut Self::CTX,
    ) -> FailToProxy {
        let error_code = match ErrorPage::for_error(e) {
            Some(page) => {
                page.send(session).await;
                page.status().as_u16()
            }
            None => 0,
        };
        FailToProxy {
            error_code,
            can_reuse_downstream: false,
        }
    }

    /// One access log line per request, like the `:443` service's, and nothing else: `:80`
    /// counts no traffic and offers no request rows, so receiver requests have neither
    /// (ADR-0069 Consequences). A request without a route is a `debug` one on the unrouted
    /// target, which keeps redirects and probes out of the log. An answered ACME challenge is
    /// logged at the access target, as part of a certificate being issued, and so is a request
    /// handed to the receiver, under its reserved route.
    async fn logging(&self, session: &mut Session, e: Option<&Error>, ctx: &mut Self::CTX) {
        let host = Forwarding::request_host(session.req_header()).unwrap_or_default();
        let record = AccessRecord::new(session, host, ctx.receiver.as_ref(), ctx.started, e)
            .header_after(ctx.header_after);
        if ctx.challenge {
            record.answered().emit();
        } else {
            record.emit();
        }
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
