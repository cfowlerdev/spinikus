//! The user's own API tokens, for the CLI and automation: create one (its secret shown
//! once), see when each was used, and revoke them. Everyone signed in has this page.

use std::collections::HashMap;

use axum::{
    Form,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use jiff::{SignedDuration, Timestamp};
use maud::{Markup, html};
use serde::Deserialize;
use spinikus_control::{Control, ControlError, NewToken, page::MAX_PAGE_SIZE};
use spinikus_core::{
    membership::{MembershipScope, Role},
    secret::Secret,
    token::ApiToken,
};

use super::{
    parse_id, problem_message, redirect,
    settings::{SettingsTab, tabs},
};
use crate::{
    auth::Signed,
    error::WebError,
    forms::{self, Input, Notice},
    layout::{self, BADGE, BUTTON_SECONDARY, CARD, csrf_field},
};

const URL: &str = "/settings/tokens";

/// How long new tokens last: (form value, label, days).
const LIFETIMES: [(&str, &str, Option<i64>); 5] = [
    ("30", "30 days", Some(30)),
    ("90", "90 days", Some(90)),
    ("365", "1 year", Some(365)),
    ("7", "7 days", Some(7)),
    ("never", "Never", None),
];

#[derive(Deserialize)]
pub struct TokenForm {
    #[serde(default)]
    name: String,
    #[serde(default = "default_role")]
    role: String,
    /// Empty for everywhere, else `project:<id>` or `environment:<id>`.
    #[serde(default)]
    scope: String,
    #[serde(default = "default_lifetime")]
    expires: String,
}

fn default_role() -> String {
    Role::Developer.as_str().to_string()
}

fn default_lifetime() -> String {
    "90".to_string()
}

impl Default for TokenForm {
    fn default() -> Self {
        Self { name: String::new(), role: default_role(), scope: String::new(), expires: default_lifetime() }
    }
}

impl TokenForm {
    fn new_token(&self) -> Result<NewToken, ControlError> {
        let invalid = |field, problem: &str| ControlError::Invalid { field, problem: problem.to_string() };
        let role =
            Role::ALL.into_iter().find(|role| role.as_str() == self.role).ok_or_else(|| invalid("role", "choose a role"))?;
        let scope = match self.scope.split_once(':') {
            None if self.scope.is_empty() => None,
            Some(("project", id)) => Some(MembershipScope::Project(id.parse().map_err(|_| invalid("scope", "choose a scope"))?)),
            Some(("environment", id)) => {
                Some(MembershipScope::Environment(id.parse().map_err(|_| invalid("scope", "choose a scope"))?))
            }
            _ => return Err(invalid("scope", "choose a scope")),
        };
        let days = LIFETIMES
            .iter()
            .find(|(value, ..)| *value == self.expires)
            .map(|(.., days)| *days)
            .ok_or_else(|| invalid("expires_at", "choose when it expires"))?;
        let expires_at = days.map(|days| Timestamp::now() + SignedDuration::from_hours(days * 24));
        Ok(NewToken { name: self.name.clone(), role, scope, expires_at })
    }
}

/// The projects and environments the user can see, as scope choices: (`project:<id>` or
/// `environment:<id>`, label).
async fn scopes(control: &Control, signed: &Signed) -> Result<Vec<(String, String)>, ControlError> {
    let mut scopes = Vec::new();
    let mut after = None;
    loop {
        let projects = control.list_projects(&signed.principal, after, MAX_PAGE_SIZE).await?;
        after = projects.items.last().map(|project| project.id);
        for project in &projects.items {
            scopes.push((format!("project:{}", project.id), format!("{} (project)", project.name)));
            let mut after = None;
            loop {
                let environments = control.list_environments(&signed.principal, project.id, after, MAX_PAGE_SIZE).await?;
                after = environments.items.last().map(|environment| environment.id);
                for environment in &environments.items {
                    scopes.push((format!("environment:{}", environment.id), format!("{} / {}", project.name, environment.name)));
                }
                if !environments.has_more {
                    break;
                }
            }
        }
        if !projects.has_more {
            return Ok(scopes);
        }
    }
}

/// What the page says above the list.
enum Alert<'a> {
    None,
    /// A token was just created: its name and secret.
    Created(&'a ApiToken, &'a Secret),
}

pub async fn tokens(State(control): State<Control>, signed: Signed) -> Result<Markup, WebError> {
    Ok(page(&control, &signed, Alert::None, &TokenForm::default(), Notice::None).await?)
}

