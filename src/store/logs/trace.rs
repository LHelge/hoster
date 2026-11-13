//! One trace as the waterfall shows it (ADR-0069 §6, §9): its spans, stored and built from the
//! proxy's request row alike, arranged as a tree in depth-first order, with what the waterfall
//! derives from them (depth, the critical path, self time, which spans a [`Collapse`] hides)
//! computed here rather than in templates.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde_json::json;

use super::{
    MicroTimestamp, RequestRow, Span, SpanId, SpanKind, SpanStatus, TraceId, UnknownLogText,
};

impl Span {
    /// The service the proxy's two spans of a request are listed under: hoster's, as its own
    /// records are (ADR-0068), so the proxy's spans and its records made inside a request share
    /// one service.
    pub const PROXY_SERVICE: &str = "hoster";

    /// When the span ended.
    pub fn end(&self) -> MicroTimestamp {
        self.start + self.duration
    }

    /// Whether the span ended in error.
    pub fn is_error(&self) -> bool {
        self.status == SpanStatus::Error
    }
}

impl RequestRow {
    /// The proxy's spans of this request (ADR-0069 §9), built as stored spans are shaped so the
    /// UI cannot tell them apart: the server span for the request, with the request's trace id
    /// (its request id) and timing, and the client span for the upstream request under it,
    /// from the upstream start to the request's end. No spans for a row written before the
    /// span ids were stored; only the server span when the upstream request never started.
    pub fn proxy_spans(&self) -> Vec<Span> {
        let Some(server_id) = self.server_span else {
            return Vec::new();
        };
        let start = MicroTimestamp::from(self.time);
        let end = start + self.total;
        let server_error = self.upstream_failed || self.status >= 500;
        let message = if self.upstream_failed {
            Some("upstream failed".to_owned())
        } else if self.status >= 500 {
            Some(format!("responded {}", self.status))
        } else {
            None
        };
        let mut server_attributes = json!({
            "server.address": self.host,
            "http.request.method": self.method,
            "url.path": self.path,
            "client.address": self.client.to_string(),
            "hoster.route.prefix": self.prefix,
        });
        if self.status != 0 {
            server_attributes["http.response.status_code"] = json!(self.status);
        }
        let resource = json!({ "service.name": Span::PROXY_SERVICE }).to_string();
        let span = |span_id, parent, name, kind, start, duration, error: bool, attributes| Span {
            trace_id: self.request_id,
            span_id,
            parent,
            stack: self.stack.clone(),
            service: Span::PROXY_SERVICE.to_owned(),
            name,
            kind,
            start,
            duration,
            status: if error {
                SpanStatus::Error
            } else {
                SpanStatus::Unset
            },
            status_message: if error { message.clone() } else { None },
            attributes: Some(attributes),
            resource: Some(resource.clone()),
            events: None,
            received: self.time,
        };
        let mut spans = vec![span(
            server_id,
            None,
            format!("{} {}", self.method, self.path),
            SpanKind::Server,
            start,
            self.total,
            server_error,
            server_attributes.to_string(),
        )];
        if let (Some(client_id), Some(upstream_start)) = (self.client_span, self.upstream_start) {
            let micros = (end.as_micros() - upstream_start.as_micros()).max(0);
            let mut attributes = json!({
                "upstream.stack": self.stack,
                "upstream.service": self.service,
            });
            if self.to_header.is_some() && self.status != 0 {
                attributes["http.response.status_code"] = json!(self.status);
            }
            spans.push(span(
                client_id,
                Some(server_id),
                format!("upstream {}", self.service),
                SpanKind::Client,
                upstream_start,
                Duration::from_micros(micros.unsigned_abs()),
                server_error,
                attributes.to_string(),
            ));
        }
        spans
    }
}

