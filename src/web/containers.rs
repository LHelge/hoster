//! The container logs (ADR-0044): each managed container's log with *tail N*, *since* and
//! *follow*, streamed into the page over SSE with HTMX's SSE extension. The log is shown on its
//! stack's logs tab (`/stacks/{name}/logs`, epic 8bf, ADR-0054), which picks the container and
//! renders the toolbar and log panel this module provides as [`LogView`]. The containers
//! themselves are listed on their stack's overview, which reads them through
//! [`Containers::of_stack`]; `GET /containers`, the list they had before, redirects to
//! `/stacks`, which lists the containers of stacks no longer in the store.
//!
//! `GET /containers/{name}/logs` redirects to the logs tab of the container's stack, with the
//! same options. A container whose stack hoster no longer has (or that has no stack label) has
//! no tab, so its log is shown on that URL itself, linked from the leftovers on `/stacks`.
//!
//! A log panel connects to `GET /containers/{name}/logs/stream` with the options of the page.
//! The stream sends the log in batches
//! of rendered lines (unnamed SSE messages, appended to the element), then a closing notice and
//! a named `end` event that tells the extension not to reconnect. Each batch carries the time of
//! its last line as the event ID, so a reconnect (the extension reconnects after a dropped
//! connection, and when a backgrounded tab comes back) resumes after it instead of repeating
//! the log. The log is read only while the response is: when the browser goes away, axum drops
//! the stream and with it the request to podman. The stream also ends, with a notice and the
//! `end` event, once the session it was opened with ends (logout, "log out everywhere", expiry;
//! ADR-0031), so an open log tab does not keep showing output to a revoked session.
//!
//! Only containers labelled `hoster.managed=true` can be viewed (ADR-0019).

use std::convert::Infallible;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use askama::Template;
use axum::Extension;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Redirect, Response};
use futures_util::stream::{self, BoxStream, Stream, StreamExt};
use serde::Deserialize;

use super::auth::{Auth, Session};
use super::pages::{Flash, FlashKind, Layout, Page, Section, Shell};
use super::stacks::Stacks;
use crate::runtime::{
    ContainerId, ContainerInfo, ContainerRuntime, LogLine, LogOptions, LogStream, RuntimeError,
    label,
};

/// What the container pages need from the runtime. Unlike [`ContainerRuntime`] it is
/// dyn-compatible, so the app holds any runtime (podman in production, the fake in tests)
/// behind one `Arc`.
#[async_trait::async_trait]
pub trait ContainerSource: Send + Sync + 'static {
    /// Every hoster-managed container.
    async fn managed(&self) -> Result<Vec<ContainerInfo>, RuntimeError>;

    /// The managed containers of `stack`, each as inspect reports it (with its start time). A
    /// container removed between listing and inspecting is left out; one that cannot be
    /// inspected otherwise is kept as the list reported it.
    async fn of_stack(&self, stack: &str) -> Result<Vec<ContainerInfo>, RuntimeError>;

    /// A container's log; see [`ContainerRuntime::logs`].
    fn log(
        &self,
        id: &ContainerId,
        options: LogOptions,
    ) -> BoxStream<'static, Result<LogLine, RuntimeError>>;
}

#[async_trait::async_trait]
impl<R: ContainerRuntime> ContainerSource for R {
    async fn managed(&self) -> Result<Vec<ContainerInfo>, RuntimeError> {
        self.list_managed().await
    }

    async fn of_stack(&self, stack: &str) -> Result<Vec<ContainerInfo>, RuntimeError> {
        let listed = self.list_managed().await?.into_iter().filter(|c| {
            c.labels
                .get(label::STACK)
                .is_some_and(|label| label == stack)
        });
        let inspected = futures_util::future::join_all(listed.map(|listed| async move {
            match self.inspect(&listed.id).await {
                Ok(info) => Some(info),
                Err(RuntimeError::NotFound { .. }) => None,
                Err(err) => {
                    tracing::warn!(container = %listed.name, error = %err, "inspecting a container failed");
                    Some(listed)
                }
            }
        }))
        .await;
        Ok(inspected.into_iter().flatten().collect())
    }

