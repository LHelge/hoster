use super::*;

/// The worked example from `docs/stack-format.md` §3, taken from the document itself so the
/// two cannot drift apart.
fn worked_example() -> &'static str {
    let doc = include_str!("../../docs/stack-format.md");
    let section = &doc[doc.find("## 3. Worked example").expect("§3 exists")..];
    let start = section.find("```toml\n").expect("§3 has a TOML block") + "```toml\n".len();
    let len = section[start..].find("```").expect("TOML block is closed");
    &section[start..start + len]
}

/// A minimal valid stack; tests append to or substitute into it.
const MINIMAL: &str = r#"
version = 1
name = "demo"

[services.app]
image = "ghcr.io/acme/app"

[[services.app.routes]]
host = "demo.example.com"
port = 8080
"#;

fn parse(source: &str) -> Result<ParsedStack, StackError> {
    Stack::parse(source)
}

fn errors(source: &str) -> Vec<String> {
    match parse(source) {
        Ok(_) => Vec::new(),
        Err(e) => e.diagnostics().iter().map(ToString::to_string).collect(),
    }
}

/// Asserts that `source` fails with exactly one diagnostic whose text contains `needle`.
#[track_caller]
fn assert_error(source: &str, needle: &str) {
    let errors = errors(source);
    assert!(
        errors.len() == 1 && errors[0].contains(needle),
        "expected one error containing {needle:?}, got {errors:#?}\nsource:\n{source}"
    );
}

fn app(stack: &Stack) -> &Service {
    &stack.services[&"app".parse::<Name>().unwrap()]
}

#[test]
fn worked_example_validates_cleanly() {
    let parsed = parse(worked_example()).expect("worked example is valid");
    assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    let stack = parsed.stack;
    assert_eq!(stack.name.as_ref(), "notes");
    assert_eq!(stack.drain_timeout, Duration::from_secs(30));
    let deploy = stack.deploy.as_ref().unwrap();
    assert_eq!(deploy.repository_id, 123456789);
    assert_eq!(deploy.refs, ["refs/tags/v*"]);
    assert_eq!(deploy.environment.as_deref(), Some("production"));
    assert_eq!(deploy.job_workflow_ref, None);

    let names: Vec<&str> = stack.services.keys().map(AsRef::as_ref).collect();
    assert_eq!(names, ["api", "storage", "web"]);

    let api = &stack.services[&"api".parse().unwrap()];
    assert!(api.image.is_ci_built());
    assert_eq!(api.track.as_deref(), Some("main"));
    assert_eq!(api.env["S3_BUCKET"], "attachments");
    assert_eq!(api.secrets["DATABASE_URL"], "notes/database-url");
    assert_eq!(api.update, UpdateStrategy::StartFirst);
    assert_eq!(api.stop_timeout, DEFAULT_STOP_TIMEOUT);
    let route = &api.routes[0];
    assert_eq!(route.host.as_ref(), "notes.example.com");
    assert_eq!(route.prefix.to_string(), "/api");
    assert!(!route.strip_prefix);
    assert_eq!(route.port, 8080);
    assert_eq!(route.timeouts.read, Some(Duration::from_secs(60)));
    assert_eq!(route.timeouts.idle, Some(Duration::from_secs(120)));
    assert_eq!(route.timeouts.connect, None);
    let health = api.health.as_ref().unwrap();
    assert_eq!(
        health.probe,
        Probe::Http {
            path: "/api/healthz".into(),
            port: 8080,
            expect_status: ExpectStatus::SuccessOrRedirect,
        }
    );
    assert_eq!(health.start_period, Some(Duration::from_secs(30)));

    let web = &stack.services[&"web".parse().unwrap()];
    assert!(web.routes[0].prefix.is_root());

    let storage = &stack.services[&"storage".parse().unwrap()];
    assert!(!storage.image.is_ci_built());
    assert_eq!(storage.image.registry(), "docker.io");
    assert_eq!(storage.image.repository(), "rustfs/rustfs");
    assert_eq!(storage.image.tag(), Some("1.0.0"));
    assert_eq!(storage.update, UpdateStrategy::StopFirst);
    assert_eq!(storage.volumes["data"], "/data");
    assert_eq!(
        storage.health.as_ref().unwrap().probe,
        Probe::Tcp { port: 9000 }
    );
    assert_eq!(
        storage.routes[0].timeouts.write,
        Some(Duration::from_secs(600))
    );
}

