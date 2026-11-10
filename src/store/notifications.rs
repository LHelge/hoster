//! Notification state (ADR-0058 §5–§9): the run-time settings row of Settings → Notifications,
//! and what the notifier keeps between runs: its cursor in the event log, the open problems it
//! has mailed (the de-duplication record) and the last delivery's result.
//!
//! The provider's API key is not here: it is a sealed secret (ADR-0029). Choosing which events
//! to mail is the notifier's job; the store only keeps the rows.

use sqlx::SqliteConnection;

use super::{Event, EventId, EventKind, Result, Store, StoreError, Timestamp};
use crate::notify::{MailAddress, ProviderKind};

/// The notification settings (ADR-0058 §5): one row, edited in Settings → Notifications. No
/// row means notifications are not configured.
///
/// A configuration may be saved disabled before it is complete, but never enabled without both
/// addresses ([`StoreError::NotificationsIncomplete`]); whether an API key is stored is the
/// secret store's to say.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NotificationSettings {
    pub provider: ProviderKind,
    /// The sender, on a domain verified at the provider.
    pub from: Option<MailAddress>,
    /// The one recipient.
    pub recipient: Option<MailAddress>,
    pub enabled: bool,
    /// Also mail every successful deploy and certificate (ADR-0058 §7).
    pub also_successes: bool,
}

impl NotificationSettings {
    /// The sender and recipient when notifications are switched on, else `None`.
    pub fn active(&self) -> Option<(&MailAddress, &MailAddress)> {
        match (self.enabled, &self.from, &self.recipient) {
            (true, Some(from), Some(to)) => Some((from, to)),
            _ => None,
        }
    }
}

/// What makes problems the same for de-duplication (ADR-0058 §8): the kind, stack and subject
/// of the event that opened it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProblemKey {
    pub kind: EventKind,
    pub stack: Option<String>,
    pub subject: Option<String>,
}

impl From<&Event> for ProblemKey {
    fn from(event: &Event) -> Self {
        Self {
            kind: event.kind,
            stack: event.stack.clone(),
            subject: event.subject.clone(),
        }
    }
}

impl ProblemKey {
    /// The key's columns: a missing stack or subject is stored as `''`, so the key is unique.
    fn columns(&self) -> (&str, &str) {
        (
            self.stack.as_deref().unwrap_or_default(),
            self.subject.as_deref().unwrap_or_default(),
        )
    }
}

/// An open problem that was mailed: the event and time of the last mail about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifiedProblem {
    pub key: ProblemKey,
    pub event: EventId,
    pub at: Timestamp,
}

/// One step of the notifier, stored in one transaction (ADR-0058 §6): the cursor moves to
/// `cursor`, the problems in `notified` are recorded (or their reminder time updated) and the
/// ones in `closed` are forgotten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifierStep {
    /// The id of the last event handled.
    pub cursor: EventId,
    pub notified: Vec<NotifiedProblem>,
    pub closed: Vec<ProblemKey>,
}

impl NotifierStep {
    /// A step that only moves the cursor to `cursor`.
    pub fn to(cursor: EventId) -> Self {
        Self {
            cursor,
            notified: Vec::new(),
            closed: Vec::new(),
        }
    }
}

/// The result of the last delivery, for the Settings status block (ADR-0058 §5, §9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastDelivery {
    pub at: Timestamp,
    pub outcome: DeliveryOutcome,
}

/// Whether a delivery went out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// The provider accepted the mail, with its id for it if it gave one.
    Sent { id: Option<String> },
    /// The mail was dropped; the error as `notify::DeliveryError` writes it (never the key).
    Failed { error: String },
}

