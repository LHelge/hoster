//! The images hoster pulled, and what its services use, for image garbage collection (ADR-0041).
//!
//! A pull adds no label to an image, so the `images` table is hoster's own record of what it
//! pulled; images missing from it are never pruned. Which recorded images to keep is the
//! orchestrator's decision, from [`Store::image_uses`].

use super::{
    DeploymentId, DeploymentState, ImageUse, ImageUseKind, RecordedImage, Result, Store, Timestamp,
};

impl Store {
    /// Records that hoster pulled `repository@digest` (`repository` is `registry/repository`).
    /// Recording it again only moves `pulled_at`.
    pub async fn record_image(&self, repository: &str, digest: &str) -> Result<()> {
        let now = Timestamp::now();
        sqlx::query!(
            "INSERT INTO images (repository, digest, pulled_at) VALUES (?, ?, ?)
             ON CONFLICT (repository, digest) DO UPDATE SET pulled_at = excluded.pulled_at",
            repository,
            digest,
            now,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every image hoster pulled and has not removed since, oldest pull first.
    pub async fn recorded_images(&self) -> Result<Vec<RecordedImage>> {
        Ok(sqlx::query_as!(
            RecordedImage,
            r#"SELECT repository, digest, pulled_at AS "pulled_at: Timestamp"
               FROM images ORDER BY pulled_at, repository, digest"#,
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// Forgets a recorded image once it is removed. Returns whether it was recorded.
    pub async fn forget_image(&self, repository: &str, digest: &str) -> Result<bool> {
        let done = sqlx::query!(
            "DELETE FROM images WHERE repository = ? AND digest = ?",
            repository,
            digest,
        )
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Every digest the services of stacks that are not removed use or may roll back to: the
    /// current pointers, the targets of deployments that went live (newest first) and those of
    /// deployments still in flight. A removed stack cannot be rolled back, so its history keeps
    /// nothing.
    pub async fn image_uses(&self) -> Result<Vec<ImageUse>> {
        let mut conn = self.pool.acquire().await?;
        let current = sqlx::query!(
            r#"SELECT p.stack, p.name, p.digest FROM services p
               JOIN stacks s ON s.name = p.stack
               WHERE s.removed_at IS NULL
               ORDER BY p.stack, p.name"#,
        )
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(|row| ImageUse {
            stack: row.stack,
            service: row.name,
            digest: row.digest,
            kind: ImageUseKind::Current,
        });
        let live = DeploymentState::Live;
        let warned = DeploymentState::LiveWithWarnings;
        let deployed = sqlx::query!(
            r#"SELECT d.id AS "id: DeploymentId", d.stack, t.service, t.digest,
                      d.finished_at IS NULL AS "in_flight!: bool"
               FROM deployment_targets t
               JOIN deployments d ON d.id = t.deployment_id
               JOIN stacks s ON s.name = d.stack
               WHERE s.removed_at IS NULL
                 AND (d.finished_at IS NULL OR d.state IN (?, ?))
               ORDER BY d.id DESC, t.service"#,
            live,
            warned,
        )
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(|row| ImageUse {
            stack: row.stack,
            service: row.service,
            digest: row.digest,
            kind: if row.in_flight {
                ImageUseKind::InFlight
            } else {
                ImageUseKind::Live(row.id)
            },
        });
        Ok(current.chain(deployed).collect())
    }
}