#[test]
fn minimal_stack_gets_defaults() {
    let parsed = parse(MINIMAL).unwrap();
    let stack = &parsed.stack;
    assert_eq!(stack.drain_timeout, DEFAULT_DRAIN_TIMEOUT);
    assert_eq!(stack.deploy, None);
    let app = app(stack);
    assert_eq!(app.health, None);
    assert_eq!(app.update, UpdateStrategy::StartFirst);
    assert_eq!(app.stop_timeout, DEFAULT_STOP_TIMEOUT);
    assert!(!app.read_only);
    assert_eq!(app.routes[0].prefix, NormalizedPrefix::root());
    assert_eq!(app.routes[0].timeouts, RouteTimeouts::default());
    assert!("x".parse::<Stack>().is_err());
    assert_eq!(MINIMAL.parse::<Stack>().unwrap(), parsed.stack);
}

#[test]
fn optional_service_fields_are_carried_over() {
    let source = MINIMAL.replace(
        "image = \"ghcr.io/acme/app\"",
        r#"image = "ghcr.io/acme/app"
command = ["serve", "--fast"]
entrypoint = []
user = "1000:1000"
read_only = true
stop_timeout = "500ms"
update = "stop-first""#,
    );
    let stack = parse(&source).unwrap().stack;
    let app = app(&stack);
    assert_eq!(
        app.command.as_deref(),
        Some(&["serve".into(), "--fast".into()][..])
    );
    assert_eq!(app.entrypoint.as_deref(), Some(&[][..]));
    assert_eq!(app.user.as_deref(), Some("1000:1000"));
    assert!(app.read_only);
    assert_eq!(app.stop_timeout, Duration::from_millis(500));
    assert_eq!(app.update, UpdateStrategy::StopFirst);
}