    fn log(
        &self,
        id: &ContainerId,
        options: LogOptions,
    ) -> BoxStream<'static, Result<LogLine, RuntimeError>> {
        self.logs(id, options)
    }
}

/// The container pages' handlers, sharing the runtime as a request extension.
#[derive(Clone)]
pub struct Containers(Arc<dyn ContainerSource>);

impl From<Arc<dyn ContainerSource>> for Containers {
    fn from(source: Arc<dyn ContainerSource>) -> Self {
        Self(source)
    }
}

/// Why a container named in a URL cannot be shown.
#[derive(Debug, thiserror::Error)]
enum LookupError {
    /// No managed container has that name.
    #[error("no such container")]
    NotFound,
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
}

impl IntoResponse for LookupError {
    fn into_response(self) -> Response {
        let status = match &self {
            LookupError::NotFound => StatusCode::NOT_FOUND,
            LookupError::Runtime(err) => {
                tracing::error!(error = %err, "listing containers failed");
                StatusCode::BAD_GATEWAY
            }
        };
        (status, format!("{self}\n")).into_response()
    }
}

impl Containers {
    /// The managed container called `name`.
    async fn find(&self, name: &str) -> Result<ContainerInfo, LookupError> {
        self.0
            .managed()
            .await?
            .into_iter()
            .find(|c| c.name == name)
            .ok_or(LookupError::NotFound)
    }

    /// The managed containers of `stack`, for its overview.
    pub(in crate::web) async fn of_stack(
        &self,
        stack: &str,
    ) -> Result<Vec<ContainerInfo>, RuntimeError> {
        self.0.of_stack(stack).await
    }

    /// Every managed container, for the leftovers of removed stacks that `/stacks` lists.
    pub(in crate::web) async fn managed(&self) -> Result<Vec<ContainerInfo>, RuntimeError> {
        self.0.managed().await
    }

    /// `GET /containers`: the containers are listed on their stacks' pages now (ADR-0054).
    pub async fn list() -> Redirect {
        Redirect::to("/stacks")
    }

    /// `GET /containers/{name}/logs`: the logs tab of the container's stack, showing it with
    /// the same options; for a leftover of a removed stack, its log page.
    pub async fn log_page(
        Extension(containers): Extension<Containers>,
        Extension(stacks): Extension<Stacks>,
        shell: Shell,
        Path(name): Path<String>,
        Query(query): Query<LogQuery>,
    ) -> Response {
        let container = match containers.find(&name).await {
            Ok(container) => container,
            Err(err) => return err.into_response(),
        };
        let container = ContainerRow::from(&container);
        if let Some(url) = container.stack_url(&stacks).await {
            let mut pairs = Vec::new();
            if !container.service.is_empty() {
                pairs.push(("service", container.service.as_str()));
            }
            pairs.push(("container", container.name.as_str()));
            pairs.extend(query.pairs());
            return Redirect::to(&format!("{url}/logs?{}", LogQuery::encode(&pairs)))
                .into_response();
        }
        let log = LogView::new(
            Some(&container.name),
            format!("/containers/{}/logs", container.name),
            Vec::new(),
            &query,
        );
        let mut layout = shell.layout(Section::Stacks);
        if let Some(message) = log.error.clone() {
            layout = layout.with_flash(Flash {
                kind: FlashKind::Error,
                message,
            });
        }
        Page(LogPage {
            layout,
            container,
            log,
        })
        .into_response()
    }

