//! Creating a service with its source, and its settings: name, source, runtime, and
//! deleting it. Developers create and edit services; admins delete them. Changes apply at the
//! next deployment.

use axum::{
    Form,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use serde::Deserialize;
use serde_json::{Map, Value};
use spinikus_control::{Control, ControlError, NewService, ServiceChanges};
use spinikus_core::{
    environment::Environment,
    id::ServiceId,
    membership::Role,
    project::Project,
    service::{Service, ServiceConfig, ServiceKind},
    source::{DeployKey, GitSource, ImageSource, Source, SourceSpec},
};

use super::{Tab, crumbs, manage::Naming, parse_id, problem, redirect, service_tabs};
use crate::{
    auth::Signed,
    error::WebError,
    forms::{self, Input, Notice},
    layout::{self, CARD, LABEL},
};

/// The source part of a form: an image, or a Git repository.
#[derive(Deserialize, Default)]
pub struct SourceForm {
    #[serde(default)]
    source_revision: Option<i64>,
    #[serde(default)]
    source_type: String,
    #[serde(default)]
    image: String,
    #[serde(default)]
    repo_url: String,
    #[serde(default)]
    branch: String,
    #[serde(default)]
    root_directory: String,
}

impl SourceForm {
    fn new() -> Self {
        Self { source_type: "image".to_string(), branch: "main".to_string(), ..Default::default() }
    }

    fn of(source: Option<&Source>) -> Self {
        let Some(source) = source else {
            return Self::new();
        };
        let mut form = match &source.spec {
            SourceSpec::Image(image) => Self { image: image.image.clone(), ..Self::new() },
            SourceSpec::Git(git) => Self {
                source_type: "git".to_string(),
                repo_url: git.repo_url.clone(),
                branch: git.branch.clone(),
                root_directory: git.root_directory.clone().unwrap_or_default(),
                ..Self::new()
            },
        };
        form.source_revision = Some(source.revision);
        form
    }

    fn spec(&self) -> SourceSpec {
        match self.source_type.as_str() {
            "git" => SourceSpec::Git(GitSource {
                repo_url: self.repo_url.trim().to_string(),
                branch: self.branch.trim().to_string(),
                root_directory: Some(self.root_directory.trim().to_string()).filter(|root| !root.is_empty()),
            }),
            _ => SourceSpec::Image(ImageSource { image: self.image.trim().to_string() }),
        }
    }

    /// The fields; the radio buttons show one group or the other with CSS alone (the ids are
    /// fixed, so a page has one source form).
    fn fields(&self) -> Markup {
        let git = self.source_type == "git";
        html! {
            div class="group space-y-4" {
                fieldset {
                    legend class=(LABEL) { "Deploy from" }
                    div class="mt-2 flex gap-6 text-sm" {
                        label class="flex items-center gap-2" {
                            input id="source-type-image" type="radio" name="source_type" value="image" checked[!git];
                            "A prebuilt image"
                        }
                        label class="flex items-center gap-2" {
                            input id="source-type-git" type="radio" name="source_type" value="git" checked[git];
                            "A Git repository"
                        }
                    }
                }
                div class="space-y-4 group-has-[#source-type-git:checked]:hidden" {
                    (Input::text("source-image", "image", "Image", &self.image).mono().placeholder("ghcr.io/acme/web:1.4").render())
                }
                div class="hidden space-y-4 group-has-[#source-type-git:checked]:block" {
                    (Input::text("source-repo", "repo_url", "Repository", &self.repo_url).mono()
                        .placeholder("https://github.com/acme/web.git").hint("HTTPS, or SSH (git@github.com:acme/web.git) with a deploy key.").render())
                    (Input::text("source-branch", "branch", "Branch", &self.branch).mono().render())
                    (Input::text("source-root", "root_directory", "Root directory", &self.root_directory).mono()
                        .hint("The directory to build from, for monorepos. The repository's root if empty.").render())
                }
                @if let Some(revision) = self.source_revision { input type="hidden" name="source_revision" value=(revision); }
            }
        }
    }
}

/// The runtime settings a form edits; the service config's `runtime` object keeps anything
/// else it has.
#[derive(Deserialize, Default)]
pub struct RuntimeForm {
    #[serde(default)]
    revision: Option<i64>,
    #[serde(default)]
    port: String,
    #[serde(default)]
    start: String,
    #[serde(default)]
    health_path: String,
    #[serde(default)]
    start_period: String,
    #[serde(default)]
    shutdown_timeout: String,
}

/// What the repository's `spinikus.json` set in the newest build: the config fields
/// (`runtime.port`, …) and the config with the file's values.
struct FromFile {
    fields: Vec<String>,
    config: ServiceConfig,
}

impl FromFile {
    fn sets(file: Option<&Self>, field: &str) -> bool {
        file.is_some_and(|file| file.fields.iter().any(|set| set == field))
    }
}

/// The runtime fields a form edits, as `spinikus.json` names them.
const PORT: &str = "runtime.port";
const START: &str = "runtime.start";
const SHUTDOWN_TIMEOUT: &str = "runtime.shutdown_timeout";
const HEALTHCHECK: &str = "runtime.healthcheck";

impl RuntimeForm {
    /// The service's settings, with the file's values for the fields it sets.
    fn of(service: &Service, file: Option<&FromFile>) -> Self {
        let mut shown = service.config.get("runtime").and_then(Value::as_object).cloned().unwrap_or_default();
        if let Some(from) = file.and_then(|file| file.config.get("runtime")).and_then(Value::as_object) {
            for field in [PORT, START, SHUTDOWN_TIMEOUT, HEALTHCHECK] {
                if FromFile::sets(file, field) {
                    let key = &field["runtime.".len()..];
                    set(&mut shown, key, from.get(key).cloned());
                }
            }
        }
        let runtime = Some(&shown);
        let text = |value: Option<&Value>| match value {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Number(number)) => number.to_string(),
            _ => String::new(),
        };
        let check = runtime.and_then(|runtime| runtime.get("healthcheck")).and_then(Value::as_object);
        Self {
            revision: Some(service.revision),
            port: text(runtime.and_then(|runtime| runtime.get("port"))),
            start: text(runtime.and_then(|runtime| runtime.get("start"))),
            health_path: text(check.and_then(|check| check.get("path"))),
            start_period: text(check.and_then(|check| check.get("start_period"))),
            shutdown_timeout: text(runtime.and_then(|runtime| runtime.get("shutdown_timeout"))),
        }
    }

    /// `config` with these settings, except those the file sets, which keep their stored
    /// values; the control layer checks the result.
    fn apply(&self, config: &ServiceConfig, file: Option<&FromFile>) -> Result<ServiceConfig, String> {
        let mut config = config.clone();
        let mut runtime = config.get("runtime").and_then(Value::as_object).cloned().unwrap_or_default();
        if !FromFile::sets(file, PORT) {
            let port = match self.port.trim() {
                "" => None,
                port => {
                    Some(port.parse::<u16>().ok().filter(|port| *port > 0).ok_or("The port must be a number from 1 to 65535.")?)
                }
            };
            set(&mut runtime, "port", port.map(Value::from));
        }
        if !FromFile::sets(file, START) {
            set(&mut runtime, "start", text(&self.start));
        }
        if !FromFile::sets(file, SHUTDOWN_TIMEOUT) {
            set(&mut runtime, "shutdown_timeout", text(&self.shutdown_timeout));
        }
        if !FromFile::sets(file, HEALTHCHECK) {
            let mut check = runtime.get("healthcheck").and_then(Value::as_object).cloned().unwrap_or_default();
            set(&mut check, "path", text(&self.health_path));
            set(&mut check, "start_period", text(&self.start_period));
            let check = Some(Value::Object(check)).filter(|check| check.as_object().is_some_and(|check| !check.is_empty()));
            set(&mut runtime, "healthcheck", check);
        }
        match runtime.is_empty() {
            true => config.remove("runtime"),
            false => config.insert("runtime".to_string(), Value::Object(runtime)),
        };
        Ok(config)
    }

    fn fields(&self, file: Option<&FromFile>) -> Markup {
        let locked = |field| FromFile::sets(file, field);
        let hint = |field, hint| if locked(field) { FILE_SETS_IT } else { hint };
        html! {
            div class="grid gap-4 sm:grid-cols-2" {
                (Input::text("runtime-port", "port", "Port", &self.port).kind("number").mono().readonly(locked(PORT))
                    .hint(hint(PORT, "The port the app listens on. Without one it gets no traffic.")).render())
                (Input::text("runtime-shutdown", "shutdown_timeout", "Shutdown timeout", &self.shutdown_timeout).mono().placeholder("30s")
                    .readonly(locked(SHUTDOWN_TIMEOUT)).hint(hint(SHUTDOWN_TIMEOUT, "How long a replaced container gets to finish requests.")).render())
            }
            (Input::text("runtime-start", "start", "Start command", &self.start).mono().readonly(locked(START))
                .hint(hint(START, "Run with /bin/sh -c instead of the image's own command. The image's command if empty.")).render())
            div class="grid gap-4 sm:grid-cols-2" {
                (Input::text("runtime-health", "health_path", "Health check path", &self.health_path).mono().placeholder("/healthz").readonly(locked(HEALTHCHECK))
                    .hint(hint(HEALTHCHECK, "Must answer 2xx or 3xx before traffic switches. Without one, the container only has to keep running.")).render())
                (Input::text("runtime-start-period", "start_period", "Health check start period", &self.start_period).mono().placeholder("30s")
                    .readonly(locked(HEALTHCHECK)).hint(hint(HEALTHCHECK, "Failures in this time after start don't count.")).render())
            }
            @if let Some(revision) = self.revision { input type="hidden" name="revision" value=(revision); }
        }
    }
}

