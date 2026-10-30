use std::{path::Path, time::Duration};

use sqlx::{
    ConnectOptions, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous}
};

use crate::StoreError;

/// How long a connection waits on a locked database before giving up.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Opens the SQLite database at `path`, creating the file and its directory if needed.
///
/// Every connection uses WAL mode with enforced foreign keys and a busy timeout.
pub async fn connect(path: &Path) -> Result<SqlitePool, StoreError> {
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|source| StoreError::CreateDir { path: dir.display().to_string(), source })?;
    }

    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(BUSY_TIMEOUT)
        .log_statements(tracing::log::LevelFilter::Trace);

    let pool = SqlitePoolOptions::new().connect_with(options).await?;
    tracing::debug!(path = %path.display(), "connected to database");
    Ok(pool)
}

/// Formats a timestamp the way every timestamp column stores it, matching
/// `strftime('%Y-%m-%dT%H:%M:%fZ', 'now')`: UTC with milliseconds, so stored values sort as text.
pub(crate) fn format_timestamp(at: jiff::Timestamp) -> String {
    at.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// The SQLite implementation of every repository trait.
#[derive(Clone)]
pub struct SqliteStore {
    pool: SqlitePool
}

impl SqliteStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Starts a write transaction up front, so reads inside it see the state the writes are based on.
    /// A deferred transaction that reads first can fail with "database is locked" when it starts writing.
    pub(crate) async fn begin_write(&self) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>, StoreError> {
        Ok(self.pool.begin_with("BEGIN IMMEDIATE").await?)
    }
}

/// Applies any pending migrations embedded from `migrations/`.
pub async fn migrate(pool: &SqlitePool) -> Result<(), StoreError> {
    sqlx::migrate!().run(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::migrated_pool;

    #[tokio::test]
    async fn creates_database_and_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/spinikus.db");

        connect(&path).await.unwrap();

        assert!(path.exists());
    }

    #[tokio::test]
    async fn applies_connection_pragmas() {
        let dir = tempfile::tempdir().unwrap();
        let pool = connect(&dir.path().join("spinikus.db")).await.unwrap();

        let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode").fetch_one(&pool).await.unwrap();
        let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys").fetch_one(&pool).await.unwrap();
        let busy_timeout: i64 = sqlx::query_scalar("PRAGMA busy_timeout").fetch_one(&pool).await.unwrap();

        assert_eq!(journal_mode, "wal");
        assert_eq!(foreign_keys, 1);
        assert_eq!(busy_timeout, 5000);
    }

    #[tokio::test]
    async fn formats_timestamps_like_sqlite() {
        let (_dir, pool) = migrated_pool().await;
        let now: String = sqlx::query_scalar("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')").fetch_one(&pool).await.unwrap();
        let parsed: jiff::Timestamp = now.parse().unwrap();

        assert_eq!(format_timestamp(parsed), now);
        assert_eq!(format_timestamp("2026-10-30T09:00:00Z".parse().unwrap()), "2026-10-30T09:00:00.000Z");
    }

    #[tokio::test]
    async fn migrations_are_idempotent() {
        let (_dir, pool) = migrated_pool().await;

        migrate(&pool).await.unwrap();
    }

    #[tokio::test]
    async fn all_tables_are_strict() {
        let (_dir, pool) = migrated_pool().await;

        let loose: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM pragma_table_list WHERE schema = 'main' AND type = 'table' AND strict = 0
             AND name NOT LIKE 'sqlite_%' AND name <> '_sqlx_migrations'"
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        assert!(loose.is_empty(), "tables without STRICT: {loose:?}");
    }

    #[tokio::test]
    async fn settings_reject_invalid_json() {
        let (_dir, pool) = migrated_pool().await;

        sqlx::query("INSERT INTO settings (key, value) VALUES ('acme_email', '\"ops@example.com\"')")
            .execute(&pool)
            .await
            .unwrap();
        let invalid = sqlx::query("INSERT INTO settings (key, value) VALUES ('broken', 'not json')").execute(&pool).await;

        assert!(invalid.is_err());
    }
}
