//! The Members tab of projects and environments, for their admins: who has which role
//! there, adding someone by email, changing a role, and removing them.

use axum::{
    Form,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use serde::Deserialize;
use spinikus_control::{Control, ControlError, Member};
use spinikus_core::{
    environment::Environment,
    id::{EnvironmentId, MembershipId, ProjectId},
    membership::{MembershipScope, Role},
    project::Project,
};

use super::{Tab, crumbs, environment_tabs, parse_id, problem_message, project_tabs, redirect};
use crate::{
    auth::Signed,
    error::WebError,
    forms::{self, Input, Notice},
    layout::{self, BUTTON_SECONDARY, CARD, INPUT, LINK, csrf_field},
};

/// Whose members a page shows.
enum Owner {
    Project(Project),
    Environment(Environment, Project),
}

impl Owner {
    async fn load(control: &Control, signed: &Signed, scope: MembershipScope) -> Result<Self, ControlError> {
        let principal = &signed.principal;
        Ok(match scope {
            MembershipScope::Project(id) => Owner::Project(control.get_project(principal, id).await?),
            MembershipScope::Environment(id) => {
                let environment = control.get_environment(principal, id).await?;
                let project = control.get_project(principal, environment.project_id).await?;
                Owner::Environment(environment, project)
            }
        })
    }

    fn scope(&self) -> MembershipScope {
        match self {
            Owner::Project(project) => MembershipScope::Project(project.id),
            Owner::Environment(environment, _) => MembershipScope::Environment(environment.id),
        }
    }
}

/// The page of a scope's members.
fn url(scope: MembershipScope) -> String {
    match scope {
        MembershipScope::Project(id) => format!("/projects/{id}/members"),
        MembershipScope::Environment(id) => format!("/environments/{id}/members"),
    }
}

/// A scope as a form value, `project:<id>` or `environment:<id>`.
fn scope_value(scope: MembershipScope) -> String {
    match scope {
        MembershipScope::Project(id) => format!("project:{id}"),
        MembershipScope::Environment(id) => format!("environment:{id}"),
    }
}

fn parse_scope(value: &str) -> Result<MembershipScope, WebError> {
    match value.split_once(':') {
        Some(("project", id)) => Ok(MembershipScope::Project(parse_id(id)?)),
        Some(("environment", id)) => Ok(MembershipScope::Environment(parse_id(id)?)),
        _ => Err(WebError::NoPage),
    }
}

fn role(value: &str) -> Result<Role, ControlError> {
    Role::ALL
        .into_iter()
        .find(|role| role.as_str() == value)
        .ok_or_else(|| ControlError::Invalid { field: "role", problem: "choose a role".to_string() })
}

fn role_options() -> Vec<(&'static str, &'static str)> {
    vec![("viewer", "Viewer"), ("developer", "Developer"), ("admin", "Admin")]
}

#[derive(Deserialize, Default)]
pub struct AddForm {
    #[serde(default)]
    email: String,
    #[serde(default)]
    role: String,
}

#[derive(Deserialize)]
pub struct Saved {
    saved: Option<String>,
}

pub async fn project_members(
    State(control): State<Control>,
    signed: Signed,
    Path(id): Path<String>,
    Query(Saved { saved }): Query<Saved>,
) -> Result<Markup, WebError> {
    let id: ProjectId = parse_id(&id)?;
    show(&control, &signed, MembershipScope::Project(id), saved.is_some()).await
}

pub async fn environment_members(
    State(control): State<Control>,
    signed: Signed,
    Path(id): Path<String>,
    Query(Saved { saved }): Query<Saved>,
) -> Result<Markup, WebError> {
    let id: EnvironmentId = parse_id(&id)?;
    show(&control, &signed, MembershipScope::Environment(id), saved.is_some()).await
}

async fn show(control: &Control, signed: &Signed, scope: MembershipScope, saved: bool) -> Result<Markup, WebError> {
    let owner = Owner::load(control, signed, scope).await?;
    let members = control.list_members(&signed.principal, scope).await?;
    let alert = saved.then_some(Alert::Saved);
    Ok(page(signed, &owner, &members, &AddForm::default(), alert, Notice::None))
}

pub async fn add_to_project(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<AddForm>,
) -> Result<Response, WebError> {
    let id: ProjectId = parse_id(&id)?;
    add(&control, &signed, &headers, MembershipScope::Project(id), form).await
}

pub async fn add_to_environment(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<AddForm>,
) -> Result<Response, WebError> {
    let id: EnvironmentId = parse_id(&id)?;
    add(&control, &signed, &headers, MembershipScope::Environment(id), form).await
}

async fn add(
    control: &Control,
    signed: &Signed,
    headers: &HeaderMap,
    scope: MembershipScope,
    form: AddForm,
) -> Result<Response, WebError> {
    let owner = Owner::load(control, signed, scope).await?;
    let added = match role(&form.role) {
        Ok(role) => control.add_member(&signed.principal, scope, &form.email, role).await,
        Err(error) => Err(error),
    };
    match added {
        Ok(_) => Ok(redirect(headers, &format!("{}?saved=1", url(scope)))),
        Err(error) => {
            let message = problem_message(error)?;
            let members = control.list_members(&signed.principal, scope).await?;
            let page = page(signed, &owner, &members, &form, None, Notice::Problem(&message));
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
    }
}

#[derive(Deserialize)]
pub struct RoleForm {
    #[serde(default)]
    role: String,
    #[serde(default)]
    revision: Option<i64>,
    /// The scope's page to go back to.
    #[serde(default)]
    scope: String,
}

pub async fn update(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<RoleForm>,
) -> Result<Response, WebError> {
    let id: MembershipId = parse_id(&id)?;
    let scope = parse_scope(&form.scope)?;
    let updated = match role(&form.role) {
        Ok(role) => control.update_member(&signed.principal, id, form.revision.unwrap_or(-1), role).await.map(|_| ()),
        Err(error) => Err(error),
    };
    after_change(&control, &signed, &headers, scope, updated).await
}

#[derive(Deserialize)]
pub struct RemoveForm {
    #[serde(default)]
    scope: String,
}

pub async fn remove(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<RemoveForm>,
) -> Result<Response, WebError> {
    let id: MembershipId = parse_id(&id)?;
    let scope = parse_scope(&form.scope)?;
    let removed = control.remove_member(&signed.principal, id).await;
    after_change(&control, &signed, &headers, scope, removed).await
}

/// Back to the scope's members, or the page with why the change was refused. Removing
/// oneself can end one's access to the page, so then it goes to the projects.
async fn after_change(
    control: &Control,
    signed: &Signed,
    headers: &HeaderMap,
    scope: MembershipScope,
    result: Result<(), ControlError>,
) -> Result<Response, WebError> {
    match result {
        Ok(()) => match control.list_members(&signed.principal, scope).await {
            Ok(_) => Ok(redirect(headers, &format!("{}?saved=1", url(scope)))),
            Err(ControlError::Forbidden | ControlError::NotFound { .. }) => Ok(redirect(headers, "/")),
            Err(error) => Err(error.into()),
        },
        Err(error) => {
            let message = problem_message(error)?;
            let owner = Owner::load(control, signed, scope).await?;
            let members = control.list_members(&signed.principal, scope).await?;
            let page = page(signed, &owner, &members, &AddForm::default(), Some(Alert::Problem(&message)), Notice::None);
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
    }
}

enum Alert<'a> {
    Saved,
    Problem(&'a str),
}

fn page(
    signed: &Signed,
    owner: &Owner,
    members: &[Member],
    form: &AddForm,
    alert: Option<Alert<'_>>,
    notice: Notice<'_>,
) -> Markup {
    let scope = owner.scope();
    let roles = role_options();
    let selected = if form.role.is_empty() { "developer" } else { form.role.as_str() };
    let fields = html! {
        div class="grid gap-4 sm:grid-cols-[1fr_12rem]" {
            (Input::text("member-email", "email", "Email", &form.email).kind("email").required()
                .hint("They need an account first; an instance admin adds users under Settings → Users.").render())
            (forms::select("member-role", "role", "Role", &roles, selected))
        }
    };
    let (name, tabs, crumbs, inherited) = match owner {
        Owner::Project(project) => {
            (project.name.as_str(), project_tabs(project, Some(Role::Admin), Tab::Members), crumbs(project, None), None)
        }
        Owner::Environment(environment, project) => (
            environment.name.as_str(),
            environment_tabs(environment, Role::Admin, Tab::Members),
            crumbs(project, None),
            Some(project),
        ),
    };
    let content = html! {
        @match alert {
            Some(Alert::Saved) => (layout::notice("Saved.")),
            Some(Alert::Problem(message)) => (layout::alert(message)),
            None => {},
        }
        (layout::heading(name, None))
        (tabs)
        div class="space-y-8" {
            section {
                h2 class="mb-1 font-semibold" { "Members" }
                p class="mb-3 text-sm text-zinc-500 dark:text-zinc-400" {
                    "Viewers read everything but secret values; developers also deploy and edit services, variables and domains; admins also manage this "
                    @if inherited.is_some() { "environment" } @else { "project" }
                    " and its members."
                    @if let Some(project) = inherited {
                        " Members of the project " a href={ "/projects/" (project.id) } class=(LINK) { (project.name) } " have their project role here too."
                    } @else {
                        " Project members have their role in every environment; instance admins are admins everywhere."
                    }
                }
                @if members.is_empty() {
                    (layout::empty("No members yet."))
                } @else {
                    ul class={ (CARD) " divide-y divide-zinc-200 dark:divide-zinc-800" } {
                        @for member in members { (member_row(signed, scope, member, &roles)) }
                    }
                }
            }
            div class="max-w-2xl" { (forms::card(signed, "Add a member", None, &url(scope), notice, fields, "Add member")) }
        }
    };
    layout::page(signed, &format!("Members · {name}"), &crumbs, content)
}

fn member_row(signed: &Signed, scope: MembershipScope, member: &Member, roles: &[(&str, &str)]) -> Markup {
    let membership = &member.membership;
    let id = format!("role-{}", membership.id);
    let yourself = member.user.id == signed.principal.user.id;
    html! {
        li class="flex flex-wrap items-center gap-x-4 gap-y-2 px-5 py-3 text-sm" {
            div class="min-w-0" {
                p class="font-medium" {
                    (member.user.name)
                    @if yourself { span class="font-normal text-zinc-500 dark:text-zinc-400" { " (you)" } }
                    @if !member.user.is_active() { span class="font-normal text-red-600 dark:text-red-400" { " (disabled)" } }
                }
                p class="truncate text-xs text-zinc-500 dark:text-zinc-400" { (member.user.email.as_str()) }
            }
            div class="ml-auto flex flex-wrap items-center gap-2" {
                form method="post" action={ "/memberships/" (membership.id) } class="flex items-center gap-2" {
                    (csrf_field(signed))
                    input type="hidden" name="revision" value=(membership.revision);
                    input type="hidden" name="scope" value=(scope_value(scope));
                    label for=(id) class="sr-only" { "Role of " (member.user.name) }
                    select id=(id) name="role" class={ (INPUT) " w-auto py-1" } {
                        @for (value, label) in roles {
                            option value=(value) selected[*value == membership.role.as_str()] { (label) }
                        }
                    }
                    button type="submit" class={ (BUTTON_SECONDARY) " py-1" } { "Save" }
                }
                form method="post" action={ "/memberships/" (membership.id) "/delete" }
                    hx-confirm=(if yourself { "Remove yourself? You may lose access here." } else { "Remove this member?" }) {
                    (csrf_field(signed))
                    input type="hidden" name="scope" value=(scope_value(scope));
                    button type="submit" class={ (BUTTON_SECONDARY) " py-1 text-red-600 dark:text-red-400" } { "Remove" }
                }
            }
        }
    }
}
