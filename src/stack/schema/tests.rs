use std::sync::LazyLock;

use jsonschema::{PatternOptions, Validator};
use serde_json::Value;

use super::{Field, StackSchema};
use crate::health;
use crate::stack::tests::{MINIMAL, dependency_example, files_example, worked_example};
use crate::stack::{
    DEFAULT_DRAIN_TIMEOUT, DEFAULT_HEALTH_PATH, DEFAULT_STOP_TIMEOUT, SUPPORTED_VERSION, Stack,
};

/// Where the schema is committed for the documentation site, relative to the crate root.
const COMMITTED_PATH: &str = "site/static/stack.schema.json";
const COMMITTED: &str = include_str!("../../../site/static/stack.schema.json");

/// The schema, compiled once with each regex engine editors' validators resemble: ECMA-like
/// backtracking (`fancy-regex`, jsonschema's default) and Rust's `regex`.
struct Validators {
    fancy: Validator,
    regex: Validator,
}

impl Validators {
    fn get() -> &'static Validators {
        static VALIDATORS: LazyLock<Validators> = LazyLock::new(|| {
            let schema = StackSchema::get().value();
            Validators {
                fancy: jsonschema::validator_for(&schema).expect("the schema compiles"),
                regex: jsonschema::options()
                    .with_pattern_options(PatternOptions::regex())
                    .build(&schema)
                    .expect("the schema compiles with the regex engine"),
            }
        });
        &VALIDATORS
    }

    /// Whether the schema accepts the TOML `source`; `false` when it is not TOML at all.
    #[track_caller]
    fn accepts(&self, source: &str) -> bool {
        let Ok(instance) = toml::from_str::<Value>(source) else {
            return false;
        };
        let fancy = self.fancy.is_valid(&instance);
        assert_eq!(
            fancy,
            self.regex.is_valid(&instance),
            "the regex engines disagree on:\n{source}"
        );
        fancy
    }

    /// The schema's errors for `source`, for assertion messages.
    fn errors(&self, source: &str) -> Vec<String> {
        let instance: Value = toml::from_str(source).expect("valid TOML");
        self.fancy
            .iter_errors(&instance)
            .map(|e| format!("{}: {e}", e.instance_path()))
            .collect()
    }
}

impl StackSchema {
    fn value(&self) -> Value {
        serde_json::from_str(self.as_ref()).expect("the schema is JSON")
    }
}

/// Asserts that the schema and the parser agree on every TOML value substituted for `@@` in
/// `template`, and that the values include both accepted and rejected ones.
#[track_caller]
fn agrees(template: &str, values: &[&str]) {
    let validators = Validators::get();
    let mut mismatches = Vec::new();
    let mut verdicts = (0, 0);
    for value in values {
        let source = template.replace("@@", value);
        let parser = Stack::parse(&source).is_ok();
        let schema = validators.accepts(&source);
        if parser {
            verdicts.0 += 1;
        } else {
            verdicts.1 += 1;
        }
        if parser != schema {
            mismatches.push(format!("{value}: parser accepts {parser}, schema {schema}"));
        }
    }
    assert!(
        mismatches.is_empty(),
        "schema and parser disagree:\n{}",
        mismatches.join("\n")
    );
    assert!(
        verdicts.0 > 0 && verdicts.1 > 0,
        "samples should include valid and invalid values"
    );
}

/// [`agrees`], with each value written as a TOML string.
#[track_caller]
fn agrees_on_strings(template: &str, values: &[&str]) {
    let literals: Vec<String> = values
        .iter()
        .map(|v| toml::Value::String((*v).to_owned()).to_string())
        .collect();
    agrees(
        template,
        &literals.iter().map(String::as_str).collect::<Vec<_>>(),
    );
}

/// `MINIMAL` with `lines` added to `[services.app]`, after its `image`.
fn with_app(lines: &str) -> String {
    let image = "image = \"ghcr.io/acme/app\"\n";
    MINIMAL.replace(image, &format!("{image}{lines}\n"))
}

#[test]
fn committed_schema_is_current() {
    let generated = StackSchema::get().to_string();
    assert!(
        COMMITTED == generated,
        "{COMMITTED_PATH} is out of date with the stack parser's types; regenerate it with\n\n    \
         cargo run -- stack schema > {COMMITTED_PATH}\n"
    );
}

