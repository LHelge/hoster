//! Runs the real binary for commands that need no host setup: `hoster version` and
//! `hoster stack check`.

use std::path::Path;
use std::process::{Command, Output};

/// Runs `hoster <args>` with an empty home directory, so no configuration exists.
fn run(args: &[&str]) -> Output {
    let home = tempfile::tempdir().unwrap();
    Command::new(env!("CARGO_BIN_EXE_hoster"))
        .args(args)
        .env_clear()
        .env("HOME", home.path())
        .output()
        .unwrap()
}

/// [`run`], asserting that it succeeds.
fn hoster(args: &[&str]) -> Output {
    let output = run(args);
    assert!(
        output.status.success(),
        "hoster {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// The highest `<version>_` prefix among the migrations, which is the schema the binary embeds.
fn latest_migration() -> i64 {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|entry| {
            let name = entry.unwrap().file_name().into_string().unwrap();
            name.split_once('_')?.0.parse().ok()
        })
        .max()
        .expect("at least one migration")
}

#[test]
fn version_json_prints_version_target_and_schema() {
    let output = hoster(&["version", "--json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["schema", "target", "version"]);
    assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
    let target = json["target"].as_str().unwrap();
    assert!(target.contains("-linux-"), "{target}");
    assert_eq!(json["schema"], latest_migration());
}

#[test]
fn version_prints_the_version() {
    let stdout = String::from_utf8(hoster(&["version"]).stdout).unwrap();
    assert!(
        stdout.starts_with(concat!("hoster ", env!("CARGO_PKG_VERSION"), " (")),
        "{stdout}"
    );
}

#[test]
fn version_flag_still_works() {
    let stdout = String::from_utf8(hoster(&["--version"]).stdout).unwrap();
    assert_eq!(stdout.trim(), concat!("hoster ", env!("CARGO_PKG_VERSION")));
}

const STACK: &str = r#"version = 1
name = "demo"

[services.app]
image = "ghcr.io/acme/app"
"#;

/// Stack files in a temporary directory, by name.
struct StackFiles(tempfile::TempDir);

impl StackFiles {
    fn new() -> Self {
        let files = Self(tempfile::tempdir().unwrap());
        files.write("valid.toml", STACK);
        files.write("invalid.toml", &STACK.replace("\"demo\"", "\"Demo\""));
        files.write("syntax.toml", &STACK.replace("\"demo\"", "\"demo"));
        files.write("unknown-field.toml", &format!("{STACK}bogus = 1\n"));
        files.write(
            "warning.toml",
            &format!("{STACK}\n[services.app.volumes]\ndata = \"/data\"\n"),
        );
        files
    }

    fn write(&self, name: &str, toml: &str) {
        std::fs::write(self.0.path().join(name), toml).unwrap();
    }

    fn path(&self, name: &str) -> String {
        self.0.path().join(name).display().to_string()
    }

    /// Runs `hoster stack check <flags> <files>`.
    fn check(&self, flags: &[&str], names: &[&str]) -> (Option<i32>, String, String) {
        let paths: Vec<String> = names.iter().map(|n| self.path(n)).collect();
        let mut args = vec!["stack", "check"];
        args.extend(flags);
        args.extend(paths.iter().map(String::as_str));
        let output = run(&args);
        (
            output.status.code(),
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    }
}

#[test]
fn stack_check_passes_a_valid_file() {
    let files = StackFiles::new();
    let (code, stdout, _) = files.check(&[], &["valid.toml"]);
    assert_eq!(code, Some(0), "{stdout}");
    let valid = files.path("valid.toml");
    assert!(
        stdout.starts_with(&format!("{valid}: valid, stack `demo` with 1 service\n")),
        "{stdout}"
    );
    assert!(stdout.contains("\nnote: not checked offline"), "{stdout}");
}

#[test]
fn stack_check_fails_an_invalid_file_with_file_line_and_column() {
    let files = StackFiles::new();
    let (code, stdout, stderr) = files.check(&[], &["valid.toml", "invalid.toml"]);
    assert_eq!(code, Some(1), "{stdout}");
    let invalid = files.path("invalid.toml");
    assert!(
        stdout.contains(&format!("{invalid}:2:8: error: name: invalid name `Demo`")),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("{invalid}: invalid, 1 error\n")),
        "{stdout}"
    );
    assert_eq!(stderr.trim_end(), "error: stack files with errors: 1 of 2");
}

#[test]
fn stack_check_fails_toml_and_schema_errors_with_file_line_and_column() {
    let files = StackFiles::new();
    for (name, line) in [("syntax.toml", 2), ("unknown-field.toml", 6)] {
        let (code, stdout, _) = files.check(&[], &[name]);
        assert_eq!(code, Some(1), "{stdout}");
        let path = files.path(name);
        let first = stdout.lines().next().unwrap();
        assert!(first.starts_with(&format!("{path}:{line}:")), "{stdout}");
        assert!(first.contains(": error: "), "{stdout}");
        assert!(
            stdout.contains(&format!("{path}: invalid, 1 error\n")),
            "{stdout}"
        );
    }
}

#[test]
fn stack_check_passes_warnings_unless_denied() {
    let files = StackFiles::new();
    let warning = files.path("warning.toml");
    let (code, stdout, _) = files.check(&[], &["warning.toml"]);
    assert_eq!(code, Some(0), "{stdout}");
    assert!(
        stdout.contains(&format!("{warning}:8:8: warning: services.app.volumes: ")),
        "{stdout}"
    );
    let (code, _, stderr) = files.check(&["--deny-warnings"], &["warning.toml"]);
    assert_eq!(code, Some(1));
    assert_eq!(stderr.trim_end(), "error: 1 warning (--deny-warnings)");
}

#[test]
fn stack_check_prints_json() {
    let files = StackFiles::new();
    let (code, stdout, _) = files.check(
        &["--format", "json", "--deny-warnings"],
        &["valid.toml", "warning.toml"],
    );
    assert_eq!(code, Some(1));
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(json["ok"], false);
    assert_eq!(json["errors"], 0);
    assert_eq!(json["warnings"], 1);
    assert_eq!(json["files"][1]["diagnostics"][0]["severity"], "warning");
    assert_eq!(json["files"][1]["diagnostics"][0]["location"]["line"], 8);
    assert!(json["skipped"].as_array().unwrap().len() >= 4, "{json}");
}

#[test]
fn stack_check_reports_a_missing_file() {
    let files = StackFiles::new();
    let (code, stdout, _) = files.check(&[], &["missing.toml"]);
    assert_eq!(code, Some(1));
    let missing = files.path("missing.toml");
    assert!(
        stdout.starts_with(&format!("{missing}: error: cannot read the file: ")),
        "{stdout}"
    );
}
