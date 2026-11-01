//! Users, in the instance settings: adding one with an initial password, and on a user's
//! page their name, email and admin flag, a new password, where they have a role, their API
//! tokens, and disabling or enabling them. Instance admins only.

use axum::{
    Form,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use serde::Deserialize;
use spinikus_control::{Control, ControlError, NewUser, UserChanges, page::MAX_PAGE_SIZE};
use spinikus_core::{
    id::{ApiTokenId, UserId},
    membership::MembershipScope,
    secret::Secret,
    user::User,
};

use super::{
    parse_id, problem, problem_message, redirect,
    settings::{SettingsTab, require_admin, tabs},
};
use crate::{
    auth::Signed,
    error::WebError,
    forms::{self, Input, Notice},
    layout::{self, BADGE, BUTTON_SECONDARY, CARD, LINK, csrf_field},
};

const URL: &str = "/settings/users";

const ADMIN_BADGE: &str =
    "bg-indigo-50 text-indigo-700 ring-indigo-600/20 dark:bg-indigo-500/10 dark:text-indigo-400 dark:ring-indigo-500/20";
const DISABLED_BADGE: &str = "bg-red-50 text-red-700 ring-red-600/20 dark:bg-red-500/10 dark:text-red-400 dark:ring-red-500/20";
const QUIET_BADGE: &str =
    "bg-zinc-50 text-zinc-600 ring-zinc-500/20 dark:bg-zinc-500/10 dark:text-zinc-400 dark:ring-zinc-500/20";

#[derive(Deserialize, Default)]
pub struct NewUserForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    password: String,
    /// `on` when checked.
    #[serde(default)]
    admin: Option<String>,
}

pub async fn users(State(control): State<Control>, signed: Signed) -> Result<Markup, WebError> {
    require_admin(&signed)?;
    Ok(list_page(&control, &signed, &NewUserForm::default(), Notice::None).await?)
}

pub async fn create(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Form(form): Form<NewUserForm>,
) -> Result<Response, WebError> {
    require_admin(&signed)?;
    let new = NewUser {
        email: form.email.clone(),
        name: form.name.clone(),
        admin: form.admin.is_some(),
        password: Some(Secret::new(&form.password)),
    };
    match control.create_user(&signed.principal, new).await {
        Ok(user) => Ok(redirect(&headers, &format!("{URL}/{}", user.id))),
        Err(error) => {
            let message = problem_message(error)?;
            let page = list_page(&control, &signed, &form, Notice::Problem(&message)).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
    }
}

async fn list_page(control: &Control, signed: &Signed, form: &NewUserForm, notice: Notice<'_>) -> Result<Markup, ControlError> {
    let mut users = Vec::new();
    let mut after = None;
    loop {
        let page = control.list_users(&signed.principal, after, MAX_PAGE_SIZE).await?;
        after = page.items.last().map(|user| user.id);
        users.extend(page.items);
        if !page.has_more {
            break;
        }
    }
    let fields = html! {
        div class="grid gap-4 sm:grid-cols-2" {
            (Input::text("user-name", "name", "Name", &form.name).required().render())
            (Input::text("user-email", "email", "Email", &form.email).kind("email").required().render())
        }
        // The typed password is not sent back after a refusal.
        (Input::text("user-password", "password", "Initial password", "").kind("password").required()
            .hint("At least 12 characters. Give it to them; they can change it under Settings → Account.").render())
        (forms::checkbox("user-admin", "admin", "Instance admin", form.admin.is_some(), false,
            Some("May do anything, in every project, and manage the instance.")))
    };
    let content = html! {
        (tabs(signed, SettingsTab::Users))
        div class="space-y-8" {
            section {
                h2 class="mb-1 font-semibold" { "Users" }
                p class="mb-3 text-sm text-zinc-500 dark:text-zinc-400" {
                    "Everyone who can sign in. Give them access to projects and environments on their Members tabs."
                }
                ul class={ (CARD) " divide-y divide-zinc-200 dark:divide-zinc-800" } {
                    @for user in &users { (user_row(user)) }
                }
            }
            div class="max-w-2xl" { (forms::card(signed, "Add a user", None, URL, notice, fields, "Add user")) }
        }
    };
    Ok(layout::page(signed, "Users · Settings", &[], content))
}

fn badges(user: &User) -> Markup {
    html! {
        @if user.admin { span class={ (BADGE) " " (ADMIN_BADGE) } { "admin" } }
        @if !user.is_active() { span class={ (BADGE) " " (DISABLED_BADGE) } { "disabled" } }
        @if !user.has_password { span class={ (BADGE) " " (QUIET_BADGE) } { "no password" } }
    }
}

fn user_row(user: &User) -> Markup {
    html! {
        li class="flex flex-wrap items-center gap-x-3 gap-y-1 px-5 py-3 text-sm" {
            div class="min-w-0" {
                a href={ (URL) "/" (user.id) } class="font-medium hover:text-indigo-600 dark:hover:text-indigo-400" { (user.name) }
                p class="truncate text-xs text-zinc-500 dark:text-zinc-400" { (user.email.as_str()) }
            }
            div class="ml-auto flex flex-wrap items-center gap-2" { (badges(user)) }
        }
    }
}

/// What the user's page says above its content, and in which card.
#[derive(Default)]
struct Notices<'a> {
    alert: Option<&'a str>,
    account: Notice<'a>,
    password: Notice<'a>,
}

