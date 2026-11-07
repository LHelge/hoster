//! Admin sessions and pending OAuth logins (ADR-0031). The policy (lifetimes, tokens) belongs to
//! the web app; the store only keeps the rows and applies the cut-offs it is given.

use super::{Result, SessionRecord, Store, Timestamp};

impl Store {
    // ---------------------------------------------------------------------------------------
    // Sessions
    // ---------------------------------------------------------------------------------------

    /// Records a new session.
    pub async fn create_session(&self, session: &SessionRecord) -> Result<()> {
        sqlx::query!(
            "INSERT INTO sessions
                 (token_hash, csrf_token, github_login, github_id, created_at, last_seen_at,
                  expires_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            session.token_hash,
            session.csrf_token,
            session.github_login,
            session.github_id,
            session.created_at,
            session.last_seen_at,
            session.expires_at,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The session with `token_hash`, if it has not expired by `now` and was last seen after
    /// `idle_since`.
    pub async fn active_session(
        &self,
        token_hash: &str,
        now: Timestamp,
        idle_since: Timestamp,
    ) -> Result<Option<SessionRecord>> {
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT token_hash, csrf_token, github_login, github_id,
                      created_at AS "created_at: Timestamp",
                      last_seen_at AS "last_seen_at: Timestamp",
                      expires_at AS "expires_at: Timestamp"
               FROM sessions
               WHERE token_hash = ? AND expires_at > ? AND last_seen_at > ?"#,
            token_hash,
            now,
            idle_since,
        )
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Records activity on a session.
    pub async fn touch_session(&self, token_hash: &str, now: Timestamp) -> Result<()> {
        sqlx::query!(
            "UPDATE sessions SET last_seen_at = ? WHERE token_hash = ?",
            now,
            token_hash,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Ends one session. Returns whether it existed.
    pub async fn remove_session(&self, token_hash: &str) -> Result<bool> {
        let done = sqlx::query!("DELETE FROM sessions WHERE token_hash = ?", token_hash)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Ends every session ("log out everywhere"). Returns how many there were.
    pub async fn remove_all_sessions(&self) -> Result<u64> {
        let done = sqlx::query!("DELETE FROM sessions")
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected())
    }

    /// Deletes the sessions that expired by `now` or were idle since `idle_since`.
    pub async fn prune_sessions(&self, now: Timestamp, idle_since: Timestamp) -> Result<u64> {
        let done = sqlx::query!(
            "DELETE FROM sessions WHERE expires_at <= ? OR last_seen_at <= ?",
            now,
            idle_since,
        )
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected())
    }

    // ---------------------------------------------------------------------------------------
    // Pending OAuth logins
    // ---------------------------------------------------------------------------------------

    /// Records an authorization request sent to GitHub, dropping the ones started before
    /// `stale_before` and all but the newest `keep`. Starting a login needs no session, so the
    /// cap keeps a flood of `/auth/github/login` requests from growing the table without bound.
    pub async fn add_oauth_login(
        &self,
        state: &str,
        pkce_verifier: &str,
        now: Timestamp,
        stale_before: Timestamp,
        keep: u32,
    ) -> Result<()> {
        let mut tx = self.begin().await?;
        sqlx::query!(
            "DELETE FROM oauth_logins WHERE created_at < ?",
            stale_before
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "INSERT INTO oauth_logins (state, pkce_verifier, created_at) VALUES (?, ?, ?)",
            state,
            pkce_verifier,
            now,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "DELETE FROM oauth_logins WHERE state NOT IN
                 (SELECT state FROM oauth_logins ORDER BY created_at DESC, rowid DESC LIMIT ?)",
            keep,
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Takes the pending login for `state`: returns its PKCE verifier and deletes it, so a
    /// `state` works once. A login started before `stale_before` is deleted but not returned.
    pub async fn take_oauth_login(
        &self,
        state: &str,
        stale_before: Timestamp,
    ) -> Result<Option<String>> {
        let row = sqlx::query!(
            r#"DELETE FROM oauth_logins WHERE state = ?
               RETURNING pkce_verifier AS "pkce_verifier!", created_at AS "created_at!: Timestamp""#,
            state,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row
            .filter(|row| row.created_at >= stale_before)
            .map(|row| row.pkce_verifier))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;

    async fn open() -> (TempDir, Store) {
        let dir = TempDir::new().unwrap();
        let store = Store::open(&dir.path().join("state")).await.unwrap();
        (dir, store)
    }

    fn session(hash: &str, created_at: Timestamp) -> SessionRecord {
        SessionRecord {
            token_hash: hash.into(),
            csrf_token: format!("csrf-{hash}"),
            github_login: "octocat".into(),
            github_id: 583231,
            created_at,
            last_seen_at: created_at,
            expires_at: created_at + Duration::from_secs(12 * 3600),
        }
    }

    const HOUR: Duration = Duration::from_secs(3600);

    #[tokio::test]
    async fn sessions_respect_absolute_and_idle_limits() {
        let (_dir, store) = open().await;
        let now = Timestamp::now();
        store.create_session(&session("fresh", now)).await.unwrap();
        store
            .create_session(&session("old", now - 13 * HOUR))
            .await
            .unwrap();
        let mut idle = session("idle", now - 3 * HOUR);
        idle.last_seen_at = now - 2 * HOUR;
        store.create_session(&idle).await.unwrap();

        let idle_since = now - HOUR;
        let fresh = store.active_session("fresh", now, idle_since).await;
        assert_eq!(fresh.unwrap(), Some(session("fresh", now)));
        // Past the absolute limit, even though it was "seen" recently.
        store.touch_session("old", now).await.unwrap();
        assert_eq!(
            store.active_session("old", now, idle_since).await.unwrap(),
            None
        );
        assert_eq!(
            store.active_session("idle", now, idle_since).await.unwrap(),
            None
        );
        assert_eq!(
            store
                .active_session("missing", now, idle_since)
                .await
                .unwrap(),
            None
        );

        assert_eq!(store.prune_sessions(now, idle_since).await.unwrap(), 2);
        assert!(store.remove_session("fresh").await.unwrap());
        assert!(!store.remove_session("fresh").await.unwrap());
    }

    #[tokio::test]
    async fn touching_keeps_a_session_alive() {
        let (_dir, store) = open().await;
        let now = Timestamp::now();
        store
            .create_session(&session("s", now - 2 * HOUR))
            .await
            .unwrap();
        assert_eq!(
            store.active_session("s", now, now - HOUR).await.unwrap(),
            None
        );
        store.touch_session("s", now).await.unwrap();
        let found = store.active_session("s", now, now - HOUR).await.unwrap();
        assert_eq!(found.unwrap().last_seen_at, now);
    }

    #[tokio::test]
    async fn log_out_everywhere_removes_every_session() {
        let (_dir, store) = open().await;
        let now = Timestamp::now();
        for hash in ["a", "b", "c"] {
            store.create_session(&session(hash, now)).await.unwrap();
        }
        assert_eq!(store.remove_all_sessions().await.unwrap(), 3);
        assert_eq!(
            store.active_session("a", now, now - HOUR).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn oauth_logins_are_taken_once_and_expire() {
        let (_dir, store) = open().await;
        let now = Timestamp::now();
        let ten_min = Duration::from_secs(600);
        store
            .add_oauth_login("s1", "v1", now, now - ten_min, 10)
            .await
            .unwrap();
        assert_eq!(
            store.take_oauth_login("s1", now - ten_min).await.unwrap(),
            Some("v1".to_owned())
        );
        assert_eq!(
            store.take_oauth_login("s1", now - ten_min).await.unwrap(),
            None
        );

        store
            .add_oauth_login("s2", "v2", now - 2 * ten_min, now - 3 * ten_min, 10)
            .await
            .unwrap();
        assert_eq!(
            store.take_oauth_login("s2", now - ten_min).await.unwrap(),
            None,
            "a stale login is not returned"
        );

        // Adding a login prunes the stale ones.
        store
            .add_oauth_login("s3", "v3", now - 2 * ten_min, now - 3 * ten_min, 10)
            .await
            .unwrap();
        store
            .add_oauth_login("s4", "v4", now, now - ten_min, 10)
            .await
            .unwrap();
        assert_eq!(
            store
                .take_oauth_login("s3", now - 4 * ten_min)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn pending_oauth_logins_are_capped() {
        let (_dir, store) = open().await;
        let now = Timestamp::now();
        let stale = now - Duration::from_secs(600);
        for i in 0..5 {
            let state = format!("s{i}");
            store
                .add_oauth_login(&state, "v", now + Duration::from_millis(i), stale, 3)
                .await
                .unwrap();
        }
        for (state, kept) in [("s0", false), ("s1", false), ("s2", true), ("s4", true)] {
            let found = store.take_oauth_login(state, stale).await.unwrap();
            assert_eq!(found.is_some(), kept, "{state}");
        }
    }
}