    /// `GET /containers/{name}/logs/stream`: the log as `text/event-stream`. A `Last-Event-ID`
    /// header (sent by the SSE extension on reconnect) resumes after that line.
    pub async fn log_stream(
        State(auth): State<Arc<Auth>>,
        Extension(containers): Extension<Containers>,
        Extension(session): Extension<Session>,
        Path(name): Path<String>,
        Query(query): Query<LogQuery>,
        headers: HeaderMap,
    ) -> Response {
        let mut options = match LogOptions::try_from(&query) {
            Ok(options) => options,
            Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
        };
        let container = match containers.find(&name).await {
            Ok(container) => container,
            Err(err) => return err.into_response(),
        };
        let after = headers
            .get("last-event-id")
            .and_then(|id| id.to_str().ok())
            .and_then(|id| id.parse::<LineTime>().ok());
        if let Some(after) = after {
            // Everything after the last line the browser has, however many lines that is.
            options.since = Some(after.0);
            options.tail = None;
        }
        let lines = containers.0.log(&container.id, options);
        let signed_out = async move {
            match auth.session_ended(&session, LogEvents::SESSION_CHECK).await {
                Ok(()) => "Signed out: sign in again to see more of the log.".to_owned(),
                Err(err) => {
                    tracing::error!(error = %err, "checking the session of a log stream failed");
                    "Cannot check the session: reload the page to see more of the log.".to_owned()
                }
            }
        };
        Sse::new(LogEvents::new(options.since, after).stream(lines, signed_out))
            .keep_alive(KeepAlive::new().interval(LogEvents::KEEP_ALIVE))
            .into_response()
    }
}

/// The log options of the log routes and the logs tab. Every field is text, as an HTML form
/// sends it.
#[derive(Debug, Default, Deserialize)]
pub struct LogQuery {
    /// How many lines to start with: a number, or empty for the whole log.
    tail: Option<String>,
    /// Where to start: a duration ago (`15m`) or a time (`2026-11-08T12:00:00Z`).
    since: Option<String>,
    /// Present (as `on`) when following.
    follow: Option<String>,
}

impl LogQuery {
    /// The tail of a first visit, before the form was submitted.
    const DEFAULT_TAIL: usize = 200;

    /// Whether no option was given at all: a first visit, which follows the last
    /// [`Self::DEFAULT_TAIL`] lines.
    fn is_first_visit(&self) -> bool {
        self.tail.is_none() && self.since.is_none() && self.follow.is_none()
    }

    fn follow(&self) -> bool {
        self.is_first_visit() || self.follow.is_some()
    }

    /// The options that were given, as query pairs.
    fn pairs(&self) -> Vec<(&'static str, &str)> {
        [
            ("tail", self.tail.as_deref()),
            ("since", self.since.as_deref()),
            ("follow", self.follow.as_deref()),
        ]
        .into_iter()
        .filter_map(|(key, value)| Some((key, value?)))
        .collect()
    }

    /// `pairs` as a query string.
    fn encode(pairs: &[(&str, &str)]) -> String {
        form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish()
    }
}

impl TryFrom<&LogQuery> for LogOptions {
    type Error = String;

    fn try_from(query: &LogQuery) -> Result<Self, Self::Error> {
        let tail = match query.tail.as_deref().map(str::trim) {
            None => Some(LogQuery::DEFAULT_TAIL),
            Some("") => None,
            Some(tail) => Some(tail.parse().map_err(|_| {
                format!("The number of lines must be a whole number, not `{tail}`.")
            })?),
        };
        let since = match query.since.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(since) => Some(since.parse::<LogSince>()?.resolve(SystemTime::now())),
        };
        Ok(LogOptions {
            follow: query.follow(),
            tail,
            since,
        })
    }
}

/// Where a log starts, as typed into the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogSince {
    /// This long before now.
    Ago(Duration),
    At(SystemTime),
}

impl LogSince {
    /// The time this means at `now`, never before the Unix epoch.
    fn resolve(self, now: SystemTime) -> SystemTime {
        let time = match self {
            LogSince::Ago(ago) => now.checked_sub(ago).unwrap_or(UNIX_EPOCH),
            LogSince::At(time) => time,
        };
        time.max(UNIX_EPOCH)
    }
}

