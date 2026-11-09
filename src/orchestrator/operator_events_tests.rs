//! The operator events of the event log (ADR-0055 §2): a stack applied or removed and a secret
//! set or deleted, written by the orchestrator against [`FakeRuntime`] and a real (temporary)
//! store, naming the operator who did it.

use super::tests::{NOTES, WITH_SECRETS, harness};
use std::sync::LazyLock;

use super::*;
use crate::store::{Event, EventKind, EventQuery, Severity, ShortDigest};

/// The operator the tests act as.
static OCTOCAT: LazyLock<Operator> = LazyLock::new(|| Operator::from("octocat"));

impl tests::Harness {
    /// The events of `kind`, oldest first.
    async fn events_of(&self, kind: EventKind) -> Vec<Event> {
        let query = EventQuery {
            kinds: [kind].into(),
            limit: EventQuery::MAX,
            ..EventQuery::default()
        };
        let mut events = self.store.event_page(&query).await.unwrap().events;
        events.reverse();
        events
    }

    /// The events of `kind` as (message, meta), oldest first.
    async fn lines_of(&self, kind: EventKind) -> Vec<(String, String)> {
        self.events_of(kind)
            .await
            .into_iter()
            .map(|e| (e.message, e.meta.unwrap_or_default()))
            .collect()
    }

    fn octocat(&self) -> Acting<'_, crate::runtime::FakeRuntime> {
        self.orchestrator.by(&OCTOCAT)
    }
}

#[tokio::test]
async fn an_apply_records_the_revision_its_hash_and_who_applied_it() {
    let h = harness().await;
    let mut ui = h.orchestrator.subscribe();
    let applied = h.octocat().apply(NOTES).await.unwrap();
    h.finish(applied.deployment.as_ref().unwrap()).await;

    let [event] = h
        .events_of(EventKind::StackApplied)
        .await
        .try_into()
        .unwrap();
    assert_eq!(event.severity, Severity::Info);
    assert_eq!(event.stack.as_deref(), Some("notes"));
    assert_eq!(event.subject.as_deref(), Some("/stacks/notes"));
    assert_eq!(event.message, "Applied revision 1 of notes");
    let hash = ShortDigest(applied.revision.hash.as_ref()).to_string();
    assert_eq!(
        event.meta.as_deref(),
        Some(format!("revision 1 · {hash} · by @octocat").as_str())
    );
    // Announced to the open pages, before the deploy events it leads to.
    let announced = loop {
        match ui.try_recv().unwrap() {
            UiEvent::Event(announced) => break announced,
            _ => continue,
        }
    };
    assert_eq!(announced, event);

    // Without an operator (no UI), the meta names nobody.
    h.apply(&NOTES.replace("rustfs:1.0.0", "rustfs:1.0.1"))
        .await;
    let events = h.events_of(EventKind::StackApplied).await;
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].message, "Applied revision 2 of notes");
    assert!(
        !events[1].meta.as_deref().unwrap().contains("by "),
        "{:?}",
        events[1].meta
    );
}

#[tokio::test]
async fn a_rejected_apply_records_nothing() {
    let h = harness().await;
    assert!(h.octocat().apply("version = 1").await.is_err());
    assert!(h.events_of(EventKind::StackApplied).await.is_empty());
}

