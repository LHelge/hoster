//! The routing proxy: hoster's [`ProxyHttp`] implementation (architecture §6).
//!
//! Every request loads the current [`RouteTable`] snapshot once and keeps the matched
//! [`RouteEntry`] in its context, so a table published mid-request never changes where that
//! request goes. Requests for an unknown host or prefix, or for a service in its stop-first gap,
//! get a built-in [`ErrorPage`] (ADR-0032), and so does a request whose host is not the TLS
//! connection's SNI name (421).
//!
//! The rewriting of ADR-0010 lives on [`Forwarding`], which is pure and unit-tested; bodies are
//! streamed because no body filter is installed (ADR-0011), and each route's timeouts end up in
//! the upstream [`PeerOptions`].

use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use bytes::Bytes;
use http::{StatusCode, header};
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::proxy::{FailToProxy, ProxyHttp, Session};
use pingora::upstreams::peer::{HttpPeer, PeerOptions};
use pingora::{Error, ErrorSource, ErrorType, OkOrErr, OrErr, Result};

use super::resolver::Sni;
use crate::config::Timeouts;
use crate::routing::{InflightGuard, RouteEntry, RouteTable, Target};
use crate::stack::RouteTimeouts;

const X_FORWARDED_FOR: &str = "x-forwarded-for";
const X_FORWARDED_HOST: &str = "x-forwarded-host";
const X_FORWARDED_PROTO: &str = "x-forwarded-proto";
const X_FORWARDED_PREFIX: &str = "x-forwarded-prefix";

/// Routes requests by host and longest prefix to the upstreams in the published route table.
pub struct RoutingProxy {
    routes: Arc<ArcSwap<RouteTable>>,
    /// Upstream timeouts for routes that set none of their own.
    defaults: Timeouts,
}

impl RoutingProxy {
    /// A proxy reading `routes`, which the orchestrator replaces as deploys happen.
    pub fn new(routes: Arc<ArcSwap<RouteTable>>, defaults: Timeouts) -> Self {
        Self { routes, defaults }
    }
}

/// Per-request state: what the request matched in the snapshot it loaded.
#[derive(Debug, Default)]
pub struct RequestCtx {
    /// The request host as the client sent it, for `X-Forwarded-Host`.
    host: String,
    /// The matched route, taken from the request's table snapshot.
    route: Option<RouteEntry>,
    /// Counts the request against its loopback upstream until it is done, for draining.
    inflight: Option<InflightGuard>,
}

#[async_trait]
impl ProxyHttp for RoutingProxy {
    type CTX = RequestCtx;

    fn new_ctx(&self) -> Self::CTX {
        RequestCtx::default()
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        let table = self.routes.load_full();
        let req = session.req_header();
        let host = Forwarding::request_host(req).map(str::to_owned);
        let sni_allows = match Sni::of(session) {
            Ok(None) => true,
            Ok(Some(sni)) => host.as_deref().is_none_or(|host| sni.allows(host)),
            Err(()) => false,
        };
        if !sni_allows {
            ErrorPage::from(StatusCode::MISDIRECTED_REQUEST)
                .send(session)
                .await;
            return Ok(true);
        }
        let route = host
            .as_deref()
            .and_then(|host| table.lookup(host, req.uri.path()))
            .cloned();
        let Some(route) = route else {
            ErrorPage::from(StatusCode::NOT_FOUND).send(session).await;
            return Ok(true);
        };
        match &route.target {
            Target::Unavailable { retry_after } => {
                ErrorPage::unavailable(*retry_after).send(session).await;
                return Ok(true);
            }
            Target::Loopback(upstream) => ctx.inflight = Some(upstream.track()),
            Target::Unix(_) => {}
        }
        ctx.host = host.unwrap_or_default();
        ctx.route = Some(route);
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let route = ctx.route()?;
        let mut peer = match &route.target {
            Target::Loopback(upstream) => HttpPeer::new(upstream.addr, false, String::new()),
            Target::Unix(upstream) => {
                let path: &Path = upstream.as_ref().as_ref();
                let path = path
                    .to_str()
                    .or_err(ErrorType::SocketError, "non-UTF-8 admin socket path")?;
                HttpPeer::new_uds(path, false, String::new())?
            }
            Target::Unavailable { .. } => {
                return Error::e_explain(ErrorType::InternalError, "unavailable route has no peer");
            }
        };
        self.defaults
            .overridden_by(&route.timeouts)
            .apply(&mut peer.options);
        Ok(Box::new(peer))
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let forwarding = Forwarding {
            host: &ctx.host,
            client: session
                .client_addr()
                .and_then(|addr| addr.as_inet())
                .map(|addr| addr.ip()),
            https: session.digest().is_some_and(|d| d.ssl_digest.is_some()),
        };
        forwarding.apply(ctx.route()?, upstream_request)
    }

