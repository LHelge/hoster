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
//!
//! Every request, answered or failed, ends in the `logging` hook, which builds one
//! [`AccessRecord`] and from it writes a `debug` line to the access log (target
//! [`ACCESS_TARGET`], or [`UNROUTED_TARGET`] for a request that matched no route), counts the
//! request in the [`Traffic`] statistics of the web UI (ADR-0045) and, for a request routed to
//! a stack, offers a [`RequestRow`] to the log store (ADR-0067 §5). The row is offered with
//! [`LogSink::try_request`], which never waits: when the recorder's channel is full the row is
//! dropped and counted. Each request gets a [`RequestId`] when it arrives, kept in its
//! [`RequestCtx`].
//!
//! A request routed to a stack also gets an [`EdgeTrace`] (ADR-0069 §8): its trace id is the
//! request id, its `traceparent` replaces the client's on the way upstream, and its span ids go
//! into the request row. Requests to hoster's own reserved hosts have none.

use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use bytes::Bytes;
use http::{StatusCode, header};
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::proxy::{FailToProxy, ProxyHttp, Session};
use pingora::upstreams::peer::{HttpPeer, PeerOptions};
use pingora::{Error, ErrorSource, ErrorType, OkOrErr, OrErr, Result};

use super::DownstreamLimits;
use super::resolver::Sni;
use super::trace::EdgeTrace;
use crate::config::Timeouts;
use crate::logging::{ACCESS_TARGET, UNROUTED_TARGET};
use crate::logs::LogSink;
use crate::routing::{InflightGuard, Owner, RouteEntry, RouteTable, Target};
use crate::stack::RouteTimeouts;
use crate::stats::{RequestOutcome, TrackedRequest, Traffic};
use crate::store::{RequestId, RequestRow, Timestamp};

const X_FORWARDED_FOR: &str = "x-forwarded-for";
const X_FORWARDED_HOST: &str = "x-forwarded-host";
const X_FORWARDED_PROTO: &str = "x-forwarded-proto";
const X_FORWARDED_PREFIX: &str = "x-forwarded-prefix";

/// Routes requests by host and longest prefix to the upstreams in the published route table.
pub struct RoutingProxy {
    routes: Arc<ArcSwap<RouteTable>>,
    /// Upstream timeouts for routes that set none of their own.
    defaults: Timeouts,
    /// What a client connection is allowed.
    limits: DownstreamLimits,
    /// The per-route request statistics the web UI shows.
    traffic: Traffic,
    /// Where the rows of requests routed to a stack go (ADR-0067 §5).
    logs: LogSink,
}

impl RoutingProxy {
    /// The upstream of `route`, with `defaults` for the timeouts the route does not set: its
    /// loopback port, or hoster's own app (or the OTLP receiver) on a Unix socket.
    pub(super) fn peer(route: &RouteEntry, defaults: Timeouts) -> Result<Box<HttpPeer>> {
        let mut peer = match &route.target {
            Target::Loopback(upstream) => HttpPeer::new(upstream.addr, false, String::new()),
            Target::Unix(upstream) => {
                let path: &Path = upstream.as_ref().as_ref();
                let path = path
                    .to_str()
                    .or_err(ErrorType::SocketError, "non-UTF-8 socket path")?;
                HttpPeer::new_uds(path, false, String::new())?
            }
            Target::Unavailable { .. } => {
                return Error::e_explain(ErrorType::InternalError, "unavailable route has no peer");
            }
        };
        defaults
            .overridden_by(&route.timeouts)
            .apply(&mut peer.options);
        Ok(Box::new(peer))
    }

    /// A proxy reading `routes`, which the orchestrator replaces as deploys happen, holding
    /// clients to `limits`, counting requests in `traffic` and offering their rows to `logs`.
    pub fn new(
        routes: Arc<ArcSwap<RouteTable>>,
        defaults: Timeouts,
        limits: DownstreamLimits,
        traffic: Traffic,
        logs: LogSink,
    ) -> Self {
        Self {
            routes,
            defaults,
            limits,
            traffic,
            logs,
        }
    }
}

