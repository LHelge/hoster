//! Trace context at the edge (ADR-0069 §8, §9): the trace the proxy starts for every request it
//! routes to a stack, and the `traceparent` it sends upstream.
//!
//! The trace id is the request id, so a container's spans and log lines carry the id of the
//! request's row. The proxy has two spans per request, a server span for the request and a
//! client span for the upstream request; they are not stored as spans but as ids (and the time
//! the upstream request started) in the request row, from which they are built when a trace is
//! read.
//!
//! What a client sends as `traceparent` or `tracestate` never reaches the upstream: a client
//! does not choose trace ids or join traces it does not own.
//!
//! - **Retries.** Every attempt at the upstream request gets a new client span, sent in its
//!   `traceparent`, and the row keeps the last attempt's span and start time: the proxy retries
//!   only what cannot have reached the app (a dead pooled connection), so the last attempt is
//!   the one the app saw. A request stays one row.
//! - **No upstream request.** A request hoster answers itself (a service in its stop-first gap),
//!   or whose upstream connection fails before the request header is written, has a server span
//!   and no client span or upstream start.
//! - **No random ids.** An id of all zeros is invalid in W3C trace context, and is what
//!   [`HexId::random`](crate::store::HexId::random) gives when the operating system's generator
//!   fails. A request whose id is all zeros has no trace: no span ids are stored and no
//!   `traceparent` is sent. A client span that comes out all zeros is left out the same way.

use std::fmt;
use std::time::SystemTime;

use pingora::Result;
use pingora::http::RequestHeader;

use crate::store::{MicroTimestamp, RequestId, RequestRow, SpanId, TraceId};

const TRACEPARENT: &str = "traceparent";
const TRACESTATE: &str = "tracestate";

/// The proxy's part of the trace of one request routed to a stack.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct EdgeTrace {
    /// The trace id (the request id) and the server span; `None` when either is all zeros.
    server: Option<(TraceId, SpanId)>,
    /// The client span of the latest upstream request and when it started.
    client: Option<(SpanId, MicroTimestamp)>,
}

impl EdgeTrace {
    /// The trace of the request with `request_id`, with a new server span.
    pub(super) fn start(request_id: RequestId) -> Self {
        Self::with_server_span(request_id, SpanId::random())
    }

    fn with_server_span(trace_id: TraceId, server_span: SpanId) -> Self {
        let valid = trace_id.is_valid() && server_span.is_valid();
        Self {
            server: valid.then_some((trace_id, server_span)),
            client: None,
        }
    }

    /// The trace id and the server span, which hoster's own records made inside the request
    /// carry (ADR-0069 §8); `None` when the request has no trace.
    pub(super) fn server(&self) -> Option<(TraceId, SpanId)> {
        self.server
    }

    /// Starts the client span of an upstream request and rewrites `req` for it: the client's
    /// `traceparent` and `tracestate` are removed and the proxy's `traceparent` is sent, with
    /// the new client span as the parent. Called once per attempt; the last one counts.
    pub(super) fn send_upstream(&mut self, req: &mut RequestHeader) -> Result<()> {
        self.send_upstream_from(req, SpanId::random(), SystemTime::now())
    }

    fn send_upstream_from(
        &mut self,
        req: &mut RequestHeader,
        client_span: SpanId,
        now: SystemTime,
    ) -> Result<()> {
        req.remove_header(TRACEPARENT);
        req.remove_header(TRACESTATE);
        self.client = None;
        let Some((trace_id, _)) = self.server else {
            return Ok(());
        };
        if !client_span.is_valid() {
            return Ok(());
        }
        self.client = Some((client_span, MicroTimestamp::from(now)));
        let traceparent = TraceParent {
            trace_id,
            parent: client_span,
        };
        req.insert_header(TRACEPARENT, traceparent.to_string())?;
        Ok(())
    }

    /// Writes the span ids and the upstream start into the request's `row`.
    pub(super) fn record_in(&self, row: &mut RequestRow) {
        row.server_span = self.server.map(|(_, span)| span);
        row.client_span = self.client.map(|(span, _)| span);
        row.upstream_start = self.client.map(|(_, start)| start);
    }
}

/// A W3C `traceparent` value of version `00`, always sampled (`01`): hoster does not sample
/// (ADR-0069 §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TraceParent {
    trace_id: TraceId,
    parent: SpanId,
}