impl FromStr for LogSince {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        humantime::parse_duration(s)
            .map(LogSince::Ago)
            .or_else(|_| humantime::parse_rfc3339_weak(s).map(LogSince::At))
            .map_err(|_| {
                format!(
                    "`{s}` is not a start time: use a duration such as 15m or 2h, or a UTC time \
                     such as 2026-11-08T12:00:00Z."
                )
            })
    }
}

/// A line's time as an SSE event ID: nanoseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LineTime(SystemTime);

impl FromStr for LineTime {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let nanos: u64 = s.parse()?;
        Ok(LineTime(UNIX_EPOCH + Duration::from_nanos(nanos)))
    }
}

impl std::fmt::Display for LineTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let nanos = self.0.duration_since(UNIX_EPOCH).unwrap_or_default();
        write!(f, "{}", nanos.as_nanos())
    }
}

/// Turns a log into SSE events: batches of rendered lines, then the end of the log.
#[derive(Debug, Clone, Copy)]
struct LogEvents {
    /// Lines from before this are dropped (podman only filters to the second).
    since: Option<SystemTime>,
    /// Lines up to and including this were already sent before a reconnect.
    after: Option<SystemTime>,
}

impl LogEvents {
    /// The most lines rendered into one event.
    const BATCH: usize = 500;
    /// How often an idle stream sends a comment, so a closed connection is noticed (and the
    /// podman request dropped) even when the container is quiet.
    const KEEP_ALIVE: Duration = Duration::from_secs(15);
    /// The named event after the last line; the page closes the connection on it.
    const END: &str = "end";
    /// How often a stream checks that its session is still signed in.
    const SESSION_CHECK: Duration = if cfg!(test) {
        Duration::from_millis(20)
    } else {
        Duration::from_secs(30)
    };

    fn new(since: Option<SystemTime>, after: Option<LineTime>) -> Self {
        Self {
            since,
            after: after.map(|t| t.0),
        }
    }

    fn keeps(&self, line: &LogLine) -> bool {
        let Some(time) = line.time else {
            return true;
        };
        self.since.is_none_or(|since| time >= since) && self.after.is_none_or(|after| time > after)
    }

    /// The events for `lines`. An error ends the log, with the error shown as its last line.
    /// When `cut` resolves first, the log ends there, with its notice.
    fn stream(
        self,
        lines: BoxStream<'static, Result<LogLine, RuntimeError>>,
        cut: impl Future<Output = String> + Send + 'static,
    ) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static {
        let batches = lines
            .ready_chunks(Self::BATCH)
            .scan(false, move |failed, batch| {
                if *failed {
                    return std::future::ready(None);
                }
                let mut chunk = LogChunk::default();
                let mut last = None;
                for line in batch {
                    match line {
                        Ok(line) if self.keeps(&line) => {
                            last = line.time.or(last);
                            chunk.lines.push(LogEntry::from(line));
                        }
                        Ok(_) => {}
                        Err(err) => {
                            *failed = true;
                            chunk.notice = Some(format!("Cannot read the log: {err}"));
                            break;
                        }
                    }
                }
                std::future::ready(Some(chunk.event(last.map(LineTime))))
            })
            .filter_map(std::future::ready);
        // `Ok` is a batch, `Err` the notice the log ends with; whichever end comes first wins.
        let log = batches.map(Ok).chain(stream::once(std::future::ready(Err(
            "End of the log.".to_owned()
        ))));
        let merged = stream::select(log, stream::once(cut).map(Err)).boxed();
        // Unfolded rather than scanned, so the stream ends right after its notice instead of
        // waiting on whichever side has not finished.
        stream::unfold(Some(merged), |merged| async move {
            let mut merged = merged?;
            Some(match merged.next().await? {
                Ok(batch) => (vec![batch], Some(merged)),
                Err(notice) => {
                    let closing = LogChunk::notice(&notice)
                        .event(None)
                        .into_iter()
                        .chain([Event::default().event(Self::END).data("")])
                        .collect();
                    (closing, None)
                }
            })
        })
        .flat_map(stream::iter)
        .map(Ok)
    }
}

