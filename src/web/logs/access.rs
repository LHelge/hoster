//! The stack logs tab's `access` source (task bq6, ADR-0067 §5): the proxy's request rows of
//! the stack's routes in the log store, oldest first, read with [`RequestQuery`] beside the
//! records query, with the same range, paging by `(time, id)` and scan cap. A row is the time,
//! the service, the method, the path (cut at [`AccessLog::PATH_CAP`] characters when closed),
//! the status toned by class as the traffic charts are (ADR-0062), the total duration as the
//! stack overview writes latencies, the response bytes and the client address. Every row opens,
//! a `<details>` as a record with fields does, on the host, the route's prefix, the whole path,
//! the time to the response header, whether the upstream failed and the request id, which links
//! the containers' records that mention it (grep, as no field name for it is fixed). A row the
//! proxy sent trace context for (ADR-0069 §8, §9: its span ids are stored) ends in its trace,
//! the request id, linking to the Traces tab, which draws it from the row for as long as the
//! row is kept; an older row, from before those columns, has `—` there and no link.
//!
//! Rows are kept by stack and service as the proxy stored them, without a join to the routes,
//! so a route that moved to another stack keeps its old rows here.
//!
//! It takes the service chips, a status filter (`status`: `4xx`, `5xx` or one code, chips for
//! the first two), grep on the path, a minimum total duration (`slower`, e.g. `500ms`), the
//! range, paging and following, but no level or field filters. The footer counts the range's
//! requests of the service picked, and how many were 4xx and 5xx ([`RequestFoot`]).
//!
//! Following ([`AccessTail`]) reads the rows committed after the page's newest, by row id, up
//! to the recorder's announced request head, in the bounded windows of
//! [`LogStore::requests_after`], as the containers' stream reads records.

use std::fmt;
use std::net::IpAddr;
use std::time::{Duration, SystemTime};

use tokio::sync::watch;

use super::{
    Chip, Download, EmptyNote, Links, LogFollow, LogMount, LogParams, LogReader, LogScope,
    LogSource, LogsView, SourceEntry, SourcePlan, SourceRead, SourceTail, TraceCell,
};
use crate::stats::{Elapsed, StatusClass};
use crate::store::{
    LogHead, LogStore, Page, RecordKey, RequestCounts, RequestEntry, RequestQuery, StatusFilter,
    Timestamp,
};
use crate::web::containers::LogEntry as LiveLine;
use crate::web::ui::Count;

/// The cells of one request row, beside the time, service and path its [`LogRow`] has.
///
/// [`LogRow`]: super::LogRow
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestCells {
    pub method: String,
    /// The status code; `-` for a request whose response never went out.
    pub status: String,
    /// The status's colour class, by class as the traffic charts tone them.
    pub tone: &'static str,
    /// The total duration.
    pub total: String,
    /// The response body bytes.
    pub bytes: String,
    pub client: String,
    pub host: String,
    /// The matched route's prefix.
    pub prefix: String,
    /// The whole path, printable; the row shows it cut when longer than
    /// [`AccessLog::PATH_CAP`].
    pub path: String,
    /// The row's path was cut.
    pub path_cut: bool,
    /// The time to the response header, `-` when none came.
    pub to_header: String,
    pub upstream_failed: bool,
    pub request_id: String,
    /// The containers' records that mention the request id.
    pub records: String,
}

/// The access log's footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestFoot {
    /// The requests of the range (of the service picked).
    pub rows: Count,
    pub client_errors: Count,
    pub server_errors: Count,
}

impl From<RequestCounts> for RequestFoot {
    fn from(counts: RequestCounts) -> Self {
        Self {
            rows: Count(counts.rows),
            client_errors: Count(counts.client_errors),
            server_errors: Count(counts.server_errors),
        }
    }
}

