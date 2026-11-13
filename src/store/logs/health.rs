//! The log store's own figures: what the System page shows of it (ADR-0067 §7) and what
//! `hoster doctor` checks of the file, read without creating, migrating or writing it.

use std::path::Path;

use super::LogStore;
use crate::store::{Migrations, Result, SchemaVersion, StoreError, Timestamp};

/// Records, request rows and spans stamped in some period.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RowCounts {
    /// In `log_records`.
    pub records: u64,
    /// In `requests`.
    pub requests: u64,
    /// In `spans`, by their start.
    pub spans: u64,
}

/// What `hoster doctor` reads of an existing `logs.db`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogFileFacts {
    /// The file was created with `auto_vacuum = INCREMENTAL`, so retention can shrink it.
    pub incremental_vacuum: bool,
    /// The log schema the file is at.
    pub schema: SchemaVersion,
}

impl LogStore {
    /// SQLite's `auto_vacuum` value for `INCREMENTAL`.
    const AUTO_VACUUM_INCREMENTAL: i64 = 2;

    /// The records, request rows and spans stamped at or after `since`, counted with the
    /// time indexes.
    pub async fn rows_since(&self, since: Timestamp) -> Result<RowCounts> {
        let (records, requests, spans): (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM log_records WHERE time >= ?1), \
                    (SELECT count(*) FROM requests WHERE time >= ?1), \
                    (SELECT count(*) FROM spans WHERE start >= ?1 * 1000)",
        )
        .bind(since)
        .fetch_one(&self.reads)
        .await?;
        Ok(RowCounts {
            records: u64::try_from(records).unwrap_or(0),
            requests: u64::try_from(requests).unwrap_or(0),
            spans: u64::try_from(spans).unwrap_or(0),
        })
    }

    /// The bytes the rows take, without free pages not yet returned: the figure the size cap
    /// ([`Retention::MAX_BYTES`](super::Retention::MAX_BYTES)) is measured by.
    pub async fn used_bytes(&self) -> Result<u64> {
        let used: i64 = sqlx::query_scalar(
            "SELECT (page_count - freelist_count) * page_size \
             FROM pragma_page_count(), pragma_freelist_count(), pragma_page_size()",
        )
        .fetch_one(&self.reads)
        .await?;
        Ok(u64::try_from(used).unwrap_or(0))
    }

    /// The vacuum mode and schema of the existing `logs.db` under `state_dir`, opened
    /// read-only; `None` when there is none. For `hoster doctor`, which runs without hoster.
    pub async fn inspect_in(state_dir: &Path) -> Result<Option<LogFileFacts>> {
        let store = match Self::open_existing(state_dir, true).await {
            Ok(store) => store,
            Err(StoreError::Missing(_)) => return Ok(None),
            Err(err) => return Err(err),
        };
        let facts = async {
            let auto_vacuum: i64 = sqlx::query_scalar("PRAGMA auto_vacuum")
                .fetch_one(&store.pool)
                .await?;
            let schema = Migrations::applied(&store.pool).await?;
            Ok(LogFileFacts {
                incremental_vacuum: auto_vacuum == Self::AUTO_VACUUM_INCREMENTAL,
                schema,
            })
        }
        .await;
        store.close().await;
        facts.map(Some)
    }
}
