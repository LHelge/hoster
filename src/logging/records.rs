//! hoster's own events as records of the log store (ADR-0068): the second layer [`Logging`]
//! installs beside the stderr formatter.
//!
//! [`Logging`]: super::Logging

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::SystemTime;

use arc_swap::ArcSwapOption;
use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_log::NormalizeEvent;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{FilterFn, Filtered};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use super::ACCESS_TARGET;
use crate::logs::{LogSink, ParsedLine, RateCap};
use crate::store::{self, LogRecord, Stream, Timestamp};

/// Where hoster's own events go once the log store's recorder runs: a handle the layer checks
/// on every event, connected to the recorder's [`LogSink`] after logging is up and disconnected
/// when the recorder stops. Without a sink, events are in the journal only (ADR-0068 §4).
///
/// Cheap to clone; every clone shares the sink, the [`RateCap`] and its count.
#[derive(Debug, Clone, Default)]
pub struct HosterRecords(Arc<Shared>);

#[derive(Debug, Default)]
struct Shared {
    sink: ArcSwapOption<LogSink>,
    /// hoster counts as one source under the line-rate cap (ADR-0067 §7).
    cap: Mutex<RateCap>,
    /// Events dropped by the cap since hoster started.
    capped: AtomicU64,
}

impl HosterRecords {
    /// The service of every record (ADR-0068 §3).
    pub const SERVICE: &str = "hoster";
    /// The most verbose level recorded, whatever `RUST_LOG` says (ADR-0068 §1).
    const LEVEL: Level = Level::INFO;
    /// The targets never recorded, with their children: the log store's own modules, so
    /// recording cannot feed itself, and the access lines the request rows cover (ADR-0068 §2).
    const EXCLUDED: [&str; 3] = ["hoster::logs", "hoster::store::logs", ACCESS_TARGET];

