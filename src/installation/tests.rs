use std::os::unix::fs::{MetadataExt, symlink};

use super::*;

fn v(s: &str) -> Version {
    s.parse().unwrap()
}

/// A layout in a temp dir, plus a stand-in binary to install.
struct Fixture {
    dir: tempfile::TempDir,
    layout: Installation,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let layout = Installation::new(dir.path().join("lib/hoster"));
        Self { dir, layout }
    }

    /// Installs a binary whose contents name `version`.
    fn install(&self, version: &str) -> PathBuf {
        let source = self.dir.path().join(format!("download-{version}"));
        fs::write(&source, format!("#!/bin/sh\necho {version}\n")).unwrap();
        self.layout.install(&v(version), &source).unwrap()
    }

    fn with(versions: &[&str], current: &str) -> Self {
        let fixture = Self::new();
        for version in versions {
            fixture.install(version);
        }
        fixture.layout.switch(&v(current)).unwrap();
        fixture
    }
}

#[test]
fn version_parses_with_or_without_v_and_displays_as_the_directory_name() {
    assert_eq!(v("1.2.3"), v("v1.2.3"));
    assert_eq!(v("1.2.3").to_string(), "v1.2.3");
    assert_eq!(v("v0.2.0-rc.1").to_string(), "v0.2.0-rc.1");
    assert!(v("v0.2.0-rc.1").is_prerelease());
    assert!(!v("v0.2.0").is_prerelease());
    assert!("latest".parse::<Version>().is_err());
    assert!("v1.2".parse::<Version>().is_err());
    assert_eq!(
        Version::running().to_string(),
        concat!("v", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn versions_order_by_semver_precedence() {
    let mut versions = vec![v("0.10.0"), v("0.2.0"), v("0.2.0-rc.1"), v("0.9.1")];
    versions.sort();
    assert_eq!(
        versions,
        vec![v("0.2.0-rc.1"), v("0.2.0"), v("0.9.1"), v("0.10.0")]
    );
}

#[test]
fn build_info_serializes_as_the_version_json() {
    let info = BuildInfo {
        version: "1.2.3".parse().unwrap(),
        target: "x86_64-unknown-linux-musl".to_owned(),
        schema: 7,
    };
    let json = serde_json::to_string(&info).unwrap();
    assert_eq!(
        json,
        r#"{"version":"1.2.3","target":"x86_64-unknown-linux-musl","schema":7}"#
    );
    assert_eq!(serde_json::from_str::<BuildInfo>(&json).unwrap(), info);
    assert_eq!(
        info.to_string(),
        "hoster 1.2.3 (x86_64-unknown-linux-musl, schema 7)"
    );

    let running = BuildInfo::running();
    assert_eq!(running.version.to_string(), env!("CARGO_PKG_VERSION"));
    assert!(!running.target.is_empty());
    assert_eq!(running.schema, i64::from(Store::expected_schema_version()));
    assert!(running.schema >= 7, "{}", running.schema);
}

#[test]
fn an_empty_layout_has_nothing() {
    let fx = Fixture::new();
    assert_eq!(fx.layout.versions().unwrap(), vec![]);
    assert_eq!(fx.layout.current().unwrap(), None);
    assert_eq!(fx.layout.previous().unwrap(), None);
    assert!(matches!(
        fx.layout.prune(),
        Err(InstallationError::NoCurrent { .. })
    ));
}

#[test]
fn install_writes_an_executable_binary_atomically() {
    let fx = Fixture::new();
    let binary = fx.install("0.1.0");
    assert_eq!(binary, fx.layout.root().join("versions/v0.1.0/hoster"));
    assert_eq!(binary, fx.layout.binary(&v("0.1.0")));
    assert_eq!(
        fs::read_to_string(&binary).unwrap(),
        "#!/bin/sh\necho 0.1.0\n"
    );
    assert_eq!(fs::metadata(&binary).unwrap().mode() & 0o777, 0o755);
    // No temporary file is left behind.
    let names: Vec<_> = fs::read_dir(binary.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec!["hoster"]);
    // Installing is not switching.
    assert_eq!(fx.layout.versions().unwrap(), vec![v("0.1.0")]);
    assert_eq!(fx.layout.current().unwrap(), None);
}

#[test]
fn install_again_replaces_the_binary() {
    let fx = Fixture::new();
    let binary = fx.install("0.1.0");
    let source = fx.dir.path().join("rebuilt");
    fs::write(&source, "new").unwrap();
    fx.layout.install(&v("0.1.0"), &source).unwrap();
    assert_eq!(fs::read_to_string(binary).unwrap(), "new");
}

#[test]
fn install_of_a_missing_file_fails_and_leaves_no_binary() {
    let fx = Fixture::new();
    let err = fx
        .layout
        .install(&v("0.1.0"), &fx.dir.path().join("nope"))
        .unwrap_err();
    assert!(matches!(err, InstallationError::Io { .. }), "{err}");
    assert_eq!(fx.layout.versions().unwrap(), vec![]);
    let dir = fx.layout.root().join("versions/v0.1.0");
    assert_eq!(fs::read_dir(dir).unwrap().count(), 0);
}

#[test]
fn switch_points_current_at_a_relative_version_dir() {
    let fx = Fixture::with(&["0.1.0"], "0.1.0");
    let link = fx.layout.current_link();
    assert_eq!(fs::read_link(&link).unwrap(), Path::new("versions/v0.1.0"));
    assert_eq!(fx.layout.current().unwrap(), Some(v("0.1.0")));
    assert_eq!(
        fs::read_to_string(link.join("hoster")).unwrap(),
        "#!/bin/sh\necho 0.1.0\n"
    );

    fx.install("0.2.0");
    fx.layout.switch(&v("0.2.0")).unwrap();
    assert_eq!(fx.layout.current().unwrap(), Some(v("0.2.0")));
    assert_eq!(fs::read_link(&link).unwrap(), Path::new("versions/v0.2.0"));
    // Only `current` and `versions` remain at the root: the temporary link was renamed.
    let mut names: Vec<_> = fs::read_dir(fx.layout.root())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, vec!["current", "versions"]);
}

#[test]
fn switch_to_an_uninstalled_version_is_refused() {
    let fx = Fixture::with(&["0.1.0"], "0.1.0");
    let err = fx.layout.switch(&v("0.3.0")).unwrap_err();
    assert!(
        matches!(err, InstallationError::NotInstalled { .. }),
        "{err}"
    );
    assert_eq!(fx.layout.current().unwrap(), Some(v("0.1.0")));
}

#[test]
fn previous_with_one_two_and_three_versions() {
    let one = Fixture::with(&["0.1.0"], "0.1.0");
    assert_eq!(one.layout.previous().unwrap(), None);

    let two = Fixture::with(&["0.1.0", "0.2.0"], "0.2.0");
    assert_eq!(two.layout.previous().unwrap(), Some(v("0.1.0")));

    let three = Fixture::with(&["0.1.0", "0.2.0", "0.3.0"], "0.3.0");
    assert_eq!(three.layout.previous().unwrap(), Some(v("0.2.0")));

    // After a rollback to 0.2.0, the previous version is the one below it.
    three.layout.switch(&v("0.2.0")).unwrap();
    assert_eq!(three.layout.previous().unwrap(), Some(v("0.1.0")));
}

#[test]
fn prune_keeps_current_and_previous() {
    let one = Fixture::with(&["0.1.0"], "0.1.0");
    assert_eq!(one.layout.prune().unwrap(), vec![]);
    assert_eq!(one.layout.versions().unwrap(), vec![v("0.1.0")]);

    let two = Fixture::with(&["0.1.0", "0.2.0"], "0.2.0");
    assert_eq!(two.layout.prune().unwrap(), vec![]);
    assert_eq!(two.layout.versions().unwrap(), vec![v("0.1.0"), v("0.2.0")]);

    let four = Fixture::with(&["0.1.0", "0.2.0", "0.3.0", "0.4.0"], "0.4.0");
    assert_eq!(four.layout.prune().unwrap(), vec![v("0.1.0"), v("0.2.0")]);
    assert_eq!(
        four.layout.versions().unwrap(),
        vec![v("0.3.0"), v("0.4.0")]
    );
    assert!(!four.layout.root().join("versions/v0.1.0").exists());
    assert_eq!(four.layout.current().unwrap(), Some(v("0.4.0")));
}

#[test]
fn dangling_current_is_no_current() {
    let fx = Fixture::with(&["0.1.0", "0.2.0"], "0.2.0");
    fs::remove_dir_all(fx.layout.root().join("versions/v0.2.0")).unwrap();
    assert_eq!(fx.layout.current().unwrap(), None);
    assert_eq!(fx.layout.previous().unwrap(), None);
    assert!(matches!(
        fx.layout.prune(),
        Err(InstallationError::NoCurrent { .. })
    ));
    // Nothing was deleted, and switching repairs it.
    assert_eq!(fx.layout.versions().unwrap(), vec![v("0.1.0")]);
    fx.layout.switch(&v("0.1.0")).unwrap();
    assert_eq!(fx.layout.current().unwrap(), Some(v("0.1.0")));
}

#[test]
fn current_outside_versions_is_an_error() {
    let fx = Fixture::with(&["0.1.0"], "0.1.0");
    let link = fx.layout.current_link();
    fs::remove_file(&link).unwrap();
    symlink("/usr/local/bin", &link).unwrap();
    let err = fx.layout.current().unwrap_err();
    assert!(
        matches!(err, InstallationError::ForeignCurrent { .. }),
        "{err}"
    );
}

#[test]
fn versions_ignores_strays() {
    let fx = Fixture::new();
    fx.install("0.1.0");
    let versions = fx.layout.root().join("versions");
    fs::create_dir_all(versions.join("v0.2.0")).unwrap(); // no binary
    fs::create_dir_all(versions.join("notes")).unwrap();
    fs::write(versions.join("v0.3.0"), "a file").unwrap();
    // Not the name install gives v0.4.0, even with a binary in it.
    fs::create_dir_all(versions.join("0.4.0")).unwrap();
    fs::write(versions.join("0.4.0/hoster"), "binary").unwrap();
    assert_eq!(fx.layout.versions().unwrap(), vec![v("0.1.0")]);
    assert_eq!(fx.layout.version_of(&versions.join("0.4.0/hoster")), None);

    // `current` naming a stray is foreign, not v0.4.0.
    symlink("versions/0.4.0", fx.layout.current_link()).unwrap();
    assert!(matches!(
        fx.layout.current(),
        Err(InstallationError::ForeignCurrent { .. })
    ));
}

#[test]
fn version_of_recognises_binaries_inside_the_layout() {
    let fx = Fixture::with(&["0.1.0", "0.2.0"], "0.2.0");
    let layout = &fx.layout;
    assert_eq!(
        layout.version_of(&layout.binary(&v("0.1.0"))),
        Some(v("0.1.0"))
    );
    assert_eq!(
        layout.version_of(&layout.current_link().join("hoster")),
        Some(v("0.2.0"))
    );
    let elsewhere = fx.dir.path().join("download-0.1.0");
    assert_eq!(layout.version_of(&elsewhere), None);
    assert_eq!(layout.version_of(&fx.dir.path().join("missing")), None);
    assert_eq!(
        Installation::new(fx.dir.path().join("none")).version_of(&elsewhere),
        None
    );
}

#[test]
fn uninstall_removes_a_version_but_never_the_current_one() {
    let fx = Fixture::with(&["0.1.0", "0.2.0"], "0.1.0");
    fx.layout.uninstall(&v("0.2.0")).unwrap();
    assert_eq!(fx.layout.versions().unwrap(), [v("0.1.0")]);
    // Uninstalling what is not installed is a no-op.
    fx.layout.uninstall(&v("0.3.0")).unwrap();
    let err = fx.layout.uninstall(&v("0.1.0")).unwrap_err();
    assert!(
        matches!(err, InstallationError::RemovingCurrent { .. }),
        "{err}"
    );
    assert_eq!(fx.layout.current().unwrap(), Some(v("0.1.0")));
}

#[tokio::test]
async fn probe_reads_another_binarys_version_json() {
    let fx = Fixture::new();
    let source = fx.dir.path().join("probe");
    fs::write(
        &source,
        "#!/bin/sh\n[ \"$1 $2\" = 'version --json' ] || exit 3\n\
         echo '{\"version\":\"0.1.0\",\"target\":\"x86_64-unknown-linux-musl\",\"schema\":5}'\n",
    )
    .unwrap();
    let binary = fx.layout.install(&v("0.1.0"), &source).unwrap();
    let info = BuildInfo::probe(&binary).await.unwrap();
    assert_eq!(
        info,
        BuildInfo {
            version: "0.1.0".parse().unwrap(),
            target: "x86_64-unknown-linux-musl".into(),
            schema: 5,
        }
    );

    // A binary that does not know the subcommand, or prints something else.
    for script in ["#!/bin/sh\nexit 2\n", "#!/bin/sh\necho hoster 0.0.1\n"] {
        fs::write(&source, script).unwrap();
        let binary = fx.layout.install(&v("0.0.1"), &source).unwrap();
        let err = BuildInfo::probe(&binary).await.unwrap_err();
        assert!(matches!(err, InstallationError::Probe { .. }), "{err}");
    }
    let err = BuildInfo::probe(&fx.dir.path().join("missing"))
        .await
        .unwrap_err();
    assert!(matches!(err, InstallationError::Probe { .. }), "{err}");
}
