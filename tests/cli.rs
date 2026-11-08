//! Runs the real binary for commands that need no host setup: `hoster version`.

use std::path::Path;
use std::process::{Command, Output};

/// Runs `hoster <args>` with an empty home directory, so no configuration exists.
fn hoster(args: &[&str]) -> Output {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_hoster"))
        .args(args)
        .env_clear()
        .env("HOME", home.path())
        .output()
        .unwrap();
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