/// A span in a [`Trace`], with where the waterfall puts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceSpan {
    pub span: Span,
    /// 0 for a root (and for a span whose parent is missing in a trace without a root), one
    /// more per level below.
    pub depth: usize,
    /// The span names a parent the trace does not have (lost, late, or past the span cap): it
    /// is shown under the trace's first root, or as a root when there is none.
    pub parent_missing: bool,
    /// How many spans are directly under it.
    pub children: usize,
    /// How many spans are under it at any depth: what collapsing it hides.
    pub descendants: usize,
    /// On the critical path: a root, or the child with the latest end of a span on it.
    pub critical: bool,
    /// Its duration minus the union of its children's intervals within it.
    pub self_time: Duration,
    /// The log records made in it.
    pub log_records: u64,
}

/// One trace's spans in depth-first order: roots by start, each followed by its children by
/// start, recursively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    id: TraceId,
    spans: Vec<TraceSpan>,
    total_spans: u64,
    log_records: u64,
}

impl Trace {
    /// The most spans a trace is read with; the earliest by start are kept, and
    /// [`Trace::total_spans`] says how many it has.
    pub const MAX_SPANS: usize = 2_000;

    /// The trace `id` of `spans`, which has `total_spans` in all (more than `spans` holds when
    /// it was cut at [`Trace::MAX_SPANS`]).
    pub fn new(id: TraceId, mut spans: Vec<Span>, total_spans: u64) -> Self {
        spans.sort_by(|a, b| (a.start, a.span_id.as_ref()).cmp(&(b.start, b.span_id.as_ref())));
        let tree = Tree::of(&spans);
        let mut slots: Vec<Option<Span>> = spans.into_iter().map(Some).collect();
        let spans = tree
            .order
            .iter()
            .zip(tree.nodes)
            .map(|(&index, node)| TraceSpan {
                span: slots[index].take().expect("each span is visited once"),
                depth: node.depth,
                parent_missing: node.parent_missing,
                children: node.children.len(),
                descendants: node.descendants,
                critical: node.critical,
                self_time: node.self_time,
                log_records: 0,
            })
            .collect::<Vec<_>>();
        let total_spans = total_spans.max(spans.len() as u64);
        Self {
            id,
            spans,
            total_spans,
            log_records: 0,
        }
    }

    /// Fills the log record counts: per span id, and `None` for records without one. Records
    /// of span ids the trace does not hold count only towards [`Trace::log_records`].
    pub(super) fn count_log_records(&mut self, counts: HashMap<Option<SpanId>, u64>) {
        self.log_records = counts.values().sum();
        for span in &mut self.spans {
            span.log_records = counts.get(&Some(span.span.span_id)).copied().unwrap_or(0);
        }
    }

    pub fn id(&self) -> TraceId {
        self.id
    }

    /// The spans in depth-first order.
    pub fn spans(&self) -> &[TraceSpan] {
        &self.spans
    }

    /// The span with `id`, if the trace holds it.
    pub fn span(&self, id: SpanId) -> Option<&TraceSpan> {
        self.spans.iter().find(|s| s.span.span_id == id)
    }

    /// How many spans the trace has, those past [`Trace::MAX_SPANS`] included.
    pub fn total_spans(&self) -> u64 {
        self.total_spans
    }

    /// Whether spans were left out at [`Trace::MAX_SPANS`].
    pub fn is_truncated(&self) -> bool {
        self.total_spans > self.spans.len() as u64
    }

    /// How many log records were made in the trace.
    pub fn log_records(&self) -> u64 {
        self.log_records
    }

    /// When its earliest span started.
    pub fn start(&self) -> MicroTimestamp {
        let starts = self.spans.iter().map(|s| s.span.start);
        starts.min().unwrap_or(MicroTimestamp::from_micros(0))
    }

    /// When its last span ended.
    pub fn end(&self) -> MicroTimestamp {
        let ends = self.spans.iter().map(|s| s.span.end());
        ends.max().unwrap_or(self.start())
    }

    /// From the earliest start to the last end: the waterfall's axis.
    pub fn duration(&self) -> Duration {
        self.offset(self.end())
    }

