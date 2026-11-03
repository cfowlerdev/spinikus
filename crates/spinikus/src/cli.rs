use clap::{Parser, Subcommand};

use crate::client::args::{
    ContextCommand, DbCommand, DeployArgs, DeploymentCommand, DomainCommand, EnvCommand, EnvironmentCommand, Global, LinkArgs,
    LoginArgs, LogsArgs, ProjectCommand, RollbackArgs, ServiceCommand, TokenCommand,
};

#[derive(Parser)]
#[command(name = "spinikus", version)]
#[command(about = "Self-hosting PaaS")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
    #[command(flatten)]
    pub global: Global,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Run the control plane
    Server(ServerArgs),
    /// Set up an instance from the server itself
    Admin(AdminArgs),
    /// Run this server's agent
    Agent(AgentArgs),
    /// Sign in to an instance; its token is saved as a context
    Login(LoginArgs),
    /// Revoke this context's token and forget it
    Logout,
    /// Show who you are signed in as, and where
    Whoami,
    /// Saved instances to sign in to
    #[command(subcommand)]
    Context(ContextCommand),
    /// Remember which environment or service this directory deploys
    Link(LinkArgs),
    /// Projects
    #[command(subcommand)]
    Project(ProjectCommand),
    /// A project's environments, such as production and staging
    #[command(subcommand)]
    Environment(EnvironmentCommand),
    /// Apps and databases in an environment
    #[command(subcommand)]
    Service(ServiceCommand),
    /// Deploy a service and follow it until it is live
    Deploy(DeployArgs),
    /// Go back to an earlier deployment
    Rollback(RollbackArgs),
    /// A service's deployments
    #[command(subcommand)]
    Deployment(DeploymentCommand),
    /// Print a service's runtime logs
    Logs(LogsArgs),
    /// Variables of a service, environment or project
    #[command(subcommand)]
    Env(EnvCommand),
    /// A service's domains
    #[command(subcommand)]
    Domain(DomainCommand),
    /// Databases, backups and restores
    #[command(subcommand)]
    Db(DbCommand),
    /// Your API tokens
    #[command(subcommand)]
    Token(TokenCommand),
}

#[derive(clap::Args)]
pub struct ServerArgs {
    #[command(flatten)]
    pub instance: InstanceArgs,
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
    #[command(flatten)]
    pub instance: InstanceArgs,
    #[command(subcommand)]
    pub command: AdminCommand,
}

#[derive(Subcommand)]
pub enum AdminCommand {
    /// Create the first admin account. Reads the password from SPINIKUS_ADMIN_PASSWORD, or prompts for it
    CreateUser(CreateUserArgs),
    /// Back up this instance (its database and master key) to a file encrypted with a passphrase. Works while the server runs. Reads the passphrase from SPINIKUS_BACKUP_PASSPHRASE, or prompts for it
    Backup(BackupArgs),
    /// Restore this instance from a backup file, keeping the current database and master key beside it. The server must be stopped. Reads the passphrase from SPINIKUS_BACKUP_PASSPHRASE, or prompts for it
    Restore(RestoreArgs),
}

#[derive(clap::Args)]
pub struct BackupArgs {
    /// Where to write the backup (default: spinikus-backup-<time>.tar.gz.age in the current directory). It must not exist
    pub file: Option<std::path::PathBuf>,
}

#[derive(clap::Args)]
pub struct RestoreArgs {
    /// The backup file, from `spinikus admin backup`
    pub file: std::path::PathBuf,
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
    #[command(flatten)]
    pub instance: InstanceArgs,
    #[command(subcommand)]
    pub command: AgentCommand,
}

#[derive(Subcommand)]
pub enum AgentCommand {
    /// Enroll this server's agent with a join token. The token comes from --join-token or SPINIKUS_JOIN_TOKEN
    Join(AgentJoinArgs),
    /// Run the agent: connect to the control plane and stay connected
    Run,
}

#[derive(clap::Args)]
pub struct AgentJoinArgs {
    /// The control plane's agent address, such as https://spinikus.example.com:8443
    pub url: String,
    /// The join token. Prefer SPINIKUS_JOIN_TOKEN, which does not show up in the process list
    #[arg(long)]
    pub join_token: Option<String>,
    /// The control plane's CA fingerprint from the join command, such as sha256:9e38…
    #[arg(long)]
    pub ca_fingerprint: String,
}

/// Where the server, admin and agent commands find the instance's configuration.
#[derive(clap::Args)]
pub struct InstanceArgs {
    /// The instance's config file. Without it: ./spinikus.toml if there is one, else
    /// /etc/spinikus/spinikus.toml
    #[arg(long, short = 'c', env = "SPINIKUS_CONFIG_FILE", value_name = "PATH")]
    pub config: Option<std::path::PathBuf>,
}
