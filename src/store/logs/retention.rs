//! Retention of the log store (ADR-0067 §7, ADR-0069 §7): records and request rows older than
//! [`Retention::MAX_AGE`] and spans older than [`Retention::SPAN_MAX_AGE`] are deleted, then
//! whole hours, oldest first and across all three tables,
//! while the file holds more than [`Retention::MAX_BYTES`]; each pass ends with an incremental
//! vacuum that gives the freed pages back to the file system.
//!
//! A pass is a [`Sweep`] that the recorder, the one writer, advances [`Sweep::step`] by step
//! between its commits: every step is one short statement (at most [`Retention::BATCH_ROWS`]
//! rows deleted, or 2048 pages (8 MiB at the default 4 KiB page) returned), so a big sweep never holds
//! the write lock against the commits of the last second.
//!
//! No step ever deletes a table's newest row: the logs UI pages and follows by id, and the
//! tables use `INTEGER PRIMARY KEY` without AUTOINCREMENT, so an emptied table would hand its
//! ids out again (see the first migration's header).

use std::time::Duration;

use super::LogStore;
use crate::store::{Result, Timestamp};

/// The limits a sweep keeps the store to. [`Retention::default`] is ADR-0067 §7's; tests use
/// smaller ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    max_age: Duration,
    span_max_age: Duration,
    max_bytes: u64,
    batch_rows: i64,
}

impl Retention {
    /// How long records and request rows are kept.
    pub const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
    /// How long spans are kept (ADR-0069 §7), by their start.
    pub const SPAN_MAX_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);
    /// How large the file may grow, measured without its free pages, before the oldest hours go.
    pub const MAX_BYTES: u64 = 1 << 30;
    /// Rows one delete statement removes at most.
    pub const BATCH_ROWS: i64 = 5_000;
    /// The unit the size cap removes, oldest first.
    const HOUR: Duration = Duration::from_secs(60 * 60);

    /// Limits other than ADR-0067's, for tests. Spans are kept for the shorter of `max_age`
    /// and [`Retention::SPAN_MAX_AGE`].
    #[cfg(test)]
    pub(crate) fn new(max_age: Duration, max_bytes: u64, batch_rows: i64) -> Self {
        Self {
            max_age,
            span_max_age: max_age.min(Self::SPAN_MAX_AGE),
            max_bytes,
            batch_rows: batch_rows.max(1),
        }
    }
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            max_age: Self::MAX_AGE,
            span_max_age: Self::SPAN_MAX_AGE,
            max_bytes: Self::MAX_BYTES,
            batch_rows: Self::BATCH_ROWS,
        }
    }
}

/// The tables a sweep keeps within the limits, in the order it sweeps them. A span's time is
/// its start, in microseconds; the statements compare it with the same millisecond bound as
/// the other tables' times, so the size pass removes the same hours from all three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Swept {
    Records,
    Requests,
    Spans,
}

impl Swept {
    const ALL: [Self; 3] = [Self::Records, Self::Requests, Self::Spans];

    fn max_age(self, retention: &Retention) -> Duration {
        match self {
            Self::Records | Self::Requests => retention.max_age,
            Self::Spans => retention.span_max_age,
        }
    }

    /// Deletes up to `?2` rows stamped before `?1`, oldest first, never the newest row.
    fn delete_before(self) -> &'static str {
        match self {
            Self::Records => {
                "DELETE FROM log_records WHERE id IN (SELECT id FROM log_records \
                 WHERE time < ?1 AND id < (SELECT max(id) FROM log_records) ORDER BY time LIMIT ?2)"
            }
            Self::Requests => {
                "DELETE FROM requests WHERE id IN (SELECT id FROM requests \
                 WHERE time < ?1 AND id < (SELECT max(id) FROM requests) ORDER BY time LIMIT ?2)"
            }
            Self::Spans => {
                "DELETE FROM spans WHERE id IN (SELECT id FROM spans \
                 WHERE start < ?1 * 1000 AND id < (SELECT max(id) FROM spans) \
                 ORDER BY start LIMIT ?2)"
            }
        }
    }

    /// The time of the oldest row a sweep may delete: any but the newest.
    fn oldest_deletable(self) -> &'static str {
        match self {
            Self::Records => {
                "SELECT min(time) FROM log_records WHERE id < (SELECT max(id) FROM log_records)"
            }
            Self::Requests => {
                "SELECT min(time) FROM requests WHERE id < (SELECT max(id) FROM requests)"
            }
            // Milliseconds, towards the past, like the other tables' times.
            Self::Spans => {
                "SELECT (min(start) - ((min(start) % 1000) + 1000) % 1000) / 1000 FROM spans \
                 WHERE id < (SELECT max(id) FROM spans)"
            }
        }
    }
}

/// Rows one part of a pass deleted, by table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Removed {
    /// From `log_records`.
    pub records: u64,
    /// From `requests`.
    pub requests: u64,
    /// From `spans`.
    pub spans: u64,
}

impl Removed {
    fn add(&mut self, table: Swept, rows: u64) {
        match table {
            Swept::Records => self.records += rows,
            Swept::Requests => self.requests += rows,
            Swept::Spans => self.spans += rows,
        }
    }
}

/// What a finished pass removed and how large it left the file: the figures of the System
/// page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepReport {
    /// When the pass started; rows older than this less the age limit were expired.
    pub started: Timestamp,
    /// Rows deleted for being older than the age limit.
    pub expired: Removed,
    /// Rows deleted, whole hours at a time, because the file was over the size cap.
    pub capped: Removed,
    /// The file's size after the vacuum (`page_count * page_size`).
    pub file_bytes: u64,
}