    /// A fresh connection to the single upstream that just refused is not worth retrying.
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

    /// Retries only what cannot have reached the app: an idempotent request without a body, on
    /// a pooled connection that turned out to be dead. Nothing is retried once a body has
    /// started (ADR-0011).
    fn error_while_proxy(
        &self,
        peer: &HttpPeer,
        session: &mut Session,
        e: Box<Error>,
        _ctx: &mut Self::CTX,
        client_reused: bool,
    ) -> Box<Error> {
        let mut e = e.more_context(format!("Peer: {peer}"));
        let replayable = session.req_header().method.is_idempotent()
            && session.body_bytes_read() == 0
            && session.is_body_empty();
        if replayable {
            e.retry.decide_reuse(client_reused);
        } else {
            e.set_retry(false);
        }
        e
    }

    async fn fail_to_proxy(
        &self,
        session: &mut Session,
        e: &Error,
        _ctx: &mut Self::CTX,
    ) -> FailToProxy {
        let error_code = match ErrorPage::for_error(e) {
            Some(page) => {
                page.send(session).await;
                page.status.as_u16()
            }
            None => 0,
        };
        FailToProxy {
            error_code,
            can_reuse_downstream: false,
        }
    }
}

impl RequestCtx {
    fn route(&self) -> Result<&RouteEntry> {
        self.route
            .as_ref()
            .or_err(ErrorType::InternalError, "request has no matched route")
    }
}

/// How one request is rewritten for its upstream (ADR-0010, architecture §6.3).
#[derive(Debug, Clone, Copy)]
pub(super) struct Forwarding<'a> {
    /// The request host as sent, for `X-Forwarded-Host`.
    host: &'a str,
    /// The client's address, appended to `X-Forwarded-For`.
    client: Option<IpAddr>,
    /// Whether the client connection is TLS, for `X-Forwarded-Proto`.
    https: bool,
}

impl Forwarding<'_> {
    /// The host a request is for: the URI authority (HTTP/2 `:authority`, or an HTTP/1
    /// absolute-form target) and otherwise the `Host` header. Any userinfo is dropped.
    pub(super) fn request_host(req: &RequestHeader) -> Option<&str> {
        let host = match req.uri.authority() {
            Some(authority) => authority.as_str(),
            None => req.headers.get(header::HOST)?.to_str().ok()?,
        };
        let host = host.rsplit('@').next().unwrap_or(host);
        (!host.is_empty()).then_some(host)
    }

    /// Rewrites `req` for `route`: strips the prefix if the route asks for it, sets the
    /// forwarding headers and overwrites any the client sent. `Host` is left alone.
    ///
    /// The upstream always gets an origin-form target (RFC 9112 §3.2.1): Pingora forwards an
    /// HTTP/1 absolute-form target verbatim unless it is replaced, and the upstream is an
    /// origin server, not a proxy. A target that is not valid UTF-8 is forwarded untouched,
    /// or refused with 400 if its prefix would have to be stripped.
    fn apply(&self, route: &RouteEntry, req: &mut RequestHeader) -> Result<()> {
        let target = req.uri.path_and_query().map_or("/", |pq| pq.as_str());
        let upstream = route.upstream_path(target);
        let stripped = upstream != target;
        let rewritten =
            (stripped || req.raw_path() != target.as_bytes()).then(|| upstream.into_owned());
        if let Some(rewritten) = rewritten {
            if req.raw_path_is_utf8() {
                let uri = http::Uri::try_from(rewritten).explain_err(
                    ErrorType::InvalidHTTPHeader,
                    |_| "rewritten path is invalid",
                )?;
                req.set_uri(uri);
            } else if stripped {
                return Err(Error::explain(
                    ErrorType::InvalidHTTPHeader,
                    "cannot strip the prefix of a non-UTF-8 request target",
                )
                .into_down());
            }
        }

        req.remove_header(X_FORWARDED_PREFIX);
        if let Some(prefix) = route.forwarded_prefix() {
            req.insert_header(X_FORWARDED_PREFIX, prefix)?;
        }

        let mut chain: Vec<String> = req
            .headers
            .get_all(X_FORWARDED_FOR)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .collect();
        chain.extend(self.client.map(|ip| ip.to_string()));
        req.remove_header(X_FORWARDED_FOR);
        if !chain.is_empty() {
            req.insert_header(X_FORWARDED_FOR, chain.join(", "))?;
        }

        let proto = if self.https { "https" } else { "http" };
        req.insert_header(X_FORWARDED_PROTO, proto)?;
        req.remove_header(X_FORWARDED_HOST);
        if !self.host.is_empty() {
            req.insert_header(X_FORWARDED_HOST, self.host)?;
        }
        Ok(())
    }
}

