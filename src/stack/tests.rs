use super::*;

/// The worked example from `site/content/wiki/stack-format.md` §3, taken from the document itself so the
/// two cannot drift apart.
pub(super) fn worked_example() -> &'static str {
    let doc = include_str!("../../site/content/wiki/stack-format.md");
    let section = &doc[doc.find("\n# 3. Worked example").expect("§3 exists")..];
    let start = section.find("```toml\n").expect("§3 has a TOML block") + "```toml\n".len();
    let len = section[start..].find("```").expect("TOML block is closed");
    &section[start..start + len]
}

/// The second worked example (§3.2): an app and a worker depending on postgres and redis.
pub(super) fn dependency_example() -> &'static str {
    let doc = include_str!("../../site/content/wiki/stack-format.md");
    let section = &doc[doc.find("\n## 3.2 Worked example").expect("§3.2 exists")..];
    let start = section.find("```toml\n").expect("§3.2 has a TOML block") + "```toml\n".len();
    let len = section[start..].find("```").expect("TOML block is closed");
    &section[start..start + len]
}

/// The third worked example (§3.3): an image configured by an inline file and a secret file.
pub(super) fn files_example() -> &'static str {
    let doc = include_str!("../../site/content/wiki/stack-format.md");
    let section = &doc[doc.find("\n## 3.3 Worked example").expect("§3.3 exists")..];
    let start = section.find("```toml\n").expect("§3.3 has a TOML block") + "```toml\n".len();
    let len = section[start..].find("```").expect("TOML block is closed");
    &section[start..start + len]
}

/// The worked example from `site/content/wiki/third-party-images.md` §3: a compose file
/// translated by hand, all four images third-party.
fn third_party_example() -> &'static str {
    let doc = include_str!("../../site/content/wiki/third-party-images.md");
    let section = &doc[doc.find("\n# 3. Worked example").expect("§3 exists")..];
    let start = section.find("```toml\n").expect("§3 has a TOML block") + "```toml\n".len();
    let len = section[start..].find("```").expect("TOML block is closed");
    &section[start..start + len]
}

#[test]
fn third_party_example_validates_without_warnings() {
    let parsed = parse(third_party_example()).expect("the guide's example is valid");
    assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    let stack = parsed.stack;
    let name = |n: &str| n.parse::<Name>().unwrap();
    assert!(
        stack.services.values().all(|s| !s.image.is_ci_built()),
        "every image is third-party"
    );
    let order: Vec<&str> = stack
        .dependency_order()
        .into_iter()
        .map(AsRef::as_ref)
        .collect();
    assert_eq!(order, ["cache", "db", "web", "worker"]);
    for (service, update) in [
        ("web", UpdateStrategy::StartFirst),
        ("worker", UpdateStrategy::StartFirst),
        ("db", UpdateStrategy::StopFirst),
        ("cache", UpdateStrategy::StopFirst),
    ] {
        assert_eq!(stack.services[&name(service)].update, update, "{service}");
    }
    for service in ["db", "cache"] {
        assert!(
            matches!(
                stack.services[&name(service)]
                    .health
                    .as_ref()
                    .map(|h| &h.probe),
                Some(Probe::Exec { .. })
            ),
            "{service}"
        );
    }
    let web = &stack.services[&name("web")];
    let paths: Vec<String> = web.files.keys().map(ToString::to_string).collect();
    assert_eq!(paths, ["/etc/tracker/config.yml"]);
}

#[test]
fn files_example_validates_with_the_documented_owners_and_modes() {
    let parsed = parse(files_example()).expect("§3.3 is valid");
    assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    let files = &parsed.stack.services[&"proxy".parse::<Name>().unwrap()].files;
    let summary: Vec<String> = files
        .iter()
        .map(|(path, file)| format!("{path} {} {}", file.resolved_mode(), file.ownership()))
        .collect();
    assert_eq!(
        summary,
        [
            "/etc/nginx/conf.d/default.conf 0444 0:0",
            "/etc/nginx/htpasswd 0440 0:101"
        ]
    );
    // The multi-line string starts at its first line, as the reference says.
    let FileSource::Inline(config) = &files.values().next().unwrap().source else {
        panic!("inline");
    };
    assert!(config.starts_with("server {\n"), "{config}");
}

#[test]
fn dependency_example_validates_with_its_dependencies_and_checks() {
    let parsed = parse(dependency_example()).expect("§3.2 is valid");
    assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    let stack = parsed.stack;
    let name = |n: &str| n.parse::<Name>().unwrap();
    let order: Vec<&str> = stack
        .dependency_order()
        .into_iter()
        .map(AsRef::as_ref)
        .collect();
    assert_eq!(order, ["cache", "db", "app", "worker"]);
    for dependent in ["app", "worker"] {
        let deps: Vec<&str> = stack
            .dependencies(&name(dependent))
            .map(AsRef::as_ref)
            .collect();
        assert_eq!(deps, ["cache", "db"], "{dependent}");
    }
    for stop_first in ["db", "cache"] {
        let service = &stack.services[&name(stop_first)];
        assert_eq!(service.update, UpdateStrategy::StopFirst, "{stop_first}");
        assert!(
            matches!(
                service.health.as_ref().map(|h| &h.probe),
                Some(Probe::Exec { .. })
            ),
            "{stop_first}"
        );
    }
    assert!(stack.services[&name("worker")].health.is_none());
}

