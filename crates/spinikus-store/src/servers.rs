use async_trait::async_trait;
use spinikus_core::{
    id::ServerId,
    server::{AgentInfo, Server, ServerFields, ServerStatus},
};

use crate::{SqliteStore, StoreError, error::parse_column};

const ENTITY: &str = "server";

#[async_trait]
pub trait ServerRepository: Send + Sync {
    async fn create_server(&self, fields: ServerFields) -> Result<Server, StoreError>;

    /// The embedded server (the one the control plane runs on), created with `fields` if
    /// there is none yet. Returned even if revoked, so a revoked one is not replaced.
    async fn get_or_create_embedded_server(&self, fields: ServerFields) -> Result<Server, StoreError>;

    /// The embedded server, if there is one (revoked or not).
    async fn find_embedded_server(&self) -> Result<Option<Server>, StoreError>;

    /// Returns revoked servers too, so their history stays visible.
    async fn get_server(&self, id: ServerId) -> Result<Server, StoreError>;

    /// Servers that are not revoked, in creation order, starting after the `after` cursor.
    async fn list_servers(&self, after: Option<ServerId>, limit: u32) -> Result<Vec<Server>, StoreError>;

    /// Replaces the server's fields if it is still at `revision`.
    async fn update_server(&self, id: ServerId, revision: i64, fields: ServerFields) -> Result<Server, StoreError>;

    /// Blocks the server's agent from connecting. Fails with `NotFound` if already revoked.
    async fn revoke_server(&self, id: ServerId) -> Result<(), StoreError>;

    /// Records the agent's `Hello`: it is connected, with these versions and facts.
    /// Fails with `NotFound` for revoked servers.
    async fn record_agent_hello(&self, id: ServerId, agent: AgentInfo) -> Result<(), StoreError>;

    /// Records a heartbeat from a connected agent. Fails with `NotFound` for revoked
    /// servers and servers whose agent never said hello.
    async fn record_heartbeat(&self, id: ServerId) -> Result<(), StoreError>;

    /// Records that the agent's connection ended.
    async fn record_disconnect(&self, id: ServerId) -> Result<(), StoreError>;

    /// Marks every server disconnected, for when the control plane starts and no agent can
    /// be connected to it yet, even if it stopped without recording their disconnects.
    async fn record_all_disconnected(&self) -> Result<u64, StoreError>;
}

/// The server columns joined with its observed state, in the order `ServerRow` reads them.
macro_rules! select_servers {
    () => {
        "SELECT servers.id, servers.name, servers.embedded, servers.revision, servers.created_at, servers.updated_at, servers.revoked_at,
                server_state.connected, server_state.agent_info, server_state.last_seen_at
         FROM servers LEFT JOIN server_state ON server_state.server_id = servers.id"
    };
}

#[derive(sqlx::FromRow)]
struct ServerRow {
    id: String,
    name: String,
    embedded: bool,
    revision: i64,
    created_at: String,
    updated_at: String,
    revoked_at: Option<String>,
    connected: Option<bool>,
    agent_info: Option<String>,
    last_seen_at: Option<String>,
}

impl TryFrom<ServerRow> for Server {
    type Error = StoreError;

    fn try_from(row: ServerRow) -> Result<Self, Self::Error> {
        let agent = row
            .agent_info
            .map(|info| serde_json::from_str::<AgentInfo>(&info))
            .transpose()
            .map_err(|error| StoreError::InvalidRow { entity: ENTITY, column: "agent_info", problem: error.to_string() })?;

        Ok(Server {
            id: parse_column(ENTITY, "id", &row.id)?,
            name: row.name,
            embedded: row.embedded,
            status: ServerStatus::derive(row.revoked_at.is_some(), row.connected),
            agent,
            last_seen_at: row.last_seen_at.map(|at| parse_column(ENTITY, "last_seen_at", &at)).transpose()?,
            revision: row.revision,
            created_at: parse_column(ENTITY, "created_at", &row.created_at)?,
            updated_at: parse_column(ENTITY, "updated_at", &row.updated_at)?,
            revoked_at: row.revoked_at.map(|at| parse_column(ENTITY, "revoked_at", &at)).transpose()?,
        })
    }
}

fn not_found(id: ServerId) -> StoreError {
    StoreError::NotFound { entity: ENTITY, id: id.to_string() }
}

fn check_affected(result: sqlx::sqlite::SqliteQueryResult, id: ServerId) -> Result<(), StoreError> {
    if result.rows_affected() == 0 {
        return Err(not_found(id));
    }
    Ok(())
}

#[async_trait]
impl ServerRepository for SqliteStore {
    async fn create_server(&self, fields: ServerFields) -> Result<Server, StoreError> {
        let id = ServerId::new();
        sqlx::query("INSERT INTO servers (id, name) VALUES (?, ?)")
            .bind(id.to_string())
            .bind(&fields.name)
            .execute(self.pool())
            .await?;

        self.get_server(id).await
    }

