//! Run-time settings of this instance (ADR-0058 §1): one typed row in `instance_settings`. The
//! only setting so far is the instance name (ADR-0058 §2); `crate::instance::Instance` caches it.

use super::{Result, Store, Timestamp};
use crate::instance::InstanceName;

impl Store {
    /// The instance name the operator set, if any. A stored name that no longer validates is
    /// reported and ignored, so the default is used.
    pub async fn instance_name(&self) -> Result<Option<InstanceName>> {
        let stored = sqlx::query_scalar!("SELECT name FROM instance_settings WHERE id = 1")
            .fetch_optional(&self.pool)
            .await?
            .flatten();
        Ok(stored.and_then(|raw| {
            raw.parse()
                .inspect_err(|err| tracing::warn!(name = raw, error = %err, "ignoring the stored instance name"))
                .ok()
        }))
    }

    /// Stores `name` as the instance name, or clears it back to the default with `None`.
    pub async fn set_instance_name(&self, name: Option<&InstanceName>) -> Result<()> {
        let name = name.map(AsRef::<str>::as_ref);
        let now = Timestamp::now();
        sqlx::query!(
            "INSERT INTO instance_settings (id, name, updated_at) VALUES (1, ?, ?)
             ON CONFLICT (id) DO UPDATE SET name = excluded.name, updated_at = excluded.updated_at",
            name,
            now,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