/// Per-request state: what the request matched in the snapshot it loaded.
#[derive(Debug)]
pub struct RequestCtx {
    /// When the request header had been read, for the access log's duration.
    started: Instant,
    /// The wall-clock time of `started`: the request row's time.
    arrived: Timestamp,
    /// The request's id in the log store, made when it arrived.
    request_id: RequestId,
    /// The request's trace, for a request routed to a stack.
    trace: Option<EdgeTrace>,
    /// How long after `started` the upstream's response header was ready for the client: the
    /// request's latency in the statistics. `None` until then, and for a request hoster
    /// answers itself.
    header_after: Option<Duration>,
    /// The request host as the client sent it, for `X-Forwarded-Host`.
    host: String,
    /// The matched route, taken from the request's table snapshot.
    route: Option<RouteEntry>,
    /// Counts the request against its loopback upstream until it is done, for draining.
    inflight: Option<InflightGuard>,
    /// Counts the request in its route's statistics; set once the route is known.
    stats: Option<TrackedRequest>,
}

impl Default for RequestCtx {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            arrived: Timestamp::now(),
            request_id: RequestId::random(),
            trace: None,
            header_after: None,
            host: String::new(),
            route: None,
            inflight: None,
            stats: None,
        }
    }
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
        ctx.host = Forwarding::request_host(req).unwrap_or_default().to_owned();
        let sni_allows = match Sni::of(session) {
            Ok(None) => true,
            Ok(Some(sni)) => ctx.host.is_empty() || sni.allows(&ctx.host),
            Err(()) => false,
        };
        if !sni_allows {
            self.limits.admit(session, self.defaults);
            ctx.stats = Some(self.traffic.unrouted());
            ErrorPage::from(StatusCode::MISDIRECTED_REQUEST)
                .send(session)
                .await;
            return Ok(true);
        }
        ctx.route = table.lookup(&ctx.host, req.uri.path()).cloned();
        if ctx
            .route
            .as_ref()
            .is_some_and(|route| matches!(route.owner, Owner::Route(_)))
        {
            ctx.trace = Some(EdgeTrace::start(ctx.request_id));
        }
        let timeouts = match &ctx.route {
            Some(route) => self.defaults.overridden_by(&route.timeouts),
            None => self.defaults,
        };
        self.limits.admit(session, timeouts);
        ctx.stats = Some(match (&ctx.route, table.known_host(&ctx.host)) {
            (Some(route), Some(host)) => self.traffic.route(host, route),
            _ => self.traffic.unrouted(),
        });
        let page = match ctx.route.as_ref().map(|route| &route.target) {
            None => ErrorPage::from(StatusCode::NOT_FOUND),
            Some(Target::Unavailable { retry_after }) => ErrorPage::unavailable(*retry_after),
            Some(Target::Loopback(upstream)) => {
                ctx.inflight = Some(upstream.track());
                return Ok(false);
            }
            Some(Target::Unix(_)) => return Ok(false),
        };
        page.send(session).await;
        Ok(true)
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        Self::peer(ctx.route()?, self.defaults)
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        Forwarding::new(session, &ctx.host).apply(ctx.route()?, upstream_request)?;
        match &mut ctx.trace {
            Some(trace) => trace.send_upstream(upstream_request),
            None => Ok(()),
        }
    }

    /// Notes when the response header arrived. What follows it (a body, an event stream, an
    /// upgraded connection) can last as long as it likes without counting as latency.
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

    /// One access log line per request, whether it was proxied, answered by hoster or failed;
    /// the request counted in its route's statistics (requests that failed before routing
    /// count as unrouted); and the row of a request routed to a stack offered to the log store.
    /// All three come from one [`AccessRecord`], so they agree on the timings.
    async fn logging(&self, session: &mut Session, e: Option<&Error>, ctx: &mut Self::CTX) {
        let record = AccessRecord::new(session, &ctx.host, ctx.route.as_ref(), ctx.started, e)
            .header_after(ctx.header_after)
            .trace(ctx.trace);
        record.emit();
        if let Some(row) = record.request_row(ctx.arrived, ctx.request_id) {
            // Dropped and counted when the recorder's channel is full; never waits.
            self.logs.try_request(row);
        }
        let outcome = RequestOutcome::from(&record);
        ctx.stats
            .get_or_insert_with(|| self.traffic.unrouted())
            .finish(outcome);
    }
}

