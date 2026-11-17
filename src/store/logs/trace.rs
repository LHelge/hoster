//! One trace assembled for the waterfall (ADR-0069 §6, §9): its spans as a tree in depth-first
//! order, with the figures the Traces tab draws from it, so templates only lay rows out.
//!
//! The spans come from two places behind the one [`Span`] type: the rows containers exported
//! over OTLP, and the proxy's two spans of a request, which are never stored as spans but built
//! from the request row ([`RequestRow::edge_spans`]). Nothing past this module can tell them
//! apart except by [`Span::EDGE_SERVICE`].
//!
//! [`Trace::new`] puts any set of spans of a trace in order:
//!
//! - **Order.** Roots by start, then each span's children by start (ties by span id), depth
//!   first.
//! - **Lost and late spans.** A span whose parent is not in the trace (lost, not yet received,
//!   cut by retention or past [`Trace::SPAN_CAP`]) is marked [`TraceSpan::parent_missing`] and
//!   shown under the first root, among its children by start. A trace with no root at all has
//!   each such span as a root of its own, so it can have several.
//! - **Malformed parents.** A span that is its own ancestor (a cycle no exporter should send)
//!   is treated as one whose parent is missing, the earliest of the cycle first. Of two spans
//!   with one id, children hang under the earlier.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::{MicroTimestamp, RequestRow, Span, SpanId, SpanKind, SpanStatus, TraceId};

impl Span {
    /// The service of the proxy's own spans, as hoster's own log records name it (ADR-0068),
    /// so a request's records made in the proxy belong to the same service as its spans.
    pub const EDGE_SERVICE: &str = "hoster";

    /// When it ended: its start plus its duration.
    pub fn end(&self) -> MicroTimestamp {
        self.start + self.duration
    }

    /// Whether it ended in error.
    pub fn is_error(&self) -> bool {
        self.status == SpanStatus::Error
    }
}

impl RequestRow {
    /// The proxy's two spans of this request (ADR-0069 §9), as a trace read shows them: a
    /// server span for the request, from its arrival for its whole duration, and under it a
    /// client span for the upstream request, from the upstream start to the end of the request.
    /// Both are in error when the upstream failed or the answer was a 5xx; the client span is
    /// missing when no upstream request started (hoster answered itself, or the connection
    /// failed first). A row written before the span ids were stored, or one whose request had
    /// no trace, has no spans.
    pub fn edge_spans(&self) -> Vec<Span> {
        let Some(server_span) = self.server_span else {
            return Vec::new();
        };
        let start = MicroTimestamp::from(self.time);
        let end = start + self.total;
        let (status, status_message) = if self.upstream_failed {
            (
                SpanStatus::Error,
                Some("upstream request failed".to_owned()),
            )
        } else if self.status >= 500 {
            (
                SpanStatus::Error,
                Some(format!("responded {}", self.status)),
            )
        } else {
            (SpanStatus::Unset, None)
        };
        let resource = json!({ "service.name": Span::EDGE_SERVICE }).to_string();
        let span = |span_id, parent, name, kind, start: MicroTimestamp, attributes: Value| Span {
            trace_id: self.request_id,
            span_id,
            parent,
            stack: self.stack.clone(),
            service: Span::EDGE_SERVICE.to_owned(),
            name,
            kind,
            start,
            duration: Duration::from_micros(
                u64::try_from(end.as_micros() - start.as_micros()).unwrap_or(0),
            ),
            status,
            status_message: status_message.clone(),
            attributes: Some(attributes.to_string()),
            resource: Some(resource.clone()),
            events: None,
            received: self.time,
        };

        let mut server = Map::new();
        server.insert("http.request.method".into(), self.method.clone().into());
        server.insert("url.path".into(), self.path.clone().into());
        server.insert("server.address".into(), self.host.clone().into());
        if self.status != 0 {
            server.insert("http.response.status_code".into(), self.status.into());
        }
        server.insert("http.response.body.size".into(), self.bytes.into());
        server.insert("client.address".into(), self.client.to_string().into());
        server.insert("hoster.route.prefix".into(), self.prefix.clone().into());
        let mut spans = vec![span(
            server_span,
            None,
            format!("{} {}", self.method, self.path),
            SpanKind::Server,
            start,
            Value::Object(server),
        )];
        if let (Some(client_span), Some(upstream_start)) = (self.client_span, self.upstream_start) {
            let mut client = Map::new();
            client.insert("server.address".into(), self.service.clone().into());
            client.insert("hoster.stack".into(), self.stack.clone().into());
            client.insert("hoster.service".into(), self.service.clone().into());
            if self.status != 0 {
                client.insert("http.response.status_code".into(), self.status.into());
            }
            spans.push(span(
                client_span,
                Some(server_span),
                format!("upstream {}", self.service),
                SpanKind::Client,
                upstream_start.min(end),
                Value::Object(client),
            ));
        }
        spans
    }
}