/// (description, original text in MINIMAL, replacement, expected error fragment)
const SUBSTITUTION_ERRORS: &[(&str, &str, &str, &str)] = &[
    // Top level
    (
        "version 2",
        "version = 1",
        "version = 2",
        "unsupported version 2",
    ),
    (
        "version string",
        "version = 1",
        "version = \"1\"",
        "invalid type",
    ),
    (
        "missing version",
        "version = 1",
        "",
        "missing field `version`",
    ),
    (
        "uppercase name",
        "\"demo\"",
        "\"Demo\"",
        "invalid name `Demo`",
    ),
    (
        "name starts with digit",
        "\"demo\"",
        "\"1demo\"",
        "invalid name",
    ),
    (
        "name with underscore",
        "\"demo\"",
        "\"de_mo\"",
        "invalid name",
    ),
    (
        "name too long",
        "\"demo\"",
        "\"abcdefghijklmnopqrstuvwxyz123456\"",
        "at most 31",
    ),
    (
        "resources rejected",
        "[[services.app.routes]]",
        "resources = {}\n[[services.app.routes]]",
        "unknown field `resources`",
    ),
    (
        "ports rejected",
        "[[services.app.routes]]",
        "ports = []\n[[services.app.routes]]",
        "unknown field `ports`",
    ),
    (
        "top-level typo",
        "version = 1",
        "version = 1\nnmae = \"x\"",
        "unknown field `nmae`",
    ),
    (
        "drain timeout",
        "name = \"demo\"",
        "name = \"demo\"\ndrain_timeout = \"30\"",
        "invalid duration `30`",
    ),
    (
        "bad update",
        "image = \"ghcr.io/acme/app\"",
        "image = \"ghcr.io/acme/app\"\nupdate = \"recreate\"",
        "unknown variant `recreate`",
    ),
    (
        "stop timeout",
        "image = \"ghcr.io/acme/app\"",
        "image = \"ghcr.io/acme/app\"\nstop_timeout = \"soon\"",
        "invalid duration `soon`",
    ),
    (
        "user spec",
        "image = \"ghcr.io/acme/app\"",
        "image = \"ghcr.io/acme/app\"\nuser = \"a:b:c\"",
        "invalid user",
    ),
    // Images (ADR-0025)
    (
        "third-party without tag",
        "ghcr.io/acme/app\"",
        "docker.io/library/nginx\"",
        "needs a tag or digest",
    ),
    (
        "short name",
        "ghcr.io/acme/app\"",
        "nginx:1.27\"",
        "must name its registry",
    ),
    (
        "docker hub short name",
        "ghcr.io/acme/app\"",
        "library/nginx:1.27\"",
        "must name its registry",
    ),
    (
        "uppercase repository",
        "ghcr.io/acme/app\"",
        "ghcr.io/Acme/app\"",
        "repository path components",
    ),
    (
        "bad digest",
        "ghcr.io/acme/app\"",
        "ghcr.io/acme/app@sha256:abc\"",
        "invalid digest",
    ),
    (
        "bad tag",
        "ghcr.io/acme/app\"",
        "ghcr.io/acme/app:-x\"",
        "invalid tag",
    ),
    // track (ADR-0043)
    (
        "track on third-party",
        "image = \"ghcr.io/acme/app\"",
        "image = \"docker.io/library/nginx:1.27\"\ntrack = \"latest\"",
        "`track` is only allowed",
    ),
    (
        "track on tagged ghcr",
        "image = \"ghcr.io/acme/app\"",
        "image = \"ghcr.io/acme/app:v1\"\ntrack = \"main\"",
        "`track` is only allowed",
    ),
    (
        "track bad tag",
        "image = \"ghcr.io/acme/app\"",
        "image = \"ghcr.io/acme/app\"\ntrack = \"a b\"",
        "invalid tag `a b`",
    ),
    // Hosts
    (
        "uppercase host",
        "demo.example.com",
        "Demo.example.com",
        "must be lowercase",
    ),
    (
        "trailing dot",
        "demo.example.com",
        "demo.example.com.",
        "must not end with a dot",
    ),
    (
        "port in host",
        "demo.example.com",
        "demo.example.com:8443",
        "must not include a port",
    ),
    (
        "scheme in host",
        "demo.example.com",
        "https://demo.example.com",
        "must not include a scheme",
    ),
    (
        "path in host",
        "demo.example.com",
        "demo.example.com/x",
        "must not include a path",
    ),
    (
        "wildcard host",
        "demo.example.com",
        "*.example.com",
        "wildcard",
    ),
    (
        "single label",
        "demo.example.com",
        "localhost",
        "fully qualified",
    ),
    (
        "empty label",
        "demo.example.com",
        "demo..example.com",
        "empty labels",
    ),
    (
        "ip address",
        "demo.example.com",
        "192.168.1.1",
        "IP addresses",
    ),
    (
        "underscore label",
        "demo.example.com",
        "de_mo.example.com",
        "may only contain",
    ),
    (
        "hyphen edge",
        "demo.example.com",
        "-demo.example.com",
        "must not start or end",
    ),
    (
        "empty host",
        "\"demo.example.com\"",
        "\"\"",
        "must not be empty",
    ),
    // Prefixes
    (
        "relative prefix",
        "port = 8080",
        "port = 8080\nprefix = \"api\"",
        "must start with `/`",
    ),
    (
        "trailing slash",
        "port = 8080",
        "port = 8080\nprefix = \"/api/\"",
        "must not end with `/`",
    ),
    (
        "double slash",
        "port = 8080",
        "port = 8080\nprefix = \"/a//b\"",
        "must not contain `//`",
    ),
    (
        "dot segment",
        "port = 8080",
        "port = 8080\nprefix = \"/a/./b\"",
        "`.` or `..`",
    ),
    (
        "dotdot segment",
        "port = 8080",
        "port = 8080\nprefix = \"/a/..\"",
        "`.` or `..`",
    ),
    (
        "query",
        "port = 8080",
        "port = 8080\nprefix = \"/a?b\"",
        "`?` or `#`",
    ),
    (
        "fragment",
        "port = 8080",
        "port = 8080\nprefix = \"/a#b\"",
        "`?` or `#`",
    ),
    (
        "percent",
        "port = 8080",
        "port = 8080\nprefix = \"/a%20b\"",
        "percent-encoding",
    ),
    (
        "space",
        "port = 8080",
        "port = 8080\nprefix = \"/a b\"",
        "character ' '",
    ),
    (
        "empty prefix",
        "port = 8080",
        "port = 8080\nprefix = \"\"",
        "must start with `/`",
    ),
    // Ports
    ("port zero", "port = 8080", "port = 0", "invalid port 0"),
    (
        "port too big",
        "port = 8080",
        "port = 65536",
        "invalid port 65536",
    ),
    (
        "negative port",
        "port = 8080",
        "port = -1",
        "invalid port -1",
    ),
    ("missing port", "port = 8080", "", "missing field `port`"),
    // Route timeouts
    (
        "bad timeout",
        "port = 8080",
        "port = 8080\ntimeouts = { read = \"forever\" }",
        "invalid duration `forever`",
    ),
    (
        "zero timeout",
        "port = 8080",
        "port = 8080\ntimeouts = { idle = \"0s\" }",
        "greater than zero",
    ),
    (
        "unknown timeout",
        "port = 8080",
        "port = 8080\ntimeouts = { total = \"1s\" }",
        "unknown field `total`",
    ),
];

