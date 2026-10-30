use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "spinikus")]
#[command(about = "Self-hosting PaaS")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    Server(ServerArgs),
    Admin(AdminArgs),
    Agent(AgentArgs),
}

#[derive(clap::Args)]
pub struct ServerArgs {
    #[command(subcommand)]
    pub command: ServerCommand,
}

#[derive(Subcommand)]
pub enum ServerCommand {
    /// Run the control plane
    Run(ServerRunArgs),
    /// Apply pending database migrations and exit
    Migrate,
}

#[derive(clap::Args)]
pub struct ServerRunArgs {
    #[arg(long)]
    pub host: Option<String>,
}

#[derive(clap::Args)]
pub struct AdminArgs {
    #[arg(long)]
    pub command: Option<String>,
}

#[derive(clap::Args)]
pub struct AgentArgs {
    #[arg(long)]
    pub command: Option<String>,
}