/// One SSE message's worth of rendered log.
#[derive(Debug, Default, Template)]
#[template(path = "log_lines.html")]
struct LogChunk {
    lines: Vec<LogEntry>,
    notice: Option<String>,
}

impl LogChunk {
    fn notice(message: &str) -> Self {
        Self {
            lines: Vec::new(),
            notice: Some(message.to_owned()),
        }
    }

    /// The chunk as an unnamed SSE message with `id`, or `None` when there is nothing to show.
    fn event(&self, id: Option<LineTime>) -> Option<Event> {
        if self.lines.is_empty() && self.notice.is_none() {
            return None;
        }
        let html = match self.render() {
            Ok(html) => html,
            Err(err) => {
                tracing::error!(error = %err, "rendering log lines failed");
                return None;
            }
        };
        let event = Event::default().data(html);
        Some(match id {
            Some(id) => event.id(id.to_string()),
            None => event,
        })
    }
}

/// A log line as the page shows it.
#[derive(Debug)]
struct LogEntry {
    stream: LogStream,
    /// RFC 3339 in UTC, to the millisecond.
    time: Option<String>,
    text: String,
}

impl LogEntry {
    /// `text` without terminal escape sequences (colours, cursor movement) and with any other
    /// control character but a tab shown as U+FFFD. SSE data may not hold a `\r` either.
    fn printable(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\u{1b}' => match chars.next() {
                    // CSI: `ESC [ … final`.
                    Some('[') => while chars.next().is_some_and(|c| !('@'..='~').contains(&c)) {},
                    // OSC (titles, hyperlinks), DCS, APC, PM: a string up to BEL or `ESC \`.
                    Some(']' | 'P' | '_' | '^') => {
                        while let Some(c) = chars.next() {
                            if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some())
                            {
                                break;
                            }
                        }
                    }
                    // Any other two-character escape.
                    _ => {}
                },
                '\t' => out.push(c),
                c if c.is_control() => out.push('\u{fffd}'),
                c => out.push(c),
            }
        }
        out
    }
}

impl From<LogLine> for LogEntry {
    fn from(line: LogLine) -> Self {
        Self {
            stream: line.stream,
            time: line
                .time
                .map(|time| humantime::format_rfc3339_millis(time).to_string()),
            text: Self::printable(&line.text),
        }
    }
}

/// The container a log page shows.
#[derive(Debug)]
pub struct ContainerRow {
    name: String,
    stack: String,
    service: String,
    generation: String,
    state: String,
}

impl From<&ContainerInfo> for ContainerRow {
    fn from(info: &ContainerInfo) -> Self {
        let label = |key: &str| info.labels.get(key).cloned().unwrap_or_default();
        Self {
            name: info.name.clone(),
            stack: label(label::STACK),
            service: label(label::SERVICE),
            generation: label(label::GENERATION),
            state: info.state.to_string(),
        }
    }
}

impl ContainerRow {
    /// The page of the container's stack, or `None` when the container has no stack label or
    /// its stack is gone (it is then listed among the leftovers on `/stacks`). When the stacks
    /// cannot be read, the stack is assumed to be there.
    async fn stack_url(&self, stacks: &Stacks) -> Option<String> {
        if self.stack.is_empty() {
            return None;
        }
        let exists = match stacks.status().await {
            Ok(list) => list.iter().any(|s| s.name == self.stack),
            Err(err) => {
                tracing::warn!(error = %err, "listing the stacks failed");
                true
            }
        };
        exists.then(|| format!("/stacks/{}", self.stack))
    }
}

/// The log form, filled in with the query it was submitted with.
#[derive(Debug)]
pub struct LogForm {
    pub tail: String,
    pub since: String,
    pub follow: bool,
}

