//! The [`EventLog`] handle (ADR-0055): producers record an event through it, and it tells the
//! open UI pages once the event is stored. The log never depends on a page keeping up.

use std::time::Duration;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::UiEvent;
use crate::store::{Event, EventPage, EventQuery, NewEvent, Store, StoreError, Timestamp};

/// Records events in the store and announces them on `ui_events`. Cheap to clone; every
/// producer (orchestrator, reconciler, ACME task, web handlers, update flow) holds one.
#[derive(Debug, Clone)]
pub struct EventLog {
    store: Store,
    ui_events: broadcast::Sender<UiEvent>,
}

impl EventLog {
    /// How long events are kept (ADR-0055 §4).
    pub const RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
    /// How often the retention sweep runs.
    const SWEEP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

    pub(super) fn new(store: Store, ui_events: broadcast::Sender<UiEvent>) -> Self {
        Self { store, ui_events }
    }

    /// Records `event` and tells the open pages. A failed write is logged at `warn` and
    /// otherwise ignored: the operation the event describes has happened regardless.
    pub async fn record(&self, event: NewEvent) {
        match self.store.record_event(&event).await {
            Ok(event) => self.announce(event),
            Err(err) => tracing::warn!(
                error = format!("{:#}", anyhow::Error::from(err)),
                kind = %event.kind,
                stack = event.stack,
                "cannot record event"
            ),
        }
    }

    /// Tells the open pages about `event`, which a store transaction already recorded.
    pub fn announce(&self, event: Event) {
        // No receiver is not an error: no UI page is open.
        let _ = self.ui_events.send(UiEvent::Event(event));
    }

    /// One page of the events `query` asks for, newest first (see [`Store::event_page`]).
    pub async fn page(&self, query: &EventQuery) -> Result<EventPage, StoreError> {
        self.store.event_page(query).await
    }

    /// Deletes the events older than [`Self::RETENTION`] once now and then daily, until
    /// `cancel` fires. A failed sweep is logged and tried again the next day.
    pub async fn run_retention(self, cancel: CancellationToken) {
        loop {
            let cutoff = Timestamp::now() - Self::RETENTION;
            match self.store.prune_events(cutoff).await {
                Ok(0) => {}
                Ok(deleted) => tracing::info!(deleted, "pruned old events"),
                Err(err) => tracing::warn!(
                    error = format!("{:#}", anyhow::Error::from(err)),
                    "cannot prune old events; trying again tomorrow"
                ),
            }
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(Self::SWEEP_INTERVAL) => {}
            }
        }
        tracing::debug!("event retention stopped");
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::store::{EventKind, Severity};

    async fn log() -> (TempDir, EventLog, broadcast::Receiver<UiEvent>) {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let (tx, rx) = broadcast::channel(16);
        (dir, EventLog::new(store, tx), rx)
    }

    #[tokio::test]
    async fn recorded_events_are_stored_then_announced() {
        let (_dir, log, mut rx) = log().await;
        log.record(
            NewEvent::new(EventKind::StackApplied, Severity::Info, "applied notes").stack("notes"),
        )
        .await;
        let UiEvent::Event(announced) = rx.try_recv().unwrap() else {
            panic!("not an event");
        };
        assert_eq!(announced.kind, EventKind::StackApplied);
        let page = log.page(&EventQuery::default()).await.unwrap();
        assert_eq!(page.events, [announced]);
    }

    #[tokio::test]
    async fn a_failed_write_is_not_announced_and_does_not_fail_the_caller() {
        let (_dir, log, mut rx) = log().await;
        log.store.close().await;
        log.record(NewEvent::new(
            EventKind::HosterStarted,
            Severity::Info,
            "started",
        ))
        .await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn retention_prunes_at_startup_and_stops_on_cancel() {
        let (_dir, log, _rx) = log().await;
        let old = NewEvent::new(EventKind::HosterStarted, Severity::Info, "long ago");
        let day = Duration::from_secs(24 * 60 * 60);
        let now = Timestamp::now();
        log.store
            .record_event_at(&old, now - EventLog::RETENTION - day)
            .await
            .unwrap();
        let kept = log
            .store
            .record_event_at(&old, now - EventLog::RETENTION + day)
            .await
            .unwrap();

        let cancel = CancellationToken::new();
        let task = tokio::spawn(log.clone().run_retention(cancel.clone()));
        let pruned = async {
            loop {
                let page = log.page(&EventQuery::default()).await.unwrap();
                if page.events.len() == 1 {
                    return page.events;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let events = tokio::time::timeout(Duration::from_secs(5), pruned)
            .await
            .expect("the startup sweep prunes the old event");
        assert_eq!(events, [kept]);
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("cancel stops the retention task")
            .unwrap();
    }
}