#[derive(Deserialize)]
pub struct Saved {
    saved: Option<String>,
}

#[derive(Deserialize)]
pub struct AccountForm {
    #[serde(default)]
    revision: Option<i64>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    admin: Option<String>,
}

impl AccountForm {
    fn of(user: &User) -> Self {
        Self {
            revision: Some(user.revision),
            name: user.name.clone(),
            email: user.email.as_str().to_string(),
            admin: user.admin.then(|| "on".to_string()),
        }
    }
}

#[derive(Deserialize)]
pub struct PasswordForm {
    #[serde(default)]
    password: String,
}

async fn load(control: &Control, signed: &Signed, id: &str) -> Result<User, WebError> {
    require_admin(signed)?;
    let id: UserId = parse_id(id)?;
    Ok(control.get_user(&signed.principal, id).await?)
}

pub async fn user(
    State(control): State<Control>,
    signed: Signed,
    Path(id): Path<String>,
    Query(Saved { saved }): Query<Saved>,
) -> Result<Markup, WebError> {
    let user = load(&control, &signed, &id).await?;
    let notice = |section: &str| if saved.as_deref() == Some(section) { Notice::Saved } else { Notice::None };
    let notices = Notices { alert: None, account: notice("account"), password: notice("password") };
    Ok(user_page(&control, &signed, &user, &AccountForm::of(&user), notices).await?)
}

