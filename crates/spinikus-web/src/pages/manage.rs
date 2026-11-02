//! Creating, editing and deleting projects and environments. Instance admins create
//! projects; project admins edit and delete them, create and delete environments, and turn
//! previews of pull requests on or off; environment admins rename environments.

use axum::{
    Form,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use serde::Deserialize;
use spinikus_control::{Control, ControlError, EnvironmentChanges, NewEnvironment, NewProject, ProjectChanges};
use spinikus_core::{
    environment::{Environment, EnvironmentKind},
    id::{EnvironmentId, ProjectId},
    membership::Role,
    project::Project,
};

use super::{Tab, crumbs, environment_tabs, parse_id, problem, project_tabs, redirect};
use crate::{
    auth::Signed,
    error::WebError,
    forms::{self, Input, Notice},
    layout,
};

/// What a name-and-slug form posts.
#[derive(Deserialize, Default)]
pub struct Naming {
    #[serde(default)]
    pub(super) revision: Option<i64>,
    #[serde(default)]
    pub(super) name: String,
    #[serde(default)]
    pub(super) slug: String,
}

impl Naming {
    pub(super) fn slug(&self) -> Option<String> {
        Some(self.slug.trim().to_string()).filter(|slug| !slug.is_empty())
    }

    pub(super) fn fields(&self, prefix: &str) -> Markup {
        let (name_id, slug_id) = (format!("{prefix}-name"), format!("{prefix}-slug"));
        let hint = match self.revision {
            None => "Used in URLs and hostnames. Made from the name if empty.",
            Some(_) => "Used in URLs and hostnames; changing it changes them. Kept if empty.",
        };
        html! {
            (Input::text(&name_id, "name", "Name", &self.name).required().render())
            (Input::text(&slug_id, "slug", "Slug", &self.slug).mono().hint(hint).render())
            @if let Some(revision) = self.revision { input type="hidden" name="revision" value=(revision); }
        }
    }
}

// Projects.

pub async fn new_project(signed: Signed) -> Result<Markup, WebError> {
    if !signed.principal.is_instance_admin() {
        return Err(ControlError::Forbidden.into());
    }
    Ok(new_project_page(&signed, &Naming::default(), Notice::None))
}

pub async fn create_project(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Form(form): Form<Naming>,
) -> Result<Response, WebError> {
    let new = NewProject { name: form.name.clone(), slug: form.slug() };
    match control.create_project(&signed.principal, new).await {
        Ok(project) => Ok(redirect(&headers, &format!("/projects/{}", project.id))),
        Err(error) => problem(error, |message| new_project_page(&signed, &form, Notice::Problem(message))),
    }
}

fn new_project_page(signed: &Signed, form: &Naming, notice: Notice<'_>) -> Markup {
    let content = html! {
        (layout::heading("New project", None))
        div class="max-w-xl" { (forms::card(signed, "Project", Some("A project groups the environments and services of one app or product."), "/projects", notice, form.fields("project"), "Create project")) }
    };
    layout::page(signed, "New project", &[("Projects", "/".to_string())], content)
}

pub async fn project_settings(
    State(control): State<Control>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Markup, WebError> {
    let project = project_as_admin(&control, &signed, parse_id(&id)?).await?;
    let form = Naming { revision: Some(project.revision), name: project.name.clone(), slug: project.slug.to_string() };
    Ok(project_settings_page(&signed, &project, &form, Notice::None))
}

pub async fn update_project(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<Naming>,
) -> Result<Response, WebError> {
    let project = project_as_admin(&control, &signed, parse_id(&id)?).await?;
    let changes = ProjectChanges { name: Some(form.name.clone()), slug: form.slug() };
    match control.update_project(&signed.principal, project.id, form.revision.unwrap_or(-1), changes).await {
        Ok(project) => Ok(redirect(&headers, &format!("/projects/{}", project.id))),
        Err(error) => problem(error, |message| project_settings_page(&signed, &project, &form, Notice::Problem(message))),
    }
}

pub async fn delete_project(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, WebError> {
    let project = project_as_admin(&control, &signed, parse_id(&id)?).await?;
    match control.delete_project(&signed.principal, project.id).await {
        Ok(()) => Ok(redirect(&headers, "/")),
        Err(ControlError::HasChildren { .. }) => {
            let form = Naming { revision: Some(project.revision), name: project.name.clone(), slug: project.slug.to_string() };
            let message = "Delete the project's environments first.";
            Ok((StatusCode::UNPROCESSABLE_ENTITY, project_settings_page(&signed, &project, &form, Notice::Problem(message)))
                .into_response())
        }
        Err(error) => Err(error.into()),
    }
}

async fn project_as_admin(control: &Control, signed: &Signed, id: ProjectId) -> Result<Project, ControlError> {
    let project = control.get_project(&signed.principal, id).await?;
    match control.project_role(&signed.principal, &project).await? {
        Some(Role::Admin) => Ok(project),
        _ => Err(ControlError::Forbidden),
    }
}

fn project_settings_page(signed: &Signed, project: &Project, form: &Naming, notice: Notice<'_>) -> Markup {
    let action = format!("/projects/{}/settings", project.id);
    let content = html! {
        (layout::heading(&project.name, Some(html! { span class="font-mono" { (project.slug) } })))
        (project_tabs(project, Some(Role::Admin), Tab::Settings))
        div class="max-w-xl space-y-6" {
            (forms::card(signed, "General", None, &action, notice, form.fields("project"), "Save"))
            (forms::danger(signed, "Delete project", "Only a project without environments can be deleted.",
                &format!("/projects/{}/delete", project.id), "Delete project", "Delete this project?"))
        }
    };
    layout::page(signed, &format!("Settings · {}", project.name), &[("Projects", "/".to_string())], content)
}

// Environments.

#[derive(Deserialize, Default)]
pub struct NewEnvironmentForm {
    #[serde(flatten)]
    naming: Naming,
    #[serde(default)]
    kind: String,
}

pub async fn new_environment(State(control): State<Control>, signed: Signed, Path(id): Path<String>) -> Result<Markup, WebError> {
    let project = project_as_admin(&control, &signed, parse_id(&id)?).await?;
    let form = NewEnvironmentForm { kind: "production".to_string(), ..Default::default() };
    Ok(new_environment_page(&signed, &project, &form, Notice::None))
}

pub async fn create_environment(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<NewEnvironmentForm>,
) -> Result<Response, WebError> {
    let project = control.get_project(&signed.principal, parse_id(&id)?).await?;
    let kind = match form.kind.as_str() {
        "staging" => EnvironmentKind::Staging,
        _ => EnvironmentKind::Production,
    };
    let new = NewEnvironment { name: form.naming.name.clone(), slug: form.naming.slug(), kind, previews: false };
    match control.create_environment(&signed.principal, project.id, new).await {
        Ok(environment) => Ok(redirect(&headers, &format!("/environments/{}", environment.id))),
        Err(error) => problem(error, |message| new_environment_page(&signed, &project, &form, Notice::Problem(message))),
    }
}

fn new_environment_page(signed: &Signed, project: &Project, form: &NewEnvironmentForm, notice: Notice<'_>) -> Markup {
    let kinds = [("production", "Production"), ("staging", "Staging")];
    let fields = html! {
        (form.naming.fields("environment"))
        (forms::select("environment-kind", "kind", "Kind", &kinds, &form.kind))
    };
    let action = format!("/projects/{}/environments", project.id);
    let content = html! {
        (layout::heading("New environment", None))
        div class="max-w-xl" {
            (forms::card(signed, "Environment", Some("Each environment has its own services, variables and domains. A project has at most one production environment."), &action, notice, fields, "Create environment"))
        }
    };
    let crumbs = [("Projects", "/".to_string()), (project.name.as_str(), format!("/projects/{}", project.id))];
    layout::page(signed, "New environment", &crumbs, content)
}

/// Which section was just saved, from `?saved=`.
#[derive(Deserialize)]
pub struct Saved {
    saved: Option<String>,
}

pub async fn environment_settings(
    State(control): State<Control>,
    signed: Signed,
    Path(id): Path<String>,
    Query(Saved { saved }): Query<Saved>,
) -> Result<Markup, WebError> {
    let (environment, project, project_admin) = environment_as_admin(&control, &signed, parse_id(&id)?).await?;
    let base = base_environment(&control, &signed, &environment).await;
    let form = naming(&environment);
    let previews = if saved.as_deref() == Some("previews") { Notice::Saved } else { Notice::None };
    let notices = Notices { general: Notice::None, previews };
    Ok(environment_settings_page(&signed, &project, &environment, base.as_ref(), project_admin, &form, notices))
}

/// The base environment of a preview, if the principal may see it.
async fn base_environment(control: &Control, signed: &Signed, environment: &Environment) -> Option<Environment> {
    control.get_environment(&signed.principal, environment.base_environment_id?).await.ok()
}

fn naming(environment: &Environment) -> Naming {
    Naming { revision: Some(environment.revision), name: environment.name.clone(), slug: environment.slug.to_string() }
}

/// What a previews form posts.
#[derive(Deserialize)]
pub struct PreviewsForm {
    revision: i64,
    previews: Option<String>,
}

pub async fn update_previews(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<PreviewsForm>,
) -> Result<Response, WebError> {
    let (environment, project, project_admin) = environment_as_admin(&control, &signed, parse_id(&id)?).await?;
    let changes = EnvironmentChanges { previews: Some(form.previews.is_some()), ..Default::default() };
    match control.update_environment(&signed.principal, environment.id, form.revision, changes).await {
        Ok(environment) => Ok(redirect(&headers, &format!("/environments/{}/settings?saved=previews", environment.id))),
        Err(error) => problem(error, |message| {
            let notices = Notices { general: Notice::None, previews: Notice::Problem(message) };
            environment_settings_page(&signed, &project, &environment, None, project_admin, &naming(&environment), notices)
        }),
    }
}

pub async fn update_environment(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<Naming>,
) -> Result<Response, WebError> {
    let (environment, project, project_admin) = environment_as_admin(&control, &signed, parse_id(&id)?).await?;
    let changes = EnvironmentChanges { name: Some(form.name.clone()), slug: form.slug(), previews: None };
    match control.update_environment(&signed.principal, environment.id, form.revision.unwrap_or(-1), changes).await {
        Ok(environment) => Ok(redirect(&headers, &format!("/environments/{}", environment.id))),
        Err(error) => problem(error, |message| {
            let notices = Notices { general: Notice::Problem(message), previews: Notice::None };
            environment_settings_page(&signed, &project, &environment, None, project_admin, &form, notices)
        }),
    }
}

pub async fn delete_environment(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, WebError> {
    let (environment, project, project_admin) = environment_as_admin(&control, &signed, parse_id(&id)?).await?;
    match control.delete_environment(&signed.principal, environment.id).await {
        Ok(()) => Ok(redirect(&headers, &format!("/projects/{}", project.id))),
        Err(ControlError::HasChildren { .. }) => {
            let notices =
                Notices { general: Notice::Problem("Delete the environment's services first."), previews: Notice::None };
            let page =
                environment_settings_page(&signed, &project, &environment, None, project_admin, &naming(&environment), notices);
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
        Err(error) => Err(error.into()),
    }
}

/// The environment, its project, and whether the principal is the project's admin (who may
/// delete it), for an environment admin.
async fn environment_as_admin(
    control: &Control,
    signed: &Signed,
    id: EnvironmentId,
) -> Result<(Environment, Project, bool), ControlError> {
    let environment = control.get_environment(&signed.principal, id).await?;
    if control.environment_role(&signed.principal, &environment).await? != Role::Admin {
        return Err(ControlError::Forbidden);
    }
    let project = control.get_project(&signed.principal, environment.project_id).await?;
    let project_admin = control.project_role(&signed.principal, &project).await? == Some(Role::Admin);
    Ok((environment, project, project_admin))
}

/// What to say above each form of the environment's settings.
struct Notices<'a> {
    general: Notice<'a>,
    previews: Notice<'a>,
}

fn environment_settings_page(
    signed: &Signed,
    project: &Project,
    environment: &Environment,
    base: Option<&Environment>,
    project_admin: bool,
    form: &Naming,
    notices: Notices<'_>,
) -> Markup {
    let action = format!("/environments/{}/settings", environment.id);
    let pr_number = environment.kind.pr_number();
    let previews = html! {
        (forms::checkbox("environment-previews", "previews", "Deploy a preview of each pull request", environment.previews, false,
            Some("A pull request to the branch a Git service here deploys gets its own environment, pr-<number>, copied from this one with its own empty volumes, and new commits redeploy it. Closing the pull request deletes the preview and its data. The services' webhooks must send pull request events too; pull requests from forks get no preview.")))
        input type="hidden" name="revision" value=(environment.revision);
    };
    let content = html! {
        (layout::heading(&environment.name, Some(html! { span class="font-mono" { (environment.slug) } })))
        (environment_tabs(environment, Role::Admin, Tab::Settings))
        div class="max-w-xl space-y-6" {
            @if let Some(number) = pr_number {
                (layout::notice(&preview_note(number, base)))
            }
            (forms::card(signed, "General", None, &action, notices.general, form.fields("environment"), "Save"))
            @if project_admin && pr_number.is_none() {
                (forms::card(signed, "Previews", None, &format!("{action}/previews"), notices.previews, previews, "Save"))
            }
            @if project_admin {
                @if pr_number.is_some() {
                    (forms::danger(signed, "Delete preview", "Deletes the preview with its services and their data. The next commit to the pull request makes it again.",
                        &format!("/environments/{}/delete", environment.id), "Delete preview", "Delete this preview and its data?"))
                } @else {
                    (forms::danger(signed, "Delete environment", "Only an environment without services can be deleted.",
                        &format!("/environments/{}/delete", environment.id), "Delete environment", "Delete this environment?"))
                }
            }
        }
    };
    layout::page(signed, &format!("Settings · {}", environment.name), &crumbs(project, None), content)
}

/// What a preview is, for its pages.
pub(super) fn preview_note(number: u32, base: Option<&Environment>) -> String {
    match base {
        Some(base) => {
            format!("The preview of pull request #{number}, copied from {}. Closing the pull request deletes it.", base.name)
        }
        None => format!("The preview of pull request #{number}. Closing the pull request deletes it."),
    }
}
