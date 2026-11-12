//! `hoster update` / `hoster rollback` over the updater's fakes.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::sync::atomic::Ordering;

use super::fake::{FakeReleases, Host, script, v};
use super::*;

/// A console with scripted answers that records what was printed and asked.
#[derive(Default)]
struct Script {
    out: String,
    asked: Vec<String>,
    /// `None` stands for a stdin that is not a terminal.
    answers: VecDeque<Option<bool>>,
}

impl Script {
    fn answering(answer: bool) -> Self {
        Self {
            answers: VecDeque::from([Some(answer)]),
            ..Self::default()
        }
    }

    fn not_interactive() -> Self {
        Self {
            answers: VecDeque::from([None]),
            ..Self::default()
        }
    }
}

impl Console for Script {
    fn print(&mut self, text: &dyn fmt::Display) -> io::Result<()> {
        self.out.push_str(&format!("{text}\n"));
        Ok(())
    }

    fn confirm(&mut self, question: &str) -> Result<bool, CommandError> {
        self.asked.push(question.to_owned());
        match self.answers.pop_front().expect("an answer was scripted") {
            Some(answer) => Ok(answer),
            None => Err(CommandError::NotInteractive),
        }
    }
}

fn update(version: Option<&str>, check: bool, yes: bool) -> UpdateCommand {
    UpdateCommand {
        version: version.map(v),
        check,
        yes,
    }
}

fn releases() -> FakeReleases {
    FakeReleases::default()
        .publish_binary("0.2.0", script("0.2.0", 7))
        .publish_binary("0.3.0-rc.1", script("0.3.0-rc.1", 7))
}

#[tokio::test]
async fn check_prints_versions_and_notes_and_changes_nothing() {
    let host = Host::new(&[("0.1.0", 7)]).await;
    let mut console = Script::default();
    update(None, true, false)
        .run(&host.updater(releases(), "0.1.0"), &mut console)
        .await
        .unwrap();
    assert!(console.out.contains("running    v0.1.0"), "{}", console.out);
    assert!(console.out.contains("latest     v0.2.0"), "{}", console.out);
    assert!(console.out.contains("notes for v0.2.0"), "{}", console.out);
    assert!(
        console.out.contains("v0.2.0 is available"),
        "{}",
        console.out
    );
    assert!(console.asked.is_empty());
    assert_eq!(host.current(), Some(v("0.1.0")));
    assert!(host.service.calls().is_empty());

    let mut console = Script::default();
    update(None, true, false)
        .run(&host.updater(releases(), "0.2.0"), &mut console)
        .await
        .unwrap();
    assert!(console.out.contains("is up to date"), "{}", console.out);
}

#[tokio::test]
async fn update_confirms_installs_and_restarts() {
    let host = Host::new(&[("0.1.0", 7)]).await;
    let mut console = Script::answering(true);
    update(None, false, false)
        .run(&host.updater(releases(), "0.1.0"), &mut console)
        .await
        .unwrap();
    assert_eq!(
        console.asked,
        ["Update hoster v0.1.0 to v0.2.0 and restart it?"]
    );
    assert!(console.out.contains("notes for v0.2.0"), "{}", console.out);
    assert!(console.out.contains("snapshot"), "{}", console.out);
    assert_eq!(host.current(), Some(v("0.2.0")));
    assert_eq!(host.service.calls(), [ServiceAction::Restart]);
}

#[tokio::test]
async fn update_with_yes_does_not_ask_and_a_named_pre_release_is_installed() {
    let host = Host::new(&[("0.1.0", 7)]).await;
    let mut console = Script::default();
    update(Some("v0.3.0-rc.1"), false, true)
        .run(&host.updater(releases(), "0.1.0"), &mut console)
        .await
        .unwrap();
    assert!(console.asked.is_empty());
    assert!(
        console.out.contains("requested  v0.3.0-rc.1"),
        "{}",
        console.out
    );
    assert!(
        console.out.contains("notes for v0.3.0-rc.1"),
        "{}",
        console.out
    );
    assert_eq!(host.current(), Some(v("0.3.0-rc.1")));
    assert_eq!(host.service.calls(), [ServiceAction::Restart]);
}