#[test]
fn admin_path_names_the_supported_version() {
    assert_eq!(
        StackSchema::ADMIN_PATH,
        format!("/schema/stack-v{SUPPORTED_VERSION}.json")
    );
}

#[test]
fn defaults_are_published() {
    let schema = StackSchema::get().value();
    let duration = |d| Value::from(Field::default_duration(d));
    for (pointer, expected) in [
        ("/properties/drain_timeout", duration(DEFAULT_DRAIN_TIMEOUT)),
        (
            "/$defs/Service/properties/stop_timeout",
            duration(DEFAULT_STOP_TIMEOUT),
        ),
        ("/$defs/Route/properties/prefix", Value::from("/")),
        (
            "/$defs/HttpProbe/properties/path",
            Value::from(DEFAULT_HEALTH_PATH),
        ),
        (
            "/$defs/Health/properties/interval",
            duration(health::DEFAULT_INTERVAL),
        ),
        (
            "/$defs/Health/properties/healthy_after",
            Value::from(health::DEFAULT_HEALTHY_AFTER),
        ),
        (
            "/$defs/Health/properties/start_period",
            duration(health::DEFAULT_START_PERIOD),
        ),
    ] {
        assert_eq!(
            schema.pointer(&format!("{pointer}/default")),
            Some(&expected),
            "{pointer}"
        );
    }
}

