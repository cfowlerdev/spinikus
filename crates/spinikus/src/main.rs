use anyhow::Context;
use clap::Parser;
use spinikus::{cli::{Cli, Commands, ServerCommand}, config, server};
use spinikus_core::config::SpinikusConfig;

async fn run_command(command: Commands, config: &SpinikusConfig) -> anyhow::Result<()> {
    match command {
        Commands::Admin(_args) => tracing::info!("Running in ADMIN mode"),
        Commands::Agent(_args) => tracing::info!("Running in AGENT mode"),
        Commands::Server(args) => match args.command {
            ServerCommand::Run(args) => server::run(config, args).await?,
            ServerCommand::Migrate => server::migrate(config).await?
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse the command line
    let cli = Cli::parse();

    // Load config
    let config = config::load().context("Failed to load Spinikus configuration")?;

    // Init logging
    config::init_logging(&config.logging);

    tracing::info!("Spinikus v0.1");
    // Decide what to do based on command line
    run_command(cli.command, &config).await
}
