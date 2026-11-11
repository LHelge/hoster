//! Third-party image updates (ADR-0061 §6–§8): the update checker's settings row, the last
//! check of each checkable service and the updates it found.
//!
//! Finding updates is the checker's job (`registry::UpdateChecker`); the store keeps one
//! [`image_checks`](ImageCheck) row per service and, under it, the highest candidate of each
//! [`UpdateKind`]. A check of another image, tag or digest than the stored one (the active
//! revision moved on) replaces the candidates found for the old one.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use sqlx::SqliteConnection;

use super::{Result, Store, Timestamp};
use crate::stack::{Bump, Digest, ImageRef};

/// How often the update checker runs (ADR-0061 §7): between [`MIN`](Self::MIN) and
/// [`MAX`](Self::MAX), 6 hours by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CheckInterval(Duration);

impl CheckInterval {
    pub const MIN: Duration = Duration::from_secs(60 * 60);
    pub const MAX: Duration = Duration::from_secs(7 * 24 * 60 * 60);
    pub const DEFAULT: Self = Self(Duration::from_secs(6 * 60 * 60));
}

impl Default for CheckInterval {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl TryFrom<Duration> for CheckInterval {
    type Error = String;

    fn try_from(interval: Duration) -> Result<Self, Self::Error> {
        if (Self::MIN..=Self::MAX).contains(&interval) {
            Ok(Self(interval))
        } else {
            Err(format!(
                "the check interval must be between {} and {}, not {}",
                humantime::format_duration(Self::MIN),
                humantime::format_duration(Self::MAX),
                humantime::format_duration(interval)
            ))
        }
    }
}

impl From<CheckInterval> for Duration {
    fn from(interval: CheckInterval) -> Self {
        interval.0
    }
}

impl fmt::Display for CheckInterval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        humantime::format_duration(self.0).fmt(f)
    }
}

/// The update checker's settings (ADR-0061 §7): one row, edited in Settings → Updates. No row
/// means the defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UpdateSettings {
    pub interval: CheckInterval,
}

/// What kind of update a candidate is (ADR-0061 §2, §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UpdateKind {
    /// The tag now points at another digest.
    Digest,
    /// A newer version tag in the same line, by the component that grew.
    Patch,
    Minor,
    Major,
}

impl UpdateKind {
    pub const ALL: [Self; 4] = [Self::Digest, Self::Patch, Self::Minor, Self::Major];

    /// The text stored in the `kind` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Digest => "digest",
            Self::Patch => "patch",
            Self::Minor => "minor",
            Self::Major => "major",
        }
    }
}

impl From<Bump> for UpdateKind {
    fn from(bump: Bump) -> Self {
        match bump {
            Bump::Patch => Self::Patch,
            Bump::Minor => Self::Minor,
            Bump::Major => Self::Major,
        }
    }
}

impl fmt::Display for UpdateKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for UpdateKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == s)
            .ok_or_else(|| format!("unknown update kind {s:?}"))
    }
}

/// An update the checker found: the tag to move to, and for a [`UpdateKind::Digest`] update
/// the digest the tag points at now (a version tag is resolved when it is applied).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub kind: UpdateKind,
    pub tag: String,
    pub digest: Option<Digest>,
}

/// A stored [`Candidate`] with when it was first found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCandidate {
    pub candidate: Candidate,
    pub first_seen: Timestamp,
}

/// What a check was made for: a service of an active revision, its image with the tag the
/// stack file names, and the digest the revision pins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageCheck {
    pub stack: String,
    pub service: String,
    pub image: ImageRef,
    pub digest: Digest,
}

impl ImageCheck {
    /// The `image` column: `registry/repository`.
    fn repository(&self) -> String {
        format!("{}/{}", self.image.registry(), self.image.repository())
    }

    fn tag(&self) -> &str {
        self.image.tag().unwrap_or_default()
    }
}