#[tokio::test]
async fn a_declined_or_unconfirmable_update_changes_nothing() {
    let host = Host::new(&[("0.1.0", 7)]).await;
    let err = update(None, false, false)
        .run(
            &host.updater(releases(), "0.1.0"),
            &mut Script::answering(false),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CommandError::Declined), "{err}");
    assert_eq!(err.exit_code(), 1);

    let err = update(None, false, false)
        .run(
            &host.updater(releases(), "0.1.0"),
            &mut Script::not_interactive(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("--yes"), "{err}");
    assert_eq!(err.exit_code(), 2);

    assert_eq!(host.current(), Some(v("0.1.0")));
    assert_eq!(host.installation.versions().unwrap(), [v("0.1.0")]);
    assert!(host.service.calls().is_empty());
}

#[tokio::test]
async fn up_to_date_succeeds_without_asking() {
    let host = Host::new(&[("0.2.0", 7)]).await;
    for version in [None, Some("v0.2.0")] {
        let mut console = Script::default();
        update(version, false, false)
            .run(&host.updater(releases(), "0.2.0"), &mut console)
            .await
            .unwrap();
        assert!(
            console.out.contains("v0.2.0 is up to date"),
            "{}",
            console.out
        );
        assert!(console.asked.is_empty());
    }
    assert!(host.service.calls().is_empty());
}

#[tokio::test]
async fn updater_errors_pass_through_with_their_message() {
    let host = Host::new(&[("0.2.0", 7)]).await;
    let source = releases().publish_binary("0.1.0", script("0.1.0", 7));
    let mut console = Script::default();
    let err = update(Some("0.1.0"), false, false)
        .run(&host.updater(source, "0.2.0"), &mut console)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("hoster rollback"), "{err}");
    assert_eq!(err.exit_code(), 1);
    assert!(console.asked.is_empty(), "a downgrade fails before asking");

    let host = Host::new(&[("0.1.0", 7)]).await;
    host.deployments.0.store(1, Ordering::SeqCst);
    // No answer is scripted: asking would panic, so this also shows it fails before asking.
    let err = update(None, false, false)
        .run(&host.updater(releases(), "0.1.0"), &mut Script::default())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        CommandError::Update(UpdateError::Deploying { count: 1 })
    ));
    assert_eq!(host.current(), Some(v("0.1.0")));
    assert!(host.service.calls().is_empty());
}

#[tokio::test]
async fn check_of_a_named_release_says_how_to_install_it_or_that_it_is_older() {
    let host = Host::new(&[("0.2.0", 7)]).await;
    let source = || releases().publish_binary("0.1.0", script("0.1.0", 7));

    let mut console = Script::default();
    update(Some("v0.3.0-rc.1"), true, false)
        .run(&host.updater(source(), "0.2.0"), &mut console)
        .await
        .unwrap();
    assert!(
        console
            .out
            .contains("run `hoster update --version v0.3.0-rc.1`"),
        "{}",
        console.out
    );

    let mut console = Script::default();
    update(Some("v0.1.0"), true, false)
        .run(&host.updater(source(), "0.2.0"), &mut console)
        .await
        .unwrap();
    assert!(
        console
            .out
            .contains("v0.1.0 is older than the running v0.2.0"),
        "{}",
        console.out
    );
    assert!(!console.out.contains("is available"), "{}", console.out);
    assert_eq!(host.current(), Some(v("0.2.0")));
    assert!(host.service.calls().is_empty());
}

#[tokio::test]
async fn rollback_shows_the_migrations_it_undoes_then_rolls_back() {
    let host = Host::new(&[("0.1.0", 5), ("0.2.0", 7)]).await;
    let mut console = Script::answering(true);
    RollbackCommand { yes: false }
        .run(
            &host.updater(FakeReleases::default(), "0.2.0"),
            &mut console,
        )
        .await
        .unwrap();
    let expected_undone =
        Store::migrations_between(SchemaVersion::from(5), Store::expected_schema_version());
    assert!(
        console.out.contains("rolling back hoster v0.2.0 → v0.1.0"),
        "{}",
        console.out
    );
    assert!(
        console.out.contains(&format!(
            "{expected_undone} database migration(s) will be undone"
        )),
        "{}",
        console.out
    );
    assert!(console.out.contains("is lost"), "{}", console.out);
    assert_eq!(console.asked, ["Roll back to v0.1.0?"]);
    assert_eq!(host.current(), Some(v("0.1.0")));
    assert_eq!(host.schema().await, SchemaVersion::from(5));
    assert_eq!(
        host.service.calls(),
        [ServiceAction::Stop, ServiceAction::Start]
    );
}