/// A trace's spans as a tree, in depth-first order: see the [module](self).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    id: TraceId,
    /// Depth-first order.
    spans: Vec<TraceSpan>,
    /// Every span the trace has, those past the cap included.
    total: u64,
}

/// One span of a [`Trace`], with its place in the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceSpan {
    span: Span,
    depth: usize,
    /// The row it is drawn under, as an index into [`Trace::spans`]; `None` for a root.
    parent: Option<usize>,
    /// Its rows, as indices into [`Trace::spans`], by start.
    children: Vec<usize>,
    descendants: usize,
    parent_missing: bool,
    critical: bool,
    self_time: Duration,
}

impl TraceSpan {
    /// The span itself.
    pub fn span(&self) -> &Span {
        &self.span
    }

    /// How far below its root it is drawn: 0 for a root.
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// It names a parent the trace does not have (lost, late, or past the cap), and is drawn
    /// under the first root instead, or as a root when the trace has none.
    pub fn parent_missing(&self) -> bool {
        self.parent_missing
    }

    /// It is on the critical path: from a root, recursively the child that ended last.
    pub fn on_critical_path(&self) -> bool {
        self.critical
    }

    /// Its duration minus the union of its children's intervals, cut to its own: the time it
    /// spent itself.
    pub fn self_time(&self) -> Duration {
        self.self_time
    }

    /// How many rows are drawn under it, all the way down: what collapsing it hides.
    pub fn descendants(&self) -> usize {
        self.descendants
    }

    /// Whether it has rows under it, so it can be collapsed.
    pub fn has_children(&self) -> bool {
        !self.children.is_empty()
    }
}

impl Trace {
    /// The most spans one trace read holds; [`Trace::total_spans`] says how many it has.
    pub const SPAN_CAP: usize = 1000;

    /// The trace `id` of `spans`, given in any order, of which there are `total` (at least as
    /// many as given: more when the read was cut at [`Trace::SPAN_CAP`]).
    pub fn new(id: TraceId, mut spans: Vec<Span>, total: u64) -> Self {
        spans.sort_by(|a, b| (a.start, a.span_id.as_ref()).cmp(&(b.start, b.span_id.as_ref())));
        let tree = Tree::new(&spans);
        let order = tree.depth_first();

        let mut at = vec![0; spans.len()];
        for (position, &(index, _)) in order.iter().enumerate() {
            at[index] = position;
        }
        let mut slots: Vec<Option<Span>> = spans.into_iter().map(Some).collect();
        let mut rows: Vec<TraceSpan> = order
            .iter()
            .map(|&(index, depth)| TraceSpan {
                span: slots[index].take().expect("each span is visited once"),
                depth,
                parent: tree.parent[index].map(|p| at[p]),
                children: tree.children[index].iter().map(|&c| at[c]).collect(),
                descendants: 0,
                parent_missing: tree.missing[index],
                critical: false,
                self_time: Duration::ZERO,
            })
            .collect();

        // Children follow their parent in depth-first order, so one pass from the end sums
        // every subtree.
        for position in (0..rows.len()).rev() {
            if let Some(parent) = rows[position].parent {
                rows[parent].descendants += rows[position].descendants + 1;
            }
        }
        for position in 0..rows.len() {
            rows[position].self_time = Self::self_time_of(&rows, position);
        }
        let roots: Vec<usize> = (0..rows.len())
            .filter(|&p| rows[p].parent.is_none())
            .collect();
        for root in roots {
            let mut on = root;
            loop {
                rows[on].critical = true;
                // The child that ended last; of equal ends, the first.
                let next = rows[on].children.iter().copied().reduce(|best, c| {
                    if rows[c].span.end() > rows[best].span.end() {
                        c
                    } else {
                        best
                    }
                });
                match next {
                    Some(child) => on = child,
                    None => break,
                }
            }
        }

        let total = total.max(u64::try_from(rows.len()).unwrap_or(u64::MAX));
        Self {
            id,
            spans: rows,
            total,
        }
    }