/// One line of the proxy's access log.
#[derive(Debug)]
pub(super) struct AccessRecord<'a> {
    /// The request host as sent; empty when the request named none.
    host: &'a str,
    method: &'a str,
    /// The request path, without the query (which may carry secrets).
    path: &'a str,
    /// The route the request matched, if any.
    route: Option<&'a RouteEntry>,
    /// The status sent to the client; `None` when no response header went out.
    status: Option<u16>,
    /// From reading the request header to the end of the request, however long its body or
    /// stream lasted.
    duration: Duration,
    /// From reading the request header to the upstream's response header, for a proxied
    /// request that got one.
    header_after: Option<Duration>,
    /// Response body bytes sent to the client.
    bytes: usize,
    client: Option<IpAddr>,
    /// The fatal error that ended the request, if one did.
    error: Option<&'a Error>,
    /// hoster answered the request on purpose without a route.
    answered: bool,
    /// The request's trace, if it was routed to a stack.
    trace: Option<EdgeTrace>,
}

impl<'a> AccessRecord<'a> {
    /// The record of the request `session` served, which arrived for `host` at `started`,
    /// matched `route` and ended in `error`, if any.
    pub(super) fn new(
        session: &'a Session,
        host: &'a str,
        route: Option<&'a RouteEntry>,
        started: Instant,
        error: Option<&'a Error>,
    ) -> Self {
        let req = session.req_header();
        Self {
            host,
            method: req.method.as_str(),
            path: req.uri.path(),
            route,
            status: session.response_written().map(|resp| resp.status.as_u16()),
            duration: started.elapsed(),
            header_after: None,
            bytes: session.body_bytes_sent(),
            client: Forwarding::client_ip(session),
            error,
            answered: false,
            trace: None,
        }
    }

    /// The record of a request whose response header came `header_after` its start, if one did.
    pub(super) fn header_after(self, header_after: Option<Duration>) -> Self {
        Self {
            header_after,
            ..self
        }
    }

    /// The record of a request with `trace`, if it has one.
    pub(super) fn trace(self, trace: Option<EdgeTrace>) -> Self {
        Self { trace, ..self }
    }

    /// The record of a request hoster answered although it matched no route (an ACME
    /// challenge), which is logged like one that did.
    pub(super) fn answered(self) -> Self {
        Self {
            answered: true,
            ..self
        }
    }

    /// Writes the line at `debug`: to [`ACCESS_TARGET`] for a request that matched a route, to
    /// [`UNROUTED_TARGET`] for the rest, which is what scanners send. The request rows in the
    /// log store are the access log the UI reads; this line is for debugging the proxy.
    pub(super) fn emit(&self) {
        let route = self.route.map(|route| route.prefix_str.as_str());
        let (stack, service) = match self.route.map(|route| &route.owner) {
            Some(Owner::Route(owner)) => (Some(owner.stack.as_ref()), Some(owner.service.as_ref())),
            Some(Owner::Reserved(purpose)) => (Some("hoster"), Some(*purpose)),
            None => (None, None),
        };
        let target = self.route.map(|route| &route.target);
        let generation = match target {
            Some(Target::Loopback(upstream)) => Some(upstream.generation),
            _ => None,
        };
        let upstream = match target {
            Some(Target::Loopback(upstream)) => Some(upstream.addr.to_string()),
            Some(Target::Unix(upstream)) => {
                let path: &Path = upstream.as_ref().as_ref();
                Some(format!("unix:{}", path.display()))
            }
            Some(Target::Unavailable { .. }) | None => None,
        };
        let client = self.client.map(tracing::field::display);
        let error = self.error.map(tracing::field::display);
        // The names hoster's own records take a request's trace from (ADR-0069 §8).
        let ids = self.trace.as_ref().and_then(EdgeTrace::server);
        let trace_id = ids.map(|(trace_id, _)| tracing::field::display(trace_id));
        let span_id = ids.map(|(_, span_id)| tracing::field::display(span_id));
        // The target of an event is fixed where it is written.
        macro_rules! line {
            ($target:expr) => {
                tracing::debug!(
                    target: $target,
                    host = self.host,
                    method = self.method,
                    path = self.path,
                    route,
                    stack,
                    service,
                    generation,
                    upstream,
                    status = self.status,
                    duration_ms = self.duration.as_secs_f64() * 1000.0,
                    bytes = self.bytes,
                    client,
                    error,
                    trace_id,
                    span_id,
                    "request"
                )
            };
        }
        if self.route.is_some() || self.answered {
            line!(ACCESS_TARGET);
        } else {
            line!(UNROUTED_TARGET);
        }
    }