#[test]
fn substitution_errors() {
    for (description, from, to, needle) in SUBSTITUTION_ERRORS {
        assert!(
            MINIMAL.contains(from),
            "{description}: {from:?} not in MINIMAL"
        );
        let source = MINIMAL.replacen(from, to, 1);
        let errors = errors(&source);
        assert!(
            errors.len() == 1 && errors[0].contains(needle),
            "{description}: expected one error containing {needle:?}, got {errors:#?}"
        );
    }
}

/// (description, text appended to MINIMAL, expected error fragment)
const APPENDED_ERRORS: &[(&str, &str, &str)] = &[
    (
        "service name",
        "[services.App]\nimage = \"ghcr.io/acme/x\"",
        "invalid name `App`",
    ),
    // env / secrets / volumes
    (
        "bad env name",
        "[services.app.env]\n\"1X\" = \"y\"",
        "invalid environment variable name `1X`",
    ),
    (
        "env with dash",
        "[services.app.env]\nMY-VAR = \"y\"",
        "invalid environment variable name",
    ),
    (
        "secret format",
        "[services.app.secrets]\nDB = \"database-url\"",
        "invalid secret name `database-url`",
    ),
    (
        "secret other stack",
        "[services.app.secrets]\nDB = \"other/database-url\"",
        "invalid secret name `other/database-url`",
    ),
    (
        "secret empty name",
        "[services.app.secrets]\nDB = \"demo/\"",
        "invalid secret name",
    ),
    (
        "env and secret",
        "[services.app.env]\nDB = \"x\"\n[services.app.secrets]\nDB = \"demo/db\"",
        "both `env` and `secrets`",
    ),
    (
        "secret value not string",
        "[services.app.secrets]\nDB = 1",
        "invalid type",
    ),
    (
        "volume name",
        "[services.app.volumes]\nData = \"/data\"",
        "invalid name `Data`",
    ),
    (
        "relative mount",
        "[services.app.volumes]\ndata = \"data\"",
        "invalid mount path",
    ),
    (
        "root mount",
        "[services.app.volumes]\ndata = \"/\"",
        "invalid mount path",
    ),
    (
        "dotdot mount",
        "[services.app.volumes]\ndata = \"/a/../b\"",
        "invalid mount path",
    ),
    (
        "trailing slash mount",
        "[services.app.volumes]\na = \"/data\"\nb = \"/data/\"",
        "invalid mount path `/data/`",
    ),
    (
        "double slash mount",
        "[services.app.volumes]\ndata = \"/srv//data\"",
        "invalid mount path",
    ),
    (
        "mount options smuggled",
        "[services.app.volumes]\ndata = \"/data:U\"",
        "invalid mount path",
    ),
    (
        "mount comma",
        "[services.app.volumes]\ndata = \"/data,ro\"",
        "invalid mount path",
    ),
    (
        "duplicate mount",
        "[services.app.volumes]\na = \"/data\"\nb = \"/data\"",
        "used by more than one volume",
    ),
    (
        "bind mount table",
        "[services.app.volumes.data]\nsource = \"/srv\"",
        "invalid type",
    ),
    // health (ADR-0038)
    (
        "health both",
        "[services.app.health]\nhttp = { port = 8080 }\ntcp = { port = 8080 }",
        "not both",
    ),
    (
        "health port not routed",
        "[services.app.health]\ntcp = { port = 9000 }",
        "health port 9000 is not the container port",
    ),
    (
        "health bad port",
        "[services.app.health]\ntcp = { port = 70000 }",
        "invalid port 70000",
    ),
    (
        "health relative path",
        "[services.app.health]\nhttp = { path = \"healthz\" }",
        "must start with `/`",
    ),
    (
        "health path with a newline",
        "[services.app.health]\nhttp = { path = \"/a\\r\\nX: y\" }",
        "invalid health path `/a\\r\\nX: y`: spaces, control",
    ),
    (
        "health path with a space",
        "[services.app.health]\nhttp = { path = \"/a b\" }",
        "must be percent-encoded",
    ),
    (
        "health bad status",
        "[services.app.health]\nhttp = { expect_status = \"2xy\" }",
        "invalid status `2xy`",
    ),
    (
        "health status out of range",
        "[services.app.health]\nhttp = { expect_status = \"600\" }",
        "invalid status `600`",
    ),
    (
        "health zero interval",
        "[services.app.health]\ninterval = \"0s\"",
        "greater than zero",
    ),
    (
        "health healthy_after",
        "[services.app.health]\nhealthy_after = 0",
        "positive integer",
    ),
    (
        "health unknown",
        "[services.app.health]\nexec = [\"true\"]",
        "unknown field `exec`",
    ),
    (
        "health bad duration",
        "[services.app.health]\nstart_period = \"1 fortnight\"",
        "invalid duration",
    ),
    // deploy (ADR-0047)
    (
        "deploy missing repo",
        "[deploy]\nrefs = [\"refs/tags/v*\"]",
        "missing field `repository_id`",
    ),
    (
        "deploy zero repo",
        "[deploy]\nrepository_id = 0",
        "positive integer",
    ),
    (
        "deploy repo string",
        "[deploy]\nrepository_id = \"acme/notes\"",
        "invalid type",
    ),
    (
        "deploy ref pattern",
        "[deploy]\nrepository_id = 1\nrefs = [\"v*\"]",
        "must start with `refs/`",
    ),
    (
        "deploy empty refs",
        "[deploy]\nrepository_id = 1\nrefs = []",
        "omit `refs` to allow any ref",
    ),
    (
        "deploy empty env",
        "[deploy]\nrepository_id = 1\nenvironment = \"\"",
        "must not be empty",
    ),
    (
        "deploy unknown",
        "[deploy]\nrepository_id = 1\nsecret = \"x\"",
        "unknown field `secret`",
    ),
    // route shape
    (
        "route unknown",
        "[[services.app.routes]]\nhost = \"b.example.com\"\nport = 1\nstrip = true",
        "unknown field `strip`",
    ),
    // TOML syntax
    ("syntax", "[services.app\n", "unclosed table"),
];