    async fn get_or_create_embedded_server(&self, fields: ServerFields) -> Result<Server, StoreError> {
        sqlx::query(
            "INSERT INTO servers (id, name, embedded) SELECT ?, ?, 1
             WHERE NOT EXISTS (SELECT 1 FROM servers WHERE embedded = 1)",
        )
        .bind(ServerId::new().to_string())
        .bind(&fields.name)
        .execute(self.pool())
        .await?;

        let row: ServerRow =
            sqlx::query_as(concat!(select_servers!(), " WHERE servers.embedded = 1")).fetch_one(self.pool()).await?;
        row.try_into()
    }

    async fn find_embedded_server(&self) -> Result<Option<Server>, StoreError> {
        let row: Option<ServerRow> =
            sqlx::query_as(concat!(select_servers!(), " WHERE servers.embedded = 1")).fetch_optional(self.pool()).await?;
        row.map(Server::try_from).transpose()
    }

    async fn get_server(&self, id: ServerId) -> Result<Server, StoreError> {
        let row: Option<ServerRow> = sqlx::query_as(concat!(select_servers!(), " WHERE servers.id = ?"))
            .bind(id.to_string())
            .fetch_optional(self.pool())
            .await?;

        row.ok_or_else(|| not_found(id))?.try_into()
    }

    async fn list_servers(&self, after: Option<ServerId>, limit: u32) -> Result<Vec<Server>, StoreError> {
        let rows: Vec<ServerRow> = sqlx::query_as(concat!(
            select_servers!(),
            " WHERE servers.revoked_at IS NULL AND servers.id > ? ORDER BY servers.id LIMIT ?"
        ))
        .bind(after.map(|id| id.to_string()).unwrap_or_default())
        .bind(limit)
        .fetch_all(self.pool())
        .await?;

        rows.into_iter().map(Server::try_from).collect()
    }

    async fn update_server(&self, id: ServerId, revision: i64, fields: ServerFields) -> Result<Server, StoreError> {
        let result = sqlx::query(
            "UPDATE servers SET name = ?, revision = revision + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ? AND revision = ? AND revoked_at IS NULL",
        )
        .bind(&fields.name)
        .bind(id.to_string())
        .bind(revision)
        .execute(self.pool())
        .await?;

        let current = self.get_server(id).await?;
        match (result.rows_affected(), current.status) {
            (0, ServerStatus::Revoked) => Err(not_found(id)),
            (0, _) => Err(StoreError::RevisionMismatch {
                entity: ENTITY,
                id: id.to_string(),
                expected: revision,
                actual: current.revision,
            }),
            _ => Ok(current),
        }
    }

    async fn revoke_server(&self, id: ServerId) -> Result<(), StoreError> {
        let result = sqlx::query(
            "UPDATE servers
             SET revoked_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                 revision = revision + 1
             WHERE id = ? AND revoked_at IS NULL",
        )
        .bind(id.to_string())
        .execute(self.pool())
        .await?;

        check_affected(result, id)
    }

    async fn record_agent_hello(&self, id: ServerId, agent: AgentInfo) -> Result<(), StoreError> {
        let agent_info = serde_json::to_string(&agent).map_err(|error| StoreError::InvalidRow {
            entity: ENTITY,
            column: "agent_info",
            problem: error.to_string(),
        })?;
        let result = sqlx::query(
            "INSERT INTO server_state (server_id, connected, agent_info, connected_at, last_seen_at)
             SELECT id, 1, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             FROM servers WHERE id = ? AND revoked_at IS NULL
             ON CONFLICT (server_id) DO UPDATE SET
                 connected = 1, agent_info = excluded.agent_info, connected_at = excluded.connected_at,
                 last_seen_at = excluded.last_seen_at",
        )
        .bind(agent_info)
        .bind(id.to_string())
        .execute(self.pool())
        .await?;

        check_affected(result, id)
    }

    async fn record_heartbeat(&self, id: ServerId) -> Result<(), StoreError> {
        let result = sqlx::query(
            "UPDATE server_state SET last_seen_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE server_id = ? AND EXISTS (SELECT 1 FROM servers WHERE id = ? AND revoked_at IS NULL)",
        )
        .bind(id.to_string())
        .bind(id.to_string())
        .execute(self.pool())
        .await?;

        check_affected(result, id)
    }

    async fn record_disconnect(&self, id: ServerId) -> Result<(), StoreError> {
        let result = sqlx::query("UPDATE server_state SET connected = 0 WHERE server_id = ?")
            .bind(id.to_string())
            .execute(self.pool())
            .await?;

        check_affected(result, id)
    }

