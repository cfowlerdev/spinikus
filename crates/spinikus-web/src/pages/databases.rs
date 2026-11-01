//! Creating a database: a service running the engine's official image with a data volume
//! and generated credentials, deployed right away. Developers of the environment create them.
//! With `?from_backup=` (from a backup's "Restore into a new database"), it starts with the
//! backup's data: only the backup's engine at its version or newer can be chosen.

use axum::{
    Form,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::Response,
};
use maud::{Markup, html};
use serde::Deserialize;
use spinikus_control::{Control, ControlError, NewDatabase};
use spinikus_core::{
    backup::Backup,
    database::{DatabaseSpec, Engine},
    environment::Environment,
    membership::Role,
    project::Project,
    service::Service,
};

use super::{crumbs, manage::Naming, parse_id, problem, redirect, servers};
use crate::{
    auth::Signed,
    error::WebError,
    forms::{self, Notice},
    layout::{self, CARD},
};

#[derive(Deserialize, Default)]
pub struct NewDatabaseForm {
    #[serde(flatten)]
    naming: Naming,
    /// `<engine>:<version>`, such as `postgres:18`.
    #[serde(default)]
    database: String,
    /// Empty for the control plane's server.
    #[serde(default)]
    server_id: String,
    /// A backup to start from; empty for none.
    #[serde(default)]
    from_backup: String,
}

#[derive(Deserialize, Default)]
pub struct FromBackupQuery {
    #[serde(default)]
    from_backup: Option<String>,
}

/// The backup a new database starts from, its database, and that database's spec.
struct Source {
    backup: Backup,
    database: Service,
    spec: DatabaseSpec,
}

impl Source {
    async fn load(control: &Control, signed: &Signed, id: &str) -> Result<Option<Self>, WebError> {
        if id.is_empty() {
            return Ok(None);
        }
        let backup = control.get_backup(&signed.principal, parse_id(id)?).await?;
        let database = control.get_service(&signed.principal, backup.service_id).await?;
        let spec = DatabaseSpec::of(&database.config).ok_or(WebError::NoPage)?;
        Ok(Some(Self { backup, database, spec }))
    }
}

impl NewDatabaseForm {
    fn new() -> Self {
        let database = format!("{}:{}", Engine::Postgres.as_str(), Engine::Postgres.default_version());
        Self { database, ..Default::default() }
    }

    fn engine_and_version(&self) -> Result<(Engine, String), ControlError> {
        let invalid =
            || ControlError::Invalid { field: "database", problem: format!("there is no database `{}`", self.database) };
        let (engine, version) = self.database.split_once(':').ok_or_else(invalid)?;
        Ok((engine.parse().map_err(|_| invalid())?, version.to_string()))
    }
}

/// The databases to choose from, newest versions first: (`engine:version`, label). Only
/// those a backup restores into, when starting from one.
fn choices(source: Option<&Source>) -> Vec<(String, String)> {
    Engine::ALL
        .into_iter()
        .flat_map(|engine| {
            engine.versions().iter().rev().filter_map(move |version| {
                let spec = DatabaseSpec::new(engine, Some(version)).expect("a known version");
                if source.is_some_and(|source| !source.spec.restores_into(&spec)) {
                    return None;
                }
                let newest = if *version == engine.default_version() { " (newest)" } else { "" };
                Some((format!("{}:{version}", engine.as_str()), format!("{} {version}{newest}", engine_name(engine))))
            })
        })
        .collect()
}

pub fn engine_name(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => "PostgreSQL",
        Engine::Redis => "Redis",
    }
}

pub async fn new_database(
    State(control): State<Control>,
    signed: Signed,
    Path(id): Path<String>,
    Query(query): Query<FromBackupQuery>,
) -> Result<Markup, WebError> {
    let (environment, project) = environment_as_developer(&control, &signed, &id).await?;
    let servers = servers::choices(&control, &signed, None).await?;
    let source = Source::load(&control, &signed, query.from_backup.as_deref().unwrap_or_default()).await?;
    let mut form = NewDatabaseForm::new();
    if let Some(source) = &source {
        form.database = format!("{}:{}", source.spec.engine().as_str(), source.spec.version());
        form.from_backup = source.backup.id.to_string();
    }
    Ok(page(&signed, &project, &environment, &servers, source.as_ref(), &form, Notice::None))
}