#[test]
fn appended_errors() {
    for (description, appended, needle) in APPENDED_ERRORS {
        let source = format!("{MINIMAL}\n{appended}\n");
        let errors = errors(&source);
        assert!(
            errors.len() == 1 && errors[0].contains(needle),
            "{description}: expected one error containing {needle:?}, got {errors:#?}"
        );
    }
}

#[test]
fn health_is_not_allowed_on_non_routed_services() {
    let source = r#"
version = 1
name = "demo"

[services.worker]
image = "ghcr.io/acme/worker"

[services.worker.health]
tcp = { port = 9000 }
"#;
    assert_error(
        source,
        "`health` is not allowed on a service without routes",
    );
}

#[test]
fn stack_needs_a_service() {
    assert_error(
        "version = 1\nname = \"demo\"\nservices = {}\n",
        "at least one service",
    );
    assert_error("version = 1\nname = \"demo\"\n", "missing field `services`");
}

#[test]
fn accepted_image_references() {
    for (image, ci_built) in [
        ("ghcr.io/acme/app", true),
        ("ghcr.io/acme/app:v1", false),
        ("docker.io/rustfs/rustfs:1.0.0", false),
        ("docker.io/library/nginx:1.27-alpine", false),
        (
            "docker.io/library/nginx:1.27@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            false,
        ),
        (
            "quay.io/org/sub/img@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            false,
        ),
        ("localhost:5000/app:dev", false),
        ("registry.example.com:5000/a.b_c-d/e__f:T_1.2", false),
    ] {
        let parsed: ImageRef = image.parse().unwrap_or_else(|e| panic!("{image}: {e}"));
        assert_eq!(parsed.is_ci_built(), ci_built, "{image}");
        assert_eq!(parsed.to_string(), image);
        let source = MINIMAL.replace("ghcr.io/acme/app", image);
        assert!(parse(&source).is_ok(), "{image}: {:?}", errors(&source));
    }
    let pinned: ImageRef = "docker.io/library/nginx:1.27@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".parse().unwrap();
    assert_eq!(pinned.tag(), Some("1.27"));
    assert!(pinned.digest().unwrap().as_ref().starts_with("sha256:0123"));
    let ported: ImageRef = "localhost:5000/app".parse().unwrap();
    assert_eq!(
        (ported.registry(), ported.repository(), ported.tag()),
        ("localhost:5000", "app", None)
    );
}

