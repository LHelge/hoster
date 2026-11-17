//! Parsing one printed line (ADR-0067 §3).
//!
//! A line that parses as a JSON object is mapped by the keys loggers commonly use:
//!
//! | Part | Keys, first present wins |
//! |---|---|
//! | level | `level`, `severity`, `lvl` |
//! | message | `message`, `msg`, then `fields.message` as `tracing-subscriber`'s JSON writes it |
//! | trace id | `trace_id`, `traceId`, then the same keys inside `fields` (ADR-0072) |
//! | span id | `span_id`, `spanId`, then the same keys inside `fields` (ADR-0072) |
//! | fields | every other key, kept as JSON |
//!
//! Level names are read in any case and folded onto [`Level`] (`warning` → warn, `fatal` and
//! `critical` → error, …). Two numeric scales are read: pino and bunyan (10, 20, …, 60) and
//! syslog (0–7). Any other number, and any unknown name, gives no level and keeps the key in
//! the fields. OpenTelemetry's severity numbers (1–24) overlap syslog's and are not read from
//! lines: they arrive over OTLP, mapped by [`OtlpSeverity`] onto the same levels.
//!
//! The ids are read from a printed line only (`From<&str>`), not from the object hoster's own
//! layer maps, which has its ids from its spans. An id is
//! taken when it is a well-formed W3C id: 32 hex digits for a trace and 16 for a span, in
//! either case, not all zero. A span id is taken only beside a trace id from the same line.
//! Anything else (a number, a shorter id, a span id on its own) stays a field.
//!
//! Any other line, a JSON value that is not an object included, is kept whole as the message
//! with no level: levels are never guessed in text (`ERROR …` has none). A line longer than
//! [`ParsedLine::MAX_LEN`] is cut at a character boundary, marked as cut and kept as text, since
//! it no longer parses.

use serde_json::{Map, Value};

use crate::store::{HexId, Level, SpanId, TraceId};

/// One printed line, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLine {
    /// `None` for text lines and for JSON lines without a level the parser knows.
    pub level: Option<Level>,
    /// The message, or the whole (possibly cut) line for text.
    pub message: String,
    /// The JSON object of a JSON line's other keys, as text; `None` when there are none.
    pub fields: Option<String>,
    /// The trace a JSON line names (ADR-0072); always `None` for text.
    pub trace_id: Option<TraceId>,
    /// The span a JSON line names, only beside [`ParsedLine::trace_id`].
    pub span_id: Option<SpanId>,
    /// The line was longer than [`ParsedLine::MAX_LEN`] and was cut.
    pub cut: bool,
}

impl ParsedLine {
    /// The longest line kept whole, in bytes (ADR-0067 §3).
    pub const MAX_LEN: usize = 16 * 1024;
    /// The keys a level is read from, first present wins.
    const LEVEL_KEYS: [&str; 3] = ["level", "severity", "lvl"];
    /// The top-level keys a message is read from, first present wins.
    const MESSAGE_KEYS: [&str; 2] = ["message", "msg"];
    /// `tracing-subscriber`'s JSON formatter nests the event's fields, message included, here.
    const TRACING_FIELDS: &str = "fields";
    /// The keys a trace id is read from, first present wins (ADR-0072).
    const TRACE_ID_KEYS: [&str; 2] = ["trace_id", "traceId"];
    /// The keys a span id is read from, first present wins (ADR-0072).
    const SPAN_ID_KEYS: [&str; 2] = ["span_id", "spanId"];

    /// A text line: kept whole, no level.
    fn text(message: &str, cut: bool) -> Self {
        Self {
            level: None,
            message: message.to_owned(),
            fields: None,
            trace_id: None,
            span_id: None,
            cut,
        }
    }

    /// The JSON object of a printed line: its ids (ADR-0072), then the common keys.
    fn printed(mut object: Map<String, Value>) -> Self {
        let (trace_id, span_id) = Self::take_ids(&mut object);
        Self {
            trace_id,
            span_id,
            ..Self::object(object)
        }
    }

    /// A JSON object, mapped by the common keys.
    fn object(mut object: Map<String, Value>) -> Self {
        let level = Self::take_level(&mut object);
        let message = Self::take_message(&mut object).unwrap_or_default();
        let fields = (!object.is_empty()).then(|| Value::Object(object).to_string());
        Self {
            level,
            message,
            fields,
            trace_id: None,
            span_id: None,
            cut: false,
        }
    }