const FILE_SETS_IT: &str = "Set by spinikus.json in the repository, as of the last build. Change it there.";

fn text(value: &str) -> Option<Value> {
    Some(value.trim()).filter(|value| !value.is_empty()).map(Value::from)
}

fn set(object: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    match value {
        Some(value) => object.insert(key.to_string(), value),
        None => object.remove(key),
    };
}

// Creating a service.

#[derive(Deserialize, Default)]
pub struct NewServiceForm {
    #[serde(flatten)]
    naming: Naming,
    #[serde(flatten)]
    source: SourceForm,
    #[serde(default)]
    port: String,
}

pub async fn new_service(State(control): State<Control>, signed: Signed, Path(id): Path<String>) -> Result<Markup, WebError> {
    let (environment, project) = environment_as_developer(&control, &signed, &id).await?;
    let form = NewServiceForm { source: SourceForm::new(), ..Default::default() };
    Ok(new_service_page(&signed, &project, &environment, &form, Notice::None))
}

pub async fn create_service(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<NewServiceForm>,
) -> Result<Response, WebError> {
    let (environment, project) = environment_as_developer(&control, &signed, &id).await?;
    let runtime = RuntimeForm { port: form.port.clone(), ..Default::default() };
    let config = match runtime.apply(&ServiceConfig::new(), None) {
        Ok(config) => config,
        Err(message) => {
            let page = new_service_page(&signed, &project, &environment, &form, Notice::Problem(&message));
            return Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
        }
    };
    let new =
        NewService { name: form.naming.name.clone(), slug: form.naming.slug(), kind: ServiceKind::App, config, server_id: None };
    match control.create_service_with_source(&signed.principal, environment.id, new, form.source.spec()).await {
        Ok(service) => Ok(redirect(&headers, &format!("/services/{}", service.id))),
        Err(error) => {
            problem(error, |message| new_service_page(&signed, &project, &environment, &form, Notice::Problem(message)))
        }
    }
}

