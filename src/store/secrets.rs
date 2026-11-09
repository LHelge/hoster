//! Sealed secrets (ADR-0029). The store only keeps the rows; sealing and opening them is
//! `secrets::SecretStore`'s job, so no plaintext ever reaches this module.

use sqlx::SqliteConnection;

use super::{Event, NewEvent, Result, SealedSecret, SecretEntry, Store, Timestamp};

impl Store {
    /// Stores a sealed secret, replacing one of the same name (its `created_at` is kept).
    pub async fn put_secret(&self, secret: &SealedSecret) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        Self::put_secret_in(&mut conn, secret).await
    }

    /// [`put_secret`](Self::put_secret), recording `event` in the event log (ADR-0055) in the
    /// same transaction. The event names the secret only; nothing sealed passes through it.
    pub async fn put_secret_recorded(
        &self,
        secret: &SealedSecret,
        event: &NewEvent,
    ) -> Result<Event> {
        let mut tx = self.begin().await?;
        Self::put_secret_in(&mut tx, secret).await?;
        let recorded = Self::insert_event(&mut tx, event, secret.updated_at).await?;
        tx.commit().await?;
        Ok(recorded)
    }

    /// The upsert on `conn`, which may be inside the caller's transaction.
    async fn put_secret_in(conn: &mut SqliteConnection, secret: &SealedSecret) -> Result<()> {
        sqlx::query!(
            "INSERT INTO secrets (name, nonce, ciphertext, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (name) DO UPDATE SET nonce = excluded.nonce,
                                              ciphertext = excluded.ciphertext,
                                              updated_at = excluded.updated_at",
            secret.name,
            secret.nonce,
            secret.ciphertext,
            secret.created_at,
            secret.updated_at,
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// The sealed secret `name`, if it exists.
    pub async fn sealed_secret(&self, name: &str) -> Result<Option<SealedSecret>> {
        Ok(sqlx::query_as!(
            SealedSecret,
            r#"SELECT name, nonce, ciphertext,
                      created_at AS "created_at: Timestamp",
                      updated_at AS "updated_at: Timestamp"
               FROM secrets WHERE name = ?"#,
            name,
        )
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Removes the secret `name`. Returns whether it existed.
    pub async fn remove_secret(&self, name: &str) -> Result<bool> {
        let done = sqlx::query!("DELETE FROM secrets WHERE name = ?", name)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// [`remove_secret`](Self::remove_secret), recording `event` in the event log (ADR-0055) in
    /// the same transaction: the event is stored if and only if the secret existed. `None` when
    /// it did not.
    pub async fn remove_secret_recorded(
        &self,
        name: &str,
        event: &NewEvent,
    ) -> Result<Option<Event>> {
        let mut tx = self.begin().await?;
        let done = sqlx::query!("DELETE FROM secrets WHERE name = ?", name)
            .execute(&mut *tx)
            .await?;
        if done.rows_affected() == 0 {
            return Ok(None);
        }
        let recorded = Self::insert_event(&mut tx, event, Timestamp::now()).await?;
        tx.commit().await?;
        Ok(Some(recorded))
    }

    /// Every secret's name and last change, by name.
    pub async fn secret_entries(&self) -> Result<Vec<SecretEntry>> {
        Ok(sqlx::query_as!(
            SecretEntry,
            r#"SELECT name, updated_at AS "updated_at: Timestamp"
               FROM secrets ORDER BY name"#,
        )
        .fetch_all(&self.pool)
        .await?)
    }
}