#[test]
fn accepted_hosts() {
    for host in [
        "example.com",
        "a.b.c.example.co.uk",
        "xn--bcher-kva.example",
        "1password.com",
        "a-b.example",
    ] {
        let parsed: Host = host.parse().unwrap_or_else(|e| panic!("{host}: {e}"));
        assert_eq!(parsed.to_string(), host);
    }
    let long_label = format!("{}.example.com", "a".repeat(64));
    assert!(long_label.parse::<Host>().unwrap_err().contains("63"));
}

#[test]
fn prefixes_normalize_into_segments() {
    for (prefix, segments) in [
        ("/", &[][..]),
        ("/api", &["api"][..]),
        ("/api/v1", &["api", "v1"][..]),
        ("/a.b/~c/@d:e", &["a.b", "~c", "@d:e"][..]),
        ("/...", &["..."][..]),
    ] {
        let parsed: NormalizedPrefix = prefix.parse().unwrap_or_else(|e| panic!("{prefix}: {e}"));
        assert_eq!(parsed.segments(), segments, "{prefix}");
        assert_eq!(parsed.to_string(), prefix);
        assert_eq!(parsed.is_root(), segments.is_empty());
    }
    let api: NormalizedPrefix = "/api".parse().unwrap();
    let apiary: NormalizedPrefix = "/apiary".parse().unwrap();
    assert_ne!(api, apiary);
    assert_eq!(api, "/api".parse().unwrap());
}

#[test]
fn expect_status_parsing_and_matching() {
    assert_eq!("2xx".parse(), Ok(ExpectStatus::Class(2)));
    assert_eq!("204".parse(), Ok(ExpectStatus::Exact(204)));
    assert!("6xx".parse::<ExpectStatus>().is_err());
    assert!("99".parse::<ExpectStatus>().is_err());
    assert!(ExpectStatus::default().matches(302));
    assert!(!ExpectStatus::default().matches(404));
    assert!(ExpectStatus::Class(2).matches(299));
    assert!(!ExpectStatus::Class(2).matches(300));
    assert!(ExpectStatus::Exact(204).matches(204));
}

