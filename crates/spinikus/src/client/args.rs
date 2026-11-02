//! The client commands' arguments.
//!
//! Projects, environments and services are named by ID (`svc_…`), by slug path
//! (`shop/production/web`), or by a slug relative to the directory's link (`web`, or
//! `production/web`); left out, the linked one is used.

use clap::{Args, Subcommand};

use super::output::Format;

/// Flags every client command takes.
#[derive(Args, Debug, Clone)]
pub struct Global {
    /// The instance to use, such as https://spinikus.example.com (default: the linked or current context's)
    #[arg(long, global = true, env = "SPINIKUS_URL", hide_env_values = true, conflicts_with = "context")]
    pub server: Option<String>,
    /// An API token to use instead of the context's
    #[arg(long, global = true, env = "SPINIKUS_TOKEN", hide_env_values = true)]
    pub token: Option<String>,
    /// The saved context to use (see `spinikus context list`)
    #[arg(long, global = true, env = "SPINIKUS_CONTEXT", hide_env_values = true)]
    pub context: Option<String>,
    /// How to print results: tables for people, or the API's JSON
    #[arg(short, long, global = true, value_enum, default_value_t)]
    pub output: Format,
    /// Print results and errors only, no progress or messages
    #[arg(short, long, global = true)]
    pub quiet: bool,
    /// No colors, also when printing to a terminal (or set NO_COLOR)
    #[arg(long, global = true)]
    pub no_color: bool,
}

#[derive(Args, Debug)]
pub struct LoginArgs {
    /// The instance, such as https://spinikus.example.com (default: the current context's)
    pub server: Option<String>,
    /// Your email (asked for if left out)
    #[arg(long)]
    pub email: Option<String>,
    /// What to call this context (default: the server's host)
    #[arg(long = "name")]
    pub context_name: Option<String>,
    /// What to call the API token created for this machine
    #[arg(long, default_value = "spinikus CLI")]
    pub token_name: String,
}

#[derive(Subcommand, Debug)]
pub enum ContextCommand {
    /// List saved contexts
    List,
    /// Make a context the current one
    Use { name: String },
    /// Forget a context and its token (without revoking it; `logout` does)
    Remove { name: String },
}

#[derive(Args, Debug)]
pub struct LinkArgs {
    /// What this directory deploys: `project/environment[/service]` or an environment or service ID (asked for if left out)
    pub target: Option<String>,
    /// Remove this directory's link instead
    #[arg(long, conflicts_with = "target")]
    pub remove: bool,
}

