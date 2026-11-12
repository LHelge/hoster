//! Parsing one printed line (ADR-0067 §3).
//!
//! A line that parses as a JSON object is mapped by the keys loggers commonly use:
//!
//! | Part | Keys, first present wins |
//! |---|---|
//! | level | `level`, `severity`, `lvl` |
//! | message | `message`, `msg`, then `fields.message` as `tracing-subscriber`'s JSON writes it |
//! | fields | every other key, kept as JSON |
//!
//! Level names are read in any case and folded onto [`Level`] (`warning` → warn, `fatal` and
//! `critical` → error, …). Two numeric scales are read: pino and bunyan (10, 20, …, 60) and
//! syslog (0–7). Any other number, and any unknown name, gives no level and keeps the key in
//! the fields. OpenTelemetry's severity numbers (1–24) overlap syslog's and are not read here:
//! they arrive over OTLP, whose receiver maps them itself.
//!
//! Any other line, a JSON value that is not an object included, is kept whole as the message
//! with no level: levels are never guessed in text (`ERROR …` has none). A line longer than
//! [`ParsedLine::MAX_LEN`] is cut at a character boundary, marked as cut and kept as text, since
//! it no longer parses.

use serde_json::{Map, Value};

use crate::store::Level;

/// One printed line, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLine {
    /// `None` for text lines and for JSON lines without a level the parser knows.
    pub level: Option<Level>,
    /// The message, or the whole (possibly cut) line for text.
    pub message: String,
    /// The JSON object of a JSON line's other keys, as text; `None` when there are none.
    pub fields: Option<String>,
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

    /// A text line: kept whole, no level.
    fn text(message: &str, cut: bool) -> Self {
        Self {
            level: None,
            message: message.to_owned(),
            fields: None,
            cut,
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
            cut: false,
        }
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

/// Parses one line, without its trailing newline (a trailing `\r` is dropped too). Never fails:
/// what is not a JSON object is text.
impl From<&str> for ParsedLine {
    fn from(line: &str) -> Self {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.len() > Self::MAX_LEN {
            return Self::text(&line[..line.floor_char_boundary(Self::MAX_LEN)], true);
        }
        match serde_json::from_str::<Value>(line) {
            Ok(Value::Object(object)) => Self::object(object),
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
