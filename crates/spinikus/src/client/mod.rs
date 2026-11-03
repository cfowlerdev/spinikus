//! The commands that talk to a Spinikus instance through its API, as any client could: the
//! CLI has no other way in. It signs in for an API token (`login`), keeps named contexts
//! (`contexts.rs`), and finds what a directory deploys from its link file (`link.rs`).

mod access;
mod api;
pub mod args;
mod contexts;
mod deploy;
mod dotenv;
mod error;
mod link;
mod output;
mod resources;
mod shell;
mod storage;
mod target;
mod variables;
mod views;

use std::path::PathBuf;

pub use api::Api;
pub use contexts::CONFIG_DIR_VAR;
pub use error::{CliError, Exit, exit_status, is_silent};

use args::Global;
use contexts::{ConfigDir, Contexts};
use error::fail;
use link::Linked;
use output::Output;

use crate::cli::Commands;

/// Runs a client command. `None` if `command` is not one (it runs the server or the agent).
pub async fn run(command: Commands, global: Global) -> Option<anyhow::Result<()>> {
    let out = Output::new(global.output, global.quiet, global.no_color);
    let result = match command {
        Commands::Server(_) | Commands::Admin(_) | Commands::Agent(_) => return None,
        Commands::Login(args) => access::login(&global, out, args).await,
        Commands::Logout => access::logout(&global, out).await,
        Commands::Whoami => match Session::open(&global, out) {
            Ok(session) => access::whoami(&session).await,
            Err(error) => Err(error),
        },
        Commands::Context(command) => access::context(out, command),
        Commands::Link(args) => match Session::open(&global, out) {
            Ok(session) => access::link(&session, args).await,
            Err(error) => Err(error),
        },
        command => match Session::open(&global, out) {
            Ok(session) => run_with(&session, command).await,
            Err(error) => Err(error),
        },
    };
    Some(result)
}

async fn run_with(session: &Session, command: Commands) -> anyhow::Result<()> {
    match command {
        Commands::Project(command) => resources::project(session, command).await,
        Commands::Environment(command) => resources::environment(session, command).await,
        Commands::Service(command) => resources::service(session, command).await,
        Commands::Deploy(args) => deploy::deploy(session, args).await,
        Commands::Rollback(args) => deploy::rollback(session, args).await,
        Commands::Restart(args) => deploy::restart(session, args).await,
        Commands::Deployment(command) => deploy::deployment(session, command).await,
        Commands::Logs(args) => deploy::logs(session, args).await,
        Commands::Shell(args) => shell::shell(session, args).await,
        Commands::Env(command) => variables::env(session, command).await,
        Commands::Domain(command) => resources::domain(session, command).await,
        Commands::Volume(command) => resources::volume(session, command).await,
        Commands::Db(command) => storage::db(session, command).await,
        Commands::Token(command) => access::token(session, command).await,
        _ => unreachable!("handled by `run`"),
    }
}

/// A signed-in connection to an instance, with the directory's link if it has one.
pub struct Session {
    pub api: Api,
    pub out: Output,
    linked: Option<Linked>,
    /// The context in use, if any (a token from the environment needs none).
    pub context: Option<String>,
}

/// Which instance a command talks to, and with which token.
struct Connection {
    server: String,
    context: Option<String>,
    token: Option<String>,
}

impl Session {
    fn open(global: &Global, out: Output) -> anyhow::Result<Self> {
        let config = ConfigDir::locate()?;
        let contexts = config.contexts()?;
        let linked = link::find(&std::env::current_dir()?)?;
        let connection = connection(global, &config, &contexts, linked.as_ref())?;
        if connection.token.is_none() {
            let server = &connection.server;
            return Err(fail(Exit::Unauthenticated, format!("not signed in to {server}; run `spinikus login {server}`")));
        }
        Ok(Self { api: Api::new(&connection.server, connection.token)?, out, linked, context: connection.context })
    }

    /// The directory's link, if it is for this instance. A link to another instance is an
    /// error when one is needed, rather than IDs looked up in the wrong place.
    pub fn link(&self) -> anyhow::Result<Option<&link::Link>> {
        match &self.linked {
            Some(linked) if linked.link.server == self.api.server() => Ok(Some(&linked.link)),
            Some(linked) => Err(fail(
                Exit::Failure,
                format!(
                    "{} is linked to {}, but this command uses {}; pass --server {} or run `spinikus link` again",
                    linked.dir.display(),
                    linked.link.server,
                    self.api.server(),
                    linked.link.server,
                ),
            )),
            None => Ok(None),
        }
    }

    pub fn linked_dir(&self) -> Option<PathBuf> {
        self.linked.as_ref().map(|linked| linked.dir.clone())
    }
}

/// `--context`, else `--server` (with a context for it, if any), else the context for the
/// directory's link, else the current context.
fn connection(global: &Global, config: &ConfigDir, contexts: &Contexts, linked: Option<&Linked>) -> anyhow::Result<Connection> {
    let named = |name: &str| -> anyhow::Result<Connection> {
        let context = contexts
            .contexts
            .get(name)
            .ok_or_else(|| fail(Exit::Failure, format!("there is no context `{name}`; `spinikus context list` shows them")))?;
        let token = match &global.token {
            Some(token) => Some(token.clone()),
            None => config.token(name)?,
        };
        Ok(Connection { server: context.server.clone(), context: Some(name.to_string()), token })
    };
    let for_server = |server: String| -> anyhow::Result<Connection> {
        match (&global.token, contexts.for_server(&server)) {
            (Some(token), context) => Ok(Connection { server, context: context.map(str::to_string), token: Some(token.clone()) }),
            (None, Some(name)) => named(name),
            (None, None) => Ok(Connection { server, context: None, token: None }),
        }
    };
    if let Some(name) = &global.context {
        return named(name);
    }
    if let Some(server) = &global.server {
        return for_server(api::server_name(&api::parse_server(server)?));
    }
    if let Some(linked) = linked {
        return for_server(linked.link.server.clone());
    }
    match &contexts.current {
        Some(name) => named(name),
        None => Err(fail(Exit::Unauthenticated, "not signed in; run `spinikus login <server>`")),
    }
}

/// Asks for confirmation on a terminal; refuses without one unless `yes`.
pub fn confirm(yes: bool, question: &str) -> anyhow::Result<()> {
    use std::io::{BufRead, IsTerminal, Write};
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        return Err(fail(Exit::Failure, format!("{question} Pass --yes to confirm without a terminal.")));
    }
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    match answer.trim().to_lowercase().as_str() {
        "y" | "yes" => Ok(()),
        _ => Err(fail(Exit::Failure, "canceled")),
    }
}