/// The size of the file in its pages.
#[derive(Debug, Clone, Copy)]
pub(super) struct FileSize {
    pub(super) total: u64,
    pub(super) free: u64,
}

impl FileSize {
    /// What the rows take: freed pages not yet returned do not count.
    pub(super) fn used(self) -> u64 {
        self.total.saturating_sub(self.free)
    }
}

/// Where a pass is.
#[derive(Debug, Clone, Copy)]
enum Phase {
    /// Deleting the rows of `Swept::ALL[table]` older than its age.
    Age { table: usize },
    /// Deciding whether the file is over the cap, and which hour goes next.
    Measure,
    /// Deleting the rows of `Swept::ALL[table]` stamped before `before`.
    Cap { before: Timestamp, table: usize },
    /// Returning free pages.
    Vacuum { free: u64 },
}

/// One retention pass in progress; see the module documentation.
#[derive(Debug)]
pub struct Sweep {
    store: LogStore,
    retention: Retention,
    phase: Phase,
    report: SweepReport,
}

impl LogStore {
    /// A pass over the store to `retention` as of `now`. Nothing happens until it is stepped.
    pub fn sweep(&self, now: Timestamp, retention: Retention) -> Sweep {
        Sweep {
            store: self.clone(),
            retention,
            phase: Phase::Age { table: 0 },
            report: SweepReport {
                started: now,
                expired: Removed::default(),
                capped: Removed::default(),
                file_bytes: 0,
            },
        }
    }

    /// Deletes up to `limit` rows of `table` stamped before `before`, never its newest.
    async fn delete_before(&self, table: Swept, before: Timestamp, limit: i64) -> Result<u64> {
        let done = sqlx::query(table.delete_before())
            .bind(before)
            .bind(limit)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected())
    }

    /// The oldest time any table holds a deletable row at.
    async fn oldest_deletable(&self) -> Result<Option<Timestamp>> {
        let mut oldest: Option<Timestamp> = None;
        for table in Swept::ALL {
            let time: Option<Timestamp> = sqlx::query_scalar(table.oldest_deletable())
                .fetch_one(&self.pool)
                .await?;
            oldest = match (oldest, time) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
        Ok(oldest)
    }

    pub(super) async fn file_size(&self) -> Result<FileSize> {
        let (total, free): (i64, i64) = sqlx::query_as(
            "SELECT page_count * page_size, freelist_count * page_size \
             FROM pragma_page_count(), pragma_freelist_count(), pragma_page_size()",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(FileSize {
            total: u64::try_from(total).unwrap_or(0),
            free: u64::try_from(free).unwrap_or(0),
        })
    }
}

impl Sweep {
    /// Does the next bounded piece of the pass. `Some` once the pass has finished, with what it
    /// did; the sweep is spent then. An error leaves what earlier steps deleted deleted.
    pub async fn step(&mut self) -> Result<Option<SweepReport>> {
        let store = &self.store;
        let batch = self.retention.batch_rows;
        self.phase = match self.phase {
            Phase::Age { table } => match Swept::ALL.get(table) {
                None => Phase::Measure,
                Some(&swept) => {
                    let before = self.report.started - swept.max_age(&self.retention);
                    let rows = store.delete_before(swept, before, batch).await?;
                    self.report.expired.add(swept, rows);
                    if Self::exhausted(rows, batch) {
                        Phase::Age { table: table + 1 }
                    } else {
                        Phase::Age { table }
                    }
                }
            },
            Phase::Measure => {
                let size = store.file_size().await?;
                // The hour in progress always stays, so a log storm larger than the cap
                // still ends the pass; every other hour goes, the oldest first.
                let current = self.report.started.floor(Retention::HOUR);
                match store.oldest_deletable().await? {
                    Some(oldest) if size.used() > self.retention.max_bytes && oldest < current => {
                        let before = (oldest.floor(Retention::HOUR) + Retention::HOUR).min(current);
                        Phase::Cap { before, table: 0 }
                    }
                    _ => Phase::Vacuum { free: size.free },
                }
            }
            Phase::Cap { before, table } => match Swept::ALL.get(table) {
                None => Phase::Measure,
                Some(&swept) => {
                    let rows = store.delete_before(swept, before, batch).await?;
                    self.report.capped.add(swept, rows);
                    if Self::exhausted(rows, batch) {
                        Phase::Cap {
                            before,
                            table: table + 1,
                        }
                    } else {
                        Phase::Cap { before, table }
                    }
                }
            },
            Phase::Vacuum { free } => {
                if free > 0 {
                    // At most 2048 pages per step; a pragma takes no bound parameters.
                    sqlx::query("PRAGMA incremental_vacuum(2048)")
                        .execute(&store.pool)
                        .await?;
                }
                let size = store.file_size().await?;
                // Stop when nothing is left to return, or a step returned nothing.
                if size.free > 0 && size.free < free {
                    Phase::Vacuum { free: size.free }
                } else {
                    // Lets the file itself shrink now rather than at the next automatic
                    // checkpoint; a reader in the way only delays that.
                    sqlx::query("PRAGMA wal_checkpoint(PASSIVE)")
                        .execute(&store.pool)
                        .await?;
                    self.report.file_bytes = size.total;
                    return Ok(Some(self.report));
                }
            }
        };
        Ok(None)
    }

    /// Steps until the pass has finished.
    pub async fn finish(mut self) -> Result<SweepReport> {
        loop {
            if let Some(report) = self.step().await? {
                return Ok(report);
            }
        }
    }

    /// A delete that removed fewer rows than it could has left none to remove.
    fn exhausted(rows: u64, batch: i64) -> bool {
        i64::try_from(rows).unwrap_or(i64::MAX) < batch
    }
}
