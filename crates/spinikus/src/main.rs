use std::process::ExitCode;

use anyhow::Context;
use clap::Parser;
use spinikus::{
    admin, agent,
    cli::{AdminCommand, AgentCommand, Cli, Commands, ServerCommand},
    client, config, server,
};
use spinikus_core::config::SpinikusConfig;

#[cfg(target_env = "musl")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

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
        _ => unreachable!("client commands run without the instance config"),
    }
    Ok(())
}

/// The server and agent commands: they read `spinikus.toml` and log.
async fn run_instance(command: Commands) -> anyhow::Result<()> {
    let instance = match &command {
        Commands::Server(args) => &args.instance,
        Commands::Admin(args) => &args.instance,
        Commands::Agent(args) => &args.instance,
        _ => unreachable!("only instance commands read the config"),
    };
    let path = config::path(instance.config.as_deref());
    let config = config::load(&path).with_context(|| format!("Failed to load the configuration from {}", path.display()))?;
    config::init_logging(&config.logging);
    tracing::info!(version = env!("CARGO_PKG_VERSION"), config = %path.display(), "Spinikus");
    run_command(command, &config).await
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        command @ (Commands::Server(_) | Commands::Admin(_) | Commands::Agent(_)) => match run_instance(command).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("Error: {error:?}");
                ExitCode::FAILURE
            }
        },
        // The client commands talk to an instance over its API and need no config file.
        command => match client::run(command, cli.global).await {
            Some(Ok(())) => ExitCode::SUCCESS,
            Some(Err(error)) => {
                eprintln!("error: {error:#}");
                ExitCode::from(client::exit_status(&error))
            }
            None => unreachable!("instance commands are handled above"),
        },
    }
}