/// A byte count as a row shows it: `512 B`, `1.5 KiB`, `12.0 MiB`.
struct Size(u64);

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
        if self.0 < 1024 {
            return write!(f, "{} B", self.0);
        }
        // The cast loses nothing a row would show.
        let mut value = self.0 as f64 / 1024.0;
        let mut unit = 0;
        while value >= 1024.0 && unit < UNITS.len() - 1 {
            value /= 1024.0;
            unit += 1;
        }
        write!(f, "{value:.1} {}", UNITS[unit])
    }
}

impl LogParams {
    /// The status filter; every status when `status` is absent or does not parse, with why.
    fn status(&self) -> (Option<StatusFilter>, Option<String>) {
        match Self::text(&self.status).map(|s| (s, s.parse::<StatusFilter>())) {
            None => (None, None),
            Some((_, Ok(filter))) => (Some(filter), None),
            Some((given, Err(_))) => (
                None,
                Some(format!(
                    "`{given}` is not a status filter: use 4xx, 5xx or a code such as 404; \
                     showing every status."
                )),
            ),
        }
    }

    /// The minimum total duration; none when `slower` is absent or does not parse, with why.
    fn slower(&self) -> (Option<Duration>, Option<String>) {
        match Self::text(&self.slower).map(|s| (s, humantime::parse_duration(s))) {
            None => (None, None),
            Some((_, Ok(min))) => (Some(min), None),
            Some((given, Err(_))) => (
                None,
                Some(format!(
                    "`{given}` is not a duration such as 500ms; showing requests of any duration."
                )),
            ),
        }
    }
}

/// The store query one page of the access log runs, and what was wrong with its options.
pub(super) struct AccessPlan {
    pub(super) query: RequestQuery,
    /// The shared options, with every problem of the access log's own folded in.
    pub(super) shared: SourcePlan,
}

impl AccessPlan {
    /// What `params` (scoped) ask for in `scope` at `now`.
    pub(super) fn new(scope: &LogScope, params: &LogParams, now: SystemTime) -> Self {
        let mut shared = SourcePlan::new(params, now);
        let (status, status_problem) = params.status();
        let (min_total, slower_problem) = params.slower();
        let mut query = RequestQuery::new(shared.range.clone(), scope.store());
        query.services = params.service().map(str::to_owned).into_iter().collect();
        query.status = status;
        query.min_total = min_total;
        query.path = shared.text.clone();
        query.page = shared.page;
        query.limit = shared.limit;
        shared.problem = shared.problem.take().or(status_problem).or(slower_problem);
        Self { query, shared }
    }
}

impl LogReader {
    /// One page of the access log and the counts of its range, or why the store cannot be
    /// read. No rows are read for a grep pattern that does not compile.
    async fn requests(
        &self,
        plan: &AccessPlan,
    ) -> Result<(Page<RequestEntry>, RequestFoot), String> {
        let store = self.store()?;
        let page = async {
            match plan.shared.reads() {
                true => store.requests(&plan.query).await,
                false => Ok(Page::default()),
            }
        };
        let read = async { tokio::try_join!(page, store.request_counts(&plan.query)) };
        match read.await {
            Ok((page, counts)) => Ok((page, counts.into())),
            Err(err) => {
                tracing::error!(error = %err, "reading the access log failed");
                Err(format!(
                    "Cannot read the log store: {:#}",
                    anyhow::Error::from(err)
                ))
            }
        }
    }
}

/// How request rows become the view's entries: on a page and in its follow stream.
#[derive(Debug, Clone)]
struct AccessLog {
    /// The services in chip order, which picks their colours.
    services: Vec<String>,
    /// The containers' view with the page's options it takes, which a request id links.
    records: Links,
}

impl AccessLog {
    /// The most characters of a path a closed row shows.
    const PATH_CAP: usize = 160;

    /// The rows of the view at `mount` with the options of `params`, for `services`.
    fn new(mount: &LogMount, params: &LogParams, services: Vec<String>) -> Self {
        // The options of the access log the containers' view takes too, as the switch keeps
        // them.
        let mut filters = Links::of(&mount.action, &mount.scope, params, LogSource::Access).filters;
        filters.retain(|(key, _)| *key != "source" && LogSource::Containers.takes(key));
        Self {
            services,
            records: Links {
                action: mount.action.clone(),
                filters,
            },
        }
    }