async fn environment_as_developer(control: &Control, signed: &Signed, id: &str) -> Result<(Environment, Project), ControlError> {
    let environment = control.get_environment(&signed.principal, parse_id(id).map_err(|_| not_found())?).await?;
    if control.environment_role(&signed.principal, &environment).await? < Role::Developer {
        return Err(ControlError::Forbidden);
    }
    let project = control.get_project(&signed.principal, environment.project_id).await?;
    Ok((environment, project))
}

fn not_found() -> ControlError {
    ControlError::NotFound { entity: "page", id: String::new() }
}

fn new_service_page(
    signed: &Signed,
    project: &Project,
    environment: &Environment,
    form: &NewServiceForm,
    notice: Notice<'_>,
) -> Markup {
    let fields = html! {
        (form.naming.fields("service"))
        (form.source.fields())
        (Input::text("service-port", "port", "Port", &form.port).kind("number").mono()
            .hint("The port the app listens on. Leave it empty for a worker that gets no traffic.").render())
    };
    let action = format!("/environments/{}/services", environment.id);
    let content = html! {
        (layout::heading("New service", None))
        div class="max-w-xl" {
            (forms::card(signed, "Service", Some("An app built from a Git repository or run from an image. You can deploy it once it's created."), &action, notice, fields, "Create service"))
        }
    };
    layout::page(signed, "New service", &crumbs(project, Some(environment)), content)
}

// The settings page.

