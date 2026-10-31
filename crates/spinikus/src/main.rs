use anyhow::Context;
use clap::Parser;
use spinikus::{
    admin, agent,
    cli::{AdminCommand, AgentCommand, Cli, Commands, ServerCommand},
    config, server,
};
use spinikus_core::config::SpinikusConfig;

async fn run_command(command: Commands, config: &SpinikusConfig) -> anyhow::Result<()> {
    match command {
        Commands::Admin(args) => match args.command {
            AdminCommand::CreateUser(args) => admin::create_user(config, args).await?,
        },
        Commands::Agent(args) => match args.command {
            AgentCommand::Join(args) => agent::join(config, args).await?,
            AgentCommand::Run => agent::run(config).await?,
        },
        Commands::Server(args) => match args.command {
            ServerCommand::Run(args) => server::run(config, args).await?,
            ServerCommand::Migrate => server::migrate(config).await?,
        },
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