/// The stored result of a service's last check, for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceUpdates {
    pub stack: String,
    pub service: String,
    /// `registry/repository`, the tag and the digest the check was made for.
    pub image: String,
    pub tag: String,
    pub digest: String,
    pub checked_at: Timestamp,
    /// Why the last check failed; the candidates are then those of the check before.
    pub error: Option<String>,
    /// The highest candidate of each kind, in [`UpdateKind`] order.
    pub candidates: Vec<StoredCandidate>,
}

impl Store {
    /// The update checker's settings: the defaults when none are stored, or when the stored
    /// row no longer validates (reported, so the checker keeps running).
    pub async fn update_settings(&self) -> Result<UpdateSettings> {
        let Some(secs) =
            sqlx::query_scalar!("SELECT interval_secs FROM update_settings WHERE id = 1")
                .fetch_optional(&self.pool)
                .await?
        else {
            return Ok(UpdateSettings::default());
        };
        let interval = u64::try_from(secs)
            .map_err(|_| format!("negative interval {secs}"))
            .map(Duration::from_secs)
            .and_then(CheckInterval::try_from);
        Ok(match interval {
            Ok(interval) => UpdateSettings { interval },
            Err(error) => {
                tracing::warn!(error, "ignoring the stored update settings");
                UpdateSettings::default()
            }
        })
    }