    /// Removes and returns the trace id and, only beside one, the span id. A span id without
    /// a trace id stays in the object: nothing can look a span up without its trace.
    fn take_ids(object: &mut Map<String, Value>) -> (Option<TraceId>, Option<SpanId>) {
        let Some(trace_id) = Self::take_id(object, Self::TRACE_ID_KEYS) else {
            return (None, None);
        };
        (Some(trace_id), Self::take_id(object, Self::SPAN_ID_KEYS))
    }

    /// Removes and returns the id of the first of `keys` present, looked for at the top level
    /// and then inside `fields`, if its value is a well-formed id. Any other value stays where
    /// it was, and a `fields` object the id leaves empty stays as an empty one.
    fn take_id<const N: usize>(
        object: &mut Map<String, Value>,
        keys: [&str; 2],
    ) -> Option<HexId<N>> {
        let holder = if keys.iter().any(|key| object.contains_key(*key)) {
            object
        } else {
            object.get_mut(Self::TRACING_FIELDS)?.as_object_mut()?
        };
        let key = keys.iter().find(|key| holder.contains_key(**key))?;
        let id = holder.get(*key).and_then(Self::id_of)?;
        holder.remove(*key);
        Some(id)
    }

    /// The id a JSON value writes: a string of exactly `2 * N` hex digits in either case, not
    /// all zero.
    fn id_of<const N: usize>(value: &Value) -> Option<HexId<N>> {
        let id: HexId<N> = value.as_str()?.to_ascii_lowercase().parse().ok()?;
        id.is_valid().then_some(id)
    }

    /// Removes and returns the level from the first level key present, if it names a level.
    /// An unknown value stays in the object.
    fn take_level(object: &mut Map<String, Value>) -> Option<Level> {
        let key = Self::LEVEL_KEYS.iter().find(|k| object.contains_key(**k))?;
        let level = object.get(*key).and_then(Self::level_of)?;
        object.remove(*key);
        Some(level)
    }

    /// The level a JSON value names: a name in any case, or a number on a known scale (also
    /// when written as a string, `"30"`).
    fn level_of(value: &Value) -> Option<Level> {
        match value {
            Value::String(name) => Self::level_named(name)
                .or_else(|| name.trim().parse().ok().and_then(Self::level_numbered)),
            Value::Number(n) => n.as_i64().and_then(Self::level_numbered),
            _ => None,
        }
    }

    fn level_named(name: &str) -> Option<Level> {
        Some(match name.trim().to_ascii_lowercase().as_str() {
            "trace" | "trc" | "verbose" | "finest" => Level::Trace,
            "debug" | "dbg" | "fine" => Level::Debug,
            "info" | "inf" | "information" | "informational" | "notice" => Level::Info,
            "warn" | "wrn" | "warning" => Level::Warn,
            "error" | "err" | "eror" | "fatal" | "ftl" | "critical" | "crit" | "alert"
            | "emerg" | "emergency" | "panic" | "dpanic" | "severe" => Level::Error,
            _ => return None,
        })
    }

    /// pino/bunyan (10 trace … 60 fatal) and syslog (0 emergency … 7 debug).
    fn level_numbered(n: i64) -> Option<Level> {
        Some(match n {
            10 => Level::Trace,
            20 => Level::Debug,
            30 => Level::Info,
            40 => Level::Warn,
            50 | 60 => Level::Error,
            0..=3 => Level::Error,
            4 => Level::Warn,
            5 | 6 => Level::Info,
            7 => Level::Debug,
            _ => return None,
        })
    }

    /// Removes and returns the message: the first top-level message key present, else
    /// `fields.message` (dropping `fields` if nothing else is left in it).
    fn take_message(object: &mut Map<String, Value>) -> Option<String> {
        if let Some(key) = Self::MESSAGE_KEYS.iter().find(|k| object.contains_key(**k)) {
            return object.remove(*key).map(Self::message_of);
        }
        let Some(Value::Object(fields)) = object.get_mut(Self::TRACING_FIELDS) else {
            return None;
        };
        let message = fields.remove("message")?;
        if fields.is_empty() {
            object.remove(Self::TRACING_FIELDS);
        }
        Some(Self::message_of(message))
    }

    /// A string as it is; anything else (a number, an object) as its JSON text.
    fn message_of(value: Value) -> String {
        match value {
            Value::String(s) => s,
            other => other.to_string(),
        }
    }
}

