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
    /// Address for the API and web UI, overriding `[http] listen`, e.g. 0.0.0.0:8080
    #[arg(long)]
    pub listen: Option<std::net::SocketAddr>,
    /// Address for agents to connect to, overriding `[agents] listen`, e.g. 0.0.0.0:8443
    #[arg(long)]
    pub agent_listen: Option<std::net::SocketAddr>,
}

#[derive(clap::Args)]
pub struct AdminArgs {
    #[command(subcommand)]
    pub command: AdminCommand,
}

#[derive(Subcommand)]
pub enum AdminCommand {
    /// Create the first admin account. Reads the password from SPINIKUS_ADMIN_PASSWORD, or prompts for it
    CreateUser(CreateUserArgs),
}

#[derive(clap::Args)]
pub struct CreateUserArgs {
    #[arg(long)]
    pub email: String,
    #[arg(long)]
    pub name: String,
}

#[derive(clap::Args)]
pub struct AgentArgs {
    #[arg(long)]
    pub command: Option<String>,
}