impl Store {
    /// The notification settings, or `None` when not configured. A stored row that no longer
    /// validates (an unknown provider, a malformed address) is reported and read as not
    /// configured, so notifications stay off until it is saved again.
    pub async fn notification_settings(&self) -> Result<Option<NotificationSettings>> {
        let Some(row) = sqlx::query!(
            r#"SELECT provider, from_address, recipient, enabled AS "enabled: bool",
                      also_successes AS "also_successes: bool"
               FROM notification_settings WHERE id = 1"#
        )
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        let address = |raw: Option<String>| -> Result<Option<MailAddress>, String> {
            raw.map(|raw| raw.parse().map_err(|err| format!("{raw:?}: {err}")))
                .transpose()
        };
        let parsed = (|| {
            Ok::<_, String>(NotificationSettings {
                provider: row.provider.parse().map_err(|err| format!("{err}"))?,
                from: address(row.from_address)?,
                recipient: address(row.recipient)?,
                enabled: row.enabled,
                also_successes: row.also_successes,
            })
        })();
        Ok(parsed
            .inspect_err(|error| {
                tracing::warn!(error, "ignoring the stored notification settings");
            })
            .ok())
    }

    /// Stores the notification settings. Switching notifications on (from off or not
    /// configured) moves the notifier's cursor to the newest event in the same transaction, so
    /// nothing from before is mailed (ADR-0058 §6), and forgets the problems mailed before:
    /// the recoveries the cursor skips can no longer close them, and an open record would
    /// silence the next occurrence as a repeat (§8).
    pub async fn set_notification_settings(&self, settings: &NotificationSettings) -> Result<()> {
        if settings.enabled && settings.active().is_none() {
            return Err(StoreError::NotificationsIncomplete);
        }
        let provider = settings.provider.name();
        let from = settings.from.as_ref().map(AsRef::<str>::as_ref);
        let recipient = settings.recipient.as_ref().map(AsRef::<str>::as_ref);
        let now = Timestamp::now();
        let mut tx = self.begin().await?;
        let was_enabled = sqlx::query_scalar!(
            r#"SELECT enabled AS "enabled: bool" FROM notification_settings WHERE id = 1"#
        )
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(false);
        sqlx::query!(
            "INSERT INTO notification_settings
                 (id, provider, from_address, recipient, enabled, also_successes, updated_at)
             VALUES (1, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET provider = excluded.provider,
                                            from_address = excluded.from_address,
                                            recipient = excluded.recipient,
                                            enabled = excluded.enabled,
                                            also_successes = excluded.also_successes,
                                            updated_at = excluded.updated_at",
            provider,
            from,
            recipient,
            settings.enabled,
            settings.also_successes,
            now,
        )
        .execute(&mut *tx)
        .await?;
        if settings.enabled && !was_enabled {
            Self::start_notifier_at_newest(&mut tx).await?;
            sqlx::query!("DELETE FROM notified_problems")
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Removes the notification settings: notifications are no longer configured. The API key
    /// is the caller's to delete from the secret store. The notifier's state is kept until
    /// notifications are switched on again, which moves the cursor and forgets the problems.
    pub async fn clear_notification_settings(&self) -> Result<()> {
        sqlx::query!("DELETE FROM notification_settings WHERE id = 1")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The id of the last event the notifier has handled. Before the cursor has been placed
    /// (by an earlier call, a notifier step or switching notifications on), the call places it
    /// at the newest event and stores it.
    pub async fn notifier_cursor(&self) -> Result<EventId> {
        let mut tx = self.begin().await?;
        let stored = sqlx::query_scalar!(
            r#"SELECT cursor AS "cursor: EventId" FROM notifier_state WHERE id = 1"#
        )
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
        let cursor = match stored {
            Some(cursor) => cursor,
            None => Self::start_notifier_at_newest(&mut tx).await?,
        };
        tx.commit().await?;
        Ok(cursor)
    }

    /// Moves the cursor to the newest event (`0` in an empty log) on `conn`, which is inside a
    /// transaction. Returns the new cursor.
    async fn start_notifier_at_newest(conn: &mut SqliteConnection) -> Result<EventId> {
        let cursor =
            sqlx::query_scalar!(r#"SELECT coalesce(max(id), 0) AS "id!: EventId" FROM events"#)
                .fetch_one(&mut *conn)
                .await?;
        sqlx::query!(
            "INSERT INTO notifier_state (id, cursor) VALUES (1, ?)
             ON CONFLICT (id) DO UPDATE SET cursor = excluded.cursor",
            cursor,
        )
        .execute(&mut *conn)
        .await?;
        Ok(cursor)
    }

    /// Stores one step of the notifier in one transaction: the cursor and the de-duplication
    /// record move together, so a crash between them neither loses nor repeats a mail
    /// (ADR-0058 §6). The cursor never moves back.
    pub async fn advance_notifier(&self, step: &NotifierStep) -> Result<()> {
        let mut tx = self.begin().await?;
        sqlx::query!(
            "INSERT INTO notifier_state (id, cursor) VALUES (1, ?1)
             ON CONFLICT (id) DO UPDATE SET cursor = max(coalesce(cursor, ?1), ?1)",
            step.cursor,
        )
        .execute(&mut *tx)
        .await?;
        for key in &step.closed {
            let (stack, subject) = key.columns();
            sqlx::query!(
                "DELETE FROM notified_problems WHERE kind = ? AND stack = ? AND subject = ?",
                key.kind,
                stack,
                subject,
            )
            .execute(&mut *tx)
            .await?;
        }
        for problem in &step.notified {
            let (stack, subject) = problem.key.columns();
            sqlx::query!(
                "INSERT INTO notified_problems (kind, stack, subject, event_id, notified_at)
                 VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT (kind, stack, subject) DO UPDATE
                     SET event_id = excluded.event_id, notified_at = excluded.notified_at",
                problem.key.kind,
                stack,
                subject,
                problem.event,
                problem.at,
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Every open problem that was mailed, oldest mail first.
    pub async fn notified_problems(&self) -> Result<Vec<NotifiedProblem>> {
        let rows = sqlx::query!(
            r#"SELECT kind AS "kind: EventKind", stack, subject, event_id AS "event_id: EventId",
                      notified_at AS "notified_at: Timestamp"
               FROM notified_problems ORDER BY notified_at, event_id"#
        )
        .fetch_all(&self.pool)
        .await?;
        let given = |column: String| (!column.is_empty()).then_some(column);
        Ok(rows
            .into_iter()
            .map(|row| NotifiedProblem {
                key: ProblemKey {
                    kind: row.kind,
                    stack: given(row.stack),
                    subject: given(row.subject),
                },
                event: row.event_id,
                at: row.notified_at,
            })
            .collect())
    }

    /// Records the result of a delivery, the notifier's or Settings' test mail's, as the last
    /// one. The cursor is left alone.
    pub async fn record_delivery(&self, delivery: &LastDelivery) -> Result<()> {
        let (id, error) = match &delivery.outcome {
            DeliveryOutcome::Sent { id } => (id.as_deref(), None),
            DeliveryOutcome::Failed { error } => (None, Some(error.as_str())),
        };
        sqlx::query!(
            "INSERT INTO notifier_state (id, delivered_at, delivery_id, delivery_error)
             VALUES (1, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET delivered_at = excluded.delivered_at,
                                            delivery_id = excluded.delivery_id,
                                            delivery_error = excluded.delivery_error",
            delivery.at,
            id,
            error,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The last delivery's result, if there has been one.
    pub async fn last_delivery(&self) -> Result<Option<LastDelivery>> {
        let row = sqlx::query!(
            r#"SELECT delivered_at AS "delivered_at: Timestamp", delivery_id, delivery_error
               FROM notifier_state WHERE id = 1"#
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|row| {
            let outcome = match row.delivery_error {
                Some(error) => DeliveryOutcome::Failed { error },
                None => DeliveryOutcome::Sent {
                    id: row.delivery_id,
                },
            };
            row.delivered_at.map(|at| LastDelivery { at, outcome })
        }))
    }
}