    /// How long after the trace's start `time` is; zero for a time before it.
    pub fn offset(&self, time: MicroTimestamp) -> Duration {
        let micros = time.as_micros() - self.start().as_micros();
        Duration::from_micros(u64::try_from(micros).unwrap_or(0))
    }

    /// The services of its spans, each once, in depth-first order.
    pub fn services(&self) -> Vec<&str> {
        let mut services: Vec<&str> = Vec::new();
        for span in &self.spans {
            if !services.contains(&span.span.service.as_str()) {
                services.push(&span.span.service);
            }
        }
        services
    }

    /// Whether any span ended in error.
    pub fn is_error(&self) -> bool {
        self.spans.iter().any(|s| s.span.is_error())
    }

    /// The spans on the critical path, in depth-first order.
    pub fn critical_path(&self) -> impl Iterator<Item = &TraceSpan> {
        self.spans.iter().filter(|s| s.critical)
    }

    /// The spans "collapse to services" collapses, in depth-first order: every server span
    /// with children and a parent, i.e. where a request enters a service below the first, so
    /// the waterfall keeps the hops between services and hides the work inside them.
    pub fn service_entries(&self) -> impl Iterator<Item = &TraceSpan> {
        self.spans.iter().filter(|s| {
            s.span.kind == SpanKind::Server && s.children > 0 && s.span.parent.is_some()
        })
    }

    /// The spans `collapse` collapses: of those with children, the service entries when it
    /// starts from them, with its toggled spans flipped.
    pub fn collapsed(&self, collapse: &Collapse) -> HashSet<SpanId> {
        let mut collapsed: HashSet<SpanId> = if collapse.services {
            self.service_entries().map(|s| s.span.span_id).collect()
        } else {
            HashSet::new()
        };
        for id in &collapse.toggled {
            if self.span(*id).is_some_and(|s| s.children > 0) && !collapsed.remove(id) {
                collapsed.insert(*id);
            }
        }
        collapsed
    }

    /// `collapse` as this trace's links write it: only toggles of its spans with children,
    /// each once, in depth-first order.
    pub fn canonical(&self, collapse: &Collapse) -> Collapse {
        let toggled = self
            .spans
            .iter()
            .filter(|s| s.children > 0 && collapse.toggled.contains(&s.span.span_id))
            .map(|s| s.span.span_id)
            .collect();
        Collapse {
            services: collapse.services,
            toggled,
        }
    }

    /// The spans the waterfall shows with `collapsed` collapsed, in depth-first order: none
    /// under a collapsed span. Each comes with whether it is collapsed itself.
    pub fn shown<'a>(
        &'a self,
        collapsed: &'a HashSet<SpanId>,
    ) -> impl Iterator<Item = (&'a TraceSpan, bool)> + 'a {
        let mut skip = 0;
        self.spans.iter().filter_map(move |span| {
            if skip > 0 {
                skip -= 1;
                return None;
            }
            let folded = span.children > 0 && collapsed.contains(&span.span.span_id);
            if folded {
                skip = span.descendants;
            }
            Some((span, folded))
        })
    }
}

/// Which spans of a trace's waterfall are collapsed, as the `?collapsed=` parameter writes it:
/// `services` when it starts from "collapse to services" ([`Trace::service_entries`]), then the
/// ids of the spans toggled from there, comma-separated. Starting from a rule rather than
/// listing every span keeps the URL short; [`Trace::collapsed`] resolves it against a trace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Collapse {
    services: bool,
    toggled: Vec<SpanId>,
}

impl Collapse {
    /// The word for "collapse to services".
    pub const SERVICES: &str = "services";

    /// "Collapse to services", no span toggled.
    pub fn services() -> Self {
        Self {
            services: true,
            toggled: Vec::new(),
        }
    }

    /// Nothing collapsed: "expand all".
    pub fn is_empty(&self) -> bool {
        !self.services && self.toggled.is_empty()
    }

    /// This with `id` toggled: collapsed if it was expanded, and the other way round.
    pub fn toggle(&self, id: SpanId) -> Self {
        let mut toggled = self.toggled.clone();
        match toggled.iter().position(|t| *t == id) {
            Some(at) => {
                toggled.remove(at);
            }
            None => toggled.push(id),
        }
        Self {
            services: self.services,
            toggled,
        }
    }
}

