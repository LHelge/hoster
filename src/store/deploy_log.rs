//! A stack's deploy log: its deployment events over a time range, in time order across its
//! deployments, as the stack logs tab's `deploy` source pages them (epic psq). It reads
//! `hoster.db`'s `deployment_events`, so it is not bound by the log store's retention.
//!
//! A page goes through the stack's deployments that overlap the range (the `(stack, id)` index,
//! at most [`DeploymentLogQuery::DEPLOYMENT_CAP`] of them) and reads their events in the range
//! by the `(deployment_id, id)` index, never the whole table. Grep matches the message in Rust
//! over at most [`DeploymentLogQuery::SCAN_CAP`] events; when it stops there before the page is
//! full, the page says where ([`Page::scan_stopped`]), as a log store page does.

use std::ops::Range;

use super::{
    DeploymentEvent, DeploymentId, DeploymentState, Page, PageAt, RecordKey, Result, Store,
    TextFilter, Timestamp,
};

/// Which of a stack's deployment events a page lists: stamped in `range`, with a message
/// `text` matches, from `page` on.
#[derive(Debug, Clone)]
pub struct DeploymentLogQuery {
    pub stack: String,
    /// Events stamped at or after the start and before the end.
    pub range: Range<Timestamp>,
    pub text: Option<TextFilter>,
    pub page: PageAt,
    /// At most this many events; clamped to `1..=`[`DeploymentLogQuery::MAX_LIMIT`].
    pub limit: u32,
}

impl DeploymentLogQuery {
    /// The most deployments one page reads the events of: the newest that overlap the range
    /// (the oldest when paging towards newer ones). Paging moves the window.
    pub const DEPLOYMENT_CAP: u32 = 1_000;
    /// The most events one page visits.
    pub const SCAN_CAP: u32 = 5_000;
    /// The most events a page holds.
    pub const MAX_LIMIT: u32 = 1_000;
}

/// A deployment event, with its id and its deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentLogEntry {
    pub id: i64,
    pub deployment: DeploymentId,
    pub event: DeploymentEvent,
}

/// A page of a stack's deploy log.
pub type DeploymentLogPage = Page<DeploymentLogEntry>;

impl DeploymentLogEntry {
    /// Its place in page order: its time, then its id.
    pub fn key(&self) -> RecordKey {
        RecordKey {
            time: self.event.at,
            id: self.id,
        }
    }

    fn matches(&self, text: Option<&TextFilter>) -> bool {
        text.is_none_or(|text| text.is_match(self.event.message.as_deref().unwrap_or_default()))
    }
}

struct Row {
    id: i64,
    deployment: DeploymentId,
    from: Option<DeploymentState>,
    to: DeploymentState,
    message: Option<String>,
    at: Timestamp,
}

impl From<Row> for DeploymentLogEntry {
    fn from(row: Row) -> Self {
        Self {
            id: row.id,
            deployment: row.deployment,
            event: DeploymentEvent {
                from: row.from,
                to: row.to,
                message: row.message,
                at: row.at,
            },
        }
    }
}

impl Store {
    /// One page of the deploy log `query` asks for, oldest first.
    pub async fn deployment_log(&self, query: &DeploymentLogQuery) -> Result<DeploymentLogPage> {
        let limit = query.limit.clamp(1, DeploymentLogQuery::MAX_LIMIT);
        let Range { start, end } = query.range;
        // One more than the page shows tells whether there is more beyond it; grep visits up
        // to the cap.
        let rows = match query.text {
            Some(_) => DeploymentLogQuery::SCAN_CAP,
            None => limit + 1,
        };
        let deployments = DeploymentLogQuery::DEPLOYMENT_CAP;
        let older = !matches!(query.page, PageAt::After(_));
        let visited: Vec<Row> = if older {
            // Older than the cursor: `(at, id) < (t, id)`; the newest page is older than the end.
            let (t, id) = match query.page {
                PageAt::Before(key) if key.time < end => (key.time, key.id),
                _ => (end, i64::MIN),
            };
            sqlx::query_as!(
                Row,
                r#"SELECT e.id AS "id!: i64", e.deployment_id AS "deployment: DeploymentId",
                          e.from_state AS "from: DeploymentState",
                          e.to_state AS "to: DeploymentState",
                          e.message, e.at AS "at: Timestamp"
                   FROM deployment_events e
                   WHERE e.deployment_id IN (
                           SELECT id FROM deployments
                           WHERE stack = ?1 AND created_at <= ?2
                             AND (finished_at IS NULL OR finished_at >= ?3)
                           ORDER BY id DESC LIMIT ?4)
                     AND e.at >= ?3 AND (e.at < ?2 OR (e.at = ?2 AND e.id < ?5))
                   ORDER BY e.at DESC, e.id DESC
                   LIMIT ?6"#,
                query.stack,
                t,
                start,
                deployments,
                id,
                rows,
            )
            .fetch_all(&self.pool)
            .await?
        } else {
            // Newer than the cursor: `(at, id) > (t, id)`, within the range.
            let (t, id) = match query.page {
                PageAt::After(key) if key.time >= start => (key.time, key.id),
                _ => (start, i64::MIN),
            };
            sqlx::query_as!(
                Row,
                r#"SELECT e.id AS "id!: i64", e.deployment_id AS "deployment: DeploymentId",
                          e.from_state AS "from: DeploymentState",
                          e.to_state AS "to: DeploymentState",
                          e.message, e.at AS "at: Timestamp"
                   FROM deployment_events e
                   WHERE e.deployment_id IN (
                           SELECT id FROM deployments
                           WHERE stack = ?1 AND created_at < ?2
                             AND (finished_at IS NULL OR finished_at >= ?3)
                           ORDER BY id LIMIT ?4)
                     AND e.at < ?2 AND (e.at > ?3 OR (e.at = ?3 AND e.id > ?5))
                   ORDER BY e.at, e.id
                   LIMIT ?6"#,
                query.stack,
                end,
                t,
                deployments,
                id,
                rows,
            )
            .fetch_all(&self.pool)
            .await?
        };
        let exhausted = visited.len() < rows as usize;
        let wanted = limit as usize + 1;
        let mut last = None;
        let mut found = Vec::with_capacity(wanted);
        for row in visited {
            let entry = DeploymentLogEntry::from(row);
            last = Some(entry.key());
            if entry.matches(query.text.as_ref()) {
                found.push(entry);
                if found.len() == wanted {
                    break;
                }
            }
        }
        let more = found.len() == wanted;
        found.truncate(limit as usize);
        let scan_stopped = (!more && !exhausted).then_some(last).flatten();
        let beyond = more || scan_stopped.is_some();
        if older {
            found.reverse();
        }
        Ok(Page {
            entries: found,
            older: if older { beyond } else { true },
            newer: if older {
                query.page != PageAt::Newest
            } else {
                beyond
            },
            scan_stopped,
        })
    }
}