pub async fn update(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<AccountForm>,
) -> Result<Response, WebError> {
    let user = load(&control, &signed, &id).await?;
    let changes =
        UserChanges { email: Some(form.email.clone()), name: Some(form.name.clone()), admin: Some(form.admin.is_some()) };
    match control.update_user(&signed.principal, user.id, form.revision.unwrap_or(-1), changes).await {
        Ok(user) => Ok(redirect(&headers, &format!("{URL}/{}?saved=account", user.id))),
        Err(error) => {
            let message = problem_message(error)?;
            let notices = Notices { account: Notice::Problem(&message), ..Default::default() };
            let page = user_page(&control, &signed, &user, &form, notices).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
    }
}

pub async fn reset_password(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<PasswordForm>,
) -> Result<Response, WebError> {
    let user = load(&control, &signed, &id).await?;
    match control.reset_password(&signed.principal, user.id, Secret::new(&form.password)).await {
        Ok(_) => Ok(redirect(&headers, &format!("{URL}/{}?saved=password", user.id))),
        Err(error) => {
            let message = problem_message(error)?;
            let notices = Notices { password: Notice::Problem(&message), ..Default::default() };
            let page = user_page(&control, &signed, &user, &AccountForm::of(&user), notices).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
    }
}

pub async fn disable(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, WebError> {
    let user = load(&control, &signed, &id).await?;
    let disabled = control.disable_user(&signed.principal, user.id).await;
    after_action(&control, &signed, &headers, &user, disabled.map(|_| ())).await
}

pub async fn enable(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, WebError> {
    let user = load(&control, &signed, &id).await?;
    let enabled = control.enable_user(&signed.principal, user.id).await;
    after_action(&control, &signed, &headers, &user, enabled.map(|_| ())).await
}

/// Revokes one of the user's tokens.
pub async fn revoke_token(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path((id, token)): Path<(String, String)>,
) -> Result<Response, WebError> {
    let user = load(&control, &signed, &id).await?;
    let token: ApiTokenId = parse_id(&token)?;
    let revoked = match control.revoke_api_token(&signed.principal, token).await {
        // Already revoked is just as done.
        Err(ControlError::NotFound { .. }) => Ok(()),
        other => other,
    };
    after_action(&control, &signed, &headers, &user, revoked).await
}

/// Back to the user's page, or the page again with why the action was refused.
async fn after_action(
    control: &Control,
    signed: &Signed,
    headers: &HeaderMap,
    user: &User,
    result: Result<(), ControlError>,
) -> Result<Response, WebError> {
    match result {
        Ok(()) => Ok(redirect(headers, &format!("{URL}/{}", user.id))),
        Err(error) => {
            let message = problem_message(error)?;
            let user = control.get_user(&signed.principal, user.id).await?;
            let notices = Notices { alert: Some(&message), ..Default::default() };
            let page = user_page(control, signed, &user, &AccountForm::of(&user), notices).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
    }
}

/// Where the user has a role: (scope label, link, role).
async fn roles(control: &Control, signed: &Signed, user: &User) -> Result<Vec<(String, String, &'static str)>, ControlError> {
    let mut roles = Vec::new();
    for membership in control.list_user_memberships(&signed.principal, user.id).await? {
        let place = match membership.scope {
            MembershipScope::Project(id) => match control.get_project(&signed.principal, id).await {
                Ok(project) => Some((format!("{} (project)", project.name), format!("/projects/{id}/members"))),
                Err(ControlError::NotFound { .. }) => None,
                Err(error) => return Err(error),
            },
            MembershipScope::Environment(id) => match control.get_environment(&signed.principal, id).await {
                Ok(environment) => {
                    let project = control.get_project(&signed.principal, environment.project_id).await?;
                    Some((format!("{} / {}", project.name, environment.name), format!("/environments/{id}/members")))
                }
                Err(ControlError::NotFound { .. }) => None,
                Err(error) => return Err(error),
            },
        };
        if let Some((label, href)) = place {
            roles.push((label, href, membership.role.as_str()));
        }
    }
    Ok(roles)
}

async fn user_page(
    control: &Control,
    signed: &Signed,
    user: &User,
    form: &AccountForm,
    notices: Notices<'_>,
) -> Result<Markup, ControlError> {
    let tokens = control.list_user_api_tokens(&signed.principal, user.id).await?;
    let roles = roles(control, signed, user).await?;
    let base = format!("{URL}/{}", user.id);
    let yourself = user.id == signed.principal.user.id;
    let account = html! {
        div class="grid gap-4 sm:grid-cols-2" {
            (Input::text("account-name", "name", "Name", &form.name).required().render())
            (Input::text("account-email", "email", "Email", &form.email).kind("email").required().render())
        }
        (forms::checkbox("account-admin", "admin", "Instance admin", form.admin.is_some(), false,
            Some("May do anything, in every project, and manage the instance. There is always at least one.")))
        @if let Some(revision) = form.revision { input type="hidden" name="revision" value=(revision); }
    };
    let password = html! {
        (Input::text("account-password", "password", "New password", "").kind("password").required()
            .hint("At least 12 characters. Their sessions end, so they sign in with it next.").render())
    };
    let content = html! {
        @if let Some(alert) = notices.alert { (layout::alert(alert)) }
        (tabs(signed, SettingsTab::Users))
        div class="max-w-2xl space-y-6" {
            div class="flex flex-wrap items-center gap-2" {
                h2 class="text-lg font-semibold" { (user.name) }
                (badges(user))
            }
            (forms::card(signed, "Account", None, &base, notices.account, account, "Save"))
            (forms::card(signed, "Password", None, &format!("{base}/password"), notices.password, password, "Set password"))
            section class={ (CARD) " p-6" } {
                h2 class="font-semibold" { "Access" }
                @if user.admin {
                    p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400" { "An instance admin has every role everywhere." }
                }
                @if roles.is_empty() {
                    p class="mt-3 text-sm text-zinc-500 dark:text-zinc-400" { "No memberships." }
                } @else {
                    ul class="mt-3 space-y-1 text-sm" {
                        @for (label, href, role) in &roles {
                            li { a href=(href) class=(LINK) { (label) } " · " (role) }
                        }
                    }
                }
            }
            section class={ (CARD) " p-6" } {
                h2 class="font-semibold" { "API tokens" }
                @if tokens.is_empty() {
                    p class="mt-3 text-sm text-zinc-500 dark:text-zinc-400" { "No API tokens." }
                } @else {
                    ul class="mt-3 divide-y divide-zinc-200 text-sm dark:divide-zinc-800" {
                        @for token in &tokens {
                            li class="flex flex-wrap items-center gap-3 py-2" {
                                span class="font-medium" { (token.name) }
                                span class="text-xs text-zinc-500 dark:text-zinc-400" {
                                    (token.role.as_str()) " · "
                                    @if let Some(used) = token.last_used_at { "used " (layout::time(used)) } @else { "never used" }
                                }
                                form method="post" action={ (base) "/tokens/" (token.id) "/revoke" } class="ml-auto"
                                    hx-confirm="Revoke this token? Anything using it stops working." {
                                    (csrf_field(signed))
                                    button type="submit" class={ (BUTTON_SECONDARY) " py-1 text-red-600 dark:text-red-400" } { "Revoke" }
                                }
                            }
                        }
                    }
                }
            }
            @if user.is_active() {
                @if !yourself {
                    (forms::danger(signed, "Disable user", "They can't sign in any more, and their sessions and API tokens stop working. Their memberships and history stay.",
                        &format!("{base}/disable"), "Disable user", "Disable this user?"))
                }
            } @else {
                section class={ (CARD) " p-6" } {
                    h2 class="font-semibold" { "Disabled" }
                    p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400" {
                        "Since " @if let Some(at) = user.disabled_at { (layout::time(at)) } ". Enabling lets them sign in again; API tokens they had work again too."
                    }
                    form method="post" action={ (base) "/enable" } class="mt-4" {
                        (csrf_field(signed))
                        button type="submit" class=(layout::BUTTON_PRIMARY) { "Enable user" }
                    }
                }
            }
            div class="text-sm" { a href=(URL) class=(LINK) { "Back to users" } }
        }
    };
    Ok(layout::page(signed, &format!("{} · Users", user.name), &[], content))
}

// The signed-in user's own account.

#[derive(Deserialize)]
pub struct ChangePasswordForm {
    #[serde(default)]
    current_password: String,
    #[serde(default)]
    new_password: String,
    #[serde(default)]
    confirm_password: String,
}

pub async fn account(signed: Signed, Query(Saved { saved }): Query<Saved>) -> Markup {
    let notice = if saved.as_deref() == Some("password") { Notice::Saved } else { Notice::None };
    account_page(&signed, notice)
}

pub async fn change_password(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Form(form): Form<ChangePasswordForm>,
) -> Result<Response, WebError> {
    let changed = match form.new_password == form.confirm_password {
        true => {
            control.change_password(&signed.principal, Secret::new(&form.current_password), Secret::new(&form.new_password)).await
        }
        false => Err(ControlError::Invalid { field: "confirm_password", problem: "the new passwords differ".to_string() }),
    };
    match changed {
        Ok(()) => Ok(redirect(&headers, "/settings/account?saved=password")),
        Err(error) => problem(error, |message| account_page(&signed, Notice::Problem(message))),
    }
}

fn account_page(signed: &Signed, notice: Notice<'_>) -> Markup {
    let user = &signed.principal.user;
    let fields = html! {
        (Input::text("password-current", "current_password", "Current password", "").kind("password").required().render())
        div class="grid gap-4 sm:grid-cols-2" {
            (Input::text("password-new", "new_password", "New password", "").kind("password").required().hint("At least 12 characters.").render())
            (Input::text("password-confirm", "confirm_password", "New password again", "").kind("password").required().render())
        }
    };
    let content = html! {
        (tabs(signed, SettingsTab::Account))
        div class="max-w-2xl space-y-6" {
            section class={ (CARD) " p-6" } {
                h2 class="font-semibold" { (user.name) }
                p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400" { (user.email.as_str()) }
                @if user.admin { p class="mt-3" { span class={ (BADGE) " " (ADMIN_BADGE) } { "instance admin" } } }
                p class="mt-3 text-xs text-zinc-500 dark:text-zinc-400" { "An instance admin changes names and emails." }
            }
            (forms::card(signed, "Change password", Some("Your other sessions end; this one stays signed in."),
                "/settings/account/password", notice, fields, "Change password"))
        }
    };
    layout::page(signed, "Account · Settings", &[], content)
}