    /// The services that get a chip: `known` in order, then those on the page and the one
    /// picked, by name.
    fn services(known: &[String], page: &[RequestEntry], picked: Option<&str>) -> Vec<String> {
        let mut others: Vec<&str> = page
            .iter()
            .map(|e| e.request.service.as_str())
            .chain(picked)
            .filter(|s| !known.iter().any(|k| k == s))
            .collect();
        others.sort_unstable();
        others.dedup();
        known
            .iter()
            .cloned()
            .chain(others.into_iter().map(str::to_owned))
            .collect()
    }

    /// The colour of `service`: its place among the chips.
    fn tone(&self, service: &str) -> usize {
        self.services
            .iter()
            .position(|s| s == service)
            .unwrap_or_default()
    }

    /// The status's colour class: the traffic charts' class colours.
    fn status_tone(status: u16) -> &'static str {
        let status = (status != 0).then_some(status);
        match StatusClass::from(status) {
            StatusClass::Success => "req-2xx",
            StatusClass::Redirection => "req-3xx",
            StatusClass::ClientError => "req-4xx",
            StatusClass::ServerError => "req-5xx",
            StatusClass::Informational | StatusClass::NoResponse => "req-other",
        }
    }

    /// `entry` as the view's entry: the path, cut, as its message.
    fn entry(&self, entry: &RequestEntry) -> SourceEntry {
        let request = &entry.request;
        let path = LiveLine::printable(&request.path);
        let path_cut = path.chars().count() > Self::PATH_CAP;
        let shown: String = path.chars().take(Self::PATH_CAP).collect();
        let request_id = request.request_id.to_string();
        let client = match request.client {
            IpAddr::V6(v6) => v6
                .to_ipv4_mapped()
                .map_or_else(|| v6.to_string(), |v4| v4.to_string()),
            v4 => v4.to_string(),
        };
        let records =
            self.records
                .to(&[("service", None), ("q", Some(&request_id)), ("regex", None)]);
        SourceEntry {
            key: entry.key(),
            label: request.service.clone(),
            href: None,
            tone: self.tone(&request.service),
            level: None,
            step: None,
            message: shown,
            // The trace opens on its focus: the deepest span in error, or the proxy's.
            trace: Some(match request.server_span {
                Some(_) => TraceCell::linked(request.request_id, None),
                None => TraceCell::default(),
            }),
            request: Some(RequestCells {
                method: LiveLine::printable(&request.method),
                status: match request.status {
                    0 => "-".to_owned(),
                    code => code.to_string(),
                },
                tone: Self::status_tone(request.status),
                total: Elapsed(request.total).to_string(),
                bytes: Size(request.bytes).to_string(),
                client,
                host: LiveLine::printable(&request.host),
                prefix: LiveLine::printable(&request.prefix),
                path,
                path_cut,
                to_header: Elapsed::cell(request.to_header),
                upstream_failed: request.upstream_failed,
                request_id,
                records,
            }),
        }
    }
}

