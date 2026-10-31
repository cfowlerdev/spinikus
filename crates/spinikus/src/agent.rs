//! `spinikus agent join` and `spinikus agent run`.

use anyhow::{Context, bail};
use spinikus_agent::{AgentFiles, Backoff, RunOptions};
use spinikus_core::{config::SpinikusConfig, secret::Secret};

use crate::cli::AgentJoinArgs;

/// Environment variable for the join token, which keeps it out of the process list.
const JOIN_TOKEN_VAR: &str = "SPINIKUS_JOIN_TOKEN";

/// Where the agent keeps its state: `<data_dir>/agent`.
pub fn files(config: &SpinikusConfig) -> AgentFiles {
    AgentFiles::new(config.data_dir.join("agent"))
}

pub async fn join(config: &SpinikusConfig, args: AgentJoinArgs) -> anyhow::Result<()> {
    let files = files(config);
    if let Ok(credentials) = files.load() {
        bail!(
            "This agent is already enrolled as server {}; delete {} to enroll it again",
            credentials.server_id,
            files.dir().display()
        );
    }
    let token = match args.token.or_else(|| std::env::var(JOIN_TOKEN_VAR).ok()) {
        Some(token) => Secret::new(token),
        None => bail!("Pass the join token with --token or {JOIN_TOKEN_VAR}"),
    };

    let credentials = spinikus_agent::join(&args.url, &token, &args.ca_fingerprint, spinikus_agent::server_facts())
        .await
        .context("Failed to enroll")?;
    files.save(&credentials).context("Failed to save the agent's credentials")?;
    println!("Enrolled as server {}. Start the agent with `spinikus agent run`.", credentials.server_id);
    Ok(())
}

pub async fn run(config: &SpinikusConfig) -> anyhow::Result<()> {
    let options = RunOptions { facts: spinikus_agent::server_facts(), backoff: Backoff::default() };
    let shutdown = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "Failed to listen for the shutdown signal");
            std::future::pending::<()>().await;
        }
        tracing::info!("Shutting down");
    };
    spinikus_agent::run(&files(config), options, shutdown).await.context("The agent stopped")
}
