//! The stack file's JSON Schema (draft 2020-12), for editors with TOML schema support (Taplo /
//! VS Code "Even Better TOML", JetBrains): completion, hover docs and validation while writing a
//! `*.stack.toml`.
//!
//! It is generated from the raw serde types in `raw.rs` with `schemars`, so a field added to the
//! parser is in the schema too: the field docs there become the `description`s, and
//! `deny_unknown_fields` becomes `additionalProperties: false`. The string rules the parser checks
//! with code (names, hosts, prefixes, durations, …) are written once here as [`Pattern`]s, and
//! tests check each one against the parser's own checks.
//!
//! The schema is published at [`StackSchema::URL`] (committed as `site/static/stack.schema.json`)
//! and served by the admin app at [`StackSchema::ADMIN_PATH`]. Rules that need more than one
//! value at a time stay parser-only; `RawStack`'s doc comment, which becomes the schema's
//! top-level description, lists them. So do a few rules on single values that a regular
//! expression can't state well: durations that overflow, or whose fraction isn't a whole number
//! of nanoseconds (`"0.001h"`).

use std::fmt;
use std::sync::LazyLock;
use std::time::Duration;

use schemars::generate::SchemaSettings;
use schemars::transform::RecursiveTransform;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};

use super::Cpus;
use super::raw::RawStack;

/// The generated schema, as pretty-printed JSON.
#[derive(Debug)]
pub struct StackSchema(String);

impl StackSchema {
    /// Where the documentation site publishes the schema; also its `$id`.
    pub const URL: &str = "https://docs.hoster.lhelge.se/stack.schema.json";
    /// Where the admin app serves the schema of the stack format version it supports.
    pub const ADMIN_PATH: &str = "/schema/stack-v1.json";

    /// The schema of the stack format this binary parses.
    pub fn get() -> &'static StackSchema {
        static SCHEMA: LazyLock<StackSchema> = LazyLock::new(StackSchema::generate);
        &SCHEMA
    }

    fn generate() -> Self {
        let mut schema = SchemaSettings::draft2020_12()
            .for_deserialize()
            .with_transform(RecursiveTransform(Self::drop_null_default))
            .into_generator()
            .into_root_schema_for::<RawStack>();
        schema.insert("$id".into(), Self::URL.into());
        let mut json = serde_json::to_string_pretty(&schema).expect("a schema serializes");
        json.push('\n');
        StackSchema(json)
    }

    /// TOML has no null: an optional field is left out, so `"default": null` (what schemars
    /// derives for an `Option` with `#[serde(default)]`) would only mislead.
    fn drop_null_default(schema: &mut Schema) {
        if schema
            .get("default")
            .is_some_and(serde_json::Value::is_null)
        {
            schema.remove("default");
        }
    }
}