impl LogsView {
    /// The access log of `mount` with the options of `params`, read from `reader`; `empty` is
    /// what an empty range says.
    pub async fn access(
        reader: &LogReader,
        mount: LogMount,
        params: &LogParams,
        empty: EmptyNote,
    ) -> Self {
        let now = SystemTime::now();
        let params = &params.clone().scoped(&mount.scope);
        let plan = AccessPlan::new(&mount.scope, params, now);
        // Read before the page: a row committed in between is on the page or followed after
        // it, never neither.
        let head = reader.request_head();
        let read = reader.requests(&plan).await;
        let (page, foot) = match read {
            Ok((page, foot)) => (Ok(page), Some(foot)),
            Err(error) => (Err(error), None),
        };
        let services = AccessLog::services(
            &mount.services,
            page.as_ref()
                .map(|p| p.entries.as_slice())
                .unwrap_or_default(),
            params.service(),
        );
        let log = AccessLog::new(&mount, params, services);
        let page = page.map(|page| Page {
            entries: page.entries.iter().map(|e| log.entry(e)).collect(),
            older: page.older,
            newer: page.newer,
            scan_stopped: page.scan_stopped,
        });
        let links = Links::of(&mount.action, &mount.scope, params, LogSource::Access);
        let download = mount
            .download
            .as_ref()
            .map(|action| Download::of(action, &links));
        let start = plan.query.range.start;
        let mut view = Self::listed(
            mount,
            params,
            now,
            LogSource::Access,
            plan.shared,
            page,
            empty,
            |page| RecordKey {
                time: start,
                // Past every row on the page, and every one committed before it was read.
                id: page
                    .entries
                    .iter()
                    .map(|e| e.key.id)
                    .max()
                    .unwrap_or(0)
                    .max(head),
            },
        );
        let mut chips = vec![Chip {
            label: "all".to_owned(),
            href: links.to(&[("service", None)]),
            current: params.service().is_none(),
            class: Self::ALL_CLASS,
        }];
        chips.extend(log.services.iter().map(|service| Chip {
            label: service.clone(),
            href: links.to(&[("service", Some(service))]),
            current: params.service() == Some(service.as_str()),
            class: Self::PALETTE[log.tone(service) % Self::PALETTE.len()],
        }));
        view.chips = chips;
        let (status, _) = params.status();
        view.statuses = Self::status_chips(status, &links);
        view.slower = Some(
            LogParams::text(&params.slower)
                .unwrap_or_default()
                .to_owned(),
        );
        view.requests = foot;
        view.download = download;
        if view.empty {
            view.cleared = Self::access_cleared(params, &links);
        }
        view
    }

    /// The status chips: every status, 4xx, 5xx, and the filter in effect when it is another.
    fn status_chips(current: Option<StatusFilter>, links: &Links) -> Vec<Chip> {
        let mut filters = vec![
            None,
            Some(StatusFilter::Class(4)),
            Some(StatusFilter::Class(5)),
        ];
        if !filters.contains(&current) {
            filters.push(current);
        }
        filters
            .into_iter()
            .map(|filter| {
                let value = filter.map(|f| f.to_string());
                Chip {
                    label: value.clone().unwrap_or_else(|| "all".to_owned()),
                    href: links.to(&[("status", value.as_deref())]),
                    current: filter == current,
                    class: match filter {
                        None => "req-all",
                        Some(StatusFilter::Class(class)) => {
                            AccessLog::status_tone(u16::from(class) * 100)
                        }
                        Some(StatusFilter::Code(code)) => AccessLog::status_tone(code),
                    },
                }
            })
            .collect()
    }

    /// For an empty page under the status, duration or grep filters: what they are, and the
    /// view without them.
    fn access_cleared(params: &LogParams, links: &Links) -> Option<(String, String)> {
        let mut active = Vec::new();
        if let (Some(status), _) = params.status() {
            active.push(format!("status {status}"));
        }
        if let (Some(_), _) = params.slower() {
            let given = LogParams::text(&params.slower).unwrap_or_default();
            active.push(format!("slower than {given}"));
        }
        if let Some(q) = params.q() {
            let kind = if params.regex() { "regex" } else { "grep" };
            active.push(format!("{kind} “{q}”"));
        }
        (!active.is_empty()).then(|| {
            (
                active.join(" and "),
                links.to(&[
                    ("status", None),
                    ("slower", None),
                    ("q", None),
                    ("regex", None),
                ]),
            )
        })
    }
}

/// What the access log's follow stream reads: the request rows committed after its cursor,
/// by id, in windows bounded as [`LogStore::requests_after`] bounds them, each time the
/// recorder announces a request head past what was read.
pub struct AccessTail {
    store: LogStore,
    head: watch::Receiver<LogHead>,
    query: RequestQuery,
    log: AccessLog,
    /// The last row id read.
    through: i64,
}