impl FromStr for Collapse {
    type Err = UnknownLogText;

    /// Of a span id given twice, the first counts; empty items are skipped. At most
    /// [`Trace::MAX_SPANS`] ids are read, as a trace has no more spans.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut collapse = Self::default();
        for item in s.split(',').map(str::trim).filter(|i| !i.is_empty()) {
            if item == Self::SERVICES {
                collapse.services = true;
            } else {
                let id = item.parse::<SpanId>()?;
                if !collapse.toggled.contains(&id) && collapse.toggled.len() < Trace::MAX_SPANS {
                    collapse.toggled.push(id);
                }
            }
        }
        Ok(collapse)
    }
}

impl fmt::Display for Collapse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut items = self
            .services
            .then_some(Self::SERVICES.to_owned())
            .into_iter()
            .chain(self.toggled.iter().map(SpanId::to_string));
        if let Some(first) = items.next() {
            f.write_str(&first)?;
        }
        items.try_for_each(|item| write!(f, ",{item}"))
    }
}

/// The tree of a trace's spans, computed over their indices in start order.
struct Tree {
    /// Indices into the spans, in depth-first order.
    order: Vec<usize>,
    /// Per position in `order`.
    nodes: Vec<Node>,
}

#[derive(Default)]
struct Node {
    depth: usize,
    parent_missing: bool,
    /// Positions in the depth-first order, by start.
    children: Vec<usize>,
    descendants: usize,
    critical: bool,
    self_time: Duration,
}

impl Tree {
    /// `spans` must be sorted by start.
    fn of(spans: &[Span]) -> Self {
        let mut first = HashMap::with_capacity(spans.len());
        for (index, span) in spans.iter().enumerate() {
            first.entry(span.span_id).or_insert(index);
        }
        let parent: Vec<Option<usize>> = spans
            .iter()
            .enumerate()
            .map(|(index, span)| {
                let found = span.parent.and_then(|p| first.get(&p).copied());
                found.filter(|&p| p != index)
            })
            .collect();
        let mut missing: Vec<bool> = spans
            .iter()
            .zip(&parent)
            .map(|(span, parent)| span.parent.is_some() && parent.is_none())
            .collect();
        let first_root = spans.iter().position(|s| s.parent.is_none());

        // Children by start, a missing parent's spans under the first root.
        let mut kids: Vec<Vec<usize>> = vec![Vec::new(); spans.len()];
        let mut tops = Vec::new();
        for index in 0..spans.len() {
            match (parent[index], missing[index], first_root) {
                (Some(p), _, _) => kids[p].push(index),
                (None, true, Some(root)) => kids[root].push(index),
                _ => tops.push(index),
            }
        }

        // Depth first, without recursion; a span not reached from the tops (a cycle of
        // parents) is shown as a root with a missing parent.
        let mut visited = vec![false; spans.len()];
        let mut order = Vec::with_capacity(spans.len());
        let mut depth = Vec::with_capacity(spans.len());
        let mut position = vec![usize::MAX; spans.len()];
        let mut next = 0;
        loop {
            let start = match tops.get(next) {
                Some(&top) => {
                    next += 1;
                    top
                }
                None => match visited.iter().position(|v| !v) {
                    Some(stray) => {
                        missing[stray] = true;
                        stray
                    }
                    None => break,
                },
            };
            if visited[start] {
                continue;
            }
            let mut stack = vec![(start, 0)];
            while let Some((index, d)) = stack.pop() {
                if visited[index] {
                    continue;
                }
                visited[index] = true;
                position[index] = order.len();
                order.push(index);
                depth.push(d);
                stack.extend(kids[index].iter().rev().map(|&k| (k, d + 1)));
            }
        }

        let mut nodes: Vec<Node> = order
            .iter()
            .zip(&depth)
            .map(|(&index, &depth)| Node {
                depth,
                parent_missing: missing[index],
                ..Node::default()
            })
            .collect();
        // A child is the span placed right under its parent in the walk, at one more depth.
        let mut path: Vec<usize> = Vec::new();
        for (pos, node) in depth.iter().enumerate() {
            path.truncate(*node);
            if let Some(&up) = path.last() {
                nodes[up].children.push(pos);
            }
            path.push(pos);
        }
        for pos in (0..nodes.len()).rev() {
            let children = std::mem::take(&mut nodes[pos].children);
            nodes[pos].descendants = children.iter().map(|&c| 1 + nodes[c].descendants).sum();
            nodes[pos].children = children;
        }

        let span_at = |pos: usize| &spans[order[pos]];
        for (pos, node) in nodes.iter_mut().enumerate() {
            node.self_time =
                Self::self_time(span_at(pos), node.children.iter().map(|&c| span_at(c)));
        }
        for pos in 0..nodes.len() {
            if nodes[pos].depth != 0 {
                continue;
            }
            let mut on = Some(pos);
            while let Some(at) = on {
                nodes[at].critical = true;
                on = nodes[at].children.iter().copied().reduce(|best, c| {
                    if span_at(c).end() > span_at(best).end() {
                        c
                    } else {
                        best
                    }
                });
            }
        }
        Self { order, nodes }
    }