/// A minimal valid stack; tests append to or substitute into it.
pub(super) const MINIMAL: &str = r#"
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

/// The documentation site's own stack, which the owner applies by pasting it into the UI.
#[test]
fn docs_site_stack_validates_cleanly() {
    let parsed = parse(include_str!("../../site/docs.stack.toml")).expect("docs stack is valid");
    assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    let stack = parsed.stack;
    assert_eq!(stack.name.as_ref(), "docs");
    let web = &stack.services[&"web".parse().unwrap()];
    assert!(web.image.is_ci_built());
    assert_eq!(web.track.as_deref(), Some("latest"));
    let deploy = stack.deploy.as_ref().unwrap();
    assert_eq!(deploy.refs, ["refs/tags/v*"]);
    assert_eq!(deploy.environment.as_deref(), Some("production"));
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
        "set at most one of `http`, `tcp` or `exec`",
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
        "[services.app.health]\nscript = \"true\"",
        "unknown field `script`",
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

/// `health` on a service without routes: only `exec` (ADR-0056 Decision 4).
#[test]
fn non_routed_services_only_declare_exec_checks() {
    let worker = |health: &str| {
        format!(
            "version = 1\nname = \"demo\"\n\n[services.worker]\nimage = \"ghcr.io/acme/worker\"\n\n[services.worker.health]\n{health}\n"
        )
    };
    let stack = parse(&worker(
        "exec = [\"pg_isready\", \"-U\", \"app\"]\ntimeout = \"3s\"",
    ))
    .unwrap()
    .stack;
    let health = stack.services[&"worker".parse().unwrap()]
        .health
        .clone()
        .unwrap();
    assert_eq!(
        health.probe,
        Probe::Exec {
            command: vec!["pg_isready".into(), "-U".into(), "app".into()]
        }
    );
    assert_eq!(health.timeout, Some(Duration::from_secs(3)));

    assert_error(
        &worker("tcp = { port = 9000 }"),
        "services.worker.health.tcp: `tcp` probes need a route",
    );
    assert_error(
        &worker("http = { path = \"/up\" }"),
        "services.worker.health.http: `http` probes need a route",
    );
    assert_error(
        &worker("start_period = \"1m\""),
        "services.worker.health: a service without routes needs `exec`",
    );
}

#[test]
fn exec_checks_on_routed_services() {
    let source = format!("{MINIMAL}\n[services.app.health]\nexec = [\"/bin/check\"]\n");
    let stack = parse(&source).unwrap().stack;
    assert_eq!(
        app(&stack).health.as_ref().unwrap().probe,
        Probe::Exec {
            command: vec!["/bin/check".into()]
        }
    );

    let with = |health: &str| format!("{MINIMAL}\n[services.app.health]\n{health}\n");
    assert_error(
        &with("exec = []"),
        "services.app.health.exec: must name the command and its arguments",
    );
    assert_error(
        &with("exec = [\" \", \"x\"]"),
        "services.app.health.exec[0]: the command must not be empty",
    );
    assert_error(
        &with("http = { path = \"/up\" }\nexec = [\"true\"]"),
        "set at most one of `http`, `tcp` or `exec`",
    );
    assert_error(
        &with("tcp = { port = 8080 }\nexec = [\"true\"]"),
        "services.app.health.exec: set at most one",
    );
}

/// The health rules of ADR-0056 Decision 4, all in one file.
#[test]
fn health_errors_snapshot() {
    let source = r#"version = 1
name = "demo"

[services.db]
image = "docker.io/library/postgres:17"
health = { tcp = { port = 5432 } }

[services.cache]
image = "docker.io/library/redis:7"
health = { interval = "5s" }

[services.queue]
image = "docker.io/library/rabbitmq:4"
health = { exec = [] }

[services.app]
image = "ghcr.io/acme/app"
health = { http = { path = "/up" }, tcp = { port = 8080 }, exec = ["true"] }

[[services.app.routes]]
host = "demo.example.com"
port = 8080

[services.web]
image = "ghcr.io/acme/web"
health = { exec = ["", "-c", "true"] }

[services.worker]
image = "ghcr.io/acme/worker"
health = { exec = [" pg_isready", "-U"] }

[services.jobs]
image = "ghcr.io/acme/jobs"
health = { exec = ["check", "a\u0000b"] }

[[services.web.routes]]
host = "web.example.com"
port = 8080
"#;
    insta::assert_snapshot!(parse(source).unwrap_err().to_string());
}

fn names<'a>(names: impl IntoIterator<Item = &'a Name>) -> Vec<&'a str> {
    names.into_iter().map(AsRef::as_ref).collect()
}

