//! First-run setup, signing in and signing out.

use axum::{
    Form,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use serde::Deserialize;
use spinikus_control::{Control, ControlError, password::MIN_PASSWORD_LEN};
use spinikus_core::secret::Secret;

use super::redirect;
use crate::{
    auth::{self, Client, SESSION_COOKIE, Signed, local_path, session_cookie},
    error::WebError,
    layout::{self, BUTTON_PRIMARY, INPUT, LABEL},
};

#[derive(Deserialize)]
pub struct Next {
    next: Option<String>,
}

pub async fn login_page(
    State(control): State<Control>,
    Query(Next { next }): Query<Next>,
    headers: HeaderMap,
) -> Result<Response, WebError> {
    if control.needs_setup().await? {
        return Ok(redirect(&headers, "/setup"));
    }
    if signed_in(&control, &headers).await? {
        return Ok(redirect(&headers, local_path(next.as_deref())));
    }
    Ok(login_form("", next.as_deref(), None).into_response())
}

#[derive(Deserialize)]
pub struct Login {
    email: String,
    password: Secret,
    next: Option<String>,
}

pub async fn login(
    State(control): State<Control>,
    Client(client): Client,
    headers: HeaderMap,
    Form(login): Form<Login>,
) -> Result<Response, WebError> {
    match control.sign_in(&login.email, login.password, client).await {
        Ok((_, secret)) => Ok(start_session(&headers, secret, local_path(login.next.as_deref()))),
        Err(ControlError::InvalidCredentials) => {
            let form = login_form(&login.email, login.next.as_deref(), Some("Wrong email or password."));
            Ok((StatusCode::UNAUTHORIZED, form).into_response())
        }
        Err(error) => Err(error.into()),
    }
}

pub async fn logout(State(control): State<Control>, signed: Signed, headers: HeaderMap) -> Result<Response, WebError> {
    control.sign_out(&signed.principal).await?;
    Ok(([(header::SET_COOKIE, auth::cleared_cookie(&headers))], redirect(&headers, "/login")).into_response())
}

pub async fn setup_page(State(control): State<Control>, headers: HeaderMap) -> Result<Response, WebError> {
    if !control.needs_setup().await? {
        return Ok(redirect(&headers, "/login"));
    }
    Ok(setup_form("", "", None).into_response())
}

#[derive(Deserialize)]
pub struct Setup {
    name: String,
    email: String,
    password: Secret,
}

/// Creates the first admin and signs them in.
pub async fn setup(
    State(control): State<Control>,
    Client(client): Client,
    headers: HeaderMap,
    Form(setup): Form<Setup>,
) -> Result<Response, WebError> {
    match control.create_first_admin(&setup.email, &setup.name, setup.password.clone()).await {
        Ok(_) => {}
        Err(ControlError::AlreadySetUp) => return Ok(redirect(&headers, "/login")),
        Err(ControlError::Invalid { problem, field }) => {
            let message = format!("The {field} is not valid: {problem}.");
            let form = setup_form(&setup.name, &setup.email, Some(&message));
            return Ok((StatusCode::UNPROCESSABLE_ENTITY, form).into_response());
        }
        Err(error) => return Err(error.into()),
    }
    let (_, secret) = control.sign_in(&setup.email, setup.password, client).await?;
    Ok(start_session(&headers, secret, "/"))
}

/// Whether the request has a live session.
async fn signed_in(control: &Control, headers: &HeaderMap) -> Result<bool, WebError> {
    let Some(cookie) = auth::cookie(headers, SESSION_COOKIE) else {
        return Ok(false);
    };
    match control.authenticate_session(cookie).await {
        Ok(_) => Ok(true),
        Err(ControlError::Unauthenticated) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn start_session(headers: &HeaderMap, secret: Secret, to: &str) -> Response {
    ([(header::SET_COOKIE, session_cookie(secret.expose(), headers))], redirect(headers, to)).into_response()
}

fn login_form(email: &str, next: Option<&str>, error: Option<&str>) -> Markup {
    let content = html! {
        input type="hidden" name="next" value=(local_path(next));
        (field("email", "Email", "email", "username", email))
        (field("password", "Password", "password", "current-password", ""))
        button type="submit" class={ (BUTTON_PRIMARY) " w-full" } { "Sign in" }
    };
    auth_card("Sign in", "Sign in to Spinikus", "/login", error, content)
}

fn setup_form(name: &str, email: &str, error: Option<&str>) -> Markup {
    let content = html! {
        (field("name", "Your name", "text", "name", name))
        (field("email", "Email", "email", "username", email))
        (field("password", "Password", "password", "new-password", ""))
        p class="text-xs text-zinc-500 dark:text-zinc-400" { "At least " (MIN_PASSWORD_LEN) " characters." }
        button type="submit" class={ (BUTTON_PRIMARY) " w-full" } { "Create admin and sign in" }
    };
    auth_card("Set up", "Create the first admin", "/setup", error, content)
}

fn auth_card(title: &str, heading: &str, action: &str, error: Option<&str>, content: Markup) -> Markup {
    layout::bare(
        title,
        html! {
            div class="w-full max-w-sm" {
                div class="mb-8 flex items-center justify-center gap-2 text-lg font-semibold tracking-tight" {
                    span class="size-3 rounded-full bg-indigo-500" {}
                    "Spinikus"
                }
                div class={ (layout::CARD) " p-6 shadow-sm" } {
                    h1 class="mb-6 text-lg font-semibold" { (heading) }
                    @if let Some(error) = error {
                        div role="alert" class="mb-4 rounded-md bg-red-50 px-3 py-2 text-sm text-red-700 dark:bg-red-500/10 dark:text-red-400" {
                            (error)
                        }
                    }
                    form method="post" action=(action) class="space-y-4" { (content) }
                }
            }
        },
    )
}

fn field(name: &str, label: &str, kind: &str, autocomplete: &str, value: &str) -> Markup {
    html! {
        div class="space-y-1.5" {
            label for=(name) class=(LABEL) { (label) }
            input id=(name) name=(name) type=(kind) autocomplete=(autocomplete) value=(value) required class=(INPUT);
        }
    }
}
