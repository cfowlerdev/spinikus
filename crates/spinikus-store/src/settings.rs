use async_trait::async_trait;
use spinikus_core::setting::Setting;

use crate::{SqliteStore, StoreError, error::parse_column};

const ENTITY: &str = "setting";

#[async_trait]
pub trait SettingRepository: Send + Sync {
    /// The stored setting, or `None` if it was never set.
    async fn get_setting(&self, key: &str) -> Result<Option<Setting>, StoreError>;

    /// Sets the value if the setting is still at `revision`, 0 for one that was never set.
    async fn put_setting(&self, key: &str, value: &serde_json::Value, revision: i64) -> Result<Setting, StoreError>;
}

#[derive(sqlx::FromRow)]
struct SettingRow {
    key: String,
    value: String,
    revision: i64,
    updated_at: String,
}

impl TryFrom<SettingRow> for Setting {
    type Error = StoreError;

    fn try_from(row: SettingRow) -> Result<Self, Self::Error> {
        Ok(Setting {
            value: serde_json::from_str(&row.value).map_err(|error| StoreError::InvalidRow {
                entity: ENTITY,
                column: "value",
                problem: error.to_string(),
            })?,
            key: row.key,
            revision: row.revision,
            updated_at: Some(parse_column(ENTITY, "updated_at", &row.updated_at)?),
        })
    }
}

#[async_trait]
impl SettingRepository for SqliteStore {
    async fn get_setting(&self, key: &str) -> Result<Option<Setting>, StoreError> {
        let row: Option<SettingRow> = sqlx::query_as("SELECT key, value, revision, updated_at FROM settings WHERE key = ?")
            .bind(key)
            .fetch_optional(self.pool())
            .await?;
        row.map(Setting::try_from).transpose()
    }

    async fn put_setting(&self, key: &str, value: &serde_json::Value, revision: i64) -> Result<Setting, StoreError> {
        let mut tx = self.begin_write().await?;
        let row: Option<SettingRow> = if revision == 0 {
            sqlx::query_as(
                "INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT (key) DO NOTHING
                 RETURNING key, value, revision, updated_at",
            )
            .bind(key)
            .bind(value.to_string())
            .fetch_optional(&mut *tx)
            .await?
        } else {
            sqlx::query_as(
                "UPDATE settings
                 SET value = ?, revision = revision + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 WHERE key = ? AND revision = ?
                 RETURNING key, value, revision, updated_at",
            )
            .bind(value.to_string())
            .bind(key)
            .bind(revision)
            .fetch_optional(&mut *tx)
            .await?
        };
        match row {
            Some(row) => {
                tx.commit().await?;
                row.try_into()
            }
            None => {
                let actual: Option<i64> =
                    sqlx::query_scalar("SELECT revision FROM settings WHERE key = ?").bind(key).fetch_optional(&mut *tx).await?;
                Err(StoreError::RevisionMismatch {
                    entity: ENTITY,
                    id: key.to_string(),
                    expected: revision,
                    actual: actual.unwrap_or(0),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::test_support::store;

    #[tokio::test]
    async fn sets_a_setting_at_its_revision() {
        let (_dir, store) = store().await;

        assert_eq!(store.get_setting("base_domain").await.unwrap(), None);
        let first = store.put_setting("base_domain", &json!("apps.example.com"), 0).await.unwrap();
        let again = store.put_setting("base_domain", &json!("other.example.com"), 0).await;
        let second = store.put_setting("base_domain", &json!(null), 1).await.unwrap();
        let stale = store.put_setting("base_domain", &json!("x.example.com"), 1).await;
        let unset = store.put_setting("acme_email", &json!("a@example.com"), 3).await;

        assert_eq!((first.value, first.revision), (json!("apps.example.com"), 1));
        assert!(matches!(again, Err(StoreError::RevisionMismatch { expected: 0, actual: 1, .. })), "{again:?}");
        assert_eq!((second.value.clone(), second.revision), (json!(null), 2));
        assert_eq!(store.get_setting("base_domain").await.unwrap(), Some(second));
        assert!(matches!(stale, Err(StoreError::RevisionMismatch { expected: 1, actual: 2, .. })), "{stale:?}");
        assert!(matches!(unset, Err(StoreError::RevisionMismatch { expected: 3, actual: 0, .. })), "{unset:?}");
    }
}