pub async fn create(State(control): State<Control>, signed: Signed, Form(form): Form<TokenForm>) -> Result<Response, WebError> {
    let created = match form.new_token() {
        Ok(new) => control.create_api_token(&signed.principal, new).await,
        Err(error) => Err(error),
    };
    match created {
        Ok((token, secret)) => {
            let page = page(&control, &signed, Alert::Created(&token, &secret), &TokenForm::default(), Notice::None).await?;
            // The page is the list's, whatever URL the form posted to.
            Ok((StatusCode::OK, [("hx-replace-url", URL)], page).into_response())
        }
        Err(error) => {
            let message = problem_message(error)?;
            let page = page(&control, &signed, Alert::None, &form, Notice::Problem(&message)).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
    }
}

pub async fn revoke(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, WebError> {
    match control.revoke_api_token(&signed.principal, parse_id(&id)?).await {
        // Already revoked, as by another tab, is just as done.
        Ok(()) | Err(ControlError::NotFound { .. }) => Ok(redirect(&headers, URL)),
        Err(error) => Err(error.into()),
    }
}

async fn page(
    control: &Control,
    signed: &Signed,
    alert: Alert<'_>,
    form: &TokenForm,
    notice: Notice<'_>,
) -> Result<Markup, ControlError> {
    let tokens = control.list_api_tokens(&signed.principal).await?;
    let scopes = scopes(control, signed).await?;
    let names: HashMap<&str, &str> = scopes.iter().map(|(value, label)| (value.as_str(), label.as_str())).collect();
    let roles: Vec<(&str, &str)> = Role::ALL.iter().rev().map(|role| (role.as_str(), role_label(*role))).collect();
    let mut scope_options = vec![("", "Everywhere I have access")];
    scope_options.extend(scopes.iter().map(|(value, label)| (value.as_str(), label.as_str())));
    let lifetimes: Vec<(&str, &str)> = LIFETIMES.iter().map(|(value, label, _)| (*value, *label)).collect();
    let fields = html! {
        (Input::text("token-name", "name", "Name", &form.name).required().placeholder("GitHub Actions").render())
        div class="grid gap-4 sm:grid-cols-3" {
            (forms::select("token-role", "role", "Role", &roles, &form.role))
            (forms::select("token-scope", "scope", "Scope", &scope_options, &form.scope))
            (forms::select("token-expires", "expires", "Expires", &lifetimes, &form.expires))
        }
        p class="text-xs text-zinc-500 dark:text-zinc-400" { "A token never does more than you can: its role caps yours wherever it applies." }
    };
    let now = Timestamp::now();
    let content = html! {
        @if let Alert::Created(token, secret) = alert {
            div role="status" class="mb-6 rounded-md bg-emerald-50 px-4 py-3 text-sm text-emerald-700 ring-1 ring-emerald-600/10 ring-inset dark:bg-emerald-500/10 dark:text-emerald-400 dark:ring-emerald-500/20" {
                p { "Created " strong { (token.name) } ". Copy the token now: it is shown only this once." }
                pre class="mt-2 whitespace-pre-wrap break-all rounded-md bg-white p-3 font-mono text-xs text-zinc-900 select-all dark:bg-zinc-900 dark:text-zinc-100" { (secret.expose()) }
            }
        }
        (tabs(signed, SettingsTab::Tokens))
        div class="space-y-8" {
            section {
                h2 class="mb-1 font-semibold" { "API tokens" }
                p class="mb-3 text-sm text-zinc-500 dark:text-zinc-400" {
                    "For the CLI and automation, sent as " code class="font-mono" { "Authorization: Bearer …" } ". Each acts as you."
                }
                @if tokens.is_empty() {
                    (layout::empty("No API tokens."))
                } @else {
                    ul class={ (CARD) " divide-y divide-zinc-200 dark:divide-zinc-800" } {
                        @for token in &tokens { (token_row(signed, token, &names, now)) }
                    }
                }
            }
            div class="max-w-2xl" {
                (forms::card(signed, "Create a token", None, URL, notice, fields, "Create token"))
            }
        }
    };
    Ok(layout::page(signed, "API tokens · Settings", &[], content))
}

fn role_label(role: Role) -> &'static str {
    match role {
        Role::Viewer => "Viewer: read only",
        Role::Developer => "Developer: also deploy and edit",
        Role::Admin => "Admin: everything",
    }
}

fn token_row(signed: &Signed, token: &ApiToken, names: &HashMap<&str, &str>, now: Timestamp) -> Markup {
    let scope = match token.scope {
        None => "everywhere".to_string(),
        Some(MembershipScope::Project(id)) => names
            .get(format!("project:{id}").as_str())
            .map_or_else(|| "a project you no longer see".to_string(), |name| name.to_string()),
        Some(MembershipScope::Environment(id)) => names
            .get(format!("environment:{id}").as_str())
            .map_or_else(|| "an environment you no longer see".to_string(), |name| name.to_string()),
    };
    let expired = token.expires_at.is_some_and(|expires_at| expires_at <= now);
    html! {
        li class="flex flex-wrap items-center gap-x-4 gap-y-2 px-5 py-3 text-sm" {
            div class="min-w-0" {
                p class="flex flex-wrap items-center gap-2" {
                    span class="font-medium" { (token.name) }
                    span class={ (BADGE) " bg-zinc-50 text-zinc-600 ring-zinc-500/20 dark:bg-zinc-500/10 dark:text-zinc-400 dark:ring-zinc-500/20" } { (token.role.as_str()) }
                    @if expired {
                        span class={ (BADGE) " bg-red-50 text-red-700 ring-red-600/20 dark:bg-red-500/10 dark:text-red-400 dark:ring-red-500/20" } { "expired" }
                    }
                }
                p class="text-xs text-zinc-500 dark:text-zinc-400" {
                    (scope) " · created " (layout::time(token.created_at))
                    " · " @if let Some(used) = token.last_used_at { "used " (layout::time(used)) } @else { "never used" }
                    @if let Some(expires_at) = token.expires_at { @if !expired { " · expires " (layout::time(expires_at)) } }
                }
            }
            form method="post" action={ (URL) "/" (token.id) "/revoke" } class="ml-auto" hx-confirm="Revoke this token? Anything using it stops working." {
                (csrf_field(signed))
                button type="submit" class={ (BUTTON_SECONDARY) " py-1 text-red-600 dark:text-red-400" } { "Revoke" }
            }
        }
    }
}
