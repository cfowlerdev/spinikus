use async_trait::async_trait;
use spinikus_core::{
    id::{ServiceId, WebhookId},
    secret::Secret,
    webhook::{Webhook, WebhookPaths},
};

use crate::{SqliteStore, StoreError, certificates::open_sealed, db::format_timestamp, error::parse_column};

const ENTITY: &str = "webhook";

/// Services' Git webhooks, at most one per service. Webhooks of deleted services are gone.
#[async_trait]
pub trait WebhookRepository: Send + Sync {
    async fn get_webhook(&self, id: WebhookId) -> Result<Webhook, StoreError>;

    async fn find_service_webhook(&self, service_id: ServiceId) -> Result<Option<Webhook>, StoreError>;

    /// Gives the service a webhook with this secret. If it has one, only the secret changes,
    /// so the URL stays the same. `NotFound` if the service does not exist or is deleted.
    async fn set_webhook(&self, service_id: ServiceId, secret: &Secret) -> Result<Webhook, StoreError>;

    async fn webhook_secret(&self, id: WebhookId) -> Result<Secret, StoreError>;

    /// Removes the service's webhook; `NotFound` if it has none.
    async fn delete_webhook(&self, service_id: ServiceId) -> Result<(), StoreError>;

    /// Records what the latest delivery did.
    async fn record_webhook_delivery(&self, id: WebhookId, result: &str) -> Result<(), StoreError>;
}

#[derive(sqlx::FromRow)]
struct WebhookRow {
    id: String,
    service_id: String,
    created_at: String,
    last_delivery_at: Option<String>,
    last_delivery: Option<String>,
}

impl TryFrom<WebhookRow> for Webhook {
    type Error = StoreError;

    fn try_from(row: WebhookRow) -> Result<Self, Self::Error> {
        let id = parse_column(ENTITY, "id", &row.id)?;
        Ok(Webhook {
            id,
            service_id: parse_column(ENTITY, "service_id", &row.service_id)?,
            paths: WebhookPaths::new(id),
            created_at: parse_column(ENTITY, "created_at", &row.created_at)?,
            last_delivery_at: row.last_delivery_at.map(|at| parse_column(ENTITY, "last_delivery_at", &at)).transpose()?,
            last_delivery: row.last_delivery,
        })
    }
}

macro_rules! select {
    ($where:literal) => {
        concat!(
            "SELECT webhooks.id, webhooks.service_id, webhooks.created_at, webhooks.last_delivery_at, webhooks.last_delivery
             FROM webhooks JOIN services ON services.id = webhooks.service_id
             WHERE services.deleted_at IS NULL AND ",
            $where
        )
    };
}

#[async_trait]
impl WebhookRepository for SqliteStore {
    async fn get_webhook(&self, id: WebhookId) -> Result<Webhook, StoreError> {
        let row: Option<WebhookRow> =
            sqlx::query_as(select!("webhooks.id = ?")).bind(id.to_string()).fetch_optional(self.pool()).await?;
        row.ok_or_else(|| StoreError::NotFound { entity: ENTITY, id: id.to_string() })?.try_into()
    }

    async fn find_service_webhook(&self, service_id: ServiceId) -> Result<Option<Webhook>, StoreError> {
        let row: Option<WebhookRow> =
            sqlx::query_as(select!("webhooks.service_id = ?")).bind(service_id.to_string()).fetch_optional(self.pool()).await?;
        row.map(Webhook::try_from).transpose()
    }