fn name(name: &str) -> Name {
    name.parse().unwrap()
}

const DEPENDENCIES: &str = r#"
version = 1
name = "demo"

[services.web]
image = "ghcr.io/acme/web"
depends_on = ["api"]

[services.api]
image = "ghcr.io/acme/api"
depends_on = ["db", "cache"]

[services.cache]
image = "docker.io/library/redis:7"
depends_on = ["db"]

[services.db]
image = "docker.io/library/postgres:17"
update = "stop-first"
health = { exec = ["pg_isready", "-U", "app"] }

[services.admin]
image = "ghcr.io/acme/admin"
"#;

#[test]
fn dependency_chains_and_diamonds() {
    let stack = parse(DEPENDENCIES).unwrap().stack;
    assert_eq!(names(stack.dependencies(&name("api"))), ["cache", "db"]);
    assert_eq!(names(stack.dependencies(&name("db"))), Vec::<&str>::new());
    assert_eq!(names(stack.dependencies(&name("nope"))), Vec::<&str>::new());
    assert_eq!(
        stack.services[&name("web")].depends_on,
        BTreeSet::from([name("api")])
    );

    // The diamond api → {cache, db}, cache → db reaches db once.
    assert_eq!(
        names(stack.transitive_dependencies(&name("web"))),
        ["api", "cache", "db"]
    );
    assert_eq!(names(stack.transitive_dependencies(&name("cache"))), ["db"]);
    assert!(stack.transitive_dependencies(&name("admin")).is_empty());

    // Each service after its dependencies; name breaks ties (admin before db, both ready).
    assert_eq!(
        names(stack.dependency_order()),
        ["admin", "db", "cache", "api", "web"]
    );

    // A subset sorts in that order; a name the stack lacks comes last.
    let mut subset = vec![name("web"), name("ghost"), name("db"), name("api")];
    stack.sort_in_dependency_order(&mut subset, |n| n);
    assert_eq!(names(subset.iter()), ["db", "api", "web", "ghost"]);
}

#[test]
fn dependency_order_is_deterministic() {
    let stack = parse(DEPENDENCIES).unwrap().stack;
    // The same graph with its service tables declared in reverse gives the same order.
    let mut blocks: Vec<&str> = DEPENDENCIES.split("\n[services.").collect();
    let header = blocks.remove(0);
    assert!(blocks.len() > 2 && blocks[0].starts_with("web]"));
    blocks.reverse();
    let reordered = format!("{header}\n[services.{}", blocks.join("\n[services."));
    assert!(reordered.find("[services.db]") < reordered.find("[services.web]"));
    let other = parse(&reordered).unwrap().stack;
    assert_eq!(stack.dependency_order(), other.dependency_order());

    // Without dependencies it is name order.
    let flat = parse(MINIMAL).unwrap().stack;
    assert_eq!(names(flat.dependency_order()), ["app"]);
    let empty = DEPENDENCIES.replace("depends_on = [\"db\"]", "depends_on = []");
    let stack = parse(&empty).unwrap().stack;
    assert_eq!(
        names(stack.dependency_order()),
        ["admin", "cache", "db", "api", "web"]
    );
}

#[test]
fn dependency_graph_cycles_and_closures() {
    use super::dependency::Graph;

    let graph = Graph::new([
        ("a", vec!["b"]),
        ("b", vec!["c", "missing"]),
        ("c", vec!["a"]),
        ("d", vec!["d"]),
        ("e", vec!["f"]),
        ("f", vec!["e", "a"]),
        ("g", vec![]),
    ]);
    // Each strongly connected component once, from its smallest node; edges to unknown nodes
    // are dropped.
    assert_eq!(
        graph.cycles(),
        vec![vec!["a", "b", "c"], vec!["d"], vec!["e", "f"]]
    );
    assert_eq!(
        graph.transitive("e").into_iter().collect::<Vec<_>>(),
        ["a", "b", "c", "e", "f"]
    );
    assert!(graph.transitive("g").is_empty());
    // Nodes on a cycle come last, in name order.
    assert_eq!(graph.order(), ["g", "a", "b", "c", "d", "e", "f"]);

    // The shortest way back, not the first one found.
    let graph = Graph::new([("a", vec!["b", "c"]), ("b", vec!["c"]), ("c", vec!["a"])]);
    assert_eq!(graph.cycles(), vec![vec!["a", "c"]]);
}

