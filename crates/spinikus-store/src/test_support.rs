use sqlx::SqlitePool;

use crate::{SqliteStore, connect, migrate};

/// A migrated database in a temporary directory, deleted when the `TempDir` is dropped.
pub async fn migrated_pool() -> (tempfile::TempDir, SqlitePool) {
    let dir = tempfile::tempdir().unwrap();
    let pool = connect(&dir.path().join("spinikus.db")).await.unwrap();
    migrate(&pool).await.unwrap();
    (dir, pool)
}

pub async fn store() -> (tempfile::TempDir, SqliteStore) {
    let (dir, pool) = migrated_pool().await;
    (dir, SqliteStore::new(pool))
}