/// An OTLP log record's severity (ADR-0069 §6): its `SeverityNumber` and `severity_text`, as
/// the payload gives them.
///
/// It maps onto the five levels as a JSON line's level does: the number by its ranges
/// (1–4 trace, 5–8 debug, 9–12 info, 13–16 warn, 17–24 error and fatal), else, when the
/// number is unspecified (0) or out of range, the text by the names the line parser knows.
/// Neither gives no level, and none is guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OtlpSeverity<'a> {
    pub number: i32,
    pub text: &'a str,
}

impl From<OtlpSeverity<'_>> for Option<Level> {
    fn from(severity: OtlpSeverity<'_>) -> Self {
        match severity.number {
            1..=4 => Some(Level::Trace),
            5..=8 => Some(Level::Debug),
            9..=12 => Some(Level::Info),
            13..=16 => Some(Level::Warn),
            17..=24 => Some(Level::Error),
            _ => ParsedLine::level_named(severity.text),
        }
    }
}

/// Maps a JSON object by the common keys, as a JSON line holding it would be, but for its trace
/// and span ids, which are left as fields. hoster's own layer (ADR-0068) builds the object its
/// JSON formatter would write and maps it here.
impl From<Map<String, Value>> for ParsedLine {
    fn from(object: Map<String, Value>) -> Self {
        Self::object(object)
    }
}