#[test]
fn dependency_errors() {
    let with = |deps: &str| {
        DEPENDENCIES.replace(
            "image = \"ghcr.io/acme/admin\"",
            &format!("image = \"ghcr.io/acme/admin\"\ndepends_on = {deps}"),
        )
    };
    assert!(parse(&with("[\"web\"]")).is_ok());
    assert_error(
        &with("[\"nope\"]"),
        "services.admin.depends_on[0]: unknown service `nope`",
    );
    assert_error(
        &with("[\"db\", \"admin\"]"),
        "services.admin.depends_on[1]: a service cannot depend on itself",
    );
    assert_error(
        &with("[\"db\", \"db\"]"),
        "services.admin.depends_on[1]: `db` is listed more than once",
    );
    // A repeated unknown or self entry: the problem once, then the duplicate.
    for (deps, first) in [
        (
            "[\"nope\", \"nope\"]",
            "depends_on[0]: unknown service `nope`",
        ),
        (
            "[\"admin\", \"admin\"]",
            "depends_on[0]: a service cannot depend on itself",
        ),
    ] {
        let error = parse(&with(deps)).unwrap_err().to_string();
        assert!(error.contains(first), "{error}");
        assert!(error.contains("depends_on[1]: `"), "{error}");
        assert!(error.contains("is listed more than once"), "{error}");
        assert_eq!(
            error.matches(first.split_once(": ").unwrap().1).count(),
            1,
            "{error}"
        );
    }
    assert_error(&with("\"db\""), "invalid type");

    // A 2-cycle, reported once at the smallest member.
    let two = DEPENDENCIES.replace("depends_on = [\"db\"]", "depends_on = [\"db\", \"api\"]");
    assert_error(
        &two,
        "services.api.depends_on[1]: dependency cycle: api → cache → api",
    );
    // A 3-cycle.
    let three = DEPENDENCIES.replace("health = { exec", "depends_on = [\"web\"]\nhealth = { exec");
    assert_error(
        &three,
        "services.api.depends_on[0]: dependency cycle: api → db → web → api",
    );
}