impl From<&LogQuery> for LogForm {
    fn from(query: &LogQuery) -> Self {
        Self {
            tail: query
                .tail
                .clone()
                .unwrap_or_else(|| LogQuery::DEFAULT_TAIL.to_string()),
            since: query.since.clone().unwrap_or_default(),
            follow: query.follow(),
        }
    }
}

impl LogForm {
    /// `keep` and the form's options with `since`, as a query string.
    fn query(&self, keep: &[(&str, &str)], since: &str) -> String {
        let mut pairs = keep.to_vec();
        pairs.extend([("tail", self.tail.as_str()), ("since", since)]);
        if self.follow {
            pairs.push(("follow", "on"));
        }
        LogQuery::encode(&pairs)
    }
}

/// A *since* preset of the log toolbar.
#[derive(Debug)]
pub struct SinceLink {
    pub label: &'static str,
    pub href: String,
    pub current: bool,
}

/// The log toolbar's options form and the log panel of one container: what `log_options.html`
/// and `log_panel.html` render.
#[derive(Debug)]
pub struct LogView {
    /// The container whose log it is; `None` when there is none to show.
    pub container: Option<String>,
    /// The page the form submits to.
    pub action: String,
    /// The query pairs of that page besides the options, kept as hidden fields.
    pub keep: Vec<(&'static str, String)>,
    pub form: LogForm,
    pub presets: Vec<SinceLink>,
    /// The stream to connect to; `None` when the options are invalid.
    pub stream_url: Option<String>,
    /// Why the options are invalid.
    pub error: Option<String>,
    /// What the container is beyond its name (generation, role, state), for the log's footer;
    /// `None` when the page says it elsewhere.
    pub detail: Option<String>,
}

impl LogView {
    /// The *since* presets, as (label, value); "all" is no start.
    const PRESETS: [(&str, &str); 5] = [
        ("15m", "15m"),
        ("1h", "1h"),
        ("6h", "6h"),
        ("24h", "24h"),
        ("all", ""),
    ];

    /// The log of `container` on the page at `action`, whose own query is `keep`, with the
    /// options of `query`.
    pub fn new(
        container: Option<&str>,
        action: String,
        keep: Vec<(&'static str, String)>,
        query: &LogQuery,
    ) -> Self {
        let form = LogForm::from(query);
        let (stream_url, error) = match LogOptions::try_from(query) {
            Ok(_) => (
                container.map(|container| {
                    format!(
                        "/containers/{container}/logs/stream?{}",
                        form.query(&[], &form.since)
                    )
                }),
                None,
            ),
            Err(message) => (None, Some(message)),
        };
        let pairs: Vec<(&str, &str)> = keep.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let presets = Self::PRESETS
            .into_iter()
            .map(|(label, value)| SinceLink {
                label,
                href: format!("{action}?{}", form.query(&pairs, value)),
                current: form.since.trim() == value,
            })
            .collect();
        Self {
            container: container.map(str::to_owned),
            action,
            keep,
            form,
            presets,
            stream_url,
            error,
            detail: None,
        }
    }

    /// The page at `action` with `keep` instead of this page's own query, and the same options.
    pub fn link(&self, keep: &[(&str, &str)]) -> String {
        format!(
            "{}?{}",
            self.action,
            self.form.query(keep, &self.form.since)
        )
    }
}

/// `GET /containers/{name}/logs` of a container whose stack is gone.
#[derive(Template)]
#[template(path = "container_logs.html")]
struct LogPage {
    layout: Layout,
    container: ContainerRow,
    log: LogView,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(tail: Option<&str>, since: Option<&str>, follow: bool) -> LogQuery {
        LogQuery {
            tail: tail.map(Into::into),
            since: since.map(Into::into),
            follow: follow.then(|| "on".into()),
        }
    }

