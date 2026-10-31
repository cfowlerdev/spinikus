use async_trait::async_trait;
use jiff::Timestamp;
use spinikus_core::{
    id::{CertificateId, ServerId},
    route::{Route, RouteTable},
};

use crate::{SqliteStore, StoreError};

const ENTITY: &str = "route table";

/// A server's route table, and the revision its agent last said it applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRoutes {
    pub table: RouteTable,
    pub applied_revision: u64,
}

impl StoredRoutes {
    /// Whether the agent has applied the current table.
    pub fn applied(&self) -> bool {
        self.applied_revision == self.table.revision
    }
}

#[async_trait]
pub trait RouteRepository: Send + Sync {
    async fn get_route_table(&self, server_id: ServerId) -> Result<Option<StoredRoutes>, StoreError>;

    /// Stores `routes` as the server's table. Unchanged routes keep their revision; changed
    /// ones get a new revision, at least the current time in milliseconds.
    async fn save_route_table(&self, server_id: ServerId, routes: Vec<Route>) -> Result<StoredRoutes, StoreError>;

    /// Records that the agent applied `revision`. Anything but the current revision is
    /// ignored: an older one is out of date, and an unknown one came from other routes.
    async fn record_routes_applied(&self, server_id: ServerId, revision: u64) -> Result<(), StoreError>;
}

#[derive(sqlx::FromRow)]
struct RouteRow {
    revision: i64,
    routes: String,
    applied_revision: i64,
}

impl TryFrom<RouteRow> for StoredRoutes {
    type Error = StoreError;

    fn try_from(row: RouteRow) -> Result<Self, Self::Error> {
        let routes = serde_json::from_str(&row.routes).map_err(|error| StoreError::InvalidRow {
            entity: ENTITY,
            column: "routes",
            problem: error.to_string(),
        })?;
        Ok(StoredRoutes {
            table: RouteTable { revision: row.revision as u64, routes },
            applied_revision: row.applied_revision as u64,
        })
    }
}

async fn fetch(conn: &mut sqlx::SqliteConnection, server_id: ServerId) -> Result<Option<StoredRoutes>, StoreError> {
    let row: Option<RouteRow> = sqlx::query_as("SELECT revision, routes, applied_revision FROM route_tables WHERE server_id = ?")
        .bind(server_id.to_string())
        .fetch_optional(conn)
        .await?;
    row.map(StoredRoutes::try_from).transpose()
}

#[async_trait]
impl RouteRepository for SqliteStore {
    async fn get_route_table(&self, server_id: ServerId) -> Result<Option<StoredRoutes>, StoreError> {
        fetch(&mut *self.pool().acquire().await?, server_id).await
    }

    async fn save_route_table(&self, server_id: ServerId, routes: Vec<Route>) -> Result<StoredRoutes, StoreError> {
        let mut tx = self.begin_write().await?;
        let current = fetch(&mut tx, server_id).await?;
        if let Some(current) = &current
            && current.table.routes == routes
        {
            return Ok(current.clone());
        }
        let revision = next_revision(current.as_ref().map(|current| current.table.revision));
        let json = serde_json::to_string(&routes).expect("routes serialize");
        sqlx::query(
            "INSERT INTO route_tables (server_id, revision, routes) VALUES (?, ?, ?)
             ON CONFLICT (server_id) DO UPDATE
             SET revision = excluded.revision, routes = excluded.routes,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
        )
        .bind(server_id.to_string())
        .bind(revision)
        .bind(json)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(StoredRoutes {
            table: RouteTable { revision: revision as u64, routes },
            applied_revision: current.map_or(0, |current| current.applied_revision),
        })
    }

    async fn record_routes_applied(&self, server_id: ServerId, revision: u64) -> Result<(), StoreError> {
        let Ok(revision) = i64::try_from(revision) else {
            return Ok(());
        };
        sqlx::query("UPDATE route_tables SET applied_revision = ? WHERE server_id = ? AND revision = ?")
            .bind(revision)
            .bind(server_id.to_string())
            .bind(revision)
            .execute(self.pool())
            .await?;
        Ok(())
    }
}

/// The certificates a server's proxy serves, and the revision its agent last applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCertificates {
    pub revision: u64,
    pub certificate_ids: Vec<CertificateId>,
    pub applied_revision: u64,
}

impl StoredCertificates {
    pub fn applied(&self) -> bool {
        self.applied_revision == self.revision
    }
}

/// The certificate snapshots sent to agents, alongside their route tables.
#[async_trait]
pub trait ServerCertificateRepository: Send + Sync {
    async fn get_server_certificates(&self, server_id: ServerId) -> Result<Option<StoredCertificates>, StoreError>;

    /// Stores the server's certificates. Unchanged ones keep their revision; changed ones
    /// get a new revision, at least the current time in milliseconds.
    async fn save_server_certificates(
        &self,
        server_id: ServerId,
        certificate_ids: Vec<CertificateId>,
    ) -> Result<StoredCertificates, StoreError>;

    /// Records that the agent applied `revision`, if it is the current one.
    async fn record_certificates_applied(&self, server_id: ServerId, revision: u64) -> Result<(), StoreError>;
}