#[test]
fn health_defaults_resolve_against_routes() {
    let source = format!("{MINIMAL}\n[services.app.health]\nstart_period = \"1m\"\n");
    let stack = parse(&source).unwrap().stack;
    let health = app(&stack).health.clone().unwrap();
    assert_eq!(
        health.probe,
        Probe::Http {
            path: DEFAULT_HEALTH_PATH.into(),
            port: 8080,
            expect_status: ExpectStatus::SuccessOrRedirect
        }
    );
    assert_eq!(health.start_period, Some(Duration::from_secs(60)));

    let source = format!(
        "{MINIMAL}\n[services.app.health]\nhttp = {{ expect_status = \"204\" }}\ninterval = \"2s\"\ntimeout = \"1s\"\nhealthy_after = 3\n"
    );
    let stack = parse(&source).unwrap().stack;
    let health = app(&stack).health.clone().unwrap();
    assert_eq!(
        health.probe,
        Probe::Http {
            path: DEFAULT_HEALTH_PATH.into(),
            port: 8080,
            expect_status: ExpectStatus::Exact(204)
        }
    );
    assert_eq!(health.interval, Some(Duration::from_secs(2)));
    assert_eq!(health.timeout, Some(Duration::from_secs(1)));
    assert_eq!(health.healthy_after, Some(3));
}

#[test]
fn strip_prefix_on_root_warns() {
    let source = MINIMAL.replace("port = 8080", "port = 8080\nstrip_prefix = true");
    let parsed = parse(&source).unwrap();
    assert_eq!(parsed.warnings.len(), 1);
    let warning = &parsed.warnings[0];
    assert_eq!(
        warning.path.to_string(),
        "services.app.routes[0].strip_prefix"
    );
    assert!(warning.message.contains("no effect"));
    assert!(app(&parsed.stack).routes[0].strip_prefix);

    let source = MINIMAL.replace(
        "port = 8080",
        "port = 8080\nprefix = \"/api\"\nstrip_prefix = true",
    );
    let parsed = parse(&source).unwrap();
    assert!(parsed.warnings.is_empty());
    assert!(app(&parsed.stack).routes[0].strip_prefix);
}

#[test]
fn volumes_with_start_first_warn() {
    let with_volume = format!("{MINIMAL}\n[services.app.volumes]\ndata = \"/data\"\n");
    let parsed = parse(&with_volume).unwrap();
    assert_eq!(parsed.warnings.len(), 1, "{:?}", parsed.warnings);
    assert_eq!(parsed.warnings[0].path.to_string(), "services.app.volumes");
    assert!(parsed.warnings[0].message.contains("stop-first"));

    let stop_first = with_volume.replace(
        "image = \"ghcr.io/acme/app\"",
        "image = \"ghcr.io/acme/app\"\nupdate = \"stop-first\"",
    );
    assert!(parse(&stop_first).unwrap().warnings.is_empty());
}

#[test]
fn container_env_adds_port_for_one_container_port_only() {
    let env = |source: &str| app(&parse(source).unwrap().stack).container_env();
    assert_eq!(env(MINIMAL)["PORT"], "8080");

    // `env` wins over the injected value.
    let own = MINIMAL.replace(
        "image = \"ghcr.io/acme/app\"",
        "image = \"ghcr.io/acme/app\"\nenv = { PORT = \"3000\", A = \"1\" }",
    );
    let own = env(&own);
    assert_eq!(own["PORT"], "3000");
    assert_eq!(own["A"], "1");

    // A secret named `PORT` sets it too: no duplicate entry.
    let secret = format!("{MINIMAL}\n[services.app.secrets]\nPORT = \"demo/port\"\n");
    assert!(!env(&secret).contains_key("PORT"));

    // Two routes, same port: still one distinct port.
    let same = format!(
        "{MINIMAL}\n[[services.app.routes]]\nhost = \"demo.example.com\"\nprefix = \"/b\"\nport = 8080\n"
    );
    assert_eq!(env(&same)["PORT"], "8080");

    // Two distinct ports: nothing.
    let multi = format!(
        "{MINIMAL}\n[[services.app.routes]]\nhost = \"demo.example.com\"\nprefix = \"/b\"\nport = 9090\n"
    );
    assert!(!env(&multi).contains_key("PORT"));

    // No routes: nothing.
    let unrouted = "version = 1\nname = \"demo\"\n[services.app]\nimage = \"ghcr.io/acme/app\"\n";
    assert!(env(unrouted).is_empty());
}