/// The service and what its settings page shows around the forms.
struct Context {
    service: Service,
    environment: Environment,
    project: Project,
    role: Role,
    source: Option<Source>,
    deploy_key: Option<DeployKey>,
    file: Option<FromFile>,
}

async fn context(control: &Control, signed: &Signed, id: ServiceId) -> Result<Context, ControlError> {
    let principal = &signed.principal;
    let service = control.get_service(principal, id).await?;
    let environment = control.get_environment(principal, service.environment_id).await?;
    let role = control.environment_role(principal, &environment).await?;
    if role < Role::Developer {
        return Err(ControlError::Forbidden);
    }
    let project = control.get_project(principal, environment.project_id).await?;
    let source = optional(control.get_source(principal, id).await)?;
    let ssh = matches!(&source, Some(Source { spec: SourceSpec::Git(git), .. }) if git.uses_ssh());
    let deploy_key = match ssh {
        true => optional(control.get_deploy_key(principal, id).await)?,
        false => None,
    };
    let file = match &source {
        Some(Source { spec: SourceSpec::Git(_), .. }) => from_file(control, signed, id).await?,
        _ => None,
    };
    Ok(Context { service, environment, project, role, source, deploy_key, file })
}

/// How many deployments to look through for the newest build.
const BUILDS_SEARCHED: u32 = 20;

/// What `spinikus.json` set in the newest deployment that read it: one that has an image and
/// came from Git.
async fn from_file(control: &Control, signed: &Signed, id: ServiceId) -> Result<Option<FromFile>, ControlError> {
    let deployments = control.list_deployments(&signed.principal, id, None, BUILDS_SEARCHED).await?;
    let built = deployments
        .items
        .into_iter()
        .find(|deployment| deployment.image.is_some() && matches!(deployment.snapshot.source, SourceSpec::Git(_)));
    Ok(built.map(|deployment| FromFile { fields: deployment.snapshot.from_file, config: deployment.snapshot.config }))
}