async fn fetch_certificates(
    conn: &mut sqlx::SqliteConnection,
    server_id: ServerId,
) -> Result<Option<StoredCertificates>, StoreError> {
    let row: Option<(i64, String, i64)> =
        sqlx::query_as("SELECT revision, certificate_ids, applied_revision FROM server_certificates WHERE server_id = ?")
            .bind(server_id.to_string())
            .fetch_optional(conn)
            .await?;
    row.map(|(revision, ids, applied)| {
        let certificate_ids = serde_json::from_str(&ids).map_err(|error| StoreError::InvalidRow {
            entity: "server certificates",
            column: "certificate_ids",
            problem: error.to_string(),
        })?;
        Ok(StoredCertificates { revision: revision as u64, certificate_ids, applied_revision: applied as u64 })
    })
    .transpose()
}

/// A revision after `previous`, at least the current time in milliseconds.
fn next_revision(previous: Option<u64>) -> i64 {
    let now = Timestamp::now().as_millisecond();
    previous.map_or(now, |previous| (previous as i64 + 1).max(now))
}

#[async_trait]
impl ServerCertificateRepository for SqliteStore {
    async fn get_server_certificates(&self, server_id: ServerId) -> Result<Option<StoredCertificates>, StoreError> {
        fetch_certificates(&mut *self.pool().acquire().await?, server_id).await
    }

    async fn save_server_certificates(
        &self,
        server_id: ServerId,
        certificate_ids: Vec<CertificateId>,
    ) -> Result<StoredCertificates, StoreError> {
        let mut tx = self.begin_write().await?;
        let current = fetch_certificates(&mut tx, server_id).await?;
        if let Some(current) = &current
            && current.certificate_ids == certificate_ids
        {
            return Ok(current.clone());
        }
        let revision = next_revision(current.as_ref().map(|current| current.revision));
        sqlx::query(
            "INSERT INTO server_certificates (server_id, revision, certificate_ids) VALUES (?, ?, ?)
             ON CONFLICT (server_id) DO UPDATE
             SET revision = excluded.revision, certificate_ids = excluded.certificate_ids,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
        )
        .bind(server_id.to_string())
        .bind(revision)
        .bind(serde_json::to_string(&certificate_ids).expect("IDs serialize"))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(StoredCertificates {
            revision: revision as u64,
            certificate_ids,
            applied_revision: current.map_or(0, |current| current.applied_revision),
        })
    }

    async fn record_certificates_applied(&self, server_id: ServerId, revision: u64) -> Result<(), StoreError> {
        let Ok(revision) = i64::try_from(revision) else {
            return Ok(());
        };
        sqlx::query("UPDATE server_certificates SET applied_revision = ? WHERE server_id = ? AND revision = ?")
            .bind(revision)
            .bind(server_id.to_string())
            .bind(revision)
            .execute(self.pool())
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use spinikus_core::{
        id::{DeploymentId, ServiceId},
        route::{ProxyTarget, RouteAction},
        server::ServerFields,
    };

    use super::*;
    use crate::{ServerRepository, test_support::store};

    fn route(hostname: &str) -> Route {
        let target = ProxyTarget { service_id: ServiceId::new(), deployment_id: DeploymentId::new(), port: 80 };
        Route { hostname: hostname.parse().unwrap(), action: RouteAction::Proxy(target) }
    }

    #[tokio::test]
    async fn bumps_the_revision_only_when_routes_change() {
        let (_dir, store) = store().await;
        let server = store.create_server(ServerFields { name: "web-1".to_string() }).await.unwrap().id;
        let routes = vec![route("app.example.com")];

        assert_eq!(store.get_route_table(server).await.unwrap(), None);
        let first = store.save_route_table(server, routes.clone()).await.unwrap();
        let same = store.save_route_table(server, routes.clone()).await.unwrap();
        store.record_routes_applied(server, first.table.revision + 1).await.unwrap();
        let unknown = store.get_route_table(server).await.unwrap().unwrap();
        store.record_routes_applied(server, first.table.revision).await.unwrap();
        let applied = store.get_route_table(server).await.unwrap().unwrap();
        let changed = store.save_route_table(server, Vec::new()).await.unwrap();
        store.record_routes_applied(server, first.table.revision).await.unwrap();

        assert!(first.table.revision >= 1_700_000_000_000, "{}", first.table.revision);
        assert_eq!((first.applied_revision, first.applied()), (0, false));
        assert_eq!(same, first);
        assert_eq!(unknown.applied_revision, 0);
        assert!(applied.applied());
        assert!(changed.table.revision > first.table.revision);
        assert_eq!(changed.table.routes, Vec::new());
        assert!(!changed.applied());
        assert_eq!(store.get_route_table(server).await.unwrap().unwrap(), changed);
    }
}

#[cfg(test)]
mod certificate_tests {
    use spinikus_core::server::ServerFields;

    use super::*;
    use crate::{ServerRepository, test_support::store};

    #[tokio::test]
    async fn bumps_the_revision_only_when_certificates_change() {
        let (_dir, store) = store().await;
        let server = store.create_server(ServerFields { name: "web-1".to_string() }).await.unwrap().id;
        let ids = vec![CertificateId::new()];

        assert_eq!(store.get_server_certificates(server).await.unwrap(), None);
        let first = store.save_server_certificates(server, ids.clone()).await.unwrap();
        let same = store.save_server_certificates(server, ids.clone()).await.unwrap();
        store.record_certificates_applied(server, first.revision).await.unwrap();
        let applied = store.get_server_certificates(server).await.unwrap().unwrap();
        let changed = store.save_server_certificates(server, Vec::new()).await.unwrap();

        assert_eq!(same, first);
        assert!(applied.applied());
        assert!(changed.revision > first.revision && !changed.applied());
    }
}