    /// Stores the update checker's settings.
    pub async fn set_update_settings(&self, settings: &UpdateSettings) -> Result<()> {
        let secs = i64::try_from(Duration::from(settings.interval).as_secs()).unwrap_or(i64::MAX);
        let now = Timestamp::now();
        sqlx::query!(
            "INSERT INTO update_settings (id, interval_secs, updated_at) VALUES (1, ?, ?)
             ON CONFLICT (id) DO UPDATE SET interval_secs = excluded.interval_secs,
                                            updated_at = excluded.updated_at",
            secs,
            now,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Records a successful check of `check` that found `candidates` (at most one per kind),
    /// and returns the ones that are new: of a kind not found before, or with another tag or
    /// digest. A candidate found again keeps its first-seen time; kinds not found any more go.
    pub async fn record_image_check(
        &self,
        check: &ImageCheck,
        candidates: &[Candidate],
    ) -> Result<Vec<Candidate>> {
        let now = Timestamp::now();
        let mut tx = self.begin().await?;
        Self::upsert_check(&mut tx, check, None, now).await?;
        let stored = Self::candidates_in(&mut tx, &check.stack, &check.service).await?;
        let kinds: Vec<&str> = candidates.iter().map(|c| c.kind.as_str()).collect();
        for gone in stored
            .iter()
            .filter(|s| !kinds.contains(&s.candidate.kind.as_str()))
        {
            let kind = gone.candidate.kind.as_str();
            sqlx::query!(
                "DELETE FROM image_updates WHERE stack = ? AND service = ? AND kind = ?",
                check.stack,
                check.service,
                kind,
            )
            .execute(&mut *tx)
            .await?;
        }
        let mut new = Vec::new();
        for candidate in candidates {
            if stored.iter().any(|s| s.candidate == *candidate) {
                continue;
            }
            let kind = candidate.kind.as_str();
            let digest = candidate.digest.as_ref().map(AsRef::<str>::as_ref);
            sqlx::query!(
                "INSERT INTO image_updates (stack, service, kind, tag, digest, first_seen_at)
                 VALUES (?, ?, ?, ?, ?, ?)
                 ON CONFLICT (stack, service, kind) DO UPDATE SET
                     tag = excluded.tag, digest = excluded.digest,
                     first_seen_at = excluded.first_seen_at",
                check.stack,
                check.service,
                kind,
                candidate.tag,
                digest,
                now,
            )
            .execute(&mut *tx)
            .await?;
            new.push(candidate.clone());
        }
        tx.commit().await?;
        Ok(new)
    }

    /// Records a failed check of `check`. The candidates found before stay, unless they were
    /// found for another image, tag or digest.
    pub async fn record_image_check_error(&self, check: &ImageCheck, error: &str) -> Result<()> {
        let mut tx = self.begin().await?;
        Self::upsert_check(&mut tx, check, Some(error), Timestamp::now()).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Forgets the checks, and with them the updates, of every service not in `keep`
    /// (`(stack, service)`): services that are gone or no longer checkable.
    pub async fn retain_image_checks(&self, keep: &BTreeSet<(String, String)>) -> Result<()> {
        let mut tx = self.begin().await?;
        let stored = sqlx::query!("SELECT stack, service FROM image_checks")
            .fetch_all(&mut *tx)
            .await?;
        for row in stored {
            if !keep.contains(&(row.stack.clone(), row.service.clone())) {
                sqlx::query!(
                    "DELETE FROM image_checks WHERE stack = ? AND service = ?",
                    row.stack,
                    row.service,
                )
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// The last check of every checked service, with the updates found, by stack and service.
    pub async fn image_updates(&self) -> Result<Vec<ServiceUpdates>> {
        let mut conn = self.pool.acquire().await?;
        let checks = sqlx::query!(
            r#"SELECT stack, service, image, tag, digest, checked_at AS "checked_at: Timestamp",
                      error
               FROM image_checks ORDER BY stack, service"#
        )
        .fetch_all(&mut *conn)
        .await?;
        let mut updates = Vec::with_capacity(checks.len());
        for row in checks {
            let candidates = Self::candidates_in(&mut conn, &row.stack, &row.service).await?;
            updates.push(ServiceUpdates {
                stack: row.stack,
                service: row.service,
                image: row.image,
                tag: row.tag,
                digest: row.digest,
                checked_at: row.checked_at,
                error: row.error,
                candidates,
            });
        }
        Ok(updates)
    }

    /// Stores `check`'s row; when it was stored for another image, tag or digest, the
    /// candidates found for that one go first.
    async fn upsert_check(
        conn: &mut SqliteConnection,
        check: &ImageCheck,
        error: Option<&str>,
        now: Timestamp,
    ) -> Result<()> {
        let image = check.repository();
        let tag = check.tag();
        let digest = check.digest.as_ref();
        sqlx::query!(
            "DELETE FROM image_updates
             WHERE (stack, service) IN (
                 SELECT stack, service FROM image_checks
                 WHERE stack = ? AND service = ? AND (image, tag, digest) IS NOT (?, ?, ?))",
            check.stack,
            check.service,
            image,
            tag,
            digest,
        )
        .execute(&mut *conn)
        .await?;
        sqlx::query!(
            "INSERT INTO image_checks (stack, service, image, tag, digest, checked_at, error)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (stack, service) DO UPDATE SET
                 image = excluded.image, tag = excluded.tag, digest = excluded.digest,
                 checked_at = excluded.checked_at, error = excluded.error",
            check.stack,
            check.service,
            image,
            tag,
            digest,
            now,
            error,
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// The stored candidates of one service, in [`UpdateKind`] order. A row that no longer
    /// decodes is reported and skipped.
    async fn candidates_in(
        conn: &mut SqliteConnection,
        stack: &str,
        service: &str,
    ) -> Result<Vec<StoredCandidate>> {
        let rows = sqlx::query!(
            r#"SELECT kind, tag, digest, first_seen_at AS "first_seen_at: Timestamp"
               FROM image_updates WHERE stack = ? AND service = ?"#,
            stack,
            service,
        )
        .fetch_all(&mut *conn)
        .await?;
        let mut candidates: Vec<StoredCandidate> = rows
            .into_iter()
            .filter_map(|row| {
                let decoded = (|| {
                    Ok::<_, String>(Candidate {
                        kind: row.kind.parse()?,
                        tag: row.tag,
                        digest: row.digest.map(|d| d.parse()).transpose()?,
                    })
                })();
                decoded
                    .inspect_err(|error| {
                        tracing::warn!(stack, service, error, "ignoring a stored image update");
                    })
                    .ok()
                    .map(|candidate| StoredCandidate {
                        candidate,
                        first_seen: row.first_seen_at,
                    })
            })
            .collect();
        candidates.sort_by_key(|c| c.candidate.kind);
        Ok(candidates)
    }
}