fn optional<T>(result: Result<T, ControlError>) -> Result<Option<T>, ControlError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(ControlError::NotFound { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Which section was just saved, from `?saved=`.
#[derive(Deserialize)]
pub struct Saved {
    saved: Option<String>,
}

/// The forms of the settings page, with what to say above each.
struct Sections<'a> {
    /// A problem with the page as a whole, such as a refused delete.
    alert: Option<&'a str>,
    general: (Naming, Notice<'a>),
    source: (SourceForm, Notice<'a>),
    runtime: (RuntimeForm, Notice<'a>),
}

impl<'a> Sections<'a> {
    fn of(context: &Context, saved: Option<&str>) -> Self {
        let notice = |section: &str| if saved == Some(section) { Notice::Saved } else { Notice::None };
        let service = &context.service;
        let general = Naming { revision: Some(service.revision), name: service.name.clone(), slug: service.slug.to_string() };
        Self {
            alert: None,
            general: (general, notice("general")),
            source: (SourceForm::of(context.source.as_ref()), notice("source")),
            runtime: (RuntimeForm::of(service, context.file.as_ref()), notice("runtime")),
        }
    }
}

pub async fn settings(
    State(control): State<Control>,
    signed: Signed,
    Path(id): Path<String>,
    Query(Saved { saved }): Query<Saved>,
) -> Result<Markup, WebError> {
    let context = context(&control, &signed, parse_id(&id)?).await?;
    let sections = Sections::of(&context, saved.as_deref());
    Ok(settings_page(&signed, &context, sections))
}

pub async fn update_general(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<Naming>,
) -> Result<Response, WebError> {
    let context = context(&control, &signed, parse_id(&id)?).await?;
    let changes = ServiceChanges { name: Some(form.name.clone()), slug: form.slug(), ..Default::default() };
    match control.update_service(&signed.principal, context.service.id, form.revision.unwrap_or(-1), changes).await {
        Ok(service) => Ok(redirect(&headers, &format!("/services/{}/settings?saved=general", service.id))),
        Err(error) => problem(error, |message| {
            let sections = Sections { general: (form, Notice::Problem(message)), ..Sections::of(&context, None) };
            settings_page(&signed, &context, sections)
        }),
    }
}

pub async fn update_source(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<SourceForm>,
) -> Result<Response, WebError> {
    let context = context(&control, &signed, parse_id(&id)?).await?;
    let (principal, id) = (&signed.principal, context.service.id);
    let saved = match (&context.source, form.source_revision) {
        (None, _) => control.create_source(principal, id, form.spec()).await,
        (Some(_), revision) => control.replace_source(principal, id, revision.unwrap_or(-1), form.spec()).await,
    };
    match saved {
        Ok(_) => Ok(redirect(&headers, &format!("/services/{id}/settings?saved=source"))),
        Err(error) => problem(error, |message| {
            let sections = Sections { source: (form, Notice::Problem(message)), ..Sections::of(&context, None) };
            settings_page(&signed, &context, sections)
        }),
    }
}

pub async fn update_runtime(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<RuntimeForm>,
) -> Result<Response, WebError> {
    let context = context(&control, &signed, parse_id(&id)?).await?;
    let config = match form.apply(&context.service.config, context.file.as_ref()) {
        Ok(config) => config,
        Err(message) => {
            let sections = Sections { runtime: (form, Notice::Problem(&message)), ..Sections::of(&context, None) };
            return Ok((StatusCode::UNPROCESSABLE_ENTITY, settings_page(&signed, &context, sections)).into_response());
        }
    };
    let changes = ServiceChanges { config: Some(config), ..Default::default() };
    match control.update_service(&signed.principal, context.service.id, form.revision.unwrap_or(-1), changes).await {
        Ok(service) => Ok(redirect(&headers, &format!("/services/{}/settings?saved=runtime", service.id))),
        Err(error) => problem(error, |message| {
            let sections = Sections { runtime: (form, Notice::Problem(message)), ..Sections::of(&context, None) };
            settings_page(&signed, &context, sections)
        }),
    }
}

pub async fn delete(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, WebError> {
    let context = context(&control, &signed, parse_id(&id)?).await?;
    match control.delete_service(&signed.principal, context.service.id).await {
        Ok(()) => Ok(redirect(&headers, &format!("/environments/{}", context.environment.id))),
        Err(ControlError::HasChildren { .. }) => {
            let sections = Sections { alert: Some("Remove the service's domains first."), ..Sections::of(&context, None) };
            Ok((StatusCode::UNPROCESSABLE_ENTITY, settings_page(&signed, &context, sections)).into_response())
        }
        Err(error) => problem(error, |message| {
            settings_page(&signed, &context, Sections { alert: Some(message), ..Sections::of(&context, None) })
        }),
    }
}

fn settings_page(signed: &Signed, context: &Context, sections: Sections<'_>) -> Markup {
    let service = &context.service;
    let base = format!("/services/{}/settings", service.id);
    let content = html! {
        @if let Some(alert) = sections.alert { (layout::alert(alert)) }
        (layout::heading(&service.name, Some(html! { span class="font-mono" { (service.slug) } })))
        (service_tabs(service, context.role, Tab::Settings))
        p class="mb-6 text-sm text-zinc-500 dark:text-zinc-400" { "Changes apply at the next deployment." }
        div class="max-w-2xl space-y-6" {
            (forms::card(signed, "General", None, &format!("{base}/general"), sections.general.1, sections.general.0.fields("service"), "Save"))
            (forms::card(signed, "Source", Some("Where deployments get the app from."), &format!("{base}/source"), sections.source.1, sections.source.0.fields(), "Save source"))
            @if let Some(key) = &context.deploy_key {
                section class={ (CARD) " p-6" } {
                    h2 class="font-semibold" { "Deploy key" }
                    p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400" { "Add this public key to the repository, read-only, so Spinikus can clone it over SSH." }
                    pre class="mt-4 whitespace-pre-wrap break-all rounded-md bg-zinc-100 p-3 font-mono text-xs select-all dark:bg-zinc-800" { (key.public_key) }
                    p class="mt-2 font-mono text-xs text-zinc-500 dark:text-zinc-400" { (key.fingerprint) }
                }
            }
            (forms::card(signed, "Runtime", Some("How the app runs. Settings that the repository's spinikus.json sets can only be changed there."), &format!("{base}/runtime"), sections.runtime.1, sections.runtime.0.fields(context.file.as_ref()), "Save runtime"))
            @if context.role >= Role::Admin {
                (forms::danger(signed, "Delete service", "Only a service without domains can be deleted. Remove them on the Domains tab first.",
                    &format!("/services/{}/delete", service.id), "Delete service", "Delete this service?"))
            }
        }
    };
    layout::page(signed, &format!("Settings · {}", service.name), &crumbs(&context.project, Some(&context.environment)), content)
}