    async fn set_webhook(&self, service_id: ServiceId, secret: &Secret) -> Result<Webhook, StoreError> {
        let mut tx = self.begin_write().await?;
        let service: Option<i64> = sqlx::query_scalar("SELECT 1 FROM services WHERE id = ? AND deleted_at IS NULL")
            .bind(service_id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        if service.is_none() {
            return Err(StoreError::NotFound { entity: "service", id: service_id.to_string() });
        }
        let existing: Option<String> = sqlx::query_scalar("SELECT id FROM webhooks WHERE service_id = ?")
            .bind(service_id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        let id = match existing {
            Some(id) => parse_column(ENTITY, "id", &id)?,
            None => WebhookId::new(),
        };
        let sealed = self.keyring().seal(secret.expose().as_bytes(), id.to_string().as_bytes());
        let row: WebhookRow = sqlx::query_as(
            "INSERT INTO webhooks (id, service_id, secret, data_key_id) VALUES (?, ?, ?, ?)
             ON CONFLICT (service_id) DO UPDATE
             SET secret = excluded.secret, data_key_id = excluded.data_key_id,
                 created_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             RETURNING id, service_id, created_at, last_delivery_at, last_delivery",
        )
        .bind(id.to_string())
        .bind(service_id.to_string())
        .bind(sealed.ciphertext)
        .bind(sealed.key_id.to_string())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        row.try_into()
    }

    async fn webhook_secret(&self, id: WebhookId) -> Result<Secret, StoreError> {
        open_sealed(self, "SELECT secret, data_key_id FROM webhooks WHERE id = ?", ENTITY, id.to_string()).await
    }

    async fn delete_webhook(&self, service_id: ServiceId) -> Result<(), StoreError> {
        let deleted = sqlx::query("DELETE FROM webhooks WHERE service_id = ?")
            .bind(service_id.to_string())
            .execute(self.pool())
            .await?
            .rows_affected();
        match deleted {
            0 => Err(StoreError::NotFound { entity: ENTITY, id: service_id.to_string() }),
            _ => Ok(()),
        }
    }

    async fn record_webhook_delivery(&self, id: WebhookId, result: &str) -> Result<(), StoreError> {
        sqlx::query("UPDATE webhooks SET last_delivery_at = ?, last_delivery = ? WHERE id = ?")
            .bind(format_timestamp(jiff::Timestamp::now()))
            .bind(result)
            .bind(id.to_string())
            .execute(self.pool())
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Map;
    use spinikus_core::{
        environment::{EnvironmentFields, EnvironmentKind},
        project::ProjectFields,
        service::{ServiceFields, ServiceKind},
    };

    use super::*;
    use crate::{EnvironmentRepository, ProjectRepository, ServiceRepository, test_support::store};

    async fn service(store: &SqliteStore) -> ServiceId {
        let project = ProjectFields { name: "Web".to_string(), slug: "web".parse().unwrap() };
        let project_id = store.create_project(project).await.unwrap().id;
        let env = EnvironmentFields { name: "Staging".to_string(), slug: "staging".parse().unwrap() };
        let env_id = store.create_environment(project_id, EnvironmentKind::Staging, env).await.unwrap().id;
        let service =
            ServiceFields { name: "Web".to_string(), slug: "web".parse().unwrap(), config: Map::new(), server_id: None };
        store.create_service(env_id, ServiceKind::App, service).await.unwrap().id
    }

    #[tokio::test]
    async fn keeps_one_webhook_per_service_with_a_sealed_secret() {
        let (_dir, store) = store().await;
        let service_id = service(&store).await;

        let none = store.find_service_webhook(service_id).await.unwrap();
        let first = store.set_webhook(service_id, &Secret::new("one")).await.unwrap();
        store.record_webhook_delivery(first.id, "deployed dep_1").await.unwrap();
        let rotated = store.set_webhook(service_id, &Secret::new("two")).await.unwrap();
        let secret = store.webhook_secret(first.id).await.unwrap();
        let missing = store.set_webhook(ServiceId::new(), &Secret::new("x")).await;

        assert_eq!(none, None);
        assert_eq!(rotated.id, first.id);
        assert_eq!(rotated.last_delivery.as_deref(), Some("deployed dep_1"));
        assert_eq!(secret.expose(), "two");
        assert_eq!(store.get_webhook(first.id).await.unwrap(), rotated);
        assert!(matches!(missing, Err(StoreError::NotFound { entity: "service", .. })), "{missing:?}");

        store.delete_webhook(service_id).await.unwrap();
        assert!(matches!(store.get_webhook(first.id).await, Err(StoreError::NotFound { .. })));
        assert!(matches!(store.delete_webhook(service_id).await, Err(StoreError::NotFound { .. })));
    }

    #[tokio::test]
    async fn a_deleted_services_webhook_is_gone() {
        let (_dir, store) = store().await;
        let service_id = service(&store).await;
        let webhook = store.set_webhook(service_id, &Secret::new("s")).await.unwrap();

        store.delete_service(service_id).await.unwrap();

        assert!(matches!(store.get_webhook(webhook.id).await, Err(StoreError::NotFound { .. })));
    }
}