#[tokio::test]
async fn a_removal_records_whether_volumes_went_and_outlives_the_stack() {
    let h = harness().await;
    h.bootstrap().await;
    h.octocat()
        .remove(RemoveStack::keeping_volumes("notes"))
        .await
        .unwrap();
    assert_eq!(
        h.lines_of(EventKind::StackRemoved).await,
        [(
            "Removed notes; its volumes were kept".to_owned(),
            "volumes kept · by @octocat".to_owned()
        )]
    );

    // The stack is gone, its history is not: the feed filtered to it still has every line.
    assert!(h.store.stack("notes").await.unwrap().is_none());
    let query = EventQuery {
        stack: Some("notes".into()),
        limit: EventQuery::MAX,
        ..EventQuery::default()
    };
    let kinds: BTreeSet<_> = h
        .store
        .event_page(&query)
        .await
        .unwrap()
        .events
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(kinds.contains(&EventKind::StackApplied), "{kinds:?}");
    assert!(kinds.contains(&EventKind::DeploySucceeded), "{kinds:?}");
    assert!(kinds.contains(&EventKind::StackRemoved), "{kinds:?}");

    // Deleting the kept volumes later is recorded too.
    h.octocat()
        .remove(RemoveStack::deleting_volumes("notes", "notes"))
        .await
        .unwrap();
    let events = h.events_of(EventKind::StackRemoved).await;
    let deleted = events.last().unwrap();
    assert_eq!(deleted.severity, Severity::Warning);
    assert_eq!(deleted.message, "Deleted the kept volumes of notes");
    assert_eq!(
        deleted.meta.as_deref(),
        Some("volumes deleted: hoster-notes-data · by @octocat")
    );
}

#[tokio::test]
async fn a_removal_that_deletes_the_volumes_says_which() {
    let h = harness().await;
    h.bootstrap().await;
    h.octocat()
        .remove(RemoveStack::deleting_volumes("notes", "notes"))
        .await
        .unwrap();
    let [event] = h
        .events_of(EventKind::StackRemoved)
        .await
        .try_into()
        .unwrap();
    assert_eq!(event.severity, Severity::Warning);
    assert_eq!(event.stack.as_deref(), Some("notes"));
    assert_eq!(event.subject.as_deref(), Some("/activity?stack=notes"));
    assert_eq!(event.message, "Removed notes and deleted its volumes");
    assert_eq!(
        event.meta.as_deref(),
        Some("volumes deleted: hoster-notes-data · by @octocat")
    );

    // A refused removal records nothing.
    assert!(
        h.octocat()
            .remove(RemoveStack::keeping_volumes("ghost"))
            .await
            .is_err()
    );
    assert_eq!(h.events_of(EventKind::StackRemoved).await.len(), 1);
}

#[tokio::test]
async fn secret_events_carry_the_name_and_scope_never_the_value() {
    let h = harness().await;
    h.apply(WITH_SECRETS).await;
    let value = "postgres://s3cr3t@db";
    let set = h
        .octocat()
        .set_secret("notes", "unused", &value.into())
        .await
        .unwrap();
    assert!(!set.existed);
    h.octocat()
        .set_secret("notes", "unused", &"postgres://other".into())
        .await
        .unwrap();
    h.octocat().delete_secret("notes", "unused").await.unwrap();
    // Deleting a secret that does not exist changes nothing and records nothing.
    h.octocat().delete_secret("notes", "unused").await.unwrap();
    // The event names the secret as stored, trimmed.
    let padded = h
        .octocat()
        .set_secret("notes", "  padded ", &value.into())
        .await
        .unwrap();
    assert_eq!(padded.secret, "notes/padded");

    let set = h.events_of(EventKind::SecretSet).await;
    let deleted = h.events_of(EventKind::SecretDeleted).await;
    let lines: Vec<_> = set
        .iter()
        .chain(&deleted)
        .map(|e| (e.message.as_str(), e.meta.as_deref().unwrap()))
        .collect();
    assert_eq!(
        lines,
        [
            (
                "Set the secret unused of notes",
                "scope notes · by @octocat"
            ),
            (
                "Replaced the secret unused of notes",
                "scope notes · by @octocat"
            ),
            (
                "Set the secret padded of notes",
                "scope notes · by @octocat"
            ),
            (
                "Deleted the secret unused of notes",
                "scope notes · by @octocat"
            ),
        ]
    );
    for event in set.iter().chain(&deleted) {
        assert_eq!(event.stack.as_deref(), Some("notes"));
        assert_eq!(event.subject.as_deref(), Some("/stacks/notes/environment"));
        assert!(!format!("{event:?}").contains("postgres"), "{event:?}");
    }
}