/// Defaults that only the field docs state, since they depend on another field.
#[test]
fn documented_defaults_match_the_constants() {
    let schema = StackSchema::get().value();
    let description = |pointer: &str| {
        schema
            .pointer(&format!("{pointer}/description"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{pointer} has a description"))
            .replace('\n', " ")
    };
    let timeout = description("/$defs/Health/properties/timeout");
    for default in [health::DEFAULT_TIMEOUT, health::DEFAULT_EXEC_TIMEOUT] {
        let default = format!("`\"{}\"`", Field::default_duration(default));
        assert!(timeout.contains(&default), "{timeout:?} lacks {default}");
    }
    let service_health = description("/$defs/Service/properties/health");
    let path = format!("`GET {DEFAULT_HEALTH_PATH}`");
    let settle = format!("{} s", health::DEFAULT_SETTLE.as_secs());
    assert!(
        service_health.contains(&path),
        "{service_health:?} lacks {path}"
    );
    assert!(
        service_health.contains(&settle),
        "{service_health:?} lacks {settle}"
    );
}

#[test]
fn schema_is_valid_draft_2020_12() {
    let schema = StackSchema::get().value();
    jsonschema::meta::validate(&schema).expect("valid against its meta-schema");
    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(schema["$id"], StackSchema::URL);
}

/// Walks every subschema, calling `f` with its JSON pointer.
fn walk(path: &str, schema: &Value, f: &mut impl FnMut(&str, &serde_json::Map<String, Value>)) {
    match schema {
        Value::Object(map) => {
            f(path, map);
            for (key, value) in map {
                walk(&format!("{path}/{key}"), value, f);
            }
        }
        Value::Array(items) => {
            for (i, value) in items.iter().enumerate() {
                walk(&format!("{path}/{i}"), value, f);
            }
        }
        _ => {}
    }
}

#[test]
fn every_field_is_described_and_every_table_closed() {
    let schema = StackSchema::get().value();
    let mut problems = Vec::new();
    walk("#", &schema, &mut |path, map| {
        let Some(Value::Object(properties)) = map.get("properties") else {
            return;
        };
        // `dependentSchemas` only forbids keys; it describes no table of its own.
        if path.contains("/dependentSchemas/") {
            return;
        }
        if map.get("additionalProperties") != Some(&Value::Bool(false)) {
            problems.push(format!("{path} allows unknown fields"));
        }
        for (key, property) in properties {
            let described = property
                .get("description")
                .and_then(Value::as_str)
                .is_some_and(|d| !d.trim().is_empty());
            if !described {
                problems.push(format!("{path}/properties/{key} has no description"));
            }
        }
    });
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn documented_stacks_validate() {
    let validators = Validators::get();
    for (name, source) in [
        ("stack-format.md §3", worked_example()),
        ("stack-format.md §3.2", dependency_example()),
        ("stack-format.md §3.3", files_example()),
        (
            "site/docs.stack.toml",
            include_str!("../../../site/docs.stack.toml"),
        ),
        ("MINIMAL", MINIMAL),
    ] {
        Stack::parse(source).unwrap_or_else(|e| panic!("{name} parses: {e}"));
        assert!(
            validators.accepts(source),
            "{name} fails the schema: {:#?}",
            validators.errors(source)
        );
    }
}

#[test]
fn schema_directive_is_a_comment_to_the_parser() {
    let source = format!("#:schema {}\n{MINIMAL}", StackSchema::URL);
    Stack::parse(&source).expect("the directive is a comment");
    assert!(Validators::get().accepts(&source));
}

#[test]
fn names_agree() {
    let names = [
        "\"demo\"",
        "\"a\"",
        "\"a-1\"",
        "\"abcdefghijklmnopqrstuvwxyz01234\"",
        "\"abcdefghijklmnopqrstuvwxyz012345\"",
        "\"Demo\"",
        "\"1demo\"",
        "\"-demo\"",
        "\"de_mo\"",
        "\"\"",
        "\"dé\"",
    ];
    agrees(
        "version = 1\nname = @@\n[services.app]\nimage = \"ghcr.io/acme/app\"\n",
        &names,
    );
    agrees(
        "version = 1\nname = \"demo\"\n[services.@@]\nimage = \"ghcr.io/acme/app\"\n",
        &names,
    );
    agrees(
        &with_app("update = \"stop-first\"\nvolumes = { @@ = \"/data\" }"),
        &names,
    );
}

#[test]
fn hosts_agree() {
    let long_label = "a".repeat(63);
    let too_long_label = "a".repeat(64);
    let labels = |len: usize| vec!["a".repeat(len); 4].join(".");
    // 251 and 255 characters: the limit is 253.
    let long_host = format!("{}.com", labels(61));
    let too_long_host = format!("{}.com", labels(62));
    agrees_on_strings(
        &MINIMAL.replace("\"demo.example.com\"", "@@"),
        &[
            "demo.example.com",
            "example.com",
            "a.b",
            "a1.example.com",
            "123.example.com",
            "xn--bcher-kva.example",
            "a-b.example.com",
            "example.co1",
            &format!("{long_label}.example.com"),
            &long_host,
            "localhost",
            "Example.com",
            "*.example.com",
            "example.com.",
            "1.2.3.4",
            "example.123",
            "-a.example.com",
            "a-.example.com",
            "a..b",
            ".example.com",
            "example.com:443",
            "https://example.com",
            "example.com/api",
            "ex_ample.com",
            "",
            &format!("{too_long_label}.example.com"),
            &too_long_host,
        ],
    );
}

#[test]
fn prefixes_agree() {
    agrees_on_strings(
        &MINIMAL.replace("port = 8080", "prefix = @@\nport = 8080"),
        &[
            "/",
            "/api",
            "/api/v1",
            "/a.b",
            "/.well-known",
            "/..x",
            "/...",
            "/~user",
            "/a:b@c",
            "/a-b_c!$&'()*+,;=",
            "api",
            "",
            "/api/",
            "//",
            "/a//b",
            "/./a",
            "/a/..",
            "/a?b",
            "/a#b",
            "/a%20b",
            "/a b",
            "/ä",
            "/a\"b",
        ],
    );
}

#[test]
fn images_agree() {
    let digest = format!("sha256:{}", "0123456789abcdef".repeat(4));
    agrees_on_strings(
        &MINIMAL.replace("\"ghcr.io/acme/app\"", "@@"),
        &[
            "ghcr.io/acme/app",
            "ghcr.io/acme/app:1.2",
            "docker.io/library/postgres:17",
            &format!("docker.io/rustfs/rustfs:1.0.0@{digest}"),
            &format!("docker.io/library/redis@{digest}"),
            "localhost:5000/app:1",
            "localhost/app:1",
            "localhost:0/app:1",
            "localhost:65535/app:1",
            "localhost:065535/app:1",
            "registry:5000/a/b/c:1",
            "registry.example:443/a:1",
            "quay.io/a__b/c.d-e:latest",
            "postgres:17",
            "library/postgres:17",
            "Docker.io/library/postgres:17",
            "docker.io/Library/postgres:17",
            "docker.io/library/postgres:-17",
            "docker.io/library/postgres@sha256:abc",
            "docker.io/library/postgres_:17",
            "docker.io//postgres:17",
            "ghcr.io/acme/app:",
            "localhost:65536/app:1",
            "localhost:99999/app:1",
            "registry:70000/a:1",
            "localhost:/app:1",
            "localhost:-1/app:1",
            "",
        ],
    );
}

#[test]
fn tracked_tags_agree() {
    agrees_on_strings(
        &with_app("track = @@"),
        &[
            "main",
            "latest",
            "v1.2.3",
            "_x",
            &"a".repeat(128),
            ".x",
            "-x",
            &"a".repeat(129),
            "",
            "a b",
            "a/b",
        ],
    );
}

#[test]
fn users_agree() {
    agrees_on_strings(
        &with_app("user = @@"),
        &[
            "1000",
            "1000:1000",
            "app",
            "app:staff",
            "www-data",
            "a.b",
            "_x",
            ":1000",
            "1000:",
            "a:b:c",
            "",
            "-x",
            "a b",
        ],
    );
}

#[test]
fn env_names_agree() {
    let names = [
        "RUST_LOG", "_x", "x1", "PORT", "1x", "A-B", "\"\"", "\"A B\"", "\"Ä\"",
    ];
    agrees(&with_app("env = { @@ = \"x\" }"), &names);
    agrees(&with_app("secrets = { @@ = \"demo/x\" }"), &names);
}

#[test]
fn secret_names_agree() {
    agrees_on_strings(
        &with_app("secrets = { DB = @@ }"),
        &[
            "demo/db-url",
            "demo/a.b_c-d",
            "demo/",
            "demo",
            "/x",
            "demo/a/b",
            "demo/a b",
            "",
        ],
    );
}

#[test]
fn mount_paths_agree() {
    agrees_on_strings(
        &with_app("update = \"stop-first\"\nvolumes = { data = @@ }"),
        &[
            "/data",
            "/var/lib/postgresql/data",
            "/.hidden",
            "/..x",
            "/...",
            "/ä",
            "/a b",
            "/",
            "data",
            "/data/",
            "//data",
            "/a/./b",
            "/a/../b",
            "/..",
            "/.",
            "/a:b",
            "/a,b",
            "/a\tb",
            "/a\u{85}b",
            "",
        ],
    );
}

/// Sizes of at least 1 MiB in every unit, around each unit's floor (ADR-0059 §3).
#[test]
fn sizes_agree() {
    let sizes = [
        "1MiB",
        "01MiB",
        "0MiB",
        "000MiB",
        "1GiB",
        "1GB",
        "8589934591GiB",
        "2MB",
        "1MB",
        "10MB",
        "1024KiB",
        "1023KiB",
        "1030KiB",
        "1100KiB",
        "2000KiB",
        "10000KiB",
        "1049KB",
        "1048KB",
        "1050KB",
        "1100KB",
        "2000KB",
        "10000KB",
        "1048576B",
        "1048575B",
        "1048580B",
        "1048600B",
        "1049000B",
        "1050000B",
        "1100000B",
        "2000000B",
        "999999B",
        "10000000B",
        "0001048576B",
        "0B",
        "",
        "512",
        "512m",
        "512M",
        "1kB",
        "1KIB",
        "1mib",
        "1TiB",
        "1.5GiB",
        "512 MiB",
        " 512MiB",
        "512MiB ",
        "-1MiB",
        "+1MiB",
        "MiB",
    ];
    agrees_on_strings(&with_app("shm_size = @@"), &sizes);
    agrees_on_strings(&with_app("tmpfs = { \"/tmp\" = @@ }"), &sizes);
    // A bare integer is not a size.
    agrees(
        &with_app("shm_size = @@"),
        &["\"64MiB\"", "67108864", "true", "1.5"],
    );
}

#[test]
fn limits_agree() {
    agrees_on_strings(
        &with_app("limits = { memory = @@ }"),
        &[
            "8MiB",
            "008MiB",
            "7MiB",
            "9MiB",
            "10MiB",
            "512MiB",
            "1GiB",
            "1GB",
            "0GiB",
            "8589934591GiB",
            "9MB",
            "8MB",
            "10MB",
            "8192KiB",
            "8191KiB",
            "8200KiB",
            "8300KiB",
            "9000KiB",
            "10000KiB",
            "8389KB",
            "8388KB",
            "8390KB",
            "8400KB",
            "9000KB",
            "10000KB",
            "8388608B",
            "8388607B",
            "8388609B",
            "8388610B",
            "8388700B",
            "8389000B",
            "8390000B",
            "8400000B",
            "9000000B",
            "9999999B",
            "10000000B",
            "1048576B",
            "1MiB",
            "512m",
            "512",
            "",
        ],
    );
    agrees(
        &with_app("limits = { memory = @@ }"),
        &["\"64MiB\"", "67108864", "true"],
    );
    agrees(
        &with_app("limits = { pids = @@ }"),
        &[
            "1",
            "512",
            "4294967295",
            "0",
            "-1",
            "4294967296",
            "1.5",
            "\"512\"",
        ],
    );
    agrees(
        &with_app("limits = { cpus = @@ }"),
        &[
            "0.01", "0.5", "1", "1.5", "2", "1024", "1024.0", "0.001", "0.0099", "0", "-1",
            "1024.5", "1025", "nan", "inf", "-inf", "\"1.5\"", "true",
        ],
    );
    agrees(
        &with_app("[services.app.limits]\n@@"),
        &[
            "",
            "memory = \"64MiB\"\npids = 64\ncpus = 0.5",
            "cpu = 1",
            "swap = \"0B\"",
        ],
    );
}

#[test]
fn tmpfs_paths_agree() {
    // The value is a table key here, so `@@` is the quoted path.
    agrees_on_strings(
        &with_app("[services.app.tmpfs]\n@@ = \"1MiB\""),
        &[
            "/tmp",
            "/var/cache/nginx",
            "/run",
            "/.hidden",
            "/..x",
            "/ä",
            "/a b",
            "/devices",
            "/proc2",
            "/sys-x",
            "/dev2/shm",
            "/",
            "tmp",
            "/tmp/",
            "//tmp",
            "/a/./b",
            "/a/../b",
            "/a:b",
            "/a,b",
            "/a\tb",
            "/proc",
            "/proc/x",
            "/sys",
            "/sys/fs/cgroup",
            "/dev",
            "/dev/shm",
            "/dev/shm/x",
            "",
        ],
    );
    agrees(&with_app("init = @@"), &["true", "false", "\"true\"", "1"]);
}

#[test]
fn file_paths_agree() {
    // The value is a table key here, so `@@` is the quoted path.
    agrees_on_strings(
        &with_app("[services.app.files]\n@@ = { content = \"x\" }"),
        &[
            "/etc/app/config.yml",
            "/.hidden",
            "/..x",
            "/...",
            "/ä",
            "/a b",
            "/a:b,c",
            "/a\tb",
            "/",
            "etc/app.conf",
            "/etc/",
            "//etc",
            "/a/./b",
            "/a/../b",
            "/..",
            "/.",
            "/a\0b",
            "",
        ],
    );
}

#[test]
fn file_sources_modes_and_owners_agree() {
    agrees(
        &with_app("[services.app.files]\n\"/etc/app.conf\" = @@"),
        &[
            "{ content = \"x\" }",
            "{ content = \"\" }",
            "{ secret = \"demo/creds\" }",
            "{ secret = \"demo/creds\", mode = \"0440\", owner = \"0:65534\" }",
            "{ content = \"x\", secret = \"demo/creds\" }",
            "{ mode = \"0444\" }",
            "{ }",
            "{ content = 1 }",
            "{ secret = \"demo/a b\" }",
            "{ secret = \"demo\" }",
            "{ content = \"x\", source = \"https://example.com\" }",
            "\"x\"",
        ],
    );
    agrees_on_strings(
        &with_app("[services.app.files]\n\"/etc/app.conf\" = { content = \"x\", mode = @@ }"),
        &[
            "0000",
            "0400",
            "0644",
            "0777",
            "644",
            "00644",
            "1755",
            "4755",
            "0788",
            "0o644",
            "rw-r--r--",
            " 0644",
            "",
        ],
    );
    agrees_on_strings(
        &with_app("[services.app.files]\n\"/etc/app.conf\" = { content = \"x\", owner = @@ }"),
        &[
            "0:0",
            "1000:1000",
            "65535:65535",
            "00000:00001",
            "59999:60000",
            "65536:0",
            "0:65536",
            "70000:0",
            "000000:0",
            "1000",
            "1000:",
            ":1000",
            "0:0:0",
            "nginx:nginx",
            "-1:0",
            "+1:0",
            "",
        ],
    );
}

const DURATIONS: &[&str] = &[
    "30s",
    "500ms",
    "1h 30m",
    "1h30m",
    "1.5s",
    " 5m ",
    "5 m",
    "1 000s",
    "1 .5s",
    "1. 5s",
    "1.5 s",
    "1\u{3000}s",
    "1\u{85}s",
    "1.5ms",
    "1.5us",
    "2 days",
    "1M",
    "1µs",
    "10 min",
    "3wk",
    "1 hours 2 mins",
    "5",
    "s",
    "5 parsecs",
    "-1s",
    "1.s",
    "1h30",
    "5m s",
    "",
    "5s,",
    "1_000s",
    "1.5ns",
    "1.0nsec",
    "1\u{feff}s",
];

#[test]
fn durations_agree() {
    let zero = ["0s", "0ms 0s", "0.0s", "00s"];
    let durations: Vec<&str> = DURATIONS.iter().chain(&zero).copied().collect();
    agrees_on_strings(
        &MINIMAL.replace("name = \"demo\"", "name = \"demo\"\ndrain_timeout = @@"),
        &durations,
    );
    agrees_on_strings(&with_app("stop_timeout = @@"), &durations);
}

#[test]
fn positive_durations_agree() {
    let around_zero = [
        "0s", "0ms 0s", "0.0s", "00s", "0 0s", "0. 0s", "0.5s", "0s 1ms", "0 1s", "10s",
    ];
    let durations: Vec<&str> = DURATIONS.iter().chain(&around_zero).copied().collect();
    agrees_on_strings(
        &MINIMAL.replace("port = 8080", "port = 8080\ntimeouts = { read = @@ }"),
        &durations,
    );
    agrees_on_strings(
        &with_app("[services.app.health]\nstart_period = @@"),
        &durations,
    );
}

#[test]
fn health_probe_fields_agree() {
    agrees_on_strings(
        &with_app("[services.app.health]\nhttp = { expect_status = @@ }"),
        &[
            "2xx", "5xx", "1xx", "204", "100", "599", "6xx", "0xx", "600", "99", "2XX", "20x", "",
            "2xx-3xx",
        ],
    );
    agrees_on_strings(
        &with_app("[services.app.health]\nhttp = { path = @@ }"),
        &[
            "/healthz", "/", "/a?b=c", "/%20", "healthz", "/a b", "/ä", "/a\tb", "",
        ],
    );
    agrees_on_strings(
        &with_app("[services.app.health]\nexec = [@@, \"arg\"]"),
        &[
            "pg_isready",
            "sh",
            "s h",
            "/bin/true",
            "",
            " ",
            " sh",
            "sh ",
            "sh\n",
            "\u{a0}sh",
            "\u{85}sh",
            "sh\u{3000}",
            "\u{feff}sh",
            "a\u{0}b",
        ],
    );
    agrees_on_strings(
        &with_app("[services.app.health]\nexec = [\"sh\", @@]"),
        &["x", "", " ", "a\u{0}b"],
    );
}

#[test]
fn deploy_refs_agree() {
    agrees_on_strings(
        &format!("{MINIMAL}\n[deploy]\nrepository_id = 1\nrefs = [@@]\n"),
        &["refs/tags/v*", "refs/heads/main", "refs/", "tags/v*", ""],
    );
}

#[test]
fn integers_and_enums_agree() {
    agrees(
        &MINIMAL.replace("port = 8080", "port = @@"),
        &["1", "8080", "65535", "0", "65536", "-1", "\"8080\"", "80.5"],
    );
    agrees(
        &MINIMAL.replace("version = 1", "version = @@"),
        &["1", "2", "0", "\"1\""],
    );
    agrees(
        &format!("{MINIMAL}\n[deploy]\nrepository_id = @@\n"),
        &[
            "1",
            "1450109946",
            "4294967296",
            "5000000000",
            "9223372036854775807",
            "0",
            "-5",
            "\"1\"",
            "9223372036854775808",
        ],
    );
    agrees(
        &with_app("[services.app.health]\nhealthy_after = @@"),
        &["1", "3", "4294967295", "0", "-1", "4294967296"],
    );
    agrees(
        &with_app("update = @@"),
        &[
            "\"start-first\"",
            "\"stop-first\"",
            "\"blue-green\"",
            "\"StartFirst\"",
            "true",
        ],
    );
    agrees(
        &with_app("read_only = @@"),
        &["true", "false", "\"yes\"", "1"],
    );
    agrees(
        &MINIMAL.replace("port = 8080", "port = 8080\nstrip_prefix = @@"),
        &["true", "false", "\"yes\""],
    );
}

#[test]
fn tables_and_lists_agree() {
    agrees(
        "version = 1\nname = \"demo\"\n@@\n",
        &[
            "services = { app = { image = \"ghcr.io/acme/app\" } }",
            "services = {}",
            "",
        ],
    );
    agrees(
        &format!("{MINIMAL}\n[deploy]\nrepository_id = 1\n@@\n"),
        &[
            "refs = [\"refs/tags/v*\"]",
            "environment = \"production\"",
            "refs = []",
            "environment = \"\"",
            "environment = \" \"",
            "environment = \"\\u3000\"",
            "environment = \"\\u0085\"",
            "environment = \"\\ufeff\"",
            "job_workflow_ref = \"\"",
        ],
    );
    agrees(
        &with_app("[services.app.health]\n@@"),
        &[
            "http = {}",
            "tcp = {}",
            "exec = [\"true\"]",
            "interval = \"5s\"",
            "http = {}\ntcp = {}",
            "http = {}\nexec = [\"true\"]",
            "tcp = {}\nexec = [\"true\"]",
            "exec = []",
        ],
    );
    agrees(
        &with_app("command = @@"),
        &[
            "[\"serve\", \"--port\", \"8080\"]",
            "[]",
            "\"serve\"",
            "[1]",
        ],
    );
    agrees(
        &MINIMAL
            .replace(
                "[services.app]\n",
                "[services.db]\nimage = \"docker.io/library/postgres:17\"\n[services.app]\n",
            )
            .replace(
                "image = \"ghcr.io/acme/app\"",
                "image = \"ghcr.io/acme/app\"\n@@",
            ),
        &[
            "depends_on = [\"db\"]",
            "depends_on = []",
            "depends_on = [\"db\", \"db\"]",
            "depends_on = [\"Db\"]",
            "depends_on = \"db\"",
        ],
    );
}

/// The rules the schema's description leaves to hoster: the parser rejects each source, the
/// schema accepts it.
#[test]
fn semantic_rules_are_parser_only() {
    let worker = "version = 1\nname = \"demo\"\n[services.worker]\nimage = \"ghcr.io/acme/w\"\n";
    let two_services = MINIMAL.replace(
        "[services.app]\n",
        "[services.db]\nimage = \"docker.io/library/postgres:17\"\n@@\n[services.app]\n",
    );
    // Route collisions are left out: the apply step checks them against every stack's routes.
    let cases = [
        ("a depends_on cycle", {
            two_services
                .replace("@@", "depends_on = [\"app\"]")
                .replace(
                    "image = \"ghcr.io/acme/app\"",
                    "image = \"ghcr.io/acme/app\"\ndepends_on = [\"db\"]",
                )
        }),
        (
            "depends_on an unknown service",
            with_app("depends_on = [\"cache\"]"),
        ),
        ("depends_on itself", with_app("depends_on = [\"app\"]")),
        (
            "a secret of another stack",
            with_app("secrets = { DB = \"other/db-url\" }"),
        ),
        (
            "a name in both env and secrets",
            with_app("env = { DB = \"x\" }\nsecrets = { DB = \"demo/db-url\" }"),
        ),
        (
            "a file in a volume",
            with_app(
                "update = \"stop-first\"\nvolumes = { a = \"/data\" }\n\
                 files = { \"/data/app.conf\" = { content = \"\" } }",
            ),
        ),
        (
            "a file above another file",
            with_app(
                "[services.app.files]\n\"/etc/app\" = { content = \"\" }\n\
                 \"/etc/app/a.conf\" = { content = \"\" }",
            ),
        ),
        (
            "a file podman manages",
            with_app("files = { \"/etc/hosts\" = { content = \"\" } }"),
        ),
        (
            "a file's secret of another stack",
            with_app("files = { \"/etc/a\" = { secret = \"other/x\" } }"),
        ),
        (
            "file content over 256 KiB",
            with_app(&format!(
                "files = {{ \"/etc/a\" = {{ content = \"{}\" }} }}",
                "x".repeat(256 * 1024 + 1)
            )),
        ),
        (
            "a mount path used twice",
            with_app("update = \"stop-first\"\nvolumes = { a = \"/data\", b = \"/data\" }"),
        ),
        (
            "a third-party image without a tag",
            MINIMAL.replace("ghcr.io/acme/app", "docker.io/library/postgres"),
        ),
        (
            "a ghcr.io image with both a tag and track",
            MINIMAL.replace("ghcr.io/acme/app", "ghcr.io/acme/app:1\"\ntrack = \"main"),
        ),
        (
            "track on a third-party image",
            two_services.replace("@@", "track = \"main\""),
        ),
        (
            "an http check on a service without routes",
            format!("{worker}[services.worker.health]\nhttp = {{}}\n"),
        ),
        (
            "a tcp check on a service without routes",
            format!("{worker}[services.worker.health]\ntcp = {{}}\n"),
        ),
        (
            "a timings-only health table on a service without routes",
            format!("{worker}[services.worker.health]\ninterval = \"5s\"\n"),
        ),
        (
            "an http check on a port no route uses",
            with_app("[services.app.health]\nhttp = { port = 9090 }"),
        ),
        (
            "a tcp check on a port no route uses",
            with_app("[services.app.health]\ntcp = { port = 9090 }"),
        ),
        (
            "an inexact fraction of an hour",
            with_app("stop_timeout = \"0.001h\""),
        ),
        (
            "a fraction finer than a nanosecond",
            with_app("stop_timeout = \"1.0000000001s\""),
        ),
        (
            "an overflowing duration",
            with_app("stop_timeout = \"99999999999999999999s\""),
        ),
        (
            "a size over i64::MAX bytes",
            with_app("shm_size = \"8589934592GiB\""),
        ),
        (
            "a tmpfs at a volume",
            with_app(
                "update = \"stop-first\"\nvolumes = { a = \"/data\" }\ntmpfs = { \"/data\" = \"1MiB\" }",
            ),
        ),
        (
            "a tmpfs in a volume",
            with_app(
                "update = \"stop-first\"\nvolumes = { a = \"/data\" }\n\
                 tmpfs = { \"/data/cache\" = \"1MiB\" }",
            ),
        ),
        (
            "a tmpfs above a config file",
            with_app(
                "files = { \"/etc/app/a.conf\" = { content = \"\" } }\n\
                 tmpfs = { \"/etc/app\" = \"1MiB\" }",
            ),
        ),
        (
            "a tmpfs above another",
            with_app("tmpfs = { \"/var\" = \"1MiB\", \"/var/cache\" = \"1MiB\" }"),
        ),
    ];
    let validators = Validators::get();
    for (rule, source) in cases {
        assert!(
            Stack::parse(&source).is_err(),
            "{rule}: the parser accepts:\n{source}"
        );
        assert!(
            validators.accepts(&source),
            "{rule}: the schema rejects it: {:#?}\n{source}",
            validators.errors(&source)
        );
    }
}

#[test]
fn unknown_fields_are_rejected_at_every_level() {
    let validators = Validators::get();
    for source in [
        format!("{MINIMAL}\nextra = 1\n"),
        with_app("ports = [\"8080:8080\"]"),
        with_app("privileged = true"),
        MINIMAL.replace("port = 8080", "port = 8080\nweight = 1"),
        MINIMAL.replace("port = 8080", "port = 8080\ntimeouts = { total = \"1s\" }"),
        with_app("[services.app.health]\nretries = 3"),
        with_app("files = { \"/etc/a\" = { content = \"\", template = true } }"),
        with_app("[services.app.health]\nhttp = { method = \"HEAD\" }"),
        with_app("[services.app.health]\ntcp = { host = \"x\" }"),
        with_app("limits = { memory_swap = \"1GiB\" }"),
        format!("{MINIMAL}\n[deploy]\nrepository_id = 1\nrepository = \"acme/app\"\n"),
    ] {
        assert!(Stack::parse(&source).is_err(), "parser accepts:\n{source}");
        assert!(!validators.accepts(&source), "schema accepts:\n{source}");
    }
}
