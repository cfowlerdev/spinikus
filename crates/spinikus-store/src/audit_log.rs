use async_trait::async_trait;
use spinikus_core::{
    audit::{AuditDetails, AuditEntry, NewAuditEntry},
    id::{AuditEntryId, ProjectId}
};

use crate::{SqliteStore, StoreError, error::parse_column};

const ENTITY: &str = "audit entry";

/// The audit log is append-only: entries can be recorded and read, never changed or removed.
#[async_trait]
pub trait AuditLogRepository: Send + Sync {
    async fn record_audit_entry(&self, new: NewAuditEntry) -> Result<AuditEntry, StoreError>;

    /// Entries newest first, optionally only those in `project_id` or about `target_id`,
    /// starting before the `before` cursor.
    async fn list_audit_entries(
        &self,
        project_id: Option<ProjectId>,
        target_id: Option<&str>,
        before: Option<AuditEntryId>,
        limit: u32
    ) -> Result<Vec<AuditEntry>, StoreError>;
}

macro_rules! columns {
    () => {
        "id, actor, token_id, action, target_id, project_id, details, created_at"
    };
}

#[derive(sqlx::FromRow)]
struct AuditRow {
    id: String,
    actor: String,
    token_id: Option<String>,
    action: String,
    target_id: Option<String>,
    project_id: Option<String>,
    details: String,
    created_at: String
}

impl TryFrom<AuditRow> for AuditEntry {
    type Error = StoreError;

    fn try_from(row: AuditRow) -> Result<Self, Self::Error> {
        Ok(AuditEntry {
            id: parse_column(ENTITY, "id", &row.id)?,
            actor: parse_column(ENTITY, "actor", &row.actor)?,
            token_id: row.token_id.map(|id| parse_column(ENTITY, "token_id", &id)).transpose()?,
            action: parse_column(ENTITY, "action", &row.action)?,
            target_id: row.target_id,
            project_id: row.project_id.map(|id| parse_column(ENTITY, "project_id", &id)).transpose()?,
            details: serde_json::from_str::<AuditDetails>(&row.details).map_err(|error| StoreError::InvalidRow {
                entity: ENTITY,
                column: "details",
                problem: error.to_string()
            })?,
            created_at: parse_column(ENTITY, "created_at", &row.created_at)?
        })
    }
}

#[async_trait]
impl AuditLogRepository for SqliteStore {
    async fn record_audit_entry(&self, new: NewAuditEntry) -> Result<AuditEntry, StoreError> {
        let row: AuditRow = sqlx::query_as(concat!(
            "INSERT INTO audit_log (id, actor, token_id, action, target_id, project_id, details)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             RETURNING ",
            columns!()
        ))
        .bind(AuditEntryId::new().to_string())
        .bind(new.actor.to_string())
        .bind(new.token_id.map(|id| id.to_string()))
        .bind(new.action.as_str())
        .bind(&new.target_id)
        .bind(new.project_id.map(|id| id.to_string()))
        .bind(serde_json::Value::Object(new.details).to_string())
        .fetch_one(self.pool())
        .await?;

        row.try_into()
    }

    async fn list_audit_entries(
        &self,
        project_id: Option<ProjectId>,
        target_id: Option<&str>,
        before: Option<AuditEntryId>,
        limit: u32
    ) -> Result<Vec<AuditEntry>, StoreError> {
        // `~` sorts after every character used in IDs, so no cursor means "from the newest".
        let rows: Vec<AuditRow> = sqlx::query_as(concat!(
            "SELECT ", columns!(), " FROM audit_log
             WHERE (?1 IS NULL OR project_id = ?1) AND (?2 IS NULL OR target_id = ?2) AND id < ?3
             ORDER BY id DESC LIMIT ?4"
        ))
        .bind(project_id.map(|id| id.to_string()))
        .bind(target_id)
        .bind(before.map(|id| id.to_string()).unwrap_or_else(|| "~".to_string()))
        .bind(limit)
        .fetch_all(self.pool())
        .await?;

        rows.into_iter().map(AuditEntry::try_from).collect()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use spinikus_core::{actor::Actor, id::UserId, project::ProjectFields};

    use super::*;
    use crate::{ProjectRepository, test_support::store};

    fn entry(action: &str, target_id: Option<&str>, project_id: Option<ProjectId>) -> NewAuditEntry {
        NewAuditEntry {
            actor: Actor::User(UserId::new()),
            token_id: None,
            action: action.parse().unwrap(),
            target_id: target_id.map(str::to_string),
            project_id,
            details: json!({ "name": "API_KEY" }).as_object().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn records_and_filters_entries_newest_first() {
        let (_dir, store) = store().await;
        let project = store
            .create_project(ProjectFields { name: "Web".to_string(), slug: "web".parse().unwrap() })
            .await
            .unwrap()
            .id;

        let reveal = store.record_audit_entry(entry("variable.reveal", Some("var_1"), Some(project))).await.unwrap();
        let login = store.record_audit_entry(entry("session.create", None, None)).await.unwrap();
        let update = store.record_audit_entry(entry("variable.update", Some("var_1"), Some(project))).await.unwrap();

        let ids = |entries: Vec<AuditEntry>| entries.into_iter().map(|entry| entry.id).collect::<Vec<_>>();
        assert_eq!(ids(store.list_audit_entries(None, None, None, 10).await.unwrap()), vec![update.id, login.id, reveal.id]);
        assert_eq!(ids(store.list_audit_entries(Some(project), None, None, 10).await.unwrap()), vec![update.id, reveal.id]);
        assert_eq!(ids(store.list_audit_entries(None, Some("var_1"), Some(update.id), 10).await.unwrap()), vec![reveal.id]);
        assert_eq!(reveal.details["name"], "API_KEY");
        assert_eq!(reveal.action.as_str(), "variable.reveal");
    }

    #[tokio::test]
    async fn entries_cannot_be_changed_or_deleted() {
        let (_dir, store) = store().await;
        store.record_audit_entry(entry("session.create", None, None)).await.unwrap();

        let updated = sqlx::query("UPDATE audit_log SET action = 'session.delete'").execute(store.pool()).await;
        let deleted = sqlx::query("DELETE FROM audit_log").execute(store.pool()).await;

        for result in [updated, deleted] {
            let error = result.unwrap_err();
            assert!(error.to_string().contains("append-only"), "{error}");
        }
        assert_eq!(store.list_audit_entries(None, None, None, 10).await.unwrap().len(), 1);
    }
}