    /// The upstream caused the error that ended the request (connect, read or write failures,
    /// timeouts).
    fn upstream_failed(&self) -> bool {
        self.error
            .is_some_and(|e| e.esource() == &ErrorSource::Upstream)
    }

    /// The log store's row of this request, which arrived at `arrived` and was given
    /// `request_id`, or `None` when it was not routed to a stack: requests hoster answered
    /// without a route (an unknown host or prefix, a host that is not the SNI name) and those
    /// to hoster's own reserved hosts have no stack to belong to.
    ///
    /// The host is the table's spelling (lowercase, no port), so the rows of one route share
    /// it. A request whose response header never went out (the client left first) has status
    /// 0, as Pingora reports it. The span ids and the upstream start are the [`EdgeTrace`]'s.
    pub(super) fn request_row(
        &self,
        arrived: Timestamp,
        request_id: RequestId,
    ) -> Option<RequestRow> {
        let route = self.route?;
        let Owner::Route(owner) = &route.owner else {
            return None;
        };
        let mut row = RequestRow {
            time: arrived,
            host: RouteTable::normalize_host(self.host).into_owned(),
            prefix: route.prefix_str.clone(),
            stack: owner.stack.to_string(),
            service: owner.service.to_string(),
            method: self.method.to_owned(),
            path: self.path.to_owned(),
            status: self.status.unwrap_or(0),
            to_header: self.header_after,
            total: self.duration,
            bytes: u64::try_from(self.bytes).unwrap_or(u64::MAX),
            // A routed request always came over TCP; `::` only if one ever did not.
            client: self
                .client
                .unwrap_or(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)),
            upstream_failed: self.upstream_failed(),
            request_id,
            server_span: None,
            client_span: None,
            upstream_start: None,
        };
        if let Some(trace) = &self.trace {
            trace.record_in(&mut row);
        }
        Some(row)
    }
}

