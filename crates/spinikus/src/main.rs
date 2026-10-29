use clap::Parser;
use spinikus::{cli::{Cli, Commands}, config::SpinikusConfig};

async fn run_command(command: Commands) {
    let result = match command {
        Commands::Admin(args) => tracing::info!("Running in ADMIN mode"),
        Commands::Agent(args) => tracing::info!("Running in AGENT mode"),
        Commands::Server(args) => tracing::info!("Running in SERVER mode")

    };
}

#[tokio::main]
async fn main() {
    // Parse the command line
    let cli = Cli::parse();

    // Load config
    let config = SpinikusConfig::load().expect("Failed to load Spinikus configuration");

    // Init logging
    config.logging.init();

    tracing::info!("Spinikus v0.1");
    // Decide what to do based on command line
    run_command(cli.command).await;
}
