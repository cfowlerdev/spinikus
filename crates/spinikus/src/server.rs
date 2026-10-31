use std::net::SocketAddr;

use anyhow::Context;
use spinikus_control::Control;
use spinikus_core::config::SpinikusConfig;
use spinikus_store::SqliteStore;
use sqlx::SqlitePool;
use tokio::{net::TcpListener, sync::watch};

use crate::{cli::ServerRunArgs, secrets};

pub async fn run(config: &SpinikusConfig, args: ServerRunArgs) -> anyhow::Result<()> {
    let control = open_control(config).await?;
    if control.needs_setup().await? {
        tracing::warn!("No users yet: create the first admin with `spinikus admin create-user`");
    }

    let http_address = args.listen.unwrap_or(config.http.listen);
    let agent_address = args.agent_listen.unwrap_or(config.agents.listen);
    let http_listener = TcpListener::bind(http_address).await.with_context(|| format!("Failed to listen on {http_address}"))?;
    let agent_listener =
        TcpListener::bind(agent_address).await.with_context(|| format!("Failed to listen for agents on {agent_address}"))?;

    let (stop, stopped) = watch::channel(false);
    let http =
        axum::serve(http_listener, spinikus_api::router(control.clone()).into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(until(stopped.clone()));
    let agent_address = agent_listener.local_addr().context("Failed to read the agent listener's address")?;
    let proxy = match config.agents.embedded {
        true => crate::agent::proxy_listeners(config).await?,
        false => None,
    };
    let embedded = config.agents.embedded.then(|| {
        let (control, files, shutdown) = (control.clone(), crate::agent::files(config), until(stopped.clone()));
        tokio::spawn(async move {
            let run = crate::embedded::run(control, files, agent_address, crate::agent::runtime(), proxy, shutdown);
            // The control plane keeps running without it; the error says why it stopped.
            if let Err(error) = run.await {
                tracing::error!("{error:#}");
            }
        })
    });
    let deployments = {
        let (control, shutdown) = (control.clone(), until(stopped.clone()));
        tokio::spawn(async move { control.run_deployments(shutdown).await })
    };
    let certificates = {
        let (control, shutdown) = (control.clone(), until(stopped.clone()));
        tokio::spawn(async move { control.run_certificates(shutdown).await })
    };
    let agents = spinikus_control::agents::serve(control, agent_listener, until(stopped));
    tracing::info!(%http_address, "Server started, press Ctrl+C to stop");

    let mut http = tokio::spawn(async move { http.await.context("HTTP server failed") });
    let mut agents =
        tokio::spawn(async move { agents.await.map_err(|error| anyhow::anyhow!(error)).context("Agent listener failed") });

    // Run until Ctrl+C or until either listener stops, then stop both and wait for them.
    let first = tokio::select! {
        signal = tokio::signal::ctrl_c() => Stopped::Signal(signal),
        result = &mut http => Stopped::Http(joined(result)),
        result = &mut agents => Stopped::Agents(joined(result)),
    };
    let _ = stop.send(true);
    let result = match first {
        Stopped::Signal(signal) => {
            tracing::info!("Shutting down");
            let (http, agents) = tokio::join!(http, agents);
            signal.context("Failed to listen for the shutdown signal").and(joined(http)).and(joined(agents))
        }
        Stopped::Http(result) => result.and(joined(agents.await)),
        Stopped::Agents(result) => result.and(joined(http.await)),
    };
    let _ = deployments.await;
    let _ = certificates.await;
    if let Some(embedded) = embedded {
        let _ = embedded.await;
    }
    tracing::info!("Shut down");
    result
}

/// What ended the server first.
enum Stopped {
    Signal(std::io::Result<()>),
    Http(anyhow::Result<()>),
    Agents(anyhow::Result<()>),
}

fn joined(result: Result<anyhow::Result<()>, tokio::task::JoinError>) -> anyhow::Result<()> {
    result.context("Listener task panicked")?
}

/// Resolves once `stopped` turns true.
async fn until(mut stopped: watch::Receiver<bool>) {
    let _ = stopped.wait_for(|stopped| *stopped).await;
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
    Ok(Control::new(store).with_log_dir(config.data_dir.join("logs")))
}

/// Connects to the database and applies any pending migrations.
async fn open_database(config: &SpinikusConfig) -> anyhow::Result<SqlitePool> {
    let path = config.database_path();
    let pool = spinikus_store::connect(&path).await.with_context(|| format!("Failed to open database at {}", path.display()))?;

    spinikus_store::migrate(&pool).await.context("Failed to apply database migrations")?;
    tracing::info!(path = %path.display(), "Database ready");
    Ok(pool)
}
