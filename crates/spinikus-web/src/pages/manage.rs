//! Creating, editing and deleting projects and environments. Instance admins create
//! projects; project admins edit and delete them and create and delete environments;
//! environment admins rename environments.

use axum::{
    Form,
    extract::{Path, State},
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

use super::{crumbs, parse_id, problem, redirect};
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
        (layout::heading("Project settings", None))
        div class="max-w-xl space-y-6" {
            (forms::card(signed, "General", None, &action, notice, form.fields("project"), "Save"))
            (forms::danger(signed, "Delete project", "Only a project without environments can be deleted.",
                &format!("/projects/{}/delete", project.id), "Delete project", "Delete this project?"))
        }
    };
    let crumbs = [("Projects", "/".to_string()), (project.name.as_str(), format!("/projects/{}", project.id))];
    layout::page(signed, &format!("Settings · {}", project.name), &crumbs, content)
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
    let new = NewEnvironment { name: form.naming.name.clone(), slug: form.naming.slug(), kind };
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

pub async fn environment_settings(
    State(control): State<Control>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Markup, WebError> {
    let (environment, project, project_admin) = environment_as_admin(&control, &signed, parse_id(&id)?).await?;
    let form =
        Naming { revision: Some(environment.revision), name: environment.name.clone(), slug: environment.slug.to_string() };
    Ok(environment_settings_page(&signed, &project, &environment, project_admin, &form, Notice::None))
}

pub async fn update_environment(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<Naming>,
) -> Result<Response, WebError> {
    let (environment, project, project_admin) = environment_as_admin(&control, &signed, parse_id(&id)?).await?;
    let changes = EnvironmentChanges { name: Some(form.name.clone()), slug: form.slug() };
    match control.update_environment(&signed.principal, environment.id, form.revision.unwrap_or(-1), changes).await {
        Ok(environment) => Ok(redirect(&headers, &format!("/environments/{}", environment.id))),
        Err(error) => problem(error, |message| {
            environment_settings_page(&signed, &project, &environment, project_admin, &form, Notice::Problem(message))
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
            let form = Naming {
                revision: Some(environment.revision),
                name: environment.name.clone(),
                slug: environment.slug.to_string(),
            };
            let notice = Notice::Problem("Delete the environment's services first.");
            let page = environment_settings_page(&signed, &project, &environment, project_admin, &form, notice);
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

fn environment_settings_page(
    signed: &Signed,
    project: &Project,
    environment: &Environment,
    project_admin: bool,
    form: &Naming,
    notice: Notice<'_>,
) -> Markup {
    let action = format!("/environments/{}/settings", environment.id);
    let content = html! {
        (layout::heading("Environment settings", None))
        div class="max-w-xl space-y-6" {
            (forms::card(signed, "General", None, &action, notice, form.fields("environment"), "Save"))
            @if project_admin {
                (forms::danger(signed, "Delete environment", "Only an environment without services can be deleted.",
                    &format!("/environments/{}/delete", environment.id), "Delete environment", "Delete this environment?"))
            }
        }
    };
    layout::page(signed, &format!("Settings · {}", environment.name), &crumbs(project, Some(environment)), content)
}
