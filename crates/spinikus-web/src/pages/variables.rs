//! The variables of a project, an environment or a service: one page per scope, listing its
//! own variables, those it inherits, and a form to add one. Developers add, edit and delete
//! them; admins reveal secrets, which is audited. Secret values never reach a page otherwise.

use axum::{
    Form,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use serde::Deserialize;
use spinikus_control::{Control, ControlError, NewVariable, VariableChanges};
use spinikus_core::{
    environment::Environment,
    id::VariableId,
    membership::Role,
    project::Project,
    secret::Secret,
    service::Service,
    variable::{Variable, VariableAvailability, VariableScope},
};

use super::{Tab, crumbs, environment_tabs, parse_id, problem, problem_message, project_tabs, redirect, service_tabs};
use crate::{
    auth::{Signed, is_htmx},
    error::WebError,
    forms::{self, Input, Notice},
    layout::{self, BADGE, BUTTON_SECONDARY, CARD, LINK, csrf_field},
};

/// What the variables belong to, with what its page needs around them.
enum Owner {
    Project(Project),
    Environment(Environment, Project),
    Service(Service, Environment, Project),
}

/// A scope's page: the scope, its owner, and the principal's role there (`None` for a project
/// the principal sees only through one of its environments).
struct Context {
    scope: VariableScope,
    owner: Owner,
    role: Option<Role>,
}

impl Context {
    async fn load(control: &Control, signed: &Signed, scope: VariableScope) -> Result<Self, ControlError> {
        let principal = &signed.principal;
        let (owner, role) = match scope {
            VariableScope::Project(id) => {
                let project = control.get_project(principal, id).await?;
                let role = control.project_role(principal, &project).await?;
                (Owner::Project(project), role)
            }
            VariableScope::Environment(id) => {
                let environment = control.get_environment(principal, id).await?;
                let role = control.environment_role(principal, &environment).await?;
                let project = control.get_project(principal, environment.project_id).await?;
                (Owner::Environment(environment, project), Some(role))
            }
            VariableScope::Service(id) => {
                let service = control.get_service(principal, id).await?;
                let environment = control.get_environment(principal, service.environment_id).await?;
                let role = control.environment_role(principal, &environment).await?;
                let project = control.get_project(principal, environment.project_id).await?;
                (Owner::Service(service, environment, project), Some(role))
            }
        };
        if role.is_none() {
            return Err(ControlError::Forbidden);
        }
        Ok(Self { scope, owner, role })
    }

    fn can_edit(&self) -> bool {
        self.role >= Some(Role::Developer)
    }

    fn can_reveal(&self) -> bool {
        self.role == Some(Role::Admin)
    }

    fn url(&self) -> String {
        url(self.scope)
    }

    /// The scopes this one inherits from, nearest first.
    fn parents(&self) -> Vec<(VariableScope, String)> {
        match &self.owner {
            Owner::Project(_) => vec![],
            Owner::Environment(_, project) => vec![(VariableScope::Project(project.id), format!("project {}", project.name))],
            Owner::Service(_, environment, project) => vec![
                (VariableScope::Environment(environment.id), format!("environment {}", environment.name)),
                (VariableScope::Project(project.id), format!("project {}", project.name)),
            ],
        }
    }
}

fn url(scope: VariableScope) -> String {
    match scope {
        VariableScope::Project(id) => format!("/projects/{id}/variables"),
        VariableScope::Environment(id) => format!("/environments/{id}/variables"),
        VariableScope::Service(id) => format!("/services/{id}/variables"),
    }
}

/// Variables set on a scope this one inherits from, with where they are set.
struct Inherited {
    from: String,
    url: String,
    variables: Vec<Variable>,
}

/// The variables of the scopes `context` inherits from that the principal may read; a
/// project the principal has no role in is left out.
async fn inherited(control: &Control, signed: &Signed, context: &Context) -> Result<Vec<Inherited>, ControlError> {
    let mut inherited = Vec::new();
    for (scope, from) in context.parents() {
        match control.list_variables(&signed.principal, scope).await {
            Ok(variables) => inherited.push(Inherited { from, url: url(scope), variables }),
            Err(ControlError::Forbidden | ControlError::NotFound { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(inherited)
}

/// What the add and edit forms post.
#[derive(Deserialize, Default)]
pub struct VariableForm {
    #[serde(default)]
    revision: Option<i64>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: String,
    #[serde(default)]
    availability: String,
    /// `on` when checked.
    #[serde(default)]
    secret: Option<String>,
}

impl VariableForm {
    fn new() -> Self {
        Self { availability: VariableAvailability::Runtime.as_str().to_string(), ..Default::default() }
    }

    fn availability(&self) -> Result<VariableAvailability, ControlError> {
        self.availability.parse().map_err(|_| ControlError::Invalid {
            field: "availability",
            problem: format!("availability must be one of runtime, build or both, not `{}`", self.availability),
        })
    }

    /// The fields; `editing` is the variable being edited, whose secret value isn't shown.
    fn fields(&self, editing: Option<&Variable>) -> Markup {
        let secret = editing.is_some_and(|variable| variable.secret);
        let availability = [("runtime", "At runtime"), ("build", "At build time"), ("both", "Both")];
        let value_hint = "Refer to another service's variable with ${{ service.NAME }}, such as ${{ postgres.DATABASE_URL }}.";
        html! {
            (Input::text("variable-name", "name", "Name", &self.name).required().mono().placeholder("API_URL").render())
            @if secret {
                (forms::textarea("variable-value", "value", "Value", "", Some("Leave it empty to keep the current value."), Some("Unchanged")))
            } @else {
                (forms::textarea("variable-value", "value", "Value", &self.value, Some(value_hint), None))
            }
            (forms::select("variable-availability", "availability", "Available", &availability, &self.availability))
            (forms::checkbox("variable-secret", "secret", "Secret", secret || self.secret.is_some(), secret,
                Some("Its value is hidden once saved and only admins can reveal it. A secret stays secret.")))
            @if let Some(revision) = self.revision { input type="hidden" name="revision" value=(revision); }
        }
    }
}

/// Values longer than this are cut in the list.
const SHOWN_VALUE_LEN: usize = 200;

pub async fn project_variables(
    State(control): State<Control>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Markup, WebError> {
    scope_page(&control, &signed, VariableScope::Project(parse_id(&id)?)).await
}

pub async fn environment_variables(
    State(control): State<Control>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Markup, WebError> {
    scope_page(&control, &signed, VariableScope::Environment(parse_id(&id)?)).await
}

pub async fn service_variables(
    State(control): State<Control>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Markup, WebError> {
    scope_page(&control, &signed, VariableScope::Service(parse_id(&id)?)).await
}

async fn scope_page(control: &Control, signed: &Signed, scope: VariableScope) -> Result<Markup, WebError> {
    let context = Context::load(control, signed, scope).await?;
    Ok(render(control, signed, &context, &VariableForm::new(), Notice::None, None).await?)
}

pub async fn create_project_variable(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<VariableForm>,
) -> Result<Response, WebError> {
    create(&control, &signed, &headers, VariableScope::Project(parse_id(&id)?), form).await
}

pub async fn create_environment_variable(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<VariableForm>,
) -> Result<Response, WebError> {
    create(&control, &signed, &headers, VariableScope::Environment(parse_id(&id)?), form).await
}

pub async fn create_service_variable(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<VariableForm>,
) -> Result<Response, WebError> {
    create(&control, &signed, &headers, VariableScope::Service(parse_id(&id)?), form).await
}

async fn create(
    control: &Control,
    signed: &Signed,
    headers: &HeaderMap,
    scope: VariableScope,
    form: VariableForm,
) -> Result<Response, WebError> {
    let context = Context::load(control, signed, scope).await?;
    let created = match form.availability() {
        Ok(availability) => {
            let new = NewVariable {
                name: form.name.trim().to_string(),
                value: Secret::new(forms::lines(&form.value)),
                availability,
                secret: form.secret.is_some(),
            };
            control.create_variable(&signed.principal, scope, new).await
        }
        Err(error) => Err(error),
    };
    match created {
        Ok(_) => Ok(redirect(headers, &context.url())),
        Err(error) => {
            let message = problem_message(error)?;
            let page = render(control, signed, &context, &form, Notice::Problem(&message), None).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
    }
}

/// Shows a secret's value: the value alone for HTMX, which puts it in place of the dots, else
/// the scope's page with it shown.
pub async fn reveal(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, WebError> {
    let variable = control.get_variable(&signed.principal, parse_id(&id)?).await?;
    let value = control.reveal_variable(&signed.principal, variable.id).await?;
    if is_htmx(&headers) {
        return Ok(
            html! { div class="flex flex-wrap items-center gap-3" { (secret_badge()) (value_code(value.expose(), false)) } }
                .into_response(),
        );
    }
    let context = Context::load(&control, &signed, variable.scope).await?;
    let page =
        render(&control, &signed, &context, &VariableForm::new(), Notice::None, Some((variable.id, value.expose()))).await?;
    Ok(page.into_response())
}

pub async fn edit(State(control): State<Control>, signed: Signed, Path(id): Path<String>) -> Result<Markup, WebError> {
    let (variable, context) = editable(&control, &signed, &id).await?;
    let form = VariableForm {
        revision: Some(variable.revision),
        name: variable.name.to_string(),
        value: variable.value.clone().unwrap_or_default(),
        availability: variable.availability.as_str().to_string(),
        secret: variable.secret.then(|| "on".to_string()),
    };
    Ok(edit_page(&signed, &context, &variable, &form, Notice::None))
}

pub async fn update(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<VariableForm>,
) -> Result<Response, WebError> {
    let (variable, context) = editable(&control, &signed, &id).await?;
    let updated = match form.availability() {
        Ok(availability) => {
            let changes = VariableChanges {
                name: Some(form.name.trim().to_string()),
                // A secret's value isn't shown, so an empty one means unchanged.
                value: match variable.secret && form.value.is_empty() {
                    true => None,
                    false => Some(Secret::new(forms::lines(&form.value))),
                },
                availability: Some(availability),
                // A secret's checkbox is disabled, so it posts nothing.
                secret: (!variable.secret).then_some(form.secret.is_some()),
            };
            control.update_variable(&signed.principal, variable.id, form.revision.unwrap_or(-1), changes).await
        }
        Err(error) => Err(error),
    };
    match updated {
        Ok(_) => Ok(redirect(&headers, &context.url())),
        Err(error) => problem(error, |message| edit_page(&signed, &context, &variable, &form, Notice::Problem(message))),
    }
}

pub async fn delete(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, WebError> {
    let (variable, context) = editable(&control, &signed, &id).await?;
    control.delete_variable(&signed.principal, variable.id).await?;
    Ok(redirect(&headers, &context.url()))
}

/// The variable and its scope's page, for a developer of the scope.
async fn editable(control: &Control, signed: &Signed, id: &str) -> Result<(Variable, Context), WebError> {
    let variable = control.get_variable(&signed.principal, parse_id(id)?).await?;
    let context = Context::load(control, signed, variable.scope).await?;
    if !context.can_edit() {
        return Err(ControlError::Forbidden.into());
    }
    Ok((variable, context))
}

/// The page around a scope's content: the owner's header and tabs, and its breadcrumbs.
fn frame(signed: &Signed, context: &Context, title: &str, content: Markup) -> Markup {
    let (header, page_crumbs) = match &context.owner {
        Owner::Project(project) => {
            let header = html! {
                (layout::heading(&project.name, Some(html! { span class="font-mono" { (project.slug) } })))
                (project_tabs(project, context.role, Tab::Variables))
            };
            (header, vec![("Projects", "/".to_string())])
        }
        Owner::Environment(environment, project) => {
            let header = html! {
                (layout::heading(&environment.name, Some(html! { span class="font-mono" { (environment.slug) } })))
                (environment_tabs(environment, context.role.unwrap_or(Role::Viewer), Tab::Variables))
            };
            (header, crumbs(project, None))
        }
        Owner::Service(service, environment, project) => {
            let header = html! {
                (layout::heading(&service.name, Some(html! { span class="font-mono" { (service.slug) } })))
                (service_tabs(service, context.role.unwrap_or(Role::Viewer), Tab::Variables))
            };
            (header, crumbs(project, Some(environment)))
        }
    };
    let content = html! { (header) (content) };
    layout::page(signed, title, &page_crumbs, content)
}

fn owner_name(context: &Context) -> &str {
    match &context.owner {
        Owner::Project(project) => &project.name,
        Owner::Environment(environment, _) => &environment.name,
        Owner::Service(service, _, _) => &service.name,
    }
}

async fn render(
    control: &Control,
    signed: &Signed,
    context: &Context,
    form: &VariableForm,
    notice: Notice<'_>,
    revealed: Option<(VariableId, &str)>,
) -> Result<Markup, ControlError> {
    let variables = control.list_variables(&signed.principal, context.scope).await?;
    let inherited = inherited(control, signed, context).await?;
    let level = context.scope.level().as_str();
    let content = html! {
        div class="space-y-8" {
            section {
                h2 class="mb-1 font-semibold" { "Variables of this " (level) }
                p class="mb-3 text-sm text-zinc-500 dark:text-zinc-400" {
                    @match context.scope {
                        VariableScope::Service(_) => "Changes apply at the next deployment. These win over the environment's and the project's.",
                        VariableScope::Environment(_) => "Every service in the environment gets these at its next deployment; they win over the project's.",
                        VariableScope::Project(_) => "Every service in every environment of the project gets these at its next deployment.",
                    }
                }
                (variable_table(signed, context, &variables, revealed))
            }
            @for parent in &inherited {
                @if !parent.variables.is_empty() {
                    section {
                        h2 class="mb-3 font-semibold" {
                            "From the " a href=(parent.url) class=(LINK) { (parent.from) }
                        }
                        (inherited_list(&parent.variables, &variables, &inherited, parent))
                    }
                }
            }
            @if context.can_edit() {
                div class="max-w-2xl" {
                    (forms::card(signed, "Add a variable", None, &context.url(), notice, form.fields(None), "Add variable"))
                }
            }
        }
    };
    let title = format!("Variables · {}", owner_name(context));
    Ok(frame(signed, context, &title, content))
}

fn variable_table(signed: &Signed, context: &Context, variables: &[Variable], revealed: Option<(VariableId, &str)>) -> Markup {
    html! {
        @if variables.is_empty() {
            (layout::empty("No variables yet."))
        } @else {
            div class={ (CARD) " overflow-x-auto" } {
                table class="w-full text-left text-sm" {
                    thead class="border-b border-zinc-200 text-xs text-zinc-500 dark:border-zinc-800 dark:text-zinc-400" {
                        tr {
                            th class="px-4 py-2 font-medium" { "Name" }
                            th class="px-4 py-2 font-medium" { "Value" }
                            th class="hidden px-4 py-2 font-medium sm:table-cell" { "Available" }
                            th class="px-4 py-2" {}
                        }
                    }
                    tbody class="divide-y divide-zinc-200 dark:divide-zinc-800" {
                        @for variable in variables {
                            tr class="align-top" {
                                td class="min-w-32 px-4 py-3 font-mono font-medium break-all" { (variable.name) }
                                td id={ "value-" (variable.id) } class="w-1/2 px-4 py-3" {
                                    @match (&variable.value, revealed.filter(|(id, _)| *id == variable.id)) {
                                        (_, Some((_, value))) => div class="flex flex-wrap items-center gap-3" { (secret_badge()) (value_code(value, false)) },
                                        (Some(value), None) => (value_code(value, true)),
                                        (None, None) => (secret_cell(signed, context, variable)),
                                    }
                                }
                                td class="hidden px-4 py-3 whitespace-nowrap text-zinc-500 sm:table-cell dark:text-zinc-400" { (availability(variable.availability)) }
                                td class="px-4 py-3" {
                                    @if context.can_edit() {
                                        div class="flex flex-col items-end gap-2 sm:flex-row sm:justify-end" {
                                            a href={ "/variables/" (variable.id) "/edit" } class={ (BUTTON_SECONDARY) " py-1" } { "Edit" }
                                            form method="post" action={ "/variables/" (variable.id) "/delete" }
                                                hx-confirm={ "Delete " (variable.name) "? Services keep it until their next deployment." } {
                                                (csrf_field(signed))
                                                button type="submit" class={ (BUTTON_SECONDARY) " py-1 text-red-600 dark:text-red-400" } { "Delete" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A value, cut to [`SHOWN_VALUE_LEN`] in lists when `cut`.
fn value_code(value: &str, cut: bool) -> Markup {
    let shown = match cut && value.chars().count() > SHOWN_VALUE_LEN {
        true => format!("{}…", value.chars().take(SHOWN_VALUE_LEN).collect::<String>()),
        false => value.to_string(),
    };
    html! {
        @if value.is_empty() {
            span class="text-zinc-400 italic" { "empty" }
        } @else {
            code class="font-mono text-xs break-all whitespace-pre-wrap" { (shown) }
        }
    }
}

/// A secret's value hidden, with a button that reveals it for admins.
fn secret_cell(signed: &Signed, context: &Context, variable: &Variable) -> Markup {
    let action = format!("/variables/{}/reveal", variable.id);
    let target = format!("#value-{}", variable.id);
    html! {
        div class="flex flex-wrap items-center gap-3" {
            (secret_badge())
            span class="font-mono text-zinc-400" aria-label="hidden" { "••••••••" }
            @if context.can_reveal() {
                form method="post" action=(action) hx-post=(action) hx-target=(target) hx-swap="innerHTML" {
                    (csrf_field(signed))
                    button type="submit" class=(LINK) { "Reveal" }
                }
            }
        }
    }
}

fn secret_badge() -> Markup {
    html! {
        span class={ (BADGE) " bg-amber-50 text-amber-700 ring-amber-600/20 dark:bg-amber-500/10 dark:text-amber-400 dark:ring-amber-500/20" } { "secret" }
    }
}

fn availability(availability: VariableAvailability) -> &'static str {
    match availability {
        VariableAvailability::Runtime => "runtime",
        VariableAvailability::Build => "build",
        VariableAvailability::Both => "build and runtime",
    }
}

/// An inherited scope's variables, marking those a nearer scope sets too.
fn inherited_list(variables: &[Variable], own: &[Variable], all: &[Inherited], this: &Inherited) -> Markup {
    // Nearer scopes come first in `all`.
    let nearer: Vec<&Variable> = own
        .iter()
        .chain(all.iter().take_while(|parent| !std::ptr::eq(*parent, this)).flat_map(|parent| parent.variables.iter()))
        .collect();
    html! {
        ul class={ (CARD) " divide-y divide-zinc-200 text-sm dark:divide-zinc-800" } {
            @for variable in variables {
                @let overridden = nearer.iter().any(|near| near.name == variable.name);
                li class="flex flex-wrap items-center gap-x-4 gap-y-1 px-5 py-2.5" {
                    span class={ "font-mono font-medium" @if overridden { " text-zinc-400 line-through" } } { (variable.name) }
                    @if variable.secret {
                        span class="text-zinc-400" { "secret" }
                    } @else if let Some(value) = &variable.value {
                        span class="min-w-0 truncate font-mono text-xs text-zinc-500 dark:text-zinc-400" { (value) }
                    }
                    @if overridden { span class="ml-auto text-xs text-zinc-500 dark:text-zinc-400" { "overridden here" } }
                }
            }
        }
    }
}

fn edit_page(signed: &Signed, context: &Context, variable: &Variable, form: &VariableForm, notice: Notice<'_>) -> Markup {
    let action = format!("/variables/{}/edit", variable.id);
    let content = html! {
        div class="max-w-2xl" {
            (forms::card(signed, &format!("Edit {}", variable.name), Some("Services get the change at their next deployment."), &action, notice, form.fields(Some(variable)), "Save"))
            div class="mt-4 text-sm" { a href=(context.url()) class=(LINK) { "Back to the variables" } }
        }
    };
    frame(signed, context, &format!("{} · {}", variable.name, owner_name(context)), content)
}