impl fmt::Display for TraceParent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "00-{}-{}-01", self.trace_id, self.parent)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const TRACE: [u8; 16] = [0xab; 16];
    const SERVER: [u8; 8] = [0x11; 8];
    const CLIENT: [u8; 8] = [0x22; 8];

    fn request(headers: &[(&'static str, &str)]) -> RequestHeader {
        let mut req = RequestHeader::build("GET", b"/", None).unwrap();
        for (name, value) in headers {
            req.append_header(*name, *value).unwrap();
        }
        req
    }

    fn values(req: &RequestHeader, name: &str) -> Vec<String> {
        let values = req.headers.get_all(name).iter();
        values.map(|v| v.to_str().unwrap().to_owned()).collect()
    }

    fn row_ids(trace: &EdgeTrace) -> (Option<SpanId>, Option<SpanId>, Option<MicroTimestamp>) {
        let mut row = RequestRow {
            time: crate::store::Timestamp::from_millis(0),
            host: String::new(),
            prefix: String::new(),
            stack: String::new(),
            service: String::new(),
            method: String::new(),
            path: String::new(),
            status: 0,
            to_header: None,
            total: Duration::ZERO,
            bytes: 0,
            client: std::net::IpAddr::from([127, 0, 0, 1]),
            upstream_failed: false,
            request_id: RequestId::from(TRACE),
            server_span: None,
            client_span: None,
            upstream_start: None,
        };
        trace.record_in(&mut row);
        (row.server_span, row.client_span, row.upstream_start)
    }

    #[test]
    fn traceparent_is_version_00_and_sampled() {
        let value = TraceParent {
            trace_id: TraceId::from(TRACE),
            parent: SpanId::from(CLIENT),
        };
        assert_eq!(
            value.to_string(),
            "00-abababababababababababababababab-2222222222222222-01"
        );
    }

    #[test]
    fn the_clients_trace_context_is_replaced_by_the_proxys() {
        let mut trace = EdgeTrace::with_server_span(TraceId::from(TRACE), SpanId::from(SERVER));
        let mut req = request(&[
            (
                "traceparent",
                "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
            ),
            (
                "traceparent",
                "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203332-00",
            ),
            ("tracestate", "vendor=evil"),
        ]);
        let at = SystemTime::UNIX_EPOCH + Duration::from_micros(1_700_000_000_000_123);
        trace
            .send_upstream_from(&mut req, SpanId::from(CLIENT), at)
            .unwrap();
        assert_eq!(
            values(&req, "traceparent"),
            ["00-abababababababababababababababab-2222222222222222-01"]
        );
        assert!(values(&req, "tracestate").is_empty());
        assert_eq!(
            row_ids(&trace),
            (
                Some(SpanId::from(SERVER)),
                Some(SpanId::from(CLIENT)),
                Some(MicroTimestamp::from_micros(1_700_000_000_000_123)),
            )
        );
    }

    /// A retried request keeps one row, with the last attempt's client span and start.
    #[test]
    fn a_retry_replaces_the_client_span() {
        let mut trace = EdgeTrace::with_server_span(TraceId::from(TRACE), SpanId::from(SERVER));
        let first = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        let second = first + Duration::from_millis(3);
        // Pingora builds each attempt's header from the client's request again.
        let mut req = request(&[]);
        trace
            .send_upstream_from(&mut req, SpanId::from(CLIENT), first)
            .unwrap();
        let mut req = request(&[]);
        trace
            .send_upstream_from(&mut req, SpanId::from([0x33; 8]), second)
            .unwrap();
        assert_eq!(
            values(&req, "traceparent"),
            ["00-abababababababababababababababab-3333333333333333-01"]
        );
        assert_eq!(
            row_ids(&trace),
            (
                Some(SpanId::from(SERVER)),
                Some(SpanId::from([0x33; 8])),
                Some(MicroTimestamp::from(second)),
            )
        );
    }

    #[test]
    fn a_request_that_never_went_upstream_has_only_its_server_span() {
        let trace = EdgeTrace::with_server_span(TraceId::from(TRACE), SpanId::from(SERVER));
        assert_eq!(row_ids(&trace), (Some(SpanId::from(SERVER)), None, None));
        assert_eq!(
            trace.server(),
            Some((TraceId::from(TRACE), SpanId::from(SERVER)))
        );
    }

    /// All-zero ids, which only a failed random generator gives, are never sent or stored; the
    /// client's trace context is removed all the same.
    #[test]
    fn zero_ids_give_no_traceparent_and_no_span_ids() {
        let incoming = [
            (
                "traceparent",
                "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
            ),
            ("tracestate", "vendor=evil"),
        ];
        let now = SystemTime::now();
        for (trace_id, server) in [([0; 16], SERVER), (TRACE, [0; 8])] {
            let mut trace =
                EdgeTrace::with_server_span(TraceId::from(trace_id), SpanId::from(server));
            assert_eq!(trace.server(), None);
            let mut req = request(&incoming);
            trace
                .send_upstream_from(&mut req, SpanId::from(CLIENT), now)
                .unwrap();
            assert!(values(&req, "traceparent").is_empty());
            assert!(values(&req, "tracestate").is_empty());
            assert_eq!(row_ids(&trace), (None, None, None));
        }

        let mut trace = EdgeTrace::with_server_span(TraceId::from(TRACE), SpanId::from(SERVER));
        let mut req = request(&incoming);
        trace
            .send_upstream_from(&mut req, SpanId::from([0; 8]), now)
            .unwrap();
        assert!(values(&req, "traceparent").is_empty());
        assert_eq!(row_ids(&trace), (Some(SpanId::from(SERVER)), None, None));
    }
}