    fn self_time_of(rows: &[TraceSpan], position: usize) -> Duration {
        let span = &rows[position].span;
        let (start, end) = (span.start.as_micros(), span.end().as_micros());
        let mut covered = 0;
        let mut reached = start;
        // Children are by start, so the union is one sweep.
        for &child in &rows[position].children {
            let child = &rows[child].span;
            let from = child.start.as_micros().max(reached);
            let to = child.end().as_micros().min(end);
            if to > from {
                covered += to - from;
                reached = to;
            }
        }
        let own = u64::try_from(end - start - covered).unwrap_or(0);
        Duration::from_micros(own)
    }

    /// The trace's id.
    pub fn id(&self) -> TraceId {
        self.id
    }

    /// Every span read, in depth-first order: the waterfall's rows.
    pub fn spans(&self) -> &[TraceSpan] {
        &self.spans
    }

    /// How many spans the trace has: more than [`Trace::spans`] holds when the read stopped at
    /// [`Trace::SPAN_CAP`].
    pub fn total_spans(&self) -> u64 {
        self.total
    }

    /// The read stopped at [`Trace::SPAN_CAP`]: there are spans it does not hold.
    pub fn is_truncated(&self) -> bool {
        self.total > u64::try_from(self.spans.len()).unwrap_or(u64::MAX)
    }

    /// Nothing was read.
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// The first row: the earliest root.
    pub fn root(&self) -> Option<&TraceSpan> {
        self.spans.first()
    }

    /// The span `id`, the first of two with one id.
    pub fn span(&self, id: SpanId) -> Option<&TraceSpan> {
        self.spans.iter().find(|s| s.span.span_id == id)
    }

    /// The row `span` is drawn under; `None` for a root.
    pub fn parent_of(&self, span: &TraceSpan) -> Option<&TraceSpan> {
        span.parent.map(|p| &self.spans[p])
    }

    /// The rows drawn right under `span`, by start.
    pub fn children_of<'a>(&'a self, span: &'a TraceSpan) -> impl Iterator<Item = &'a TraceSpan> {
        span.children.iter().map(|&c| &self.spans[c])
    }

    /// When the earliest span started: the waterfall's zero.
    pub fn start(&self) -> Option<MicroTimestamp> {
        self.spans.iter().map(|s| s.span.start).min()
    }

    /// When the last span ended.
    pub fn end(&self) -> Option<MicroTimestamp> {
        self.spans.iter().map(|s| s.span.end()).max()
    }

    /// From the earliest start to the latest end: the waterfall's axis.
    pub fn duration(&self) -> Duration {
        match (self.start(), self.end()) {
            (Some(start), Some(end)) => Duration::from_micros(
                u64::try_from(end.as_micros() - start.as_micros()).unwrap_or(0),
            ),
            _ => Duration::ZERO,
        }
    }

    /// How long after the trace's start `span` started: where its bar begins.
    pub fn offset(&self, span: &TraceSpan) -> Duration {
        let start = self.start().map_or(0, MicroTimestamp::as_micros);
        Duration::from_micros(u64::try_from(span.span.start.as_micros() - start).unwrap_or(0))
    }

    /// How many levels the tree has: 1 for a trace of roots only, 0 for an empty one.
    pub fn depth(&self) -> usize {
        self.spans.iter().map(|s| s.depth + 1).max().unwrap_or(0)
    }

    /// How many spans ended in error.
    pub fn errors(&self) -> usize {
        self.spans.iter().filter(|s| s.span.is_error()).count()
    }

    /// Whether any span ended in error.
    pub fn is_error(&self) -> bool {
        self.errors() > 0
    }

    /// Each service with its number of spans, in the order the rows first show it: the legend.
    pub fn services(&self) -> Vec<(&str, usize)> {
        let mut services: Vec<(&str, usize)> = Vec::new();
        for span in &self.spans {
            let service = span.span.service.as_str();
            match services.iter_mut().find(|(s, _)| *s == service) {
                Some((_, count)) => *count += 1,
                None => services.push((service, 1)),
            }
        }
        services
    }

    /// The span a trace opens on: the first span in error, in row order, without a child in
    /// error (the end of the first error chain, where it went wrong); the root when nothing
    /// failed.
    pub fn default_span(&self) -> Option<&TraceSpan> {
        self.spans
            .iter()
            .find(|s| s.span.is_error() && !self.children_of(s).any(|c| c.span.is_error()))
            .or_else(|| self.root())
    }