    async fn record_all_disconnected(&self) -> Result<u64, StoreError> {
        let result = sqlx::query("UPDATE server_state SET connected = 0 WHERE connected = 1").execute(self.pool()).await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use spinikus_core::server::ServerFacts;

    use super::*;
    use crate::test_support::store;

    fn fields(name: &str) -> ServerFields {
        ServerFields { name: name.to_string() }
    }

    fn agent(version: &str) -> AgentInfo {
        AgentInfo {
            agent_version: version.to_string(),
            protocol_version: 1,
            facts: ServerFacts {
                hostname: "web-1".to_string(),
                os: "linux".to_string(),
                arch: "x86_64".to_string(),
                cpu_count: 4,
                memory_bytes: 8 << 30,
            },
        }
    }

    #[tokio::test]
    async fn marks_every_server_disconnected() {
        let (_dir, store) = store().await;
        let first = store.create_server(fields("web-1")).await.unwrap();
        let second = store.create_server(fields("web-2")).await.unwrap();
        store.record_agent_hello(first.id, agent("0.1.0")).await.unwrap();
        store.record_agent_hello(second.id, agent("0.1.0")).await.unwrap();

        assert_eq!(store.record_all_disconnected().await.unwrap(), 2);
        assert_eq!(store.get_server(first.id).await.unwrap().status, ServerStatus::Disconnected);
        assert_eq!(store.record_all_disconnected().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn there_is_one_embedded_server_even_once_revoked() {
        let (_dir, store) = store().await;
        let other = store.create_server(fields("web-2")).await.unwrap();
        assert_eq!(store.find_embedded_server().await.unwrap(), None);

        let embedded = store.get_or_create_embedded_server(fields("host-a")).await.unwrap();
        let again = store.get_or_create_embedded_server(fields("host-b")).await.unwrap();
        store.revoke_server(embedded.id).await.unwrap();
        let revoked = store.get_or_create_embedded_server(fields("host-c")).await.unwrap();

        assert!(embedded.embedded && !other.embedded);
        assert_eq!((again.id, again.name.as_str()), (embedded.id, "host-a"));
        assert_eq!((revoked.id, revoked.status), (embedded.id, ServerStatus::Revoked));
        assert_eq!(store.find_embedded_server().await.unwrap().map(|server| server.id), Some(embedded.id));
    }

    #[tokio::test]
    async fn a_new_server_is_pending() {
        let (_dir, store) = store().await;

        let server = store.create_server(fields("web-1")).await.unwrap();

        assert_eq!(store.get_server(server.id).await.unwrap(), server);
        assert_eq!(server.status, ServerStatus::Pending);
        assert_eq!((server.agent, server.last_seen_at), (None, None));
    }

    #[tokio::test]
    async fn tracks_the_agent_connection() {
        let (_dir, store) = store().await;
        let id = store.create_server(fields("web-1")).await.unwrap().id;

        store.record_agent_hello(id, agent("0.1.0")).await.unwrap();
        let connected = store.get_server(id).await.unwrap();
        store.record_heartbeat(id).await.unwrap();
        store.record_disconnect(id).await.unwrap();
        let disconnected = store.get_server(id).await.unwrap();
        store.record_agent_hello(id, agent("0.2.0")).await.unwrap();
        let reconnected = store.get_server(id).await.unwrap();

        assert_eq!(connected.status, ServerStatus::Connected);
        assert_eq!(connected.agent, Some(agent("0.1.0")));
        assert!(connected.last_seen_at.is_some());
        assert_eq!(disconnected.status, ServerStatus::Disconnected);
        assert_eq!(reconnected.status, ServerStatus::Connected);
        assert_eq!(reconnected.agent, Some(agent("0.2.0")));
        assert_eq!(reconnected.revision, 1, "agent reports must not touch the user-managed revision");
    }

    #[tokio::test]
    async fn a_heartbeat_needs_a_hello_first() {
        let (_dir, store) = store().await;
        let id = store.create_server(fields("web-1")).await.unwrap().id;

        let result = store.record_heartbeat(id).await;

        assert!(matches!(result, Err(StoreError::NotFound { .. })), "{result:?}");
    }

    #[tokio::test]
    async fn revoking_blocks_the_agent_and_hides_the_server_from_the_list() {
        let (_dir, store) = store().await;
        let id = store.create_server(fields("web-1")).await.unwrap().id;
        store.record_agent_hello(id, agent("0.1.0")).await.unwrap();

        store.revoke_server(id).await.unwrap();

        let server = store.get_server(id).await.unwrap();
        assert_eq!(server.status, ServerStatus::Revoked);
        assert!(server.revoked_at.is_some());
        assert!(store.list_servers(None, 10).await.unwrap().is_empty());
        assert!(matches!(store.record_agent_hello(id, agent("0.1.0")).await, Err(StoreError::NotFound { .. })));
        assert!(matches!(store.record_heartbeat(id).await, Err(StoreError::NotFound { .. })));
        assert!(matches!(store.revoke_server(id).await, Err(StoreError::NotFound { .. })));
        store.record_disconnect(id).await.unwrap();
    }

    #[tokio::test]
    async fn renames_at_the_current_revision() {
        let (_dir, store) = store().await;
        let id = store.create_server(fields("web-1")).await.unwrap().id;

        let renamed = store.update_server(id, 1, fields("edge-1")).await.unwrap();
        let stale = store.update_server(id, 1, fields("stale")).await;
        store.revoke_server(id).await.unwrap();
        let revoked = store.update_server(id, 3, fields("gone")).await;

        assert_eq!((renamed.name.as_str(), renamed.revision), ("edge-1", 2));
        assert!(matches!(stale, Err(StoreError::RevisionMismatch { actual: 2, .. })), "{stale:?}");
        assert!(matches!(revoked, Err(StoreError::NotFound { .. })), "{revoked:?}");
    }
}