    #[test]
    fn first_visit_follows_the_last_lines() {
        let options = LogOptions::try_from(&LogQuery::default()).unwrap();
        assert_eq!(
            options,
            LogOptions {
                follow: true,
                tail: Some(LogQuery::DEFAULT_TAIL),
                since: None,
            }
        );
    }

    #[test]
    fn submitted_forms_set_tail_since_and_follow() {
        let options = LogOptions::try_from(&query(Some(""), Some(""), false)).unwrap();
        assert_eq!(
            options,
            LogOptions::default(),
            "empty fields mean everything"
        );

        let before = SystemTime::now();
        let options = LogOptions::try_from(&query(Some(" 50 "), Some("15m"), true)).unwrap();
        assert!(options.follow);
        assert_eq!(options.tail, Some(50));
        let since = options.since.unwrap();
        assert!(since <= before - Duration::from_secs(15 * 60) + Duration::from_secs(5));

        let options =
            LogOptions::try_from(&query(None, Some("2026-11-08T12:00:00Z"), false)).unwrap();
        assert_eq!(
            options.since,
            Some(humantime::parse_rfc3339("2026-11-08T12:00:00Z").unwrap())
        );
        assert_eq!(options.tail, Some(LogQuery::DEFAULT_TAIL));

        assert!(LogOptions::try_from(&query(Some("-1"), None, false)).is_err());
        let err = LogOptions::try_from(&query(None, Some("yesterday"), false)).unwrap_err();
        assert!(err.contains("15m"), "{err}");
    }

    #[test]
    fn relative_since_saturates_at_the_epoch() {
        let since: LogSince = "2h".parse().unwrap();
        assert_eq!(since, LogSince::Ago(Duration::from_secs(7200)));
        assert_eq!(
            since.resolve(UNIX_EPOCH + Duration::from_secs(60)),
            UNIX_EPOCH
        );
    }

    #[test]
    fn line_times_round_trip_as_event_ids() {
        let time = LineTime(UNIX_EPOCH + Duration::from_nanos(1_762_560_000_123_456_789));
        assert_eq!(time.to_string(), "1762560000123456789");
        assert_eq!(time.to_string().parse::<LineTime>().unwrap(), time);
        assert!("not-a-number".parse::<LineTime>().is_err());
    }

    #[test]
    fn terminal_escapes_and_control_characters_are_not_shown() {
        assert_eq!(
            LogEntry::printable("\u{1b}[1;31merror\u{1b}[0m:\tdisk\rfull\u{7}"),
            "error:\tdisk\u{fffd}full\u{fffd}"
        );
        assert_eq!(LogEntry::printable("\u{1b}7saved"), "saved");
        assert_eq!(
            LogEntry::printable(
                "\u{1b}]8;;https://example.com\u{1b}\\link\u{1b}]8;;\u{1b}\\ \u{1b}]0;title\u{7}done"
            ),
            "link done"
        );
        assert_eq!(LogEntry::printable("plain <b>"), "plain <b>");
    }

    #[test]
    fn rendered_lines_are_escaped_and_marked_by_stream() {
        let chunk = LogChunk {
            lines: vec![
                LogEntry::from(LogLine {
                    stream: LogStream::Stderr,
                    time: Some(humantime::parse_rfc3339("2026-11-08T12:00:00.5Z").unwrap()),
                    text: "<script>alert(1)</script>".into(),
                }),
                LogEntry::from(LogLine {
                    stream: LogStream::Stdout,
                    time: None,
                    text: "ok".into(),
                }),
            ],
            notice: None,
        };
        let html = chunk.render().unwrap();
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains(r#"class="log-line log-stderr""#), "{html}");
        assert!(html.contains(r#"class="log-line log-stdout""#), "{html}");
        assert!(
            html.contains(r#"<time datetime="2026-11-08T12:00:00.500Z">"#),
            "{html}"
        );
        assert!(!html.contains('\n'), "one line per message: {html}");
    }
}