impl From<&AccessRecord<'_>> for RequestOutcome {
    /// The outcome the statistics count. The latency is the time to the response header, so a
    /// long download, an event stream or a WebSocket counts as fast as its upstream answered;
    /// a request without one (answered by hoster, or failed first) counts its whole duration.
    /// An error counts against the upstream when the upstream caused it (connect, read or
    /// write failures, timeouts).
    fn from(record: &AccessRecord<'_>) -> Self {
        RequestOutcome {
            status: record.status,
            latency: record.header_after.unwrap_or(record.duration),
            upstream_error: record.upstream_failed(),
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

impl<'a> Forwarding<'a> {
    /// The forwarding of a request `session` serves, sent for `host`.
    pub(super) fn new(session: &Session, host: &'a str) -> Self {
        Self {
            host,
            client: Self::client_ip(session),
            https: session.digest().is_some_and(|d| d.ssl_digest.is_some()),
        }
    }

    /// The client's IP address, when it connected over TCP.
    pub(super) fn client_ip(session: &Session) -> Option<IpAddr> {
        session
            .client_addr()
            .and_then(|addr| addr.as_inet())
            .map(|addr| addr.ip())
    }

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

    /// Leaves `req` at most one `Content-Length`, as a plain decimal number.
    ///
    /// Pingora accepts a repeated field and a list (`5, 5`) when every value is the same
    /// number, and forwards them as they came. An upstream that reads such a field differently
    /// would disagree with the proxy about where the body ends, which is what request
    /// smuggling needs (RFC 9112 §6.3), so the upstream only ever sees the one number. Values
    /// that are not all the same number are refused with 400; Pingora already refuses them.
    fn single_content_length(req: &mut RequestHeader) -> Result<()> {
        let mut values = req
            .headers
            .get_all(header::CONTENT_LENGTH)
            .iter()
            .flat_map(|value| value.as_bytes().split(|b| *b == b','))
            .map(|value| {
                std::str::from_utf8(value.trim_ascii())
                    .ok()
                    .filter(|digits| {
                        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
                    })
                    .and_then(|digits| digits.parse::<u64>().ok())
            });
        let Some(first) = values.next() else {
            return Ok(());
        };
        let length = first
            .filter(|length| values.all(|value| value == Some(*length)))
            .or_err(ErrorType::InvalidHTTPHeader, "ambiguous Content-Length")
            .map_err(|e| e.into_down())?;
        req.insert_header(header::CONTENT_LENGTH, length)?;
        Ok(())
    }

    /// Rewrites `req` for `route`: strips the prefix if the route asks for it, sets the
    /// forwarding headers and overwrites any the client sent. `Host` is left alone.
    ///
    /// The upstream always gets an origin-form target (RFC 9112 §3.2.1): Pingora forwards an
    /// HTTP/1 absolute-form target verbatim unless it is replaced, and the upstream is an
    /// origin server, not a proxy. A target that is not valid UTF-8 is forwarded untouched,
    /// or refused with 400 if its prefix would have to be stripped.
    pub(super) fn apply(&self, route: &RouteEntry, req: &mut RequestHeader) -> Result<()> {
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

        Self::single_content_length(req)?;

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
    pub(super) fn for_error(e: &Error) -> Option<Self> {
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

    /// The page's status.
    pub(super) fn status(&self) -> StatusCode {
        self.status
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
    fn content_length_goes_upstream_as_one_number() {
        let lengths = |req: &RequestHeader| -> Vec<String> {
            let values = req.headers.get_all("content-length").iter();
            values.map(|v| v.to_str().unwrap().to_owned()).collect()
        };
        for sent in [
            &["5"][..],
            &["5", "5"],
            &["5, 5"],
            &["5,5", " 5 "],
            &["005"],
        ] {
            let headers: Vec<_> = sent.iter().map(|v| ("content-length", *v)).collect();
            let mut req = request("/", &headers);
            forwarding("a.example.com")
                .apply(&route("/", false), &mut req)
                .unwrap();
            assert_eq!(lengths(&req), ["5"], "{sent:?}");
        }
        for sent in [
            &["5", "6"][..],
            &["5, 6"],
            &["+5"],
            &["0x5"],
            &["5 5"],
            &[""],
            &["5,"],
        ] {
            let headers: Vec<_> = sent.iter().map(|v| ("content-length", *v)).collect();
            let mut req = request("/", &headers);
            let err = forwarding("a.example.com")
                .apply(&route("/", false), &mut req)
                .unwrap_err();
            assert_eq!(err.esource(), &ErrorSource::Downstream, "{sent:?}");
            assert_eq!(ErrorPage::for_error(&err).unwrap().status, 400, "{sent:?}");
        }
        let mut req = request("/", &[]);
        forwarding("a.example.com")
            .apply(&route("/", false), &mut req)
            .unwrap();
        assert!(lengths(&req).is_empty());
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
    fn access_records_log_the_request_route_and_outcome() {
        use crate::logging::LogFormat;
        use crate::logging::capture::{self, subscriber};
        use crate::routing::{RouteOwner, Upstream};

        let upstream =
            Upstream::loopback(20001, "blog".parse().unwrap(), "web".parse().unwrap(), 4);
        let entry = RouteEntry {
            owner: Owner::Route(RouteOwner {
                stack: "blog".parse().unwrap(),
                service: "web".parse().unwrap(),
            }),
            target: Target::Loopback(Arc::new(upstream)),
            ..route("/api", false)
        };
        let error = Error::new(ErrorType::ReadTimedout).into_up();
        let trace = EdgeTrace::start(RequestId::from([0xab; 16]));
        let (buffer, subscriber) = subscriber(LogFormat::Json, capture::GLOBAL_FILTER);
        tracing::subscriber::with_default(subscriber, || {
            AccessRecord {
                host: "blog.example.com",
                method: "POST",
                path: "/api/posts",
                route: Some(&entry),
                status: Some(504),
                duration: Duration::from_micros(1500),
                header_after: None,
                bytes: 24,
                client: Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))),
                error: Some(&error),
                answered: false,
                trace: Some(trace),
            }
            .emit();
            let bare = AccessRecord {
                host: "",
                method: "GET",
                path: "/",
                route: None,
                status: None,
                duration: Duration::ZERO,
                header_after: None,
                bytes: 0,
                client: None,
                error: None,
                answered: false,
                trace: None,
            };
            // Every line is a `debug` one; without a route it goes to the unrouted target, which
            // the filter leaves at `info`.
            bare.emit();
            bare.answered().emit();
        });
        let lines = buffer.json();
        assert_eq!(lines.len(), 2, "{lines:?}");
        let line = &lines[0];
        assert_eq!(line["target"], "hoster::access");
        assert_eq!(line["host"], "blog.example.com");
        assert_eq!(line["method"], "POST");
        assert_eq!(line["path"], "/api/posts");
        assert_eq!(line["route"], "/api");
        assert_eq!(line["stack"], "blog");
        assert_eq!(line["service"], "web");
        assert_eq!(line["generation"], 4);
        assert_eq!(line["upstream"], "127.0.0.1:20001");
        assert_eq!(line["status"], 504);
        assert_eq!(line["duration_ms"], 1.5);
        assert_eq!(line["bytes"], 24);
        assert_eq!(line["client"], "203.0.113.7");
        assert!(
            line["error"].as_str().unwrap().contains("ReadTimedout"),
            "{line}"
        );
        let (trace_id, span_id) = trace.server().unwrap();
        assert_eq!(line["trace_id"], trace_id.to_string());
        assert_eq!(line["span_id"], span_id.to_string());

        let bare = lines[1].as_object().unwrap();
        for absent in [
            "route",
            "stack",
            "service",
            "generation",
            "upstream",
            "status",
            "client",
            "error",
            "trace_id",
            "span_id",
        ] {
            assert!(!bare.contains_key(absent), "{absent}: {bare:?}");
        }
        assert_eq!(bare["host"], "");
    }

    /// A stream open for half a minute whose upstream answered at once is a fast request.
    #[test]
    fn the_counted_latency_is_the_time_to_the_response_header() {
        let record = AccessRecord {
            host: "blog.example.com",
            method: "GET",
            path: "/events",
            route: None,
            status: Some(200),
            duration: Duration::from_secs(30),
            header_after: None,
            bytes: 512,
            client: None,
            error: None,
            answered: false,
            trace: None,
        };
        // Without an upstream response header (hoster's own answer) the whole request counts.
        assert_eq!(
            RequestOutcome::from(&record).latency,
            Duration::from_secs(30)
        );
        let record = record.header_after(Some(Duration::from_millis(4)));
        assert_eq!(
            RequestOutcome::from(&record).latency,
            Duration::from_millis(4)
        );
    }

    /// Only a request routed to a stack has a row; one whose response never went out (the
    /// client left first) has status 0.
    #[test]
    fn only_requests_routed_to_a_stack_have_a_request_row() {
        use crate::routing::RouteOwner;

        let stacked = RouteEntry {
            owner: Owner::Route(RouteOwner {
                stack: "blog".parse().unwrap(),
                service: "web".parse().unwrap(),
            }),
            ..route("/api", true)
        };
        let reserved = route("/", false);
        let record = |route| AccessRecord {
            host: "Blog.Example.com:443",
            method: "PUT",
            path: "/api/posts/1",
            route,
            status: None,
            duration: Duration::from_millis(80),
            header_after: None,
            bytes: 7,
            client: Some(IpAddr::V6("2001:db8::1".parse().unwrap())),
            error: None,
            answered: false,
            trace: None,
        };
        let id = RequestId::from([0xab; 16]);
        let at = Timestamp::from_millis(1_700_000_000_000);

        let row = record(Some(&stacked)).request_row(at, id).unwrap();
        assert_eq!(
            row,
            RequestRow {
                time: at,
                host: "blog.example.com".into(),
                prefix: "/api".into(),
                stack: "blog".into(),
                service: "web".into(),
                method: "PUT".into(),
                path: "/api/posts/1".into(),
                status: 0,
                to_header: None,
                total: Duration::from_millis(80),
                bytes: 7,
                client: IpAddr::V6("2001:db8::1".parse().unwrap()),
                upstream_failed: false,
                request_id: id,
                server_span: None,
                client_span: None,
                upstream_start: None,
            }
        );
        assert_eq!(record(Some(&reserved)).request_row(at, id), None);
        assert_eq!(record(None).request_row(at, id), None);

        // A traced request's row has its server span; it never went upstream, so no more.
        let trace = EdgeTrace::start(id);
        let (trace_id, server_span) = trace.server().unwrap();
        assert_eq!(trace_id, id, "the trace id is the request id");
        let row = record(Some(&stacked))
            .trace(Some(trace))
            .request_row(at, id)
            .unwrap();
        assert_eq!(row.server_span, Some(server_span));
        assert_eq!(row.client_span, None);
        assert_eq!(row.upstream_start, None);
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