#[tokio::test]
async fn rollback_warns_about_log_records_and_reports_the_log_store() {
    let host = Host::new(&[("0.1.0", 5), ("0.2.0", 7)]).await;
    host.reinstall("0.1.0", 5, Some(1));
    host.open_logs().await;
    let mut console = Script::answering(true);
    RollbackCommand { yes: false }
        .run(
            &host.updater(FakeReleases::default(), "0.2.0"),
            &mut console,
        )
        .await
        .unwrap();
    let undone =
        LogStore::migrations_between(SchemaVersion::from(1), LogStore::expected_schema_version());
    for expected in [
        format!(
            "{undone} log store migration(s) will be undone (log schema {} → 1)",
            LogStore::expected_schema_version()
        ),
        "log records stored by the newer schema may be lost".to_owned(),
        "the log store is at log schema 1".to_owned(),
    ] {
        assert!(
            console.out.contains(&expected),
            "{expected}\n{}",
            console.out
        );
    }
    assert_eq!(host.log_schema().await, Some(SchemaVersion::from(1)));
}

#[tokio::test]
async fn rollback_says_when_the_log_store_was_moved_aside() {
    let host = Host::new(&[("0.1.0", 5), ("0.2.0", 7)]).await;
    host.reinstall("0.1.0", 5, Some(1));
    host.open_logs().await;
    host.log_sql("CREATE INDEX cursors_by_nth ON log_cursors (nth)")
        .await;
    let mut console = Script::default();
    RollbackCommand { yes: true }
        .run(
            &host.updater(FakeReleases::default(), "0.2.0"),
            &mut console,
        )
        .await
        .unwrap();
    let aside = host.state_dir.join("logs.db.rollback-v0.2.0");
    assert!(
        console.out.contains(&format!(
            "it was moved aside to {} and v0.1.0 starts with an empty one",
            aside.display()
        )),
        "{}",
        console.out
    );
    assert_eq!(host.current(), Some(v("0.1.0")));
}

#[tokio::test]
async fn rollback_to_a_binary_without_a_log_store_leaves_it_alone() {
    let host = Host::new(&[("0.1.0", 5), ("0.2.0", 7)]).await;
    host.open_logs().await;
    let mut console = Script::default();
    RollbackCommand { yes: true }
        .run(
            &host.updater(FakeReleases::default(), "0.2.0"),
            &mut console,
        )
        .await
        .unwrap();
    assert!(
        console
            .out
            .contains("v0.1.0 predates the log store; logs.db is left as it is"),
        "{}",
        console.out
    );
    assert!(
        !console.out.contains("log store migration"),
        "{}",
        console.out
    );
    assert_eq!(
        host.log_schema().await,
        Some(LogStore::expected_schema_version())
    );
}

#[tokio::test]
async fn a_declined_rollback_changes_nothing() {
    let host = Host::new(&[("0.1.0", 5), ("0.2.0", 7)]).await;
    let err = RollbackCommand { yes: false }
        .run(
            &host.updater(FakeReleases::default(), "0.2.0"),
            &mut Script::answering(false),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CommandError::Declined), "{err}");
    assert_eq!(host.current(), Some(v("0.2.0")));
    assert_eq!(host.schema().await, Store::expected_schema_version());
    assert!(host.service.calls().is_empty());
}

#[tokio::test]
async fn rollback_with_yes_and_nothing_to_undo() {
    let latest = i64::from(Store::expected_schema_version());
    let host = Host::new(&[("0.1.0", latest), ("0.2.0", latest)]).await;
    let mut console = Script::default();
    RollbackCommand { yes: true }
        .run(
            &host.updater(FakeReleases::default(), "0.2.0"),
            &mut console,
        )
        .await
        .unwrap();
    assert!(console.asked.is_empty());
    assert!(
        console
            .out
            .contains("no database migrations need to be undone"),
        "{}",
        console.out
    );
    assert_eq!(host.current(), Some(v("0.1.0")));
}

#[tokio::test]
async fn rollback_without_a_previous_version_reports_the_updater_error() {
    let host = Host::new(&[("0.2.0", 7)]).await;
    let err = RollbackCommand { yes: true }
        .run(
            &host.updater(FakeReleases::default(), "0.2.0"),
            &mut Script::default(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("nothing to roll back to"), "{err}");
    assert_eq!(err.exit_code(), 1);
}

#[test]
fn root_is_refused_with_a_hint() {
    let err = Uid::from(0).refuse_root().unwrap_err();
    for hint in ["sudo -iu hoster", "sudo machinectl shell hoster@"] {
        assert!(err.to_string().contains(hint), "{err}");
    }
    assert_eq!(err.exit_code(), 1);
    Uid::from(1000).refuse_root().unwrap();
}
