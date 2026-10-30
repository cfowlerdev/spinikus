use anyhow::Context;
use spinikus_control::Control;
use spinikus_core::config::SpinikusConfig;
use spinikus_store::SqliteStore;
use sqlx::SqlitePool;

use crate::{cli::ServerRunArgs, secrets};

pub async fn run(config: &SpinikusConfig, _args: ServerRunArgs) -> anyhow::Result<()> {
    let control = open_control(config).await?;
    if control.needs_setup().await? {
        tracing::warn!("No users yet: create the first admin with `spinikus admin create-user`");
    }

    tracing::info!("Server started, press Ctrl+C to stop");
    tokio::signal::ctrl_c().await.context("Failed to listen for shutdown signal")?;
    tracing::info!("Shutting down");
    Ok(())
}

pub async fn migrate(config: &SpinikusConfig) -> anyhow::Result<()> {
    open_database(config).await?;
    Ok(())
}

/// Opens the database (applying migrations) and the secrets keyring, and builds the service layer.
pub async fn open_control(config: &SpinikusConfig) -> anyhow::Result<Control> {
    let pool = open_database(config).await?;
    let master_key = secrets::load_master_key(config)?;
    let store = SqliteStore::open(pool, &master_key).await.context("Failed to load the secrets keyring")?;
    tracing::info!(master_key = master_key.id(), "Secrets keyring ready");
    Ok(Control::new(store))
}

/// Connects to the database and applies any pending migrations.
async fn open_database(config: &SpinikusConfig) -> anyhow::Result<SqlitePool> {
    let path = config.database_path();
    let pool = spinikus_store::connect(&path).await.with_context(|| format!("Failed to open database at {}", path.display()))?;

    spinikus_store::migrate(&pool).await.context("Failed to apply database migrations")?;
    tracing::info!(path = %path.display(), "Database ready");
    Ok(pool)
}
