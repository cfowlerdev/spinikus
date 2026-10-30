use spinikus_store::{MasterKey, SqliteStore, connect, migrate};

use crate::Control;

/// A service layer over a migrated database in a temporary directory, deleted when the `TempDir` is dropped.
pub async fn control() -> (tempfile::TempDir, Control) {
    let dir = tempfile::tempdir().unwrap();
    let pool = connect(&dir.path().join("spinikus.db")).await.unwrap();
    migrate(&pool).await.unwrap();
    let store = SqliteStore::open(pool, &MasterKey::generate()).await.unwrap();
    (dir, Control::new(store))
}