    /// `span`'s duration minus the union of its `children`'s intervals (by start), each cut to
    /// the span.
    fn self_time<'a>(span: &Span, children: impl Iterator<Item = &'a Span>) -> Duration {
        let (start, end) = (span.start.as_micros(), span.end().as_micros());
        let mut covered = 0;
        let mut reached = start;
        for child in children {
            let from = child.start.as_micros().max(reached);
            let to = child.end().as_micros().min(end);
            if to > from {
                covered += to - from;
                reached = to;
            }
        }
        let own = (end - start - covered).max(0);
        Duration::from_micros(own.unsigned_abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Timestamp;

    fn id(name: &str) -> SpanId {
        let mut bytes = [0; 8];
        bytes[..name.len()].copy_from_slice(name.as_bytes());
        SpanId::from(bytes)
    }

    /// A span of the mockup's sample traces, its start and duration in milliseconds after the
    /// trace's start.
    fn span(name: &str, parent: Option<&str>, service: &str, start: f64, ms: f64) -> Span {
        Span {
            trace_id: TraceId::from([0x4b; 16]),
            span_id: id(name),
            parent: parent.map(id),
            stack: "outline".into(),
            service: service.into(),
            name: name.into(),
            kind: SpanKind::Internal,
            start: MicroTimestamp::from_micros(1_000_000 + (start * 1000.0).round() as i64),
            duration: Duration::from_micros((ms * 1000.0).round() as u64),
            status: SpanStatus::Unset,
            status_message: None,
            attributes: None,
            resource: None,
            events: None,
            received: Timestamp::from_millis(0),
        }
    }

    /// `t1` of `docs/design/Telemetry.dc.html`, in the mockup's order.
    fn t1() -> Vec<Span> {
        let app = "outline-app";
        let mut spans = vec![
            span("s0", None, "hoster", 0.0, 1912.4),
            span("s1", Some("s0"), "hoster", 0.6, 1910.9),
            span("s2", Some("s1"), app, 3.8, 1905.2),
            span("s3", Some("s2"), app, 5.1, 17.6),
            span("s4", Some("s3"), app, 6.9, 1.8),
            span("s5", Some("s3"), app, 10.4, 9.2),
            span("s6", Some("s2"), app, 23.9, 1880.3),
            span("s12", Some("s6"), app, 24.6, 0.9),
            span("s7", Some("s6"), app, 26.2, 14.1),
            span("s8", Some("s6"), app, 41.0, 8.3),
            span("s9", Some("s6"), app, 51.7, 1800.4),
            span("s10", Some("s6"), app, 1853.6, 2.7),
        ];
        for error in [0, 1, 2, 6, 10] {
            spans[error].status = SpanStatus::Error;
        }
        // The mockup's kinds: the proxy's server and client spans, the app's server span, its
        // two handlers, and the calls to redis and postgres.
        for (at, kind) in [
            (0, SpanKind::Server),
            (1, SpanKind::Client),
            (2, SpanKind::Server),
        ] {
            spans[at].kind = kind;
        }
        for client in [4, 5, 7, 8, 9, 10, 11] {
            spans[client].kind = SpanKind::Client;
        }
        spans
    }

    fn names(spans: &[TraceSpan]) -> Vec<&str> {
        spans.iter().map(|s| s.span.name.as_str()).collect()
    }

    fn of<'a>(trace: &'a Trace, name: &str) -> &'a TraceSpan {
        trace.span(id(name)).unwrap()
    }

    #[test]
    fn the_sample_trace_is_in_depth_first_order_with_depths() {
        let mut spans = t1();
        spans.reverse();
        let trace = Trace::new(TraceId::from([0x4b; 16]), spans, 12);
        assert_eq!(
            names(trace.spans()),
            [
                "s0", "s1", "s2", "s3", "s4", "s5", "s6", "s12", "s7", "s8", "s9", "s10"
            ]
        );
        let depths: Vec<usize> = trace.spans().iter().map(|s| s.depth).collect();
        assert_eq!(depths, [0, 1, 2, 3, 4, 4, 3, 4, 4, 4, 4, 4]);
        assert_eq!(of(&trace, "s2").children, 2);
        assert_eq!(of(&trace, "s2").descendants, 9);
        assert_eq!(of(&trace, "s6").descendants, 5);
        assert_eq!(of(&trace, "s4").descendants, 0);
        assert!(trace.spans().iter().all(|s| !s.parent_missing));
        assert_eq!(trace.services(), ["hoster", "outline-app"]);
        assert!(trace.is_error() && !trace.is_truncated());
        assert_eq!(trace.duration(), Duration::from_micros(1_912_400));
        assert_eq!(
            trace.offset(of(&trace, "s9").span.start),
            Duration::from_micros(51_700)
        );
    }

    #[test]
    fn the_critical_path_follows_the_child_that_ends_last() {
        let trace = Trace::new(TraceId::from([0x4b; 16]), t1(), 12);
        // s6 ends at 1904.2 ms, after s3 (22.7); of s6's children s10 ends last (1856.3), just
        // after s9 (1852.1).
        assert_eq!(
            names(&trace.critical_path().cloned().collect::<Vec<_>>()),
            ["s0", "s1", "s2", "s6", "s10"]
        );
    }

    #[test]
    fn collapse_to_services_collapses_the_server_spans_with_children_and_a_parent() {
        let trace = Trace::new(TraceId::from([0x4b; 16]), t1(), 12);
        // s0 is a server span without a parent; s2, the app's, is the only one below it.
        assert_eq!(
            names(&trace.service_entries().cloned().collect::<Vec<_>>()),
            ["s2"]
        );
        let collapsed = trace.collapsed(&Collapse::services());
        assert_eq!(collapsed, HashSet::from([id("s2")]));
        let shown: Vec<(&str, bool)> = trace
            .shown(&collapsed)
            .map(|(s, folded)| (s.span.name.as_str(), folded))
            .collect();
        assert_eq!(shown, [("s0", false), ("s1", false), ("s2", true)]);
        assert_eq!(of(&trace, "s2").descendants, 9, "the +N of s2");
    }

    #[test]
    fn toggled_spans_flip_the_rule_and_hide_their_subtrees() {
        let trace = Trace::new(TraceId::from([0x4b; 16]), t1(), 12);
        let collapse = Collapse::default().toggle(id("s6")).toggle(id("s3"));
        let collapsed = trace.collapsed(&collapse);
        assert_eq!(collapsed, HashSet::from([id("s3"), id("s6")]));
        let shown: Vec<&str> = trace
            .shown(&collapsed)
            .map(|(s, _)| s.span.name.as_str())
            .collect();
        assert_eq!(shown, ["s0", "s1", "s2", "s3", "s6"]);
        // From the rule, s2 toggled open again and s6 collapsed under it.
        let collapse = Collapse::services().toggle(id("s2")).toggle(id("s6"));
        assert_eq!(
            trace.collapsed(&collapse),
            HashSet::from([id("s6")]),
            "s2 is expanded"
        );
        // A collapsed span inside a collapsed one stays collapsed, hidden.
        let collapse = Collapse::services().toggle(id("s6"));
        let collapsed = trace.collapsed(&collapse);
        assert_eq!(trace.shown(&collapsed).count(), 3);
        // Toggling twice is no toggle; a leaf or a span of another trace collapses nothing.
        assert!(collapse.toggle(id("s6")).toggle(id("s6")) == collapse);
        let stray = Collapse::default().toggle(id("s4")).toggle(id("nope"));
        assert!(trace.collapsed(&stray).is_empty());
        assert_eq!(trace.shown(&trace.collapsed(&stray)).count(), 12);
    }

    #[test]
    fn a_collapse_is_written_as_the_rule_and_its_toggles() {
        let trace = Trace::new(TraceId::from([0x4b; 16]), t1(), 12);
        let collapse = Collapse::services().toggle(id("s6")).toggle(id("s3"));
        let text = collapse.to_string();
        assert_eq!(text, format!("services,{},{}", id("s6"), id("s3")));
        assert_eq!(text.parse::<Collapse>().unwrap(), collapse);
        assert_eq!(Collapse::default().to_string(), "");
        assert!("".parse::<Collapse>().unwrap().is_empty());
        assert!(!Collapse::services().is_empty());
        // Canonical: the spans with children, each once, in depth-first order.
        let messy: Collapse = format!("{0},,{1}, {0},{2}", id("s6"), id("s4"), id("s3"))
            .parse()
            .unwrap();
        assert_eq!(
            trace.canonical(&messy).to_string(),
            format!("{},{}", id("s3"), id("s6"))
        );
        assert!("services,nope".parse::<Collapse>().is_err());
    }

    #[test]
    fn self_time_is_the_duration_minus_the_union_of_the_children() {
        let trace = Trace::new(TraceId::from([0x4b; 16]), t1(), 12);
        let self_us = |name| of(&trace, name).self_time.as_micros();
        // 1880.3 − (0.9 + 14.1 + 8.3 + 1800.4 + 2.7)
        assert_eq!(self_us("s6"), 53_900);
        // 1905.2 − (17.6 + 1880.3)
        assert_eq!(self_us("s2"), 7_300);
        // 1912.4 − 1910.9, and 1910.9 − 1905.2
        assert_eq!(self_us("s0"), 1_500);
        assert_eq!(self_us("s1"), 5_700);
        assert_eq!(self_us("s9"), 1_800_400);
    }

    #[test]
    fn overlapping_and_overhanging_children_are_counted_once_within_the_parent() {
        let spans = vec![
            span("p", None, "a", 0.0, 10.0),
            span("x", Some("p"), "a", 2.0, 4.0),
            span("y", Some("p"), "a", 3.0, 2.0),
            span("z", Some("p"), "a", 8.0, 5.0),
        ];
        let trace = Trace::new(TraceId::from([1; 16]), spans, 4);
        // Covered: 2–6 and 8–10 (z cut at the parent's end).
        assert_eq!(of(&trace, "p").self_time, Duration::from_millis(4));
    }

    #[test]
    fn spans_with_a_missing_parent_go_under_the_first_root_and_are_marked() {
        let spans = vec![
            span("late", Some("lost"), "a", 5.0, 1.0),
            span("root", None, "a", 0.0, 10.0),
            span("second", None, "b", 20.0, 1.0),
            span("child", Some("root"), "a", 1.0, 2.0),
        ];
        let trace = Trace::new(TraceId::from([1; 16]), spans, 4);
        assert_eq!(names(trace.spans()), ["root", "child", "late", "second"]);
        let late = of(&trace, "late");
        assert!(late.parent_missing && late.depth == 1);
        assert!(!of(&trace, "child").parent_missing);
        assert_eq!(of(&trace, "second").depth, 0);
        assert!(
            of(&trace, "second").critical,
            "every root starts a critical path"
        );
    }

    #[test]
    fn without_a_root_spans_with_missing_parents_are_roots_and_cycles_end() {
        let spans = vec![
            span("a", Some("gone"), "x", 0.0, 1.0),
            span("b", Some("c"), "x", 1.0, 1.0),
            span("c", Some("b"), "x", 2.0, 1.0),
            span("d", Some("d"), "x", 3.0, 1.0),
        ];
        let trace = Trace::new(TraceId::from([1; 16]), spans, 9);
        assert_eq!(trace.spans().len(), 4);
        assert!(trace.is_truncated() && trace.total_spans() == 9);
        // A span that is its own parent has none; a cycle is entered at its earliest span,
        // after the spans whose parents are simply missing.
        assert_eq!(names(trace.spans()), ["a", "d", "b", "c"]);
        let depths: Vec<usize> = trace.spans().iter().map(|s| s.depth).collect();
        assert_eq!(depths, [0, 0, 0, 1]);
        assert!(of(&trace, "a").parent_missing && of(&trace, "b").parent_missing);
        assert!(of(&trace, "d").parent_missing);
    }

    fn row() -> RequestRow {
        RequestRow {
            time: Timestamp::from_millis(1_700_000_000_000),
            host: "wiki.example.net".into(),
            prefix: "/".into(),
            stack: "outline".into(),
            service: "app".into(),
            method: "POST".into(),
            path: "/api/documents.info".into(),
            status: 500,
            to_header: Some(Duration::from_millis(1_900)),
            total: Duration::from_millis(1_912),
            bytes: 10,
            client: std::net::IpAddr::from([192, 168, 1, 34]),
            upstream_failed: false,
            request_id: TraceId::from([0x4b; 16]),
            server_span: Some(id("srv")),
            client_span: Some(id("cli")),
            upstream_start: Some(
                MicroTimestamp::from(Timestamp::from_millis(1_700_000_000_000))
                    + Duration::from_micros(600),
            ),
        }
    }

    #[test]
    fn a_request_row_gives_the_proxys_two_spans() {
        let spans = row().proxy_spans();
        assert_eq!(spans.len(), 2);
        let (server, client) = (&spans[0], &spans[1]);
        assert_eq!(server.trace_id, TraceId::from([0x4b; 16]));
        assert_eq!((server.span_id, server.parent), (id("srv"), None));
        assert_eq!(
            (server.kind, server.status),
            (SpanKind::Server, SpanStatus::Error)
        );
        assert_eq!(server.name, "POST /api/documents.info");
        assert_eq!(server.service, Span::PROXY_SERVICE);
        assert_eq!(server.duration, Duration::from_millis(1_912));
        assert_eq!(
            (client.span_id, client.parent),
            (id("cli"), Some(id("srv")))
        );
        assert_eq!(client.kind, SpanKind::Client);
        assert_eq!(client.duration, Duration::from_micros(1_911_400));
        assert_eq!(client.end(), server.end());
        let attributes: serde_json::Value =
            serde_json::from_str(server.attributes.as_deref().unwrap()).unwrap();
        assert_eq!(attributes["http.response.status_code"], 500);
        assert_eq!(attributes["server.address"], "wiki.example.net");
    }

    #[test]
    fn a_row_without_an_upstream_request_or_span_ids_gives_fewer_spans() {
        let mut answered_by_hoster = row();
        answered_by_hoster.status = 200;
        (
            answered_by_hoster.client_span,
            answered_by_hoster.upstream_start,
        ) = (None, None);
        let spans = answered_by_hoster.proxy_spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].status, SpanStatus::Unset);

        let mut older = row();
        older.server_span = None;
        assert!(older.proxy_spans().is_empty());
    }
}