impl AccessTail {
    /// The tail of `mount`'s access log with the options of `params`, or why it cannot be
    /// followed.
    pub fn open(reader: &LogReader, mount: &LogMount, params: &LogParams) -> Result<Self, String> {
        let store = reader.store()?.clone();
        let params = &params.clone().scoped(&mount.scope);
        let AccessPlan { mut query, shared } =
            AccessPlan::new(&mount.scope, params, SystemTime::now());
        if let Some(invalid) = shared.invalid {
            return Err(invalid);
        }
        // Every row from the start of the range on, however far its time is ahead.
        query.range.end = Timestamp::from_millis(i64::MAX);
        let services = AccessLog::services(&mount.services, &[], params.service());
        Ok(Self {
            store,
            head: reader.head.clone(),
            query,
            log: AccessLog::new(mount, params, services),
            through: 0,
        })
    }
}

#[async_trait::async_trait]
impl SourceTail for AccessTail {
    async fn changed(&mut self) -> bool {
        let through = self.through;
        self.head
            .wait_for(|head| head.request > through)
            .await
            .is_ok()
    }

    async fn after(&mut self, cursor: RecordKey, limit: u32) -> Result<SourceRead, String> {
        let head = self.head.borrow().request;
        let mut query = self.query.clone();
        query.limit = limit.min(LogFollow::CHUNK);
        let tail = self
            .store
            .requests_after(&query, cursor.id, head)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "following the access log failed");
                format!("Cannot read the log store: {:#}", anyhow::Error::from(err))
            })?;
        self.through = tail.through;
        let mut entries: Vec<SourceEntry> =
            tail.entries.iter().map(|e| self.log.entry(e)).collect();
        // In page order within the message; commit order between messages.
        entries.sort_by_key(|e| e.key);
        let through = RecordKey {
            time: cursor.time,
            id: tail.through,
        };
        Ok(SourceRead {
            entries,
            more: tail.more.then_some(through),
            through: Some(through),
        })
    }

    /// The newest committed row: a stream opened without a cursor sends what follows it.
    fn start(&self, now: Timestamp) -> RecordKey {
        RecordKey {
            time: now,
            id: self.head.borrow().request,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_bytes_then_binary_units() {
        assert_eq!(Size(0).to_string(), "0 B");
        assert_eq!(Size(1023).to_string(), "1023 B");
        assert_eq!(Size(1536).to_string(), "1.5 KiB");
        assert_eq!(Size(12 << 20).to_string(), "12.0 MiB");
        assert_eq!(Size(1 << 40).to_string(), "1.0 TiB");
    }

    #[test]
    fn statuses_take_the_traffic_charts_tones() {
        assert_eq!(AccessLog::status_tone(200), "req-2xx");
        assert_eq!(AccessLog::status_tone(304), "req-3xx");
        assert_eq!(AccessLog::status_tone(404), "req-4xx");
        assert_eq!(AccessLog::status_tone(502), "req-5xx");
        assert_eq!(AccessLog::status_tone(101), "req-other");
        assert_eq!(AccessLog::status_tone(0), "req-other");
    }

    #[test]
    fn status_and_slower_parse_or_say_why() {
        let params = |pairs: &[(&str, &str)]| {
            <LogParams as From<Vec<(String, String)>>>::from(
                pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect::<Vec<_>>(),
            )
        };
        let given = params(&[("status", "5XX"), ("slower", "1500ms")]);
        assert_eq!(given.status(), (Some(StatusFilter::Class(5)), None));
        assert_eq!(given.slower(), (Some(Duration::from_millis(1500)), None));
        let wrong = params(&[("status", "teapot"), ("slower", "fast")]);
        assert!(wrong.status().0.is_none() && wrong.status().1.is_some());
        assert!(wrong.slower().0.is_none() && wrong.slower().1.is_some());
        assert_eq!(params(&[]).status(), (None, None));
    }
}
