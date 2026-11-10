//! The event log (ADR-0055): deploy outcomes, container restarts and health changes,
//! certificate outcomes and operator and system actions, kept 30 days.
//!
//! Producers record through the orchestrator's `EventLog`, which also tells open pages; a store
//! method whose state change an event describes writes it in the same transaction with
//! [`Store::insert_event`]. The table is append-only apart from [`Store::prune_events`].

use sqlx::SqliteConnection;

use super::{
    Event, EventId, EventKind, EventPage, EventQuery, NewEvent, Result, Severity, Store, Timestamp,
};

impl Store {
    /// Records `event` as happening now.
    pub async fn record_event(&self, event: &NewEvent) -> Result<Event> {
        let mut conn = self.pool.acquire().await?;
        Self::insert_event(&mut conn, event, Timestamp::now()).await
    }

    /// Records `event` as happening at `at`; for tests, which need to place events in time.
    #[cfg(test)]
    pub(crate) async fn record_event_at(&self, event: &NewEvent, at: Timestamp) -> Result<Event> {
        let mut conn = self.pool.acquire().await?;
        Self::insert_event(&mut conn, event, at).await
    }

    /// Appends `event` to the event log on `conn`, which may be inside a transaction: the event
    /// is then stored only if the state change it describes is.
    pub(super) async fn insert_event(
        conn: &mut SqliteConnection,
        event: &NewEvent,
        at: Timestamp,
    ) -> Result<Event> {
        let id = sqlx::query_scalar!(
            r#"INSERT INTO events (at, kind, severity, stack, subject, message, meta)
               VALUES (?, ?, ?, ?, ?, ?, ?)
               RETURNING id AS "id: EventId""#,
            at,
            event.kind,
            event.severity,
            event.stack,
            event.subject,
            event.message,
            event.meta,
        )
        .fetch_one(conn)
        .await?;
        Ok(Event {
            id,
            at,
            kind: event.kind,
            severity: event.severity,
            stack: event.stack.clone(),
            subject: event.subject.clone(),
            message: event.message.clone(),
            meta: event.meta.clone(),
        })
    }

    /// One page of the events `query` asks for, newest first, and where the next one starts.
    pub async fn event_page(&self, query: &EventQuery) -> Result<EventPage> {
        // A zero limit would return an empty page with no cursor, which reads as the end of
        // the log; the limit may come from a query string, so it is bounded above too.
        let limit = query.limit.clamp(1, EventQuery::MAX);
        // The kind and severity filters are JSON arrays of their column texts, for
        // `json_each`; `NULL` kinds means every kind.
        let kinds = (!query.kinds.is_empty()).then(|| {
            let kinds: Vec<_> = query.kinds.iter().map(|k| k.as_str()).collect();
            serde_json::Value::from(kinds).to_string()
        });
        let severities: Vec<_> = Severity::ALL
            .into_iter()
            .filter(|s| *s >= query.min_severity)
            .map(Severity::as_str)
            .collect();
        let severities = serde_json::Value::from(severities).to_string();
        let before_at = query.before.map(|c| c.at);
        let before_id = query.before.map(|c| c.id);
        // One more than asked for tells whether there is a next page.
        let fetch = i64::from(limit) + 1;
        let mut events = sqlx::query_as!(
            Event,
            r#"SELECT id AS "id: EventId", at AS "at: Timestamp", kind AS "kind: EventKind",
                      severity AS "severity: Severity", stack, subject, message, meta
               FROM events
               WHERE (?1 IS NULL OR stack = ?1)
                 AND (?2 IS NULL OR kind IN (SELECT value FROM json_each(?2)))
                 AND severity IN (SELECT value FROM json_each(?3))
                 AND (?4 IS NULL OR at < ?4 OR (at = ?4 AND id < ?5))
               ORDER BY at DESC, id DESC
               LIMIT ?6"#,
            query.stack,
            kinds,
            severities,
            before_at,
            before_id,
            fetch,
        )
        .fetch_all(&self.pool)
        .await?;
        let next = if events.len() > limit as usize {
            events.truncate(limit as usize);
            events.last().map(Event::cursor)
        } else {
            None
        };
        Ok(EventPage { events, next })
    }

    /// The events after `cursor` (by id, the order they were recorded in), oldest first, at
    /// most `limit` of them: what the notifier has not handled yet (ADR-0058 §6).
    pub async fn events_after(&self, cursor: EventId, limit: u32) -> Result<Vec<Event>> {
        Ok(sqlx::query_as!(
            Event,
            r#"SELECT id AS "id: EventId", at AS "at: Timestamp", kind AS "kind: EventKind",
                      severity AS "severity: Severity", stack, subject, message, meta
               FROM events WHERE id > ? ORDER BY id LIMIT ?"#,
            cursor,
            limit,
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// Deletes every event older than `cutoff` (the retention sweep) except the newest one.
    /// Returns how many.
    ///
    /// `events.id` has no AUTOINCREMENT, so SQLite gives a new row `max(id) + 1`: emptying the
    /// table would hand out ids again from 1, below the notifier's cursor, and those events
    /// would never be mailed (ADR-0058 §6). Keeping the newest row keeps ids increasing.
    pub async fn prune_events(&self, cutoff: Timestamp) -> Result<u64> {
        let done = sqlx::query!(
            "DELETE FROM events WHERE at < ? AND id < (SELECT max(id) FROM events)",
            cutoff
        )
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected())
    }
}