#[derive(Subcommand, Debug)]
pub enum ProjectCommand {
    /// List the projects you can see
    List,
    /// Create a project (instance admins)
    Create {
        name: String,
        /// Derived from the name if left out
        #[arg(long)]
        slug: Option<String>,
    },
    /// Show a project and its environments
    Show { project: Option<String> },
    /// Delete a project (it must have no environments)
    Delete {
        project: String,
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum EnvironmentCommand {
    /// List a project's environments
    List { project: Option<String> },
    /// Create an environment
    Create {
        name: String,
        /// The project (default: the linked one)
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        slug: Option<String>,
        #[arg(long, value_parser = ["production", "staging"], default_value = "production")]
        kind: String,
        /// Deploy a preview of each pull request to the branch its Git services deploy
        #[arg(long)]
        previews: bool,
    },
    /// Show an environment and its services
    Show { environment: Option<String> },
    /// Turn previews of pull requests on or off: each gets a copy of the environment, deleted
    /// with its data when the pull request closes
    Previews {
        #[arg(value_parser = ["on", "off"])]
        state: String,
        /// The environment (default: the linked one)
        environment: Option<String>,
    },
    /// Delete an environment (it must have no services, unless it is a pull request's preview)
    Delete {
        environment: String,
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ServiceCommand {
    /// List an environment's services
    List { environment: Option<String> },
    /// Create an app from an image or a Git repository
    Create(ServiceCreateArgs),
    /// Show a service: its source, what runs and where it answers
    Show { service: Option<String> },
    /// Delete a service; its deployments are stopped and removed
    Delete {
        service: String,
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Args, Debug)]
pub struct ServiceCreateArgs {
    pub name: String,
    /// The environment (default: the linked one)
    #[arg(short, long)]
    pub environment: Option<String>,
    #[arg(long)]
    pub slug: Option<String>,
    /// Deploy this image, such as ghcr.io/acme/web:1
    #[arg(long, required_unless_present = "git", conflicts_with = "git")]
    pub image: Option<String>,
    /// Build this Git repository
    #[arg(long)]
    pub git: Option<String>,
    /// The branch to build
    #[arg(long, default_value = "main", requires = "git")]
    pub branch: String,
    /// The directory in the repository to build, for monorepos
    #[arg(long, requires = "git")]
    pub root_dir: Option<String>,
    /// The port the app listens on
    #[arg(long)]
    pub port: Option<u16>,
    /// The server to run on (default: the control plane's)
    #[arg(long = "server-id")]
    pub server_id: Option<String>,
}

#[derive(Args, Debug)]
pub struct DeployArgs {
    pub service: Option<String>,
    /// Don't wait: print the deployment and return
    #[arg(short, long)]
    pub detach: bool,
}

#[derive(Args, Debug)]
pub struct RollbackArgs {
    pub service: Option<String>,
    /// The deployment to go back to (default: the one the live deployment replaced)
    #[arg(long)]
    pub to: Option<String>,
    #[arg(short, long)]
    pub detach: bool,
}

#[derive(Subcommand, Debug)]
pub enum DeploymentCommand {
    /// List a service's deployments, newest first
    List { service: Option<String> },
    /// Show a deployment and its history
    Show { deployment: String },
    /// Print a deployment's log (the build, pull and health check)
    Logs {
        deployment: String,
        /// Keep printing until it is live or has finished
        #[arg(short, long)]
        follow: bool,
    },
    /// Cancel a deployment in progress
    Cancel { deployment: String },
}

#[derive(Args, Debug)]
pub struct LogsArgs {
    pub service: Option<String>,
    /// Keep printing new lines
    #[arg(short, long)]
    pub follow: bool,
    /// How many lines to start with (0: all)
    #[arg(short = 'n', long, default_value_t = 100)]
    pub tail: u32,
    /// Lines since this time, such as 2026-11-01T09:00:00Z
    #[arg(long)]
    pub since: Option<String>,
    /// A deployment's container instead of the live one
    #[arg(long)]
    pub deployment: Option<String>,
    /// Start lines with their time
    #[arg(short, long)]
    pub timestamps: bool,
}

/// Where variables live: a service, an environment or a project. With none of these, the
/// linked service (or environment, if the link names none).
#[derive(Args, Debug, Clone)]
pub struct VariableScope {
    /// A service's variables (no value: the linked service)
    #[arg(short, long, num_args = 0..=1, default_missing_value = "", conflicts_with_all = ["environment", "project"])]
    pub service: Option<String>,
    /// An environment's variables, shared by its services (no value: the linked environment)
    #[arg(short, long, num_args = 0..=1, default_missing_value = "", conflicts_with = "project")]
    pub environment: Option<String>,
    /// A project's variables, shared by all its services (no value: the linked project)
    #[arg(short, long, num_args = 0..=1, default_missing_value = "")]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Availability {
    Runtime,
    Build,
    Both,
}

impl Availability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Build => "build",
            Self::Both => "both",
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum EnvCommand {
    /// List variables (secret values aren't shown)
    List {
        #[command(flatten)]
        scope: VariableScope,
    },
    /// Set variables: NAME=value, or NAME alone to type the value (hidden) or pipe it in
    Set {
        #[arg(required = true, value_name = "NAME[=VALUE]")]
        variables: Vec<String>,
        /// Keep the values secret: write-only, and redacted from build logs
        #[arg(long)]
        secret: bool,
        /// When the variables are available
        #[arg(long, value_enum)]
        availability: Option<Availability>,
        #[command(flatten)]
        scope: VariableScope,
    },
    /// Remove variables
    Unset {
        #[arg(required = true, value_name = "NAME")]
        names: Vec<String>,
        #[command(flatten)]
        scope: VariableScope,
    },
    /// Set the variables in a .env file (`-` reads stdin)
    Import {
        file: String,
        #[arg(long)]
        secret: bool,
        #[arg(long, value_enum)]
        availability: Option<Availability>,
        #[command(flatten)]
        scope: VariableScope,
    },
    /// Print variables as a .env file
    Export {
        /// Include secret values (scope admins; each reveal is audited)
        #[arg(long)]
        reveal: bool,
        #[command(flatten)]
        scope: VariableScope,
    },
}

#[derive(Subcommand, Debug)]
pub enum DomainCommand {
    /// List a service's domains and its generated hostname, with HTTPS status
    List {
        #[arg(short, long)]
        service: Option<String>,
    },
    /// Add a domain to a service
    Add {
        hostname: String,
        /// Redirect to another of the service's domains instead of serving the app
        #[arg(long)]
        redirect_to: Option<String>,
        #[arg(short, long)]
        service: Option<String>,
    },
    /// Remove a domain
    Remove {
        hostname: String,
        #[arg(short, long)]
        service: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Engine {
    Postgres,
    Redis,
}

#[derive(Subcommand, Debug)]
pub enum DbCommand {
    /// Create a database and deploy it, empty or with the data of a backup
    Create {
        name: String,
        /// Required unless it starts from a backup
        #[arg(long, value_enum, required_unless_present = "from_backup")]
        engine: Option<Engine>,
        /// A major version, such as 17 (default: the newest, or the backup's database's)
        #[arg(long)]
        version: Option<String>,
        /// A backup of another database (its ID) to restore into this one once it runs
        #[arg(long = "from-backup")]
        from_backup: Option<String>,
        /// The environment (default: the linked one)
        #[arg(short, long)]
        environment: Option<String>,
        #[arg(long)]
        slug: Option<String>,
        /// The server to run on (default: the control plane's)
        #[arg(long = "server-id")]
        server_id: Option<String>,
        /// Don't wait for its first deployment
        #[arg(short, long)]
        detach: bool,
    },
    /// Back a database up now
    Backup {
        database: Option<String>,
        /// The storage destination, by name or ID (default: the only one)
        #[arg(long)]
        destination: Option<String>,
        #[arg(short, long)]
        detach: bool,
    },
    /// List a database's backups, newest first
    Backups { database: Option<String> },
    /// Restore a backup into its database, after a safety backup of what is there now
    Restore {
        backup: String,
        #[arg(short, long)]
        yes: bool,
        #[arg(short, long)]
        detach: bool,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Role {
    Viewer,
    Developer,
    Admin,
}

#[derive(Subcommand, Debug)]
pub enum TokenCommand {
    /// List your API tokens
    List,
    /// Create an API token; it is printed once
    Create {
        name: String,
        #[arg(long, value_enum, default_value = "developer")]
        role: Role,
        /// Only for this project
        #[arg(long, conflicts_with = "environment")]
        project: Option<String>,
        /// Only for this environment
        #[arg(long)]
        environment: Option<String>,
        /// Days until it expires, or `never`
        #[arg(long, default_value = "90")]
        expires: String,
    },
    /// Revoke an API token
    Revoke { id: String },
}
