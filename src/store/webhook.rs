//! Deploy webhook state (ADR-0047): the single-use token ids and the log of refused attempts.
//! Verifying tokens is the web app's job; the store only keeps the rows.

use super::{Result, Store, Timestamp, WebhookRejection};

impl Store {
    /// How many refused attempts are kept per stack; older ones are dropped.
    pub const KEPT_REJECTIONS: u32 = 50;

    /// Marks the token id `jti` as used until `expires_at`. Returns `false` when it was used
    /// before (a replay). Ids whose tokens have expired are forgotten on the way.
    pub async fn claim_token(&self, jti: &str, expires_at: Timestamp) -> Result<bool> {
        let mut tx = self.begin().await?;
        sqlx::query!(
            "DELETE FROM webhook_tokens WHERE expires_at < ?",
            Timestamp::now()
        )
        .execute(&mut *tx)
        .await?;
        let done = sqlx::query!(
            "INSERT INTO webhook_tokens (jti, expires_at) VALUES (?, ?) ON CONFLICT DO NOTHING",
            jti,
            expires_at,
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(done.rows_affected() == 1)
    }

    /// Records a refused webhook attempt on its stack. Attempts naming a stack hoster does not
    /// know are not kept (returns `false`): anyone can mint a GitHub token.
    pub async fn record_rejection(&self, rejection: &WebhookRejection) -> Result<bool> {
        let mut tx = self.begin().await?;
        let done = sqlx::query!(
            "INSERT INTO webhook_rejections
                 (stack, reason, repository, repository_id, git_ref, environment, workflow_ref,
                  job_workflow_ref, actor, run_id, at)
             SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
             WHERE EXISTS (SELECT 1 FROM stacks WHERE name = ? AND removed_at IS NULL)",
            rejection.stack,
            rejection.reason,
            rejection.repository,
            rejection.repository_id,
            rejection.git_ref,
            rejection.environment,
            rejection.workflow_ref,
            rejection.job_workflow_ref,
            rejection.actor,
            rejection.run_id,
            rejection.at,
            rejection.stack,
        )
        .execute(&mut *tx)
        .await?;
        let kept = Self::KEPT_REJECTIONS;
        sqlx::query!(
            "DELETE FROM webhook_rejections WHERE stack = ? AND id NOT IN
                 (SELECT id FROM webhook_rejections WHERE stack = ? ORDER BY id DESC LIMIT ?)",
            rejection.stack,
            rejection.stack,
            kept,
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(done.rows_affected() == 1)
    }

    /// The newest `limit` refused webhook attempts on `stack`, newest first.
    pub async fn rejections(&self, stack: &str, limit: u32) -> Result<Vec<WebhookRejection>> {
        Ok(sqlx::query_as!(
            WebhookRejection,
            r#"SELECT stack, reason, repository, repository_id, git_ref, environment,
                      workflow_ref, job_workflow_ref, actor, run_id, at AS "at: Timestamp"
               FROM webhook_rejections WHERE stack = ? ORDER BY id DESC LIMIT ?"#,
            stack,
            limit,
        )
        .fetch_all(&self.pool)
        .await?)
    }
}
