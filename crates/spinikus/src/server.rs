use std::net::SocketAddr;

use anyhow::Context;
use spinikus_control::Control;
use spinikus_core::config::SpinikusConfig;
use spinikus_store::SqliteStore;
use sqlx::SqlitePool;
use tokio::net::TcpListener;

use crate::{cli::ServerRunArgs, secrets};

pub async fn run(config: &SpinikusConfig, args: ServerRunArgs) -> anyhow::Result<()> {
    let control = open_control(config).await?;
    if control.needs_setup().await? {
        tracing::warn!("No users yet: create the first admin with `spinikus admin create-user`");
    }

    let address = args.listen.unwrap_or(config.http.listen);
    let listener = TcpListener::bind(address).await.with_context(|| format!("Failed to listen on {address}"))?;
    let app = spinikus_api::router(control).into_make_service_with_connect_info::<SocketAddr>();
    tracing::info!(%address, "Server started, press Ctrl+C to stop");
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await.context("HTTP server failed")?;
    tracing::info!("Shut down");
    Ok(())
}

async fn shutdown_signal() {
    match tokio::signal::ctrl_c().await {
        Ok(()) => tracing::info!("Shutting down"),
        Err(error) => tracing::error!(%error, "Failed to listen for the shutdown signal"),
    }
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