impl Timeouts {
    /// These defaults with every timeout `route` sets taking precedence (ADR-0011).
    fn overridden_by(self, route: &RouteTimeouts) -> Self {
        Self {
            connect: route.connect.unwrap_or(self.connect),
            read: route.read.unwrap_or(self.read),
            write: route.write.unwrap_or(self.write),
            idle: route.idle.unwrap_or(self.idle),
        }
    }

    /// Sets these timeouts on an upstream peer. `connect` bounds the TCP connect; `idle` is how
    /// long a pooled keep-alive connection may sit unused.
    fn apply(self, options: &mut PeerOptions) {
        options.connection_timeout = Some(self.connect);
        options.total_connection_timeout = Some(self.connect);
        options.read_timeout = Some(self.read);
        options.write_timeout = Some(self.write);
        options.idle_timeout = Some(self.idle);
    }
}

/// A minimal built-in response: a status line as plain text (ADR-0032).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ErrorPage {
    status: StatusCode,
    retry_after: Option<Duration>,
}

impl From<StatusCode> for ErrorPage {
    fn from(status: StatusCode) -> Self {
        Self {
            status,
            retry_after: None,
        }
    }
}

impl ErrorPage {
    /// 503 with `Retry-After`, for a service in its stop-first gap.
    fn unavailable(retry_after: Duration) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            retry_after: Some(retry_after),
        }
    }

    /// The page for a request that failed while proxying, or `None` when the client is gone.
    /// Upstream timeouts are 504, every other upstream failure (refused, reset) 502.
    fn for_error(e: &Error) -> Option<Self> {
        use ErrorType::*;
        let status = match (e.etype(), e.esource()) {
            (HTTPStatus(code), _) => StatusCode::from_u16(*code).ok()?,
            (ConnectTimedout | ReadTimedout | WriteTimedout, ErrorSource::Upstream) => {
                StatusCode::GATEWAY_TIMEOUT
            }
            (_, ErrorSource::Upstream) => StatusCode::BAD_GATEWAY,
            (WriteError | ReadError | ConnectionClosed, ErrorSource::Downstream) => return None,
            (_, ErrorSource::Downstream) => StatusCode::BAD_REQUEST,
            (_, ErrorSource::Internal | ErrorSource::Unset) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Some(status.into())
    }

    pub(super) fn body(&self) -> Bytes {
        let reason = self.status.canonical_reason().unwrap_or_default();
        Bytes::from(format!("{} {reason}\n", self.status.as_u16()))
    }

    pub(super) fn header(&self, body_len: usize) -> Result<ResponseHeader> {
        let mut resp = ResponseHeader::build(self.status, Some(4))?;
        resp.insert_header(header::CONTENT_TYPE, "text/plain; charset=utf-8")?;
        resp.insert_header(header::CONTENT_LENGTH, body_len)?;
        resp.insert_header(header::CACHE_CONTROL, "no-store")?;
        if let Some(retry_after) = self.retry_after {
            // Whole seconds, rounded up so a client never retries before the gap is over, and
            // at least 1: `Retry-After: 0` invites a tight retry loop.
            let secs = retry_after.as_secs() + u64::from(retry_after.subsec_nanos() > 0);
            resp.insert_header(header::RETRY_AFTER, secs.max(1))?;
        }
        Ok(resp)
    }

    /// Writes the page unless a response has already started; the connection is not reused.
    pub(super) async fn send(&self, session: &mut Session) {
        let body = self.body();
        let result = match self.header(body.len()) {
            Ok(resp) => session.write_error_response(resp, body).await,
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            tracing::debug!(status = %self.status, error = %e, "cannot send error page");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use crate::routing::{Owner, UnixUpstream};

    fn route(prefix: &str, strip_prefix: bool) -> RouteEntry {
        RouteEntry {
            owner: Owner::Reserved("test"),
            prefix: prefix.parse().unwrap(),
            prefix_str: prefix.to_owned(),
            strip_prefix,
            target: Target::Unix(Arc::new(UnixUpstream::from(std::path::PathBuf::from(
                "/run/test.sock",
            )))),
            timeouts: RouteTimeouts::default(),
        }
    }

    fn request(target: &str, headers: &[(&'static str, &str)]) -> RequestHeader {
        let mut req = RequestHeader::build("GET", target.as_bytes(), None).unwrap();
        for (name, value) in headers {
            req.append_header(*name, *value).unwrap();
        }
        req
    }

    fn forwarding(host: &str) -> Forwarding<'_> {
        Forwarding {
            host,
            client: Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))),
            https: true,
        }
    }

    fn header<'a>(req: &'a RequestHeader, name: &str) -> Option<&'a str> {
        req.headers.get(name).map(|v| v.to_str().unwrap())
    }

    #[test]
    fn prefix_is_kept_by_default_and_announced() {
        let mut req = request("/api/users?x=1", &[("host", "app.example.com")]);
        forwarding("app.example.com")
            .apply(&route("/api", false), &mut req)
            .unwrap();
        assert_eq!(req.uri.to_string(), "/api/users?x=1");
        assert_eq!(header(&req, "x-forwarded-prefix"), Some("/api"));
        assert_eq!(header(&req, "host"), Some("app.example.com"));
    }

    #[test]
    fn strip_prefix_removes_segments_and_keeps_the_query() {
        for (target, expected) in [
            ("/api/users?x=1", "/users?x=1"),
            ("/api", "/"),
            ("/api/", "/"),
            ("/api?x", "/?x"),
        ] {
            let mut req = request(target, &[]);
            forwarding("a.example.com")
                .apply(&route("/api", true), &mut req)
                .unwrap();
            assert_eq!(req.uri.to_string(), expected, "{target}");
            assert_eq!(header(&req, "x-forwarded-prefix"), Some("/api"));
        }
    }

    #[test]
    fn absolute_form_targets_go_upstream_in_origin_form() {
        for (strip, expected) in [(false, "/api/x?q=1"), (true, "/x?q=1")] {
            let mut req = request("http://a.example.com/api/x?q=1", &[]);
            assert_eq!(req.raw_path(), b"http://a.example.com/api/x?q=1");
            forwarding("a.example.com")
                .apply(&route("/api", strip), &mut req)
                .unwrap();
            assert_eq!(req.raw_path(), expected.as_bytes(), "strip={strip}");
        }
    }

    #[test]
    fn non_utf8_targets_pass_untouched_but_cannot_be_stripped() {
        let mut req = RequestHeader::build("GET", b"/api/\xff", None).unwrap();
        forwarding("a.example.com")
            .apply(&route("/api", false), &mut req)
            .unwrap();
        assert_eq!(req.raw_path(), b"/api/\xff");

        let mut req = RequestHeader::build("GET", b"/api/\xff", None).unwrap();
        let err = forwarding("a.example.com")
            .apply(&route("/api", true), &mut req)
            .unwrap_err();
        assert_eq!(
            ErrorPage::for_error(&err).map(|p| p.status.as_u16()),
            Some(400)
        );
    }

    #[test]
    fn root_prefix_sends_no_forwarded_prefix_and_drops_a_spoofed_one() {
        let mut req = request("/x", &[("x-forwarded-prefix", "/evil")]);
        forwarding("a.example.com")
            .apply(&route("/", true), &mut req)
            .unwrap();
        assert_eq!(req.uri.to_string(), "/x");
        assert_eq!(header(&req, "x-forwarded-prefix"), None);
    }

    #[test]
    fn client_forwarding_headers_are_overwritten_and_for_is_appended() {
        let mut req = request(
            "/",
            &[
                ("x-forwarded-for", "198.51.100.1"),
                ("x-forwarded-for", "198.51.100.2"),
                ("x-forwarded-proto", "gopher"),
                ("x-forwarded-host", "evil.example.com"),
                ("x-forwarded-prefix", "/evil"),
            ],
        );
        forwarding("app.example.com:443")
            .apply(&route("/blog", false), &mut req)
            .unwrap();
        assert_eq!(
            header(&req, "x-forwarded-for"),
            Some("198.51.100.1, 198.51.100.2, 203.0.113.7")
        );
        assert_eq!(header(&req, "x-forwarded-proto"), Some("https"));
        assert_eq!(
            header(&req, "x-forwarded-host"),
            Some("app.example.com:443")
        );
        assert_eq!(header(&req, "x-forwarded-prefix"), Some("/blog"));
        for name in [
            "x-forwarded-for",
            "x-forwarded-proto",
            "x-forwarded-host",
            "x-forwarded-prefix",
        ] {
            assert_eq!(req.headers.get_all(name).iter().count(), 1, "{name}");
        }
    }

    #[test]
    fn plain_http_is_announced_as_http() {
        let mut req = request("/", &[]);
        Forwarding {
            https: false,
            ..forwarding("a.example.com")
        }
        .apply(&route("/", false), &mut req)
        .unwrap();
        assert_eq!(header(&req, "x-forwarded-proto"), Some("http"));
    }

    #[test]
    fn request_host_prefers_the_authority() {
        // HTTP/2 puts `:authority` in the URI.
        let with_authority = |uri: &'static str, headers| {
            let mut req = request("/", headers);
            req.set_uri(http::Uri::from_static(uri));
            req
        };
        let req = with_authority("https://h2.example.com/x", &[("host", "other.example.com")]);
        assert_eq!(Forwarding::request_host(&req), Some("h2.example.com"));
        let req = request("/x", &[("host", "App.Example.com:8443")]);
        assert_eq!(Forwarding::request_host(&req), Some("App.Example.com:8443"));
        let req = with_authority("https://user@h2.example.com/x", &[]);
        assert_eq!(Forwarding::request_host(&req), Some("h2.example.com"));
        // HTTP/1 absolute-form keeps the authority off the URI; Pingora has already checked
        // that it equals `Host`.
        let req = request("http://app.example.com/x", &[("host", "app.example.com")]);
        assert_eq!(Forwarding::request_host(&req), Some("app.example.com"));
        let req = request("/x", &[]);
        assert_eq!(Forwarding::request_host(&req), None);
    }

    #[test]
    fn route_timeouts_override_the_defaults() {
        let defaults = Timeouts::default();
        let route = RouteTimeouts {
            read: Some(Duration::from_secs(600)),
            idle: Some(Duration::from_secs(1)),
            ..RouteTimeouts::default()
        };
        let mut options = PeerOptions::new();
        defaults.overridden_by(&route).apply(&mut options);
        assert_eq!(options.connection_timeout, Some(defaults.connect));
        assert_eq!(options.read_timeout, Some(Duration::from_secs(600)));
        assert_eq!(options.write_timeout, Some(defaults.write));
        assert_eq!(options.idle_timeout, Some(Duration::from_secs(1)));
    }

    #[test]
    fn errors_map_to_built_in_pages() {
        let status = |etype, source| {
            let e = Error::new(etype);
            let e = match source {
                ErrorSource::Upstream => e.into_up(),
                ErrorSource::Downstream => e.into_down(),
                _ => e.into_in(),
            };
            ErrorPage::for_error(&e).map(|p| p.status.as_u16())
        };
        use ErrorType::*;
        assert_eq!(status(ConnectRefused, ErrorSource::Upstream), Some(502));
        assert_eq!(status(ConnectionClosed, ErrorSource::Upstream), Some(502));
        assert_eq!(status(ReadTimedout, ErrorSource::Upstream), Some(504));
        assert_eq!(status(ConnectTimedout, ErrorSource::Upstream), Some(504));
        assert_eq!(status(ReadError, ErrorSource::Downstream), None);
        assert_eq!(
            status(InvalidHTTPHeader, ErrorSource::Downstream),
            Some(400)
        );
        assert_eq!(status(HTTPStatus(413), ErrorSource::Downstream), Some(413));
        assert_eq!(status(InternalError, ErrorSource::Internal), Some(500));
    }

    #[test]
    fn unavailable_page_carries_retry_after() {
        let page = ErrorPage::unavailable(Duration::from_millis(200));
        let resp = page.header(page.body().len()).unwrap();
        assert_eq!(resp.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(resp.headers["retry-after"], "1");
        assert_eq!(page.body(), Bytes::from("503 Service Unavailable\n"));

        for (gap, expected) in [(0, "1"), (1500, "2"), (7000, "7")] {
            let page = ErrorPage::unavailable(Duration::from_millis(gap));
            let resp = page.header(page.body().len()).unwrap();
            assert_eq!(resp.headers["retry-after"], expected, "{gap} ms");
        }
    }
}