    /// What "collapse to services" collapses: every server span below a root that has rows
    /// under it, so each service's work shows as its entry span.
    pub fn service_entries(&self) -> HashSet<SpanId> {
        self.spans
            .iter()
            .filter(|s| s.span.kind == SpanKind::Server && s.parent.is_some() && s.has_children())
            .map(|s| s.span.span_id)
            .collect()
    }

    /// The rows shown when the spans in `collapsed` are collapsed: everything but the rows
    /// under a collapsed span. A collapsed span without children hides nothing.
    pub fn visible<'a>(&'a self, collapsed: &HashSet<SpanId>) -> Vec<&'a TraceSpan> {
        let mut shown = Vec::with_capacity(self.spans.len());
        let mut hiding_below: Option<usize> = None;
        for span in &self.spans {
            if let Some(depth) = hiding_below {
                if span.depth > depth {
                    continue;
                }
                hiding_below = None;
            }
            shown.push(span);
            if span.has_children() && collapsed.contains(&span.span.span_id) {
                hiding_below = Some(span.depth);
            }
        }
        shown
    }
}

/// The parent links of a trace's spans, by index into the spans sorted by start, with lost and
/// malformed parents resolved.
struct Tree {
    /// The span each one is drawn under.
    parent: Vec<Option<usize>>,
    /// Each one's children, by start.
    children: Vec<Vec<usize>>,
    /// The roots, by start.
    roots: Vec<usize>,
    /// Its parent is missing.
    missing: Vec<bool>,
}

impl Tree {
    /// `spans` are sorted by start, then span id.
    fn new(spans: &[Span]) -> Self {
        let n = spans.len();
        let mut index: HashMap<SpanId, usize> = HashMap::with_capacity(n);
        for (i, span) in spans.iter().enumerate() {
            index.entry(span.span_id).or_insert(i);
        }
        let mut tree = Self {
            parent: vec![None; n],
            children: vec![Vec::new(); n],
            roots: Vec::new(),
            missing: vec![false; n],
        };
        for (i, span) in spans.iter().enumerate() {
            match span.parent.map(|p| index.get(&p).copied()) {
                None => tree.roots.push(i),
                Some(Some(p)) if p != i => {
                    tree.parent[i] = Some(p);
                    tree.children[p].push(i);
                }
                Some(_) => tree.missing[i] = true,
            }
        }

        // What the roots and the spans with a missing parent do not reach hangs in a cycle:
        // the earliest span of each is cut from its parent.
        let mut reached = vec![false; n];
        let starts: Vec<usize> = (0..n).filter(|&i| tree.parent[i].is_none()).collect();
        for start in starts {
            tree.reach(start, &mut reached);
        }
        for i in 0..n {
            if !reached[i] {
                if let Some(p) = tree.parent[i].take() {
                    tree.children[p].retain(|&c| c != i);
                }
                tree.missing[i] = true;
                tree.reach(i, &mut reached);
            }
        }

        // Spans with a missing parent go under the first root, or are roots themselves.
        let orphans = (0..n).filter(|&i| tree.missing[i]);
        match tree.roots.first().copied() {
            Some(root) => {
                for orphan in orphans.collect::<Vec<_>>() {
                    tree.parent[orphan] = Some(root);
                    tree.children[root].push(orphan);
                }
                // Indices follow the start order.
                tree.children[root].sort_unstable();
            }
            None => tree.roots = orphans.collect(),
        }
        tree
    }

    /// Marks `from` and everything under it.
    fn reach(&self, from: usize, reached: &mut [bool]) {
        let mut stack = vec![from];
        while let Some(i) = stack.pop() {
            if !std::mem::replace(&mut reached[i], true) {
                stack.extend(self.children[i].iter().copied());
            }
        }
    }

    /// Every span with its depth, roots by start and children by start, depth first.
    fn depth_first(&self) -> Vec<(usize, usize)> {
        let mut order = Vec::with_capacity(self.parent.len());
        let mut stack: Vec<(usize, usize)> = self.roots.iter().rev().map(|&r| (r, 0)).collect();
        while let Some((i, depth)) = stack.pop() {
            order.push((i, depth));
            stack.extend(self.children[i].iter().rev().map(|&c| (c, depth + 1)));
        }
        order
    }
}

#[cfg(test)]
mod tests;