#[test]
fn errors_carry_location_and_path() {
    let source = MINIMAL.replace("demo.example.com", "Demo.example.com");
    let err = parse(&source).unwrap_err();
    let [diagnostic] = err.diagnostics() else {
        panic!("{err}");
    };
    assert_eq!(diagnostic.path.to_string(), "services.app.routes[0].host");
    // `host = "Demo..."` is on line 9; the value starts after `host = `.
    assert_eq!(diagnostic.location, Some(Location { line: 9, column: 8 }));

    // Deserialization errors carry the path too.
    let source = MINIMAL.replace("port = 8080", "port = \"8080\"");
    let err = parse(&source).unwrap_err();
    let [diagnostic] = err.diagnostics() else {
        panic!("{err}");
    };
    assert_eq!(diagnostic.path.to_string(), "services.app.routes[0].port");
    assert_eq!(diagnostic.location.map(|l| l.line), Some(10));
}

#[test]
fn location_counts_characters() {
    let source = "a\nbé c\n";
    assert_eq!(Location::of(source, 0), Location { line: 1, column: 1 });
    assert_eq!(Location::of(source, 2), Location { line: 2, column: 1 });
    assert_eq!(Location::of(source, 5), Location { line: 2, column: 3 });
    // Inside `é`: rounds down to the character start.
    assert_eq!(Location::of(source, 4), Location { line: 2, column: 2 });
    assert_eq!(Location::of(source, 100), Location { line: 3, column: 1 });
}

#[test]
fn field_paths_quote_unusual_keys() {
    let root = FieldPath::default();
    assert!(root.is_root());
    assert_eq!(
        root.key("services").key("api").index(2).to_string(),
        "services.api[2]"
    );
    assert_eq!(root.key("env").key("a b").to_string(), "env.\"a b\"");
}

/// Many independent problems in one file are all reported, in source order.
#[test]
fn error_messages_snapshot() {
    let source = r#"version = 2
name = "Bad_Name"
drain_timeout = "a while"

[deploy]
repository_id = -5
refs = ["main"]

[services.api]
image = "nginx"
track = "main"
stop_timeout = "10"
user = "::"

[services.api.env]
"bad-name" = "x"

[services.api.secrets]
TOKEN = "other/token"

[services.api.volumes]
data = "relative"

[[services.api.routes]]
host = "API.example.com."
prefix = "/api/"
port = 0
timeouts = { read = "0s" }

[[services.api.routes]]
host = "example.com:443"
prefix = "/a/../b"
port = 80

[services.worker]
image = "docker.io/library/redis"

[services.worker.health]
tcp = { port = 6379 }

[services.web]
image = "ghcr.io/acme/web"

[[services.web.routes]]
host = "example.com"
strip_prefix = true
port = 8080

[services.web.health]
http = { path = "health", port = 9090, expect_status = "200-299" }
healthy_after = -1
"#;
    insta::assert_snapshot!(parse(source).unwrap_err().to_string());
}

#[test]
fn deserialization_error_snapshot() {
    let source = MINIMAL.replace(
        "image = \"ghcr.io/acme/app\"",
        "image = \"ghcr.io/acme/app\"\nresources = { memory = \"512m\" }",
    );
    insta::assert_snapshot!(parse(&source).unwrap_err().to_string());
}

#[test]
fn probe_and_timing_errors_are_both_reported() {
    let source = format!(
        "{MINIMAL}\n[services.app.health]\nhttp = {{ path = \"x\" }}\nhealthy_after = -1\n"
    );
    let errors = errors(&source);
    assert_eq!(errors.len(), 2, "{errors:#?}");
    assert!(errors[0].contains("invalid health path `x`"));
    assert!(errors[1].contains("healthy_after: must be a positive integer"));
}