    /// The handle of the process subscriber [`Logging::init`](super::Logging::init) installs.
    pub fn global() -> &'static HosterRecords {
        static GLOBAL: OnceLock<HosterRecords> = OnceLock::new();
        GLOBAL.get_or_init(HosterRecords::default)
    }

    /// Records events into `sink` from now on.
    pub fn connect(&self, sink: LogSink) {
        self.0.sink.store(Some(Arc::new(sink)));
    }

    /// Stops recording: events are in the journal only again.
    pub fn disconnect(&self) {
        self.0.sink.store(None);
    }

    /// Events the line-rate cap dropped since hoster started. Those the recorder's full
    /// channel dropped are counted by the sink ([`Drops::hoster_records`]).
    ///
    /// [`Drops::hoster_records`]: crate::logs::Drops::hoster_records
    pub fn capped(&self) -> u64 {
        self.0.capped.load(Ordering::Relaxed)
    }

    /// The layer recording into this handle, with its own fixed filter.
    pub(super) fn layer<S>(&self) -> Filtered<RecordLayer, FilterFn, S>
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        let filter = FilterFn::new(Self::enabled as fn(&Metadata<'_>) -> bool)
            .with_max_level_hint(Self::LEVEL);
        RecordLayer(self.clone()).with_filter(filter)
    }

    /// The layer's filter: [`Self::LEVEL`] and above, outside [`Self::EXCLUDED`].
    fn enabled(metadata: &Metadata<'_>) -> bool {
        *metadata.level() <= Self::LEVEL && !Self::excluded(metadata.target())
    }

    /// Whether `target` is one of [`Self::EXCLUDED`] or below one.
    fn excluded(target: &str) -> bool {
        Self::EXCLUDED.iter().any(|excluded| {
            target
                .strip_prefix(excluded)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
        })
    }

    /// Offers `record` under the rate cap, after the marker of the gap it ends, if any.
    fn offer(&self, sink: &LogSink, record: LogRecord) {
        let admission = self
            .0
            .cap
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .admit(record.time.into());
        if let Some(gap) = admission.ended {
            sink.try_record(LogRecord {
                time: Timestamp::from(gap.last),
                stack: None,
                service: Self::SERVICE.to_owned(),
                generation: None,
                stream: Stream::Hoster,
                level: Some(store::Level::Warn),
                message: gap.message(),
                fields: None,
                cut: false,
                marker: true,
            });
        }
        if admission.keep {
            sink.try_record(record);
        } else {
            self.0.capped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The layer itself; built by [`HosterRecords::layer`].
pub struct RecordLayer(HosterRecords);

/// The fields of a span as the JSON formatter writes them, kept in the span's extensions.
struct SpanFields(Map<String, Value>);

impl<S> Layer<S> for RecordLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut fields = JsonFields::default();
        attrs.record(&mut fields);
        span.extensions_mut().insert(SpanFields(fields.0));
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut extensions = span.extensions_mut();
        if let Some(SpanFields(map)) = extensions.get_mut::<SpanFields>() {
            let mut fields = JsonFields(std::mem::take(map));
            values.record(&mut fields);
            *map = fields.0;
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let Some(sink) = self.0.0.sink.load_full() else {
            return;
        };
        // A `log` record (Pingora's) names its own target and level in its fields.
        let normalized = event.normalized_metadata();
        let metadata = normalized.as_ref().unwrap_or_else(|| event.metadata());
        if HosterRecords::excluded(metadata.target()) {
            return;
        }
        let time = Timestamp::from(SystemTime::now());

        // The object the JSON formatter writes, without its `timestamp` (the record's time).
        let mut fields = JsonFields::default();
        event.record(&mut fields);
        let mut object = fields.0;
        let event_stack = object
            .get("stack")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let event_generation = object.get("generation").and_then(generation_of);
        object.insert("level".into(), metadata.level().as_str().into());
        object.insert("target".into(), metadata.target().into());

        // The enclosing spans, innermost first.
        let mut spans = Vec::new();
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope {
                let mut entry = span
                    .extensions()
                    .get::<SpanFields>()
                    .map(|fields| fields.0.clone())
                    .unwrap_or_default();
                entry.insert("name".into(), span.name().into());
                spans.push(entry);
            }
        }
        let stack = event_stack.or_else(|| {
            spans
                .iter()
                .find_map(|span| span.get("stack").and_then(Value::as_str))
                .map(str::to_owned)
        });
        // A span's generation counts only for the same stack: an event naming another stack
        // than its span's does not take the span's deployment.
        let generation = event_generation.or_else(|| {
            spans.iter().find_map(|span| {
                let span_stack = span.get("stack").and_then(Value::as_str);
                if span_stack.is_some() && span_stack != stack.as_deref() {
                    return None;
                }
                span.get("generation").and_then(generation_of)
            })
        });
        if !spans.is_empty() {
            spans.reverse();
            object.insert(
                "spans".into(),
                Value::Array(spans.into_iter().map(Value::Object).collect()),
            );
        }

        let parsed = ParsedLine::from(object);
        let record = LogRecord {
            time,
            stack,
            service: HosterRecords::SERVICE.to_owned(),
            generation,
            stream: Stream::Hoster,
            level: parsed.level,
            message: parsed.message,
            fields: parsed.fields,
            cut: false,
            marker: false,
        };
        self.0.offer(&sink, record);
    }
}

/// A deployment generation as a field holds it: a number, or its text.
fn generation_of(value: &Value) -> Option<u32> {
    match value {
        Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Fields as `tracing-subscriber`'s JSON formatter writes them: numbers and booleans as JSON,
/// everything else as its `Debug` text (a `%value` as its `Display`). A `log` record's own
/// metadata (`log.target`, …) is left out, as is the `r#` of a raw identifier.
#[derive(Default)]
struct JsonFields(Map<String, Value>);

impl JsonFields {
    fn insert(&mut self, field: &Field, value: Value) {
        let name = field.name();
        if name.starts_with("log.") {
            return;
        }
        let name = name.strip_prefix("r#").unwrap_or(name);
        self.0.insert(name.to_owned(), value);
    }
}

impl Visit for JsonFields {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.insert(field, value.into());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field, value.into());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field, value.into());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field, value.into());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert(field, value.into());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.insert(field, format!("{value:?}").into());
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::SubscriberExt;

    use super::super::capture;
    use super::super::{LogFormat, LogSpan};
    use super::*;
    use crate::logs::Tap;

    /// A subscriber with only the record layer, its handle connected to a tapped sink.
    fn recording() -> (HosterRecords, Tap, impl Subscriber + Send + Sync) {
        capture::global();
        let records = HosterRecords::default();
        let (sink, tap) = LogSink::tapped(4 * RateCap::LINES_PER_SECOND as usize);
        records.connect(sink);
        let subscriber = Registry::default().with(records.layer());
        (records, tap, subscriber)
    }

    fn drain(tap: &mut Tap) -> Vec<LogRecord> {
        tap.drain().into_iter().map(|(record, _)| record).collect()
    }

    fn fields(record: &LogRecord) -> Value {
        record
            .fields
            .as_deref()
            .map_or(Value::Null, |f| serde_json::from_str(f).unwrap())
    }

    #[test]
    fn events_at_info_and_above_are_recorded_whatever_rust_log_says() {
        let (_records, mut tap, subscriber) = recording();
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!("too verbose");
            tracing::info!(host = "blog.example", "certificate ordered");
            tracing::error!(error = %"refused", "upstream failed");
        });
        let records = drain(&mut tap);
        assert_eq!(records.len(), 2, "{records:?}");
        let [info, error] = &records[..] else {
            unreachable!()
        };
        assert_eq!(info.service, "hoster");
        assert_eq!(info.stream, Stream::Hoster);
        assert_eq!(info.level, Some(store::Level::Info));
        assert_eq!(info.message, "certificate ordered");
        assert_eq!(info.stack, None);
        assert_eq!(info.generation, None);
        assert!(!info.marker && !info.cut);
        assert_eq!(
            fields(info),
            json!({"host": "blog.example", "target": "hoster::logging::records::tests"})
        );
        assert_eq!(error.level, Some(store::Level::Error));
        assert_eq!(fields(error)["error"], "refused");
    }

    #[test]
    fn the_log_stores_own_targets_and_access_lines_are_excluded() {
        let (_records, mut tap, subscriber) = recording();
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(target: "hoster::logs::recorder", "cannot commit log records");
            tracing::warn!(target: "hoster::logs::follower", "lost lines");
            tracing::warn!(target: "hoster::store::logs::retention", "sweep failed");
            tracing::info!(target: ACCESS_TARGET, status = 200, "request");
            tracing::info!(target: "hoster::logsearch", "a sibling, not a child");
        });
        let records = drain(&mut tap);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].message, "a sibling, not a child");
    }

    #[test]
    fn a_recorder_error_does_not_produce_a_record() {
        let (_records, mut tap, subscriber) = recording();
        // A commit that fails is logged by the recorder under its own module's target.
        let target = std::any::type_name::<crate::logs::LogRecorder>();
        assert!(target.starts_with("hoster::logs::"), "{target}");
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(target: "hoster::logs::recorder", error = "disk full", "log commit failed");
        });
        assert!(drain(&mut tap).is_empty());
    }

    #[test]
    fn stack_and_generation_come_from_the_nearest_span_or_the_event() {
        let (_records, mut tap, subscriber) = recording();
        tracing::subscriber::with_default(subscriber, || {
            let deployment = LogSpan::deployment("blog", 42, None);
            let _d = deployment.enter();
            tracing::info!("pulling");
            deployment.record("generation", 7u32);
            let _s = LogSpan::service("blog", "web", 7).entered();
            tracing::info!(digest = "sha256:abc", "container started");
            tracing::warn!(stack = "shop", "a route of another stack moved");
            tracing::warn!(stack = "shop", generation = 3, "and its own generation");
        });
        let records = drain(&mut tap);
        let found: Vec<_> = records
            .iter()
            .map(|r| (r.message.as_str(), r.stack.as_deref(), r.generation))
            .collect();
        assert_eq!(
            found,
            [
                ("pulling", Some("blog"), None),
                ("container started", Some("blog"), Some(7)),
                ("a route of another stack moved", Some("shop"), None),
                ("and its own generation", Some("shop"), Some(3)),
            ]
        );
        assert_eq!(
            fields(&records[1])["spans"],
            json!([
                {"name": "deployment", "stack": "blog", "deployment_id": "42", "generation": 7},
                {"name": "service", "stack": "blog", "service": "web", "generation": 7},
            ])
        );
    }

    #[test]
    fn fields_are_what_parsing_hosters_own_json_line_gives() {
        let (_records, mut tap, recorder) = recording();
        let (buffer, formatter) = capture::subscriber(LogFormat::Json, "info");
        let emit = || {
            let _d = LogSpan::deployment("blog", 9, Some(2)).entered();
            tracing::warn!(attempt = 2, ok = false, url = %"http://web:80/", "health check failed");
        };
        tracing::subscriber::with_default(recorder, emit);
        tracing::subscriber::with_default(formatter, emit);
        let record = drain(&mut tap).pop().unwrap();
        let mut line = buffer.json().pop().unwrap();
        line.as_object_mut().unwrap().remove("timestamp");
        let parsed = ParsedLine::from(line.to_string().as_str());
        assert_eq!(record.level, parsed.level);
        assert_eq!(record.message, parsed.message);
        let parsed_fields: Value = serde_json::from_str(parsed.fields.as_deref().unwrap()).unwrap();
        assert_eq!(fields(&record), parsed_fields);
    }

    #[test]
    fn nothing_is_recorded_without_a_sink() {
        capture::global();
        let records = HosterRecords::default();
        let subscriber = Registry::default().with(records.layer());
        let (sink, mut tap) = LogSink::tapped(16);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("before the recorder");
            records.connect(sink);
            tracing::info!("while it runs");
            records.disconnect();
            tracing::info!("after it stopped");
        });
        let messages: Vec<_> = drain(&mut tap).into_iter().map(|r| r.message).collect();
        assert_eq!(messages, ["while it runs"]);
    }

    #[test]
    fn hoster_is_one_source_under_the_rate_cap() {
        let (records, mut tap, subscriber) = recording();
        let excess = 5;
        tracing::subscriber::with_default(subscriber, || {
            // Spread over two seconds at most, so a second's turn inside the burst still
            // leaves the excess of one second dropped.
            for i in 0..2 * RateCap::LINES_PER_SECOND + excess {
                tracing::info!(i, "busy");
            }
        });
        let kept = drain(&mut tap);
        let capped = records.capped();
        assert!(capped > 0, "some events were capped");
        let lines = kept.iter().filter(|r| !r.marker).count() as u64;
        assert_eq!(
            lines + capped,
            u64::from(2 * RateCap::LINES_PER_SECOND + excess)
        );
        // A gap is marked once the next event is kept, in a later second.
        std::thread::sleep(std::time::Duration::from_millis(1_100));
        tracing::subscriber::with_default(Registry::default().with(records.layer()), || {
            tracing::info!("calm again");
        });
        let after = drain(&mut tap);
        let markers: Vec<_> = kept.iter().chain(&after).filter(|r| r.marker).collect();
        let marked: u64 = markers
            .iter()
            .map(|m| {
                assert_eq!(m.service, "hoster");
                assert_eq!(m.stream, Stream::Hoster);
                assert_eq!(m.level, Some(store::Level::Warn));
                m.message.split(' ').next().unwrap().parse::<u64>().unwrap()
            })
            .sum();
        assert_eq!(marked, capped);
        assert_eq!(after.last().unwrap().message, "calm again");
    }
}