/// Parses one line, without its trailing newline (a trailing `\r` is dropped too). Never fails:
/// what is not a JSON object is text. Only here are a JSON object's trace and span ids read
/// (ADR-0072).
impl From<&str> for ParsedLine {
    fn from(line: &str) -> Self {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.len() > Self::MAX_LEN {
            return Self::text(&line[..line.floor_char_boundary(Self::MAX_LEN)], true);
        }
        match serde_json::from_str::<Value>(line) {
            Ok(Value::Object(object)) => Self::printed(object),
            _ => Self::text(line, false),
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde_json::json;

    use super::*;

    fn parse(line: &str) -> ParsedLine {
        ParsedLine::from(line)
    }

    #[test]
    fn otlp_severities_map_by_number_then_by_text() {
        let level = |number, text| Option::<Level>::from(OtlpSeverity { number, text });
        for (numbers, expected) in [
            (1..=4, Level::Trace),
            (5..=8, Level::Debug),
            (9..=12, Level::Info),
            (13..=16, Level::Warn),
            (17..=24, Level::Error),
        ] {
            for number in numbers {
                assert_eq!(level(number, ""), Some(expected), "{number}");
            }
        }
        // The number wins over the text.
        assert_eq!(level(13, "ERROR"), Some(Level::Warn));
        // Unspecified or out of range: the text, by the line parser's names.
        assert_eq!(level(0, "Warning"), Some(Level::Warn));
        assert_eq!(level(0, "FATAL"), Some(Level::Error));
        assert_eq!(level(25, "info"), Some(Level::Info));
        // Neither: no level, as for a text line.
        assert_eq!(level(0, ""), None);
        assert_eq!(level(-3, "loud"), None);
    }

    /// The fields as JSON, for comparing without depending on key order.
    fn fields(line: &ParsedLine) -> Value {
        line.fields
            .as_deref()
            .map_or(Value::Null, |f| serde_json::from_str(f).unwrap())
    }

    #[test]
    fn tracing_subscriber_json_maps_level_message_and_keeps_the_rest() {
        let line = parse(
            r#"{"timestamp":"2026-11-12T10:00:00.000Z","level":"WARN","fields":{"message":"slow query","ms":812},"target":"blog::db","span":{"name":"request","id":"7"},"spans":[{"name":"request","id":"7"}]}"#,
        );
        assert_eq!(line.level, Some(Level::Warn));
        assert_eq!(line.message, "slow query");
        assert_eq!(
            fields(&line),
            json!({
                "timestamp": "2026-11-12T10:00:00.000Z",
                "fields": {"ms": 812},
                "target": "blog::db",
                "span": {"name": "request", "id": "7"},
                "spans": [{"name": "request", "id": "7"}],
            })
        );
        assert!(!line.cut);
    }

    #[test]
    fn tracing_fields_holding_only_the_message_are_dropped() {
        let line = parse(r#"{"level":"INFO","fields":{"message":"listening"},"target":"aphid"}"#);
        assert_eq!(line.message, "listening");
        assert_eq!(fields(&line), json!({"target": "aphid"}));
    }

    #[test]
    fn flattened_tracing_events_and_aphid_server() {
        // `flatten_event(true)`, which aphid-server's APHID_LOG_FORMAT=json writes.
        let line = parse(
            r#"{"timestamp":"2026-11-12T10:00:00Z","level":"INFO","message":"GET /blog 200","target":"aphid_server","status":200}"#,
        );
        assert_eq!(line.level, Some(Level::Info));
        assert_eq!(line.message, "GET /blog 200");
        assert_eq!(fields(&line)["status"], json!(200));
    }

    #[test]
    fn other_loggers() {
        let cases = [
            // pino
            (
                r#"{"level":30,"time":1731405600000,"pid":7,"hostname":"x","msg":"ready"}"#,
                Some(Level::Info),
                "ready",
            ),
            // bunyan
            (
                r#"{"name":"api","level":50,"msg":"boom","v":0}"#,
                Some(Level::Error),
                "boom",
            ),
            // zap
            (
                r#"{"level":"warn","ts":1731405600.1,"caller":"main.go:12","msg":"retrying"}"#,
                Some(Level::Warn),
                "retrying",
            ),
            // logrus
            (
                r#"{"level":"warning","msg":"disk low","time":"2026-11-12T10:00:00Z"}"#,
                Some(Level::Warn),
                "disk low",
            ),
            // structlog
            (
                r#"{"event":"login","level":"debug","timestamp":"2026-11-12T10:00:00Z"}"#,
                Some(Level::Debug),
                "",
            ),
            // GELF-style syslog numbers
            (
                r#"{"severity":3,"message":"failed"}"#,
                Some(Level::Error),
                "failed",
            ),
            (
                r#"{"lvl":"Fatal","msg":"dying"}"#,
                Some(Level::Error),
                "dying",
            ),
            (
                r#"{"level":"30","msg":"numeric text"}"#,
                Some(Level::Info),
                "numeric text",
            ),
        ];
        for (raw, level, message) in cases {
            let line = parse(raw);
            assert_eq!(
                (line.level, line.message.as_str()),
                (level, message),
                "{raw}"
            );
        }
    }

    #[test]
    fn the_first_level_key_present_wins_and_an_unknown_value_stays() {
        let line = parse(r#"{"level":"error","severity":"info","msg":"x"}"#);
        assert_eq!(line.level, Some(Level::Error));
        assert_eq!(fields(&line), json!({"severity": "info"}));

        for raw in [
            r#"{"level":"loud","msg":"x"}"#,
            r#"{"level":35,"msg":"x"}"#,
            r#"{"level":null,"msg":"x"}"#,
            r#"{"level":true,"msg":"x"}"#,
        ] {
            let line = parse(raw);
            assert_eq!(line.level, None, "{raw}");
            assert!(fields(&line).get("level").is_some(), "{raw}");
        }
    }

    const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
    const SPAN: &str = "00f067aa0ba902b7";

    fn ids(line: &ParsedLine) -> (Option<String>, Option<String>) {
        (
            line.trace_id.map(|id| id.to_string()),
            line.span_id.map(|id| id.to_string()),
        )
    }

    fn both() -> (Option<String>, Option<String>) {
        (Some(TRACE.into()), Some(SPAN.into()))
    }

    #[test]
    fn trace_and_span_ids_are_read_in_both_spellings_and_leave_the_fields() {
        for (trace_key, span_key) in [
            ("trace_id", "span_id"),
            ("traceId", "spanId"),
            ("trace_id", "spanId"),
            ("traceId", "span_id"),
        ] {
            let raw = json!({"level": "info", "msg": "paid", trace_key: TRACE, span_key: SPAN, "user": 7});
            let line = parse(&raw.to_string());
            assert_eq!(ids(&line), both(), "{raw}");
            assert_eq!(fields(&line), json!({"user": 7}), "{raw}");
            assert_eq!(
                (line.level, line.message.as_str()),
                (Some(Level::Info), "paid")
            );

            // The same keys inside `fields`, where tracing's JSON formatter puts them.
            let raw = json!({"level": "INFO", "fields": {"message": "paid", trace_key: TRACE, span_key: SPAN, "user": 7}, "target": "shop"});
            let line = parse(&raw.to_string());
            assert_eq!(ids(&line), both(), "{raw}");
            assert_eq!(
                fields(&line),
                json!({"fields": {"user": 7}, "target": "shop"}),
                "{raw}"
            );
            assert_eq!(line.message, "paid");
        }
        // Nothing but ids: no fields are left.
        let line = parse(&json!({"trace_id": TRACE, "span_id": SPAN}).to_string());
        assert_eq!(ids(&line), both());
        assert_eq!(line.fields, None);
    }

    #[test]
    fn the_first_id_key_present_wins_at_the_top_level_before_fields() {
        let other = "11111111111111111111111111111111";
        let line = parse(&json!({"trace_id": TRACE, "traceId": other}).to_string());
        assert_eq!(ids(&line).0.as_deref(), Some(TRACE));
        assert_eq!(fields(&line), json!({"traceId": other}));

        let line = parse(
            &json!({"traceId": TRACE, "fields": {"trace_id": other, "span_id": SPAN}}).to_string(),
        );
        assert_eq!(ids(&line), both(), "each id from where its key is found");
        assert_eq!(fields(&line), json!({"fields": {"trace_id": other}}));

        // The first key present decides, as for the level: an unusable value is not skipped.
        let line = parse(&json!({"trace_id": "none", "traceId": TRACE}).to_string());
        assert_eq!(ids(&line), (None, None));
        assert_eq!(fields(&line), json!({"trace_id": "none", "traceId": TRACE}));
        let line = parse(&json!({"trace_id": 7, "fields": {"trace_id": TRACE}}).to_string());
        assert_eq!(ids(&line), (None, None));
    }

    #[test]
    fn upper_case_ids_are_stored_in_lower_case() {
        let line = parse(
            &json!({"trace_id": TRACE.to_uppercase(), "span_id": SPAN.to_uppercase()}).to_string(),
        );
        assert_eq!(ids(&line), both());
        assert_eq!(line.fields, None);
    }

    #[test]
    fn values_that_are_not_w3c_ids_stay_fields() {
        for value in [
            json!("4bf92f3577b34da6a3ce929d0e0e473"),      // 31 digits
            json!("4bf92f3577b34da6a3ce929d0e0e47360"),    // 33
            json!("4bf92f3577b34da6a3ce929d0e0e473g"),     // not hex
            json!(" 4bf92f3577b34da6a3ce929d0e0e4736"),    // padded
            json!("4bf92f35-77b3-4da6-a3ce-929d0e0e4736"), // a UUID
            json!("00000000000000000000000000000000"),     // all zero
            json!("00f067aa0ba902b7"),                     // a span id
            json!("6790871234567890123"),                  // a vendor's decimal id
            json!(""),
            json!(1234),
            json!(null),
            json!(true),
            json!({"id": TRACE}),
            json!([TRACE]),
        ] {
            let raw = json!({"trace_id": value, "span_id": SPAN});
            let line = parse(&raw.to_string());
            assert_eq!(ids(&line), (None, None), "{raw}");
            assert_eq!(fields(&line), raw, "every key stays: {raw}");
        }
        for value in [
            json!("00f067aa0ba902b"),
            json!("00f067aa0ba902b7a"),
            json!("0000000000000000"),
            json!(TRACE),
            json!(42),
            json!(null),
        ] {
            let raw = json!({"trace_id": TRACE, "span_id": value});
            let line = parse(&raw.to_string());
            assert_eq!(ids(&line), (Some(TRACE.into()), None), "{raw}");
            assert_eq!(fields(&line), json!({"span_id": value}), "{raw}");
        }
    }

    #[test]
    fn a_span_id_is_taken_only_beside_a_trace_id() {
        let line = parse(&json!({"span_id": SPAN, "msg": "x"}).to_string());
        assert_eq!(ids(&line), (None, None));
        assert_eq!(fields(&line), json!({"span_id": SPAN}));

        // tracing's own span objects are not ids, and are not looked into.
        let raw =
            json!({"span": {"name": "request", "trace_id": TRACE}, "spans": [{"span_id": SPAN}]});
        let line = parse(&raw.to_string());
        assert_eq!(ids(&line), (None, None));
        assert_eq!(fields(&line), raw);

        let line = parse(&json!({"trace_id": TRACE, "msg": "x"}).to_string());
        assert_eq!(ids(&line), (Some(TRACE.into()), None));
        assert_eq!(line.fields, None);
    }

    #[test]
    fn a_fields_object_the_ids_leave_empty_is_kept_as_an_empty_one_is() {
        let emptied =
            parse(&json!({"msg": "x", "fields": {"trace_id": TRACE, "span_id": SPAN}}).to_string());
        let empty = parse(&json!({"msg": "x", "fields": {}}).to_string());
        assert_eq!(ids(&emptied), both());
        assert_eq!(emptied.fields, empty.fields);
        assert_eq!(fields(&empty), json!({"fields": {}}));

        // With the message in it too, it goes as it does when the message alone was there.
        let line = parse(&json!({"fields": {"message": "x", "trace_id": TRACE}}).to_string());
        assert_eq!(line.message, "x");
        assert_eq!(line.fields, None);

        // `fields` that is not an object is not looked into.
        let line = parse(&json!({"fields": TRACE}).to_string());
        assert_eq!(ids(&line), (None, None));
    }

    #[test]
    fn ids_are_read_from_printed_lines_only() {
        // Text is never searched, and a cut line is text.
        let text = format!("trace_id={TRACE} span_id={SPAN}");
        assert_eq!(parse(&text), ParsedLine::text(&text, false));
        let long = json!({"trace_id": TRACE, "msg": "x".repeat(ParsedLine::MAX_LEN)}).to_string();
        assert_eq!(ids(&parse(&long)), (None, None));

        // The object hoster's own layer maps keeps them as fields (ADR-0068).
        let Value::Object(object) = json!({"message": "x", "trace_id": TRACE, "span_id": SPAN})
        else {
            unreachable!()
        };
        let line = ParsedLine::from(object);
        assert_eq!(ids(&line), (None, None));
        assert_eq!(fields(&line), json!({"trace_id": TRACE, "span_id": SPAN}));
    }

    #[test]
    fn a_message_that_is_not_a_string_is_its_json() {
        assert_eq!(parse(r#"{"msg":42}"#).message, "42");
        assert_eq!(parse(r#"{"message":{"a":1}}"#).message, r#"{"a":1}"#);
    }

    #[test]
    fn json_without_a_message_keeps_every_key() {
        let line = parse(r#"{"level":"info","user":"me"}"#);
        assert_eq!(line.message, "");
        assert_eq!(fields(&line), json!({"user": "me"}));
        assert_eq!(parse("{}").fields, None);
    }

    #[test]
    fn text_lines_are_kept_whole_with_no_level() {
        for raw in [
            "ERROR something broke",
            r#"192.0.2.1 - - [12/Nov/2026:10:00:00 +0000] "GET / HTTP/1.1" 200 612 "-" "curl/8""#,
            "",
            "{not json",
            "42",
            r#""a quoted string""#,
            r#"["an","array"]"#,
            "null",
        ] {
            let line = parse(raw);
            assert_eq!(line, ParsedLine::text(raw, false), "{raw}");
        }
    }

    #[test]
    fn a_trailing_carriage_return_is_dropped() {
        assert_eq!(parse("hello\r").message, "hello");
        assert_eq!(parse("{\"msg\":\"hi\"}\r").message, "hi");
    }

    #[test]
    fn duplicate_keys_keep_the_last_value() {
        assert_eq!(parse(r#"{"msg":"first","msg":"second"}"#).message, "second");
    }

    #[test]
    fn long_lines_are_cut_at_a_character_boundary_and_kept_as_text() {
        let long = format!(r#"{{"msg":"{}"}}"#, "é".repeat(ParsedLine::MAX_LEN));
        let line = parse(&long);
        assert!(line.cut);
        assert_eq!(line.level, None);
        assert!(line.message.len() <= ParsedLine::MAX_LEN);
        assert!(line.message.len() > ParsedLine::MAX_LEN - 4);
        assert!(long.starts_with(&line.message));

        let exact = "x".repeat(ParsedLine::MAX_LEN);
        assert!(!parse(&exact).cut);
    }

    proptest! {
        #[test]
        fn any_line_parses_within_the_cap(line in ".{0,6000}") {
            let parsed = ParsedLine::from(line.as_str());
            let line = line.strip_suffix('\r').unwrap_or(&line);
            prop_assert!(parsed.message.len() <= ParsedLine::MAX_LEN);
            prop_assert_eq!(parsed.cut, line.len() > ParsedLine::MAX_LEN);
        }

        #[test]
        fn keys_the_parser_does_not_read_are_kept_as_they_were(
            map in prop::collection::btree_map("k_[a-z]{1,8}", "\\PC{0,12}", 0..6)
        ) {
            let object: Map<String, Value> =
                map.into_iter().map(|(k, v)| (k, Value::from(v))).collect();
            let parsed = ParsedLine::from(Value::Object(object.clone()).to_string().as_str());
            prop_assert_eq!(parsed.level, None);
            prop_assert_eq!(parsed.message, "");
            let stored: Map<String, Value> = parsed
                .fields
                .as_deref()
                .map(|f| serde_json::from_str(f).unwrap())
                .unwrap_or_default();
            prop_assert_eq!(stored, object);
        }
    }
}