impl AsRef<str> for StackSchema {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Pretty-printed JSON, ending with a newline: what `hoster stack schema` prints.
impl fmt::Display for StackSchema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Unicode `White_Space`, which the parser's `str::trim` and humantime skip, spelled out for a
/// character class: `\s` means something else in each regex engine (ECMA-262 adds U+FEFF and
/// drops U+0085, Java's is ASCII-only).
macro_rules! whitespace {
    () => {
        r"\x09-\x0D \x85\xA0\u1680\u2000-\u200A\u2028\u2029\u202F\u205F\u3000"
    };
}

/// A registry port as `u16::from_str` reads it: 0–65535, leading zeros allowed.
macro_rules! registry_port {
    () => {
        r"0*(6553[0-5]|655[0-2][0-9]|65[0-4][0-9]{2}|6[0-4][0-9]{3}|[1-5][0-9]{4}|[0-9]{1,4})"
    };
}

/// A uid or gid of a config file: up to five digits, at most 65535.
macro_rules! file_id {
    () => {
        "(6553[0-5]|655[0-2][0-9]|65[0-4][0-9]{2}|6[0-4][0-9]{3}|[0-5][0-9]{4}|[0-9]{1,4})"
    };
}

/// One humantime number: digits, with whitespace allowed between them and before the unit
/// (`"1 000 s"`).
macro_rules! duration_number {
    () => {
        concat!("[0-9][0-9", whitespace!(), "]*")
    };
}

/// The parser's string rules as regular expressions, in the subset that ECMA-262, Rust's `regex`
/// and Java agree on (no look-around, no `\s`), since editors validate with any of them. One
/// difference remains: Java's `$` also matches before a final line terminator, so a JetBrains IDE
/// accepts e.g. a name ending in `\n` that hoster rejects.
struct Pattern;

impl Pattern {
    /// Stack, service and volume names ([`Name`](super::Name)).
    pub const NAME: &str = "^[a-z][a-z0-9-]{0,30}$";
    /// A lowercase FQDN of at least two labels ([`Host`](super::Host)); see [`Field::host`] for
    /// the length and numeric-TLD rules.
    pub const HOST: &str =
        r"^([a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$";
    /// A host ending in an all-digit label, i.e. an IP address.
    pub const NUMERIC_TLD: &str = r"\.[0-9]+$";
    /// `/`, or `/`-separated segments of RFC 3986 `pchar`s without percent-encoding, none of
    /// them `.` or `..` ([`NormalizedPrefix`](super::NormalizedPrefix)).
    pub const PREFIX: &str = concat!(
        r"^(/|(/([A-Za-z0-9_~!$&'()*+,;=:@-][A-Za-z0-9_~!$&'()*+,;=:@.-]*",
        r"|\.[A-Za-z0-9_~!$&'()*+,;=:@-][A-Za-z0-9_~!$&'()*+,;=:@.-]*",
        r"|\.\.[A-Za-z0-9_~!$&'()*+,;=:@.-]+))+)$",
    );
    /// An absolute mount path without empty, `.` or `..` segments, `:`, `,` or control
    /// characters.
    pub const MOUNT_PATH: &str = concat!(
        r"^(/([^/:,.\x00-\x1f\x7f-\x9f][^/:,\x00-\x1f\x7f-\x9f]*",
        r"|\.[^/:,.\x00-\x1f\x7f-\x9f][^/:,\x00-\x1f\x7f-\x9f]*",
        r"|\.\.[^/:,\x00-\x1f\x7f-\x9f]+))+$",
    );
    /// An absolute config file path without empty, `.` or `..` components or NUL
    /// ([`ContainerPath`](super::ContainerPath)).
    pub const FILE_PATH: &str = concat!(
        r"^(/([^/.\x00][^/\x00]*",
        r"|\.[^/.\x00][^/\x00]*",
        r"|\.\.[^/\x00]+))+$",
    );
    /// A file mode: four octal digits without setuid, setgid or sticky bits
    /// ([`FileMode`](super::FileMode)).
    pub const FILE_MODE: &str = "^0[0-7]{3}$";
    /// A numeric `uid:gid`, each at most 65535 ([`FileOwner`](super::FileOwner)).
    pub const FILE_OWNER: &str = concat!("^", file_id!(), ":", file_id!(), "$");
    /// A size of at least 1 MiB ([`ByteSize`](super::ByteSize) with the floor of `shm_size` and
    /// `tmpfs`): for each unit, the numbers whose bytes reach 2²⁰, leading zeros allowed.
    pub const SIZE: &str = concat!(
        "^0*(",
        // n ≥ 1048576
        "([1-9][0-9]{7,}|[2-9][0-9]{6}|1[1-9][0-9]{5}|10[5-9][0-9]{4}|1049[0-9]{3}",
        "|1048[6-9][0-9]{2}|10485[89][0-9]|104857[6-9])B",
        // n ≥ 1049 (1048.576)
        "|([1-9][0-9]{4,}|[2-9][0-9]{3}|1[1-9][0-9]{2}|10[5-9][0-9]|1049)KB",
        // n ≥ 1024
        "|([1-9][0-9]{4,}|[2-9][0-9]{3}|1[1-9][0-9]{2}|10[3-9][0-9]|102[4-9])KiB",
        // n ≥ 2 (1.048576)
        "|([1-9][0-9]+|[2-9])MB",
        "|[1-9][0-9]*(MiB|GB|GiB)",
        ")$",
    );
    /// A `memory` limit of at least 8 MiB ([`Limits::MIN_MEMORY`](super::Limits::MIN_MEMORY)),
    /// written like [`SIZE`](Self::SIZE).
    pub const MEMORY: &str = concat!(
        "^0*(",
        // n ≥ 8388608
        "([1-9][0-9]{7,}|9[0-9]{6}|8[4-9][0-9]{5}|839[0-9]{4}|8389[0-9]{3}",
        "|8388[7-9][0-9]{2}|83886[1-9][0-9]|838860[89])B",
        // n ≥ 8389 (8388.608)
        "|([1-9][0-9]{4,}|9[0-9]{3}|8[4-9][0-9]{2}|839[0-9]|8389)KB",
        // n ≥ 8192
        "|([1-9][0-9]{4,}|9[0-9]{3}|8[2-9][0-9]{2}|819[2-9])KiB",
        // n ≥ 9 (8.388608)
        "|([1-9][0-9]+|9)MB",
        // n ≥ 8
        "|([1-9][0-9]+|[89])MiB",
        "|[1-9][0-9]*(GB|GiB)",
        ")$",
    );
    /// A path at or in `/proc`, `/sys` or `/dev`, which podman mounts over.
    pub const MOUNTED_OVER: &str = "^/(proc|sys|dev)(/|$)";
    /// An environment variable name.
    pub const ENV_NAME: &str = "^[A-Za-z_][A-Za-z0-9_]*$";
    /// A secret name, `<stack>/<name>`.
    pub const SECRET: &str = "^[a-z][a-z0-9-]{0,30}/[A-Za-z0-9._-]+$";
    /// `uid[:gid]`, each numeric or a name.
    pub const USER: &str = "^[A-Za-z0-9_][A-Za-z0-9_.-]*(:[A-Za-z0-9_][A-Za-z0-9_.-]*)?$";
    /// An OCI tag.
    pub const TAG: &str = "^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$";
    /// `registry/repository[:tag][@sha256:…]` with an explicit registry
    /// ([`ImageRef`](super::ImageRef)).
    pub const IMAGE: &str = concat!(
        r"^(([a-z0-9.-]*\.[a-z0-9.-]*|localhost)(:",
        registry_port!(),
        r")?|[a-z0-9.-]+:",
        registry_port!(),
        ")",
        r"/[a-z0-9]([a-z0-9._-]*[a-z0-9])?(/[a-z0-9]([a-z0-9._-]*[a-z0-9])?)*",
        r"(:[A-Za-z0-9_][A-Za-z0-9_.-]{0,127})?(@sha256:[0-9a-f]{64})?$",
    );
    /// A humantime duration such as `500ms`, `30s`, `1h 30m` or `1.5s`. A fraction can't be of
    /// nanoseconds.
    pub const DURATION: &str = concat!(
        "^[",
        whitespace!(),
        "]*(",
        duration_number!(),
        r"(\.[",
        whitespace!(),
        "]*",
        duration_number!(),
        r"(usec|us|µs|millis|msec|ms",
        r"|seconds|second|secs|sec|s|minutes|minute|mins|min|m|hours|hour|hrs|hr|h",
        r"|days|day|d|weeks|week|wks|wk|w|months|month|M|years|year|yrs|yr|y)",
        r"|(nanos|nsec|ns|usec|us|µs|millis|msec|ms",
        r"|seconds|second|secs|sec|s|minutes|minute|mins|min|m|hours|hour|hrs|hr|h",
        r"|days|day|d|weeks|week|wks|wk|w|months|month|M|years|year|yrs|yr|y))[",
        whitespace!(),
        "]*)+$",
    );
    /// A duration whose every number is zero.
    pub const ZERO_DURATION: &str = concat!(
        "^[",
        whitespace!(),
        "]*(0[0",
        whitespace!(),
        r"]*(\.[",
        whitespace!(),
        "]*0[0",
        whitespace!(),
        "]*)?[A-Za-zµ]+[",
        whitespace!(),
        "]*)+$",
    );
    /// `2xx`-style status classes or one status code.
    pub const EXPECT_STATUS: &str = "^([1-5]xx|[1-5][0-9][0-9])$";
    /// A health check path: starts with `/`, printable ASCII only.
    pub const HEALTH_PATH: &str = "^/[!-~]*$";
    /// A GitHub ref glob.
    pub const REF: &str = "^refs/";
    /// The program of an `exec` check: not blank, no surrounding whitespace, no NUL.
    pub const PROGRAM: &str = concat!(
        "^[^",
        whitespace!(),
        r"\x00]([^\x00]*[^",
        whitespace!(),
        r"\x00])?$",
    );
    /// Something besides whitespace.
    pub const NOT_BLANK: &str = concat!("[^", whitespace!(), "]");
    /// No NUL characters.
    pub const NO_NUL: &str = r"^[^\x00]*$";
}

/// Field schemas shared by several raw types, for `#[schemars(schema_with = "…")]`.
pub(super) struct Field;

impl Field {
    pub fn version(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "integer", "const": super::SUPPORTED_VERSION })
    }

    pub fn name(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string", "pattern": Pattern::NAME })
    }

    pub fn host(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": Pattern::HOST,
            "maxLength": 253,
            "not": { "pattern": Pattern::NUMERIC_TLD },
        })
    }

    pub fn prefix(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string", "pattern": Pattern::PREFIX })
    }

    pub fn image(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string", "pattern": Pattern::IMAGE })
    }

    pub fn tag(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string", "pattern": Pattern::TAG })
    }

    pub fn user(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string", "pattern": Pattern::USER })
    }

    pub fn duration(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string", "pattern": Pattern::DURATION })
    }

    /// A duration greater than zero.
    pub fn positive_duration(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": Pattern::DURATION,
            "not": { "pattern": Pattern::ZERO_DURATION },
        })
    }

    pub fn port(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "integer", "minimum": 1, "maximum": u16::MAX })
    }

    pub fn positive_integer(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "integer", "minimum": 1, "maximum": u32::MAX })
    }

    /// GitHub's repository ids are positive `i64`s, like every TOML integer.
    pub fn repository_id(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "integer", "minimum": 1, "maximum": i64::MAX })
    }

    pub fn non_empty(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string", "pattern": Pattern::NOT_BLANK })
    }

    pub fn strings(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "array", "items": { "type": "string" } })
    }

    pub fn env(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "object",
            "propertyNames": { "pattern": Pattern::ENV_NAME },
            "additionalProperties": { "type": "string" },
        })
    }

    pub fn secrets(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "object",
            "propertyNames": { "pattern": Pattern::ENV_NAME },
            "additionalProperties": { "type": "string", "pattern": Pattern::SECRET },
        })
    }

    /// One secret name, `<stack>/<name>`.
    pub fn secret(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string", "pattern": Pattern::SECRET })
    }

    /// A table of config files by container path: `T` is the raw file type.
    pub fn files<T: JsonSchema>(generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "object",
            "propertyNames": { "pattern": Pattern::FILE_PATH },
            "additionalProperties": generator.subschema_for::<T>(),
        })
    }

    pub fn file_mode(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string", "pattern": Pattern::FILE_MODE, "examples": ["0440"] })
    }

    pub fn file_owner(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": Pattern::FILE_OWNER,
            "examples": ["0:65534"],
        })
    }

    /// A table of named services: `T` is the raw service type.
    pub fn services<T: JsonSchema>(generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "object",
            "minProperties": 1,
            "propertyNames": { "pattern": Pattern::NAME },
            "additionalProperties": generator.subschema_for::<T>(),
        })
    }

    pub fn volumes(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "object",
            "propertyNames": { "pattern": Pattern::NAME },
            "additionalProperties": { "type": "string", "pattern": Pattern::MOUNT_PATH },
        })
    }

    /// A size of at least 1 MiB.
    pub fn size(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": Pattern::SIZE,
            "examples": ["64MiB", "1GiB"],
        })
    }

    /// A `memory` limit: a size of at least 8 MiB.
    pub fn memory(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": Pattern::MEMORY,
            "examples": ["512MiB", "2GiB"],
        })
    }

    /// A `cpus` limit: an integer or a float from 0.01 to 1024.
    pub fn cpus(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "number",
            "minimum": Cpus::MIN,
            "maximum": Cpus::MAX,
            "examples": [0.5, 1.5, 2],
        })
    }

    /// The `tmpfs` table: mount path → size.
    pub fn tmpfs(generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "object",
            "propertyNames": {
                "pattern": Pattern::MOUNT_PATH,
                "not": { "pattern": Pattern::MOUNTED_OVER },
            },
            "additionalProperties": Self::size(generator),
        })
    }

    pub fn depends_on(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "array",
            "items": { "type": "string", "pattern": Pattern::NAME },
            "uniqueItems": true,
        })
    }

    pub fn refs(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "array",
            "items": { "type": "string", "pattern": Pattern::REF },
            "minItems": 1,
        })
    }

    pub fn exec(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "array",
            "prefixItems": [{ "type": "string", "pattern": Pattern::PROGRAM }],
            "items": { "type": "string", "pattern": Pattern::NO_NUL },
            "minItems": 1,
        })
    }

    pub fn expect_status(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": Pattern::EXPECT_STATUS,
            "examples": ["2xx", "204"],
        })
    }

    pub fn health_path(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string", "pattern": Pattern::HEALTH_PATH })
    }

    /// A duration as a stack file writes it, for a field's `default`, e.g. `"30s"`. Defaults go
    /// in the field's `extend`: schemars overwrites one set by `schema_with` with the serde
    /// default, which is `null` for an `Option`.
    pub fn default_duration(duration: Duration) -> String {
        humantime::format_duration(duration).to_string()
    }
}

#[cfg(test)]
mod tests;
