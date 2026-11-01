//! Creating a database: a service running the engine's official image with a data volume
//! and generated credentials, deployed right away. Developers of the environment create them.

use axum::{
    Form,
    extract::{Path, State},
    http::HeaderMap,
    response::Response,
};
use maud::{Markup, html};
use serde::Deserialize;
use spinikus_control::{Control, ControlError, NewDatabase};
use spinikus_core::{database::Engine, environment::Environment, membership::Role, project::Project, service::Service};

use super::{crumbs, manage::Naming, parse_id, problem, redirect};
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

/// The databases to choose from, newest versions first: (`engine:version`, label).
fn choices() -> Vec<(String, String)> {
    Engine::ALL
        .into_iter()
        .flat_map(|engine| {
            engine.versions().iter().rev().map(move |version| {
                let newest = if *version == engine.default_version() { " (newest)" } else { "" };
                (format!("{}:{version}", engine.as_str()), format!("{} {version}{newest}", engine_name(engine)))
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

pub async fn new_database(State(control): State<Control>, signed: Signed, Path(id): Path<String>) -> Result<Markup, WebError> {
    let (environment, project) = environment_as_developer(&control, &signed, &id).await?;
    Ok(page(&signed, &project, &environment, &NewDatabaseForm::new(), Notice::None))
}

pub async fn create_database(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<NewDatabaseForm>,
) -> Result<Response, WebError> {
    let (environment, project) = environment_as_developer(&control, &signed, &id).await?;
    let created = match form.engine_and_version() {
        Ok((engine, version)) => {
            let new = NewDatabase {
                name: form.naming.name.clone(),
                slug: form.naming.slug(),
                engine,
                version: Some(version),
                server_id: None,
            };
            control.create_database(&signed.principal, environment.id, new).await
        }
        Err(error) => Err(error),
    };
    match created {
        Ok(created) => Ok(redirect(&headers, &format!("/services/{}", created.service.id))),
        Err(error) => problem(error, |message| page(&signed, &project, &environment, &form, Notice::Problem(message))),
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

fn page(signed: &Signed, project: &Project, environment: &Environment, form: &NewDatabaseForm, notice: Notice<'_>) -> Markup {
    let choices = choices();
    let options: Vec<(&str, &str)> = choices.iter().map(|(value, label)| (value.as_str(), label.as_str())).collect();
    let fields = html! {
        (form.naming.fields("database"))
        (forms::select("database-engine", "database", "Database", &options, &form.database))
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