pub async fn create_database(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<NewDatabaseForm>,
) -> Result<Response, WebError> {
    let (environment, project) = environment_as_developer(&control, &signed, &id).await?;
    let servers = servers::choices(&control, &signed, servers::parse_choice(&form.server_id).ok().flatten()).await?;
    let source = Source::load(&control, &signed, &form.from_backup).await?;
    let created = match (form.engine_and_version(), servers::parse_choice(&form.server_id)) {
        (Ok((engine, version)), Ok(server_id)) => {
            let new = NewDatabase {
                name: form.naming.name.clone(),
                slug: form.naming.slug(),
                engine: Some(engine),
                version: Some(version),
                server_id,
                from_backup: source.as_ref().map(|source| source.backup.id),
            };
            control.create_database(&signed.principal, environment.id, new).await
        }
        (Err(error), _) | (_, Err(error)) => Err(error),
    };
    match created {
        // Its Backups tab shows the restore.
        Ok(created) if created.restore.is_some() => Ok(redirect(&headers, &format!("/services/{}/backups", created.service.id))),
        Ok(created) => Ok(redirect(&headers, &format!("/services/{}", created.service.id))),
        Err(error) => problem(error, |message| {
            page(&signed, &project, &environment, &servers, source.as_ref(), &form, Notice::Problem(message))
        }),
    }
}

async fn environment_as_developer(control: &Control, signed: &Signed, id: &str) -> Result<(Environment, Project), WebError> {
    let environment = control.get_environment(&signed.principal, parse_id(id)?).await?;
    if control.environment_role(&signed.principal, &environment).await? < Role::Developer {
        return Err(ControlError::Forbidden.into());
    }
    let project = control.get_project(&signed.principal, environment.project_id).await?;
    Ok((environment, project))
}

fn page(
    signed: &Signed,
    project: &Project,
    environment: &Environment,
    servers: &Option<Vec<(String, String)>>,
    source: Option<&Source>,
    form: &NewDatabaseForm,
    notice: Notice<'_>,
) -> Markup {
    let choices = choices(source);
    let options: Vec<(&str, &str)> = choices.iter().map(|(value, label)| (value.as_str(), label.as_str())).collect();
    let fields = html! {
        @if let Some(source) = source {
            p class="text-sm text-zinc-600 dark:text-zinc-400" {
                "Starts with the data of " a href={ "/services/" (source.database.id) "/backups" } class=(layout::LINK) { (source.database.name) }
                " from its backup of " (layout::time(source.backup.created_at))
                ", restored once the new database is running. " (source.database.name) " itself is not changed."
            }
            input type="hidden" name="from_backup" value=(source.backup.id);
        }
        (form.naming.fields("database"))
        (forms::select("database-engine", "database", "Database", &options, &form.database))
        @if let Some(servers) = servers { (servers::select("database-server", servers, &form.server_id)) }
    };
    let action = format!("/environments/{}/databases", environment.id);
    let description = "Runs the official image with its data on a volume, and generated credentials in its variables. \
                       Other services in the environment reach it by its slug.";
    let content = html! {
        (layout::heading("New database", None))
        div class="max-w-xl" { (forms::card(signed, "Database", Some(description), &action, notice, fields, "Create database")) }
    };
    layout::page(signed, "New database", &crumbs(project, Some(environment)), content)
}

/// How other services reach a database: its host and port on the environment's network, and
/// the variable to refer to for its URL.
pub fn connection(service: &Service, engine: Engine) -> Markup {
    let url = match engine {
        Engine::Postgres => "DATABASE_URL",
        Engine::Redis => "REDIS_URL",
    };
    let reference = format!("${{{{ {}.{url} }}}}", service.slug);
    html! {
        section class="mb-8" {
            h2 class="mb-3 font-semibold" { "Connecting" }
            div class={ (CARD) " space-y-3 p-5 text-sm" } {
                p {
                    (engine_name(engine)) " at "
                    code class="font-mono" { (service.slug) ":" (engine.port()) }
                    ", for the services of this environment only."
                }
                p {
                    "In another service's variables, set " code class="font-mono" { (url) } " to "
                    code class="rounded bg-zinc-100 px-1.5 py-0.5 font-mono select-all dark:bg-zinc-800" { (reference) }
                    " to get its URL with the password."
                }
            }
        }
    }
}