/// Every kind of dependency error in one file, each with its path and location.
#[test]
fn dependency_errors_snapshot() {
    let source = r#"version = 1
name = "demo"

[services.app]
image = "ghcr.io/acme/app"
depends_on = ["worker", "ghost", "worker"]

[services.worker]
image = "ghcr.io/acme/worker"
depends_on = ["app", "worker"]

[services.a]
image = "ghcr.io/acme/a"
depends_on = ["b"]

[services.b]
image = "ghcr.io/acme/b"
depends_on = ["c"]

[services.c]
image = "ghcr.io/acme/c"
depends_on = ["a"]
"#;
    insta::assert_snapshot!(parse(source).unwrap_err().to_string());
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

#[test]
fn counted_pluralises() {
    assert_eq!(Counted(1, "service").to_string(), "1 service");
    assert_eq!(Counted(3, "route").to_string(), "3 routes");
    assert_eq!(Counted(0, "warning").to_string(), "0 warnings");
}

/// `MINIMAL` with `lines` added to `[services.app]`, after its `image`.
fn app_with(lines: &str) -> String {
    let image = "image = \"ghcr.io/acme/app\"\n";
    MINIMAL.replace(image, &format!("{image}{lines}\n"))
}

#[test]
fn files_parse_with_their_defaults() {
    let source = app_with(
        r#"user = "1000"
[services.app.files]
"/etc/app/config.yml" = { content = """
listen: 8080
""" }
"/etc/app/empty" = { content = "" }
"/run/secrets/creds.json" = { secret = "demo/creds" }
"/etc/app/shared.key" = { secret = "demo/key", mode = "0440", owner = "0:65534" }
"#,
    );
    let parsed = parse(&source).expect("valid");
    assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    let files = &app(&parsed.stack).files;
    let file = |path: &str| &files[&path.parse::<ContainerPath>().unwrap()];
    assert_eq!(files.len(), 4);

    let config = file("/etc/app/config.yml");
    assert_eq!(config.source, FileSource::Inline("listen: 8080\n".into()));
    assert_eq!(config.resolved_mode().to_string(), "0444");
    assert_eq!(config.ownership().to_string(), "0:0");
    assert_eq!(
        file("/etc/app/empty").source,
        FileSource::Inline(String::new())
    );

    let creds = file("/run/secrets/creds.json");
    assert_eq!(creds.source, FileSource::Secret("demo/creds".into()));
    assert_eq!(creds.resolved_mode().to_string(), "0400");
    assert_eq!(creds.ownership().to_string(), "user");

    let key = file("/etc/app/shared.key");
    assert_eq!(key.mode, Some("0440".parse().unwrap()));
    assert_eq!(key.ownership().to_string(), "0:65534");
}

/// Every format and path rule of ADR-0057 §1 and §2, all in one file.
#[test]
fn file_errors_snapshot() {
    let source = r#"version = 1
name = "demo"

[services.app]
image = "docker.io/library/nginx:1.27"
update = "stop-first"
volumes = { data = "/var/lib/app", cache = "/cache" }

[services.app.files]
"relative.conf" = { content = "" }
"/" = { content = "" }
"/etc/app/" = { content = "" }
"/etc//app" = { content = "" }
"/etc/./app" = { content = "" }
"/etc/../app" = { content = "" }
"/etc/nul\u0000" = { content = "" }
"/proc/sys/x" = { content = "" }
"/sys" = { content = "" }
"/dev/shm/x" = { content = "" }
"/etc/hosts" = { content = "" }
"/etc/hostname" = { content = "" }
"/etc/resolv.conf" = { content = "" }
"/run/.containerenv" = { content = "" }
"/etc/passwd" = { content = "" }
"/etc/group" = { content = "" }
"/var/lib/app" = { content = "" }
"/var/lib/app/config.yml" = { content = "" }
"/var/lib" = { content = "" }
"/etc/nginx" = { content = "" }
"/etc/nginx/nginx.conf" = { content = "" }
"/etc/a/both" = { content = "", secret = "demo/x" }
"/etc/a/neither" = { mode = "0644" }
"/etc/a/other-stack" = { secret = "other/x" }
"/etc/a/malformed" = { secret = "demo/a b" }
"/etc/a/short-mode" = { content = "", mode = "644" }
"/etc/a/sticky" = { content = "", mode = "1755" }
"/etc/a/not-octal" = { content = "", mode = "0808" }
"/etc/a/no-gid" = { content = "", owner = "1000" }
"/etc/a/big-uid" = { content = "", owner = "65536:0" }
"/etc/a/names" = { content = "", owner = "nginx:nginx" }
"/etc/a/two-problems" = { mode = "rw", owner = ":" }
"#;
    insta::assert_snapshot!(parse(source).unwrap_err().to_string());
}

#[test]
fn file_tables_reject_unknown_keys() {
    let source = app_with("files = { \"/etc/a\" = { content = \"\", template = true } }");
    let errors = errors(&source);
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert!(
        errors[0].contains("unknown field `template`"),
        "{errors:#?}"
    );
}

#[test]
fn file_paths_differing_by_a_prefix_only_are_independent() {
    // `/etc/app.d` is not below `/etc/app`, and `/datastore` is not in the volume at `/data`.
    let source = app_with(
        "update = \"stop-first\"\nvolumes = { data = \"/data\" }\n[services.app.files]\n\
         \"/etc/app\" = { content = \"\" }\n\"/etc/app.d/x\" = { content = \"\" }\n\
         \"/datastore/x\" = { content = \"\" }\n",
    );
    assert_eq!(errors(&source), Vec::<String>::new());
}

#[test]
fn inline_content_is_capped_per_file_and_per_stack() {
    let file = |path: &str, bytes: usize| {
        format!("\"{path}\" = {{ content = \"{}\" }}\n", "x".repeat(bytes))
    };
    let stack = |files: &str| app_with(&format!("[services.app.files]\n{files}"));

    // Exactly 256 KiB fits; one byte more does not.
    assert_eq!(
        errors(&stack(&file("/a", ConfigFile::MAX_BYTES))),
        Vec::<String>::new()
    );
    let over = errors(&stack(&file("/a", ConfigFile::MAX_BYTES + 1)));
    assert_eq!(over.len(), 1, "{}", over.len());
    assert!(
        over[0].contains("services.app.files.\"/a\".content")
            && over[0].contains("262145 bytes; a file holds at most 256 KiB"),
        "{}",
        &over[0][..200.min(over[0].len())]
    );

    // Four full files are exactly 1 MiB; a fifth of one byte is over, and reported once, there.
    let full: String = (0..4)
        .map(|i| file(&format!("/f{i}"), ConfigFile::MAX_BYTES))
        .collect();
    assert_eq!(errors(&stack(&full)), Vec::<String>::new());
    let more = format!("{full}{}{}", file("/g", 1), file("/h", 1));
    let over = errors(&stack(&more));
    assert_eq!(over.len(), 1, "{}", over.len());
    assert!(
        over[0].contains("services.app.files.\"/g\".content")
            && over[0].contains("over 1024 KiB in total"),
        "{}",
        &over[0][..200.min(over[0].len())]
    );

    // Secret files hold no content in the stack file and count as nothing.
    let secrets = format!("{full}\"/s\" = {{ secret = \"demo/s\" }}\n");
    assert_eq!(errors(&stack(&secrets)), Vec::<String>::new());
}

#[test]
fn container_paths_know_their_ancestors() {
    let path: ContainerPath = "/etc/app/conf.d/a.conf".parse().unwrap();
    assert_eq!(
        path.ancestors().collect::<Vec<_>>(),
        ["/etc", "/etc/app", "/etc/app/conf.d"]
    );
    let top: ContainerPath = "/a.conf".parse().unwrap();
    assert_eq!(top.ancestors().count(), 0);
    assert_eq!(path.to_string(), "/etc/app/conf.d/a.conf");
    let dir: ContainerPath = "/etc/app".parse().unwrap();
    assert!(dir.is_ancestor_of(&path));
    assert!(!path.is_ancestor_of(&dir));
    assert!(!dir.is_ancestor_of(&dir));
}

/// The check a deploy repeats on the path the container resolves (ADR-0057 §3), which also
/// sees the image's volumes, as images declare them.
#[test]
fn placement_is_checked_against_any_volume_list() {
    let path = |p: &str| p.parse::<ContainerPath>().unwrap();
    let image_volumes = ["/var/lib/postgresql/data/", "/prometheus"];
    assert!(path("/etc/app.conf").check_placement(image_volumes).is_ok());
    assert!(
        path("/prometheus-extra/x")
            .check_placement(image_volumes)
            .is_ok()
    );
    for inside in ["/prometheus/a.yml", "/var/lib/postgresql/data/pg.conf"] {
        let reason = path(inside).check_placement(image_volumes).unwrap_err();
        assert!(reason.contains("is in the volume mounted at"), "{reason}");
    }
    let reason = path("/prometheus")
        .check_placement(image_volumes)
        .unwrap_err();
    assert!(reason.contains("is the mount path of a volume"), "{reason}");
    let reason = path("/var/lib").check_placement(image_volumes).unwrap_err();
    assert!(
        reason.contains("parent directory of the volume mounted at `/var/lib/postgresql/data`"),
        "{reason}"
    );
    // A resolved path can land where the declared one did not look reserved.
    assert!(path("/dev/stdout").check_placement([]).is_err());
    assert!(path("/anything").check_placement(["/"]).is_err());
}

#[test]
fn file_modes_and_owners_round_trip() {
    for mode in ["0000", "0400", "0444", "0644", "0777"] {
        assert_eq!(mode.parse::<FileMode>().unwrap().to_string(), mode);
    }
    assert_eq!(u32::from("0640".parse::<FileMode>().unwrap()), 0o640);
    for mode in [
        "",
        "644",
        "00644",
        "0o644",
        "1755",
        "0788",
        "+644",
        "rw-r--r--",
    ] {
        assert!(mode.parse::<FileMode>().is_err(), "{mode}");
    }
    for owner in ["0:0", "1000:1000", "65535:65535", "0:65534"] {
        assert_eq!(owner.parse::<FileOwner>().unwrap().to_string(), owner);
    }
    for owner in [
        "", "0", "0:", ":0", "0:0:0", "65536:0", "0:65536", "-1:0", "+1:0", "a:b", "1 :0",
        "000000:0",
    ] {
        assert!(owner.parse::<FileOwner>().is_err(), "{owner}");
    }
}

#[test]
fn secret_refs_list_env_then_files() {
    let refs = |lines: &str| -> Vec<String> {
        let parsed = parse(&app_with(lines)).expect("valid");
        app(&parsed.stack)
            .secret_refs()
            .map(|r| format!("{} for {}", r.secret, r.usage))
            .collect()
    };
    assert_eq!(refs(""), Vec::<String>::new());
    assert_eq!(
        refs("secrets = { DB = \"demo/db\", API_KEY = \"demo/key\" }"),
        ["demo/key for API_KEY", "demo/db for DB"]
    );
    // Inline files reference nothing.
    assert_eq!(
        refs(
            "[services.app.files]\n\"/etc/a.conf\" = { content = \"x\" }\n\
             \"/run/secrets/b\" = { secret = \"demo/b\" }\n\
             \"/etc/app/a.json\" = { secret = \"demo/a\" }"
        ),
        [
            "demo/a for file /etc/app/a.json",
            "demo/b for file /run/secrets/b"
        ]
    );
    // One secret used for both is listed once per use, its env use first.
    assert_eq!(
        refs(
            "secrets = { DB = \"demo/db\" }\n\
             files = { \"/etc/db\" = { secret = \"demo/db\" }, \"/etc/db2\" = { secret = \"demo/db\" } }"
        ),
        [
            "demo/db for DB",
            "demo/db for file /etc/db",
            "demo/db for file /etc/db2"
        ]
    );
}

#[test]
fn runtime_options_parse() {
    let source = app_with(
        r#"read_only = true
shm_size = "256MiB"
init = true
tmpfs = { "/tmp" = "64MiB", "/var/cache/nginx" = "16MB" }"#,
    );
    let parsed = parse(&source).expect("valid");
    assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    let service = app(&parsed.stack);
    assert_eq!(service.shm_size.map(u64::from), Some(256 << 20));
    assert!(service.init);
    let tmpfs: Vec<(String, u64)> = service
        .tmpfs
        .iter()
        .map(|(path, size)| (path.to_string(), u64::from(*size)))
        .collect();
    assert_eq!(
        tmpfs,
        [
            ("/tmp".to_owned(), 64 << 20),
            ("/var/cache/nginx".to_owned(), 16_000_000)
        ]
    );

    // Unset, and the explicit forms of unset.
    for source in [MINIMAL.to_owned(), app_with("tmpfs = {}\ninit = false")] {
        let stack = parse(&source).unwrap().stack;
        let service = app(&stack);
        assert_eq!(service.shm_size, None);
        assert!(service.tmpfs.is_empty());
        assert!(!service.init);
    }
    // The floor is inclusive, in any unit.
    let floor = parse(&app_with(
        "shm_size = \"1MiB\"\ntmpfs = { \"/a\" = \"1049KB\", \"/b\" = \"1048576B\" }",
    ));
    assert!(floor.is_ok(), "{floor:?}");
}

/// Every rule on `shm_size`, `tmpfs` and `init` (ADR-0059 §1, §3), all in one file.
#[test]
fn runtime_option_errors_snapshot() {
    let source = r#"version = 1
name = "demo"

[services.app]
image = "docker.io/library/nginx:1.27"
update = "stop-first"
volumes = { data = "/var/lib/app" }
files = { "/etc/app/app.conf" = { content = "" }, "/etc/one.conf" = { content = "" } }
shm_size = "512KiB"

[services.app.tmpfs]
"relative" = "1MiB"
"/" = "1MiB"
"/cache/" = "1MiB"
"//cache" = "1MiB"
"/a/./b" = "1MiB"
"/a/../b" = "1MiB"
"/a:b" = "1MiB"
"/a,b" = "1MiB"
"/dev/shm" = "1MiB"
"/dev/shm/x" = "1MiB"
"/dev" = "1MiB"
"/proc/x" = "1MiB"
"/sys" = "1MiB"
"/var/lib/app" = "1MiB"
"/var/lib/app/cache" = "1MiB"
"/var" = "1MiB"
"/etc/app" = "1MiB"
"/etc/one.conf" = "1MiB"
"/etc/one.conf/x" = "1MiB"
"/srv" = "1MiB"
"/srv/inner" = "1MiB"
"/zero" = "0B"
"/small" = "1048575B"
"/bare" = "512"
"/integer" = 512
"/docker" = "512m"
"/fraction" = "1.5GiB"
"/spaced" = "512 MiB"
"/unknown" = "1TiB"
"/huge" = "8589934592GiB"
"/both:" = "0B"

[services.other]
image = "docker.io/library/redis:7"
shm_size = 268435456
"#;
    insta::assert_snapshot!(parse(source).unwrap_err().to_string());
}

#[test]
fn tmpfs_paths_differing_by_a_prefix_only_are_independent() {
    let source = app_with(
        "update = \"stop-first\"\nvolumes = { data = \"/data\" }\n\
         files = { \"/etc/app.conf\" = { content = \"\" } }\n\
         tmpfs = { \"/data2\" = \"1MiB\", \"/etc/app.conf.d\" = \"1MiB\", \"/run\" = \"1MiB\", \
         \"/run2\" = \"1MiB\", \"/devices\" = \"1MiB\", \"/dev2/shm\" = \"1MiB\" }",
    );
    let parsed = parse(&source);
    assert!(parsed.is_ok(), "{parsed:?}");
}

#[test]
fn limits_parse() {
    let source = app_with("[services.app.limits]\nmemory = \"512MiB\"\npids = 512\ncpus = 1.5");
    let parsed = parse(&source).expect("valid");
    assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    let limits = app(&parsed.stack).limits;
    assert_eq!(limits.memory.map(u64::from), Some(512 << 20));
    assert_eq!(limits.pids, Some(512));
    assert_eq!(limits.cpus.map(f64::from), Some(1.5));
    // An integer is a number of CPUs too.
    let whole = parse(&app_with("limits = { cpus = 2 }")).unwrap().stack;
    assert_eq!(app(&whole).limits.cpus.map(f64::from), Some(2.0));

    // Unset, and an empty table, are no limits at all.
    for source in [MINIMAL.to_owned(), app_with("limits = {}")] {
        let stack = parse(&source).unwrap().stack;
        assert!(app(&stack).limits.is_empty());
    }
    // The bounds are inclusive, in any unit.
    for floor in [
        "memory = \"8MiB\", pids = 1, cpus = 0.01",
        "memory = \"8389KB\", pids = 4294967295, cpus = 1024",
        "memory = \"8388608B\"",
        "memory = \"8192KiB\"",
        "memory = \"9MB\"",
    ] {
        let parsed = parse(&app_with(&format!("limits = {{ {floor} }}")));
        assert!(parsed.is_ok(), "{floor}: {parsed:?}");
    }
}

/// Every rule on `limits` (ADR-0059 §2), all in one file.
#[test]
fn limit_errors_snapshot() {
    let source = r#"version = 1
name = "demo"

[services.small]
image = "docker.io/library/nginx:1.27"
limits = { memory = "4MiB", pids = 0, cpus = 0.001 }

[services.large]
image = "docker.io/library/nginx:1.27"
limits = { memory = "8388607B", pids = 4294967296, cpus = 1024.5 }

[services.odd]
image = "docker.io/library/nginx:1.27"
limits = { memory = 536870912, pids = -1, cpus = nan }

[services.infinite]
image = "docker.io/library/nginx:1.27"
limits = { memory = "512m", cpus = inf }

[services.negative]
image = "docker.io/library/nginx:1.27"
limits = { cpus = -1 }
"#;
    insta::assert_snapshot!(parse(source).unwrap_err().to_string());
}

#[test]
fn limits_are_a_closed_table() {
    for unknown in [
        "memory_swap = \"1GiB\"",
        "cpu = 1",
        "memory_reservation = \"64MiB\"",
    ] {
        let err = parse(&app_with(&format!("limits = {{ {unknown} }}"))).unwrap_err();
        assert!(
            err.to_string().contains("unknown field"),
            "{unknown}: {err}"
        );
    }
    let err = parse(&app_with("limits = { cpus = \"1.5\" }")).unwrap_err();
    assert!(
        err.to_string().contains("a number of CPUs such as 1.5"),
        "{err}"
    );
}

/// ADR-0059 §2: shared memory and tmpfs are charged to the memory limit, so reaching it is a
/// warning at `memory`.
#[test]
fn shm_and_tmpfs_reaching_memory_are_warned_about() {
    let warning = |lines: &str| {
        let parsed = parse(&app_with(lines)).expect("valid");
        parsed
            .warnings
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    };
    assert!(warning("shm_size = \"63MiB\"\nlimits = { memory = \"64MiB\" }").is_empty());
    assert!(warning("shm_size = \"256MiB\"\ntmpfs = { \"/tmp\" = \"64MiB\" }").is_empty());
    assert_eq!(
        warning("shm_size = \"64MiB\"\nlimits = { memory = \"64MiB\" }"),
        [
            "line 8, column 21: services.app.limits.memory: `shm_size` (64MiB) reaches `memory` \
          (64MiB): shared memory and tmpfs count against the memory limit, so filling them \
          leaves the app nothing and can get the container OOM-killed"
        ]
    );
    let both = warning(
        "shm_size = \"32MiB\"\ntmpfs = { \"/tmp\" = \"16MiB\", \"/run\" = \"16MiB\" }\n\
         limits = { memory = \"64MiB\" }",
    );
    assert_eq!(both.len(), 1, "{both:?}");
    assert!(
        both[0].contains("`shm_size` and the `tmpfs` sizes (64MiB) reach `memory` (64MiB)"),
        "{both:?}"
    );
    let tmpfs = warning("tmpfs = { \"/tmp\" = \"1GiB\" }\nlimits = { memory = \"512MiB\" }");
    assert!(
        tmpfs[0].contains("the `tmpfs` sizes (1GiB) reach `memory` (512MiB)"),
        "{tmpfs:?}"
    );
}

/// `auto_update` (ADR-0061 §4): every policy on the images that may use it, `off` everywhere.
#[test]
fn auto_update_parses_with_its_default() {
    let source = r#"version = 1
name = "demo"

[services.plain]
image = "docker.io/library/postgres:16"

[services.floating]
image = "docker.io/library/postgres:16"
auto_update = "digest"

[services.pinned]
image = "ghcr.io/immich-app/immich-server:v2.3.1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
auto_update = "minor"

[services.suffixed]
image = "docker.io/library/redis:7.4-alpine"
auto_update = "patch"

[services.latest]
image = "docker.io/library/nginx:latest"
auto_update = "digest"

[services.ci]
image = "ghcr.io/acme/app"
track = "main"
auto_update = "off"
"#;
    let stack = parse(source).unwrap().stack;
    let policy = |name: &str| stack.services[&name.parse::<Name>().unwrap()].auto_update;
    assert_eq!(policy("plain"), AutoUpdate::Off);
    assert_eq!(policy("floating"), AutoUpdate::Digest);
    assert_eq!(policy("pinned"), AutoUpdate::Minor);
    assert_eq!(policy("suffixed"), AutoUpdate::Patch);
    assert_eq!(policy("latest"), AutoUpdate::Digest);
    assert_eq!(policy("ci"), AutoUpdate::Off);
    assert!(AutoUpdate::Minor.follows_versions() && !AutoUpdate::Digest.follows_versions());
}

/// Every rule on `auto_update` (ADR-0061 §4), all in one file.
#[test]
fn auto_update_errors_snapshot() {
    let source = r#"version = 1
name = "demo"

[services.ci]
image = "ghcr.io/acme/app"
auto_update = "digest"

[services.tracked]
image = "ghcr.io/acme/worker"
track = "main"
auto_update = "patch"

[services.digest-only]
image = "docker.io/library/postgres@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
auto_update = "digest"

[services.latest]
image = "docker.io/library/nginx:latest"
auto_update = "minor"

[services.four-parts]
image = "docker.io/acme/tool:1.2.3.4"
auto_update = "patch"
"#;
    insta::assert_snapshot!(parse(source).unwrap_err().to_string());
}

/// There is no `major` policy: majors are only ever shown (ADR-0061 §3).
#[test]
fn auto_update_has_no_major_policy() {
    let source = MINIMAL.replace(
        "image = \"ghcr.io/acme/app\"",
        "image = \"docker.io/library/postgres:16\"\nauto_update = \"major\"",
    );
    let err = parse(&source).unwrap_err().to_string();
    assert!(
        err.contains("unknown variant `major`, expected one of `off`, `digest`, `patch`, `minor`"),
        "{err}"
    );
}
