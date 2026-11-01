//! A deployment: its status and history, its log (followed live while it is in progress),
//! and canceling it or rolling back to it.
//!
//! While a deployment is in progress the page connects to `/deployments/{id}/stream` with
//! the HTMX SSE extension: `log` events are lines appended to the log, `status` events make
//! the summary reload itself, and `end` closes the connection. The stream starts at the
//! beginning of the log, so the page itself renders the log only for finished deployments.

use std::convert::Infallible;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use maud::{Markup, html};
use spinikus_control::{Control, ControlError, deployments::stream::DeploymentUpdate};
use spinikus_core::{
    deployment::{Deployment, DeploymentEvent},
    id::DeploymentId,
    membership::Role,
};
use tokio_stream::StreamExt;

use super::{
    crumbs, parse_id, redirect, refused,
    services::{can_roll_back_to, what_runs},
};
use crate::{
    auth::Signed,
    error::WebError,
    layout::{self, BUTTON_SECONDARY, CARD, LINK, csrf_field, status_badge},
};

pub async fn deployment(State(control): State<Control>, signed: Signed, Path(id): Path<String>) -> Result<Markup, WebError> {
    Ok(deployment_page(&control, &signed, parse_id(&id)?, None).await?)
}

/// The deployment's log as a text file, as far as it goes.
pub async fn download_log(State(control): State<Control>, signed: Signed, Path(id): Path<String>) -> Result<Response, WebError> {
    let id: DeploymentId = parse_id(&id)?;
    let log = control.deployment_logs(&signed.principal, id).await?;
    // An ID is letters, digits and an underscore.
    let disposition =
        HeaderValue::from_str(&format!("attachment; filename=\"{id}.log\"")).expect("an ID is a valid header value");
    let headers = [
        (header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8")),
        (header::CONTENT_DISPOSITION, disposition),
    ];
    Ok((headers, log).into_response())
}

/// Whether the deployment may still have a container to read the runtime logs of: one that
/// started and wasn't cleaned up yet (failed ones are kept for their logs).
fn may_have_container(deployment: &Deployment) -> bool {
    use spinikus_core::deployment::DeploymentStatus::*;
    deployment.cleaned_at.is_none() && matches!(deployment.status, Starting | Verifying | Live | Draining | Superseded | Failed)
}

/// The summary alone, which reloads itself when the status changes.
pub async fn summary(State(control): State<Control>, signed: Signed, Path(id): Path<String>) -> Result<Markup, WebError> {
    let deployment = control.get_deployment(&signed.principal, parse_id(&id)?).await?;
    let role = role(&control, &signed, &deployment).await?;
    let events = control.list_deployment_events(&signed.principal, deployment.id).await?;
    Ok(summary_section(&signed, &deployment, role, &events))
}

/// Cancels a deployment that hasn't gone live.
pub async fn cancel(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, WebError> {
    let id = parse_id(&id)?;
    match control.cancel_deployment(&signed.principal, id).await {
        Ok(_) => Ok(redirect(&headers, &format!("/deployments/{id}"))),
        Err(error) => match refused(&error) {
            Some(message) => {
                let page = deployment_page(&control, &signed, id, Some(&message)).await?;
                Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
            }
            None => Err(error.into()),
        },
    }
}

/// The deployment as server-sent events for the page: rendered log lines, status changes,
/// and the end. A reconnecting browser resumes after the last line it got (`Last-Event-ID`).
pub async fn stream(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, WebError> {
    let offset = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    let updates = control.follow_deployment(&signed.principal, parse_id(&id)?, offset).await?;
    let events = updates.map(|update| {
        Ok::<_, Infallible>(match update {
            DeploymentUpdate::Log { line, offset } => {
                Event::default().event("log").id(offset.to_string()).data(log_line(&line).into_string())
            }
            DeploymentUpdate::Status { status, .. } => Event::default().event("status").data(status.as_str()),
            DeploymentUpdate::End(status) => Event::default().event("end").data(status.as_str()),
        })
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::default()).into_response())
}

async fn role(control: &Control, signed: &Signed, deployment: &Deployment) -> Result<Role, ControlError> {
    let service = control.get_service(&signed.principal, deployment.service_id).await?;
    let environment = control.get_environment(&signed.principal, service.environment_id).await?;
    control.environment_role(&signed.principal, &environment).await
}

async fn deployment_page(
    control: &Control,
    signed: &Signed,
    id: DeploymentId,
    alert: Option<&str>,
) -> Result<Markup, ControlError> {
    let principal = &signed.principal;
    let deployment = control.get_deployment(principal, id).await?;
    let service = control.get_service(principal, deployment.service_id).await?;
    let environment = control.get_environment(principal, service.environment_id).await?;
    let project = control.get_project(principal, environment.project_id).await?;
    let role = control.environment_role(principal, &environment).await?;
    let events = control.list_deployment_events(principal, id).await?;
    let following = deployment.status.is_in_progress();
    let log = match following {
        true => String::new(),
        false => control.deployment_logs(principal, id).await?,
    };
    let stream = format!("/deployments/{id}/stream");
    let content = html! {
        @if let Some(alert) = alert { (layout::alert(alert)) }
        div class="mb-6" {
            h1 class="text-2xl font-semibold tracking-tight" { "Deployment of " (service.name) }
            p class="mt-1 font-mono text-sm text-zinc-500 dark:text-zinc-400" { (deployment.id) }
        }
        // Everything that follows the deployment lives under the SSE connection.
        div hx-ext=[following.then_some("sse")] sse-connect=[following.then_some(&stream)] sse-close=[following.then_some("end")] {
            (summary_section(signed, &deployment, role, &events))
            section {
                div class="mb-3 flex flex-wrap items-baseline gap-x-4 gap-y-1" {
                    h2 class="font-semibold" { "Log" }
                    @if !following {
                        a href={ "/deployments/" (deployment.id) "/log" } hx-boost="false" download class={ (LINK) " text-sm" } { "Download" }
                    }
                    @if may_have_container(&deployment) {
                        a href={ "/services/" (service.id) "/logs?deployment_id=" (deployment.id) } class={ (LINK) " text-sm" } { "Container logs" }
                    }
                }
                div class="flex max-h-[36rem] flex-col-reverse overflow-y-auto rounded-lg bg-zinc-950 p-4 font-mono text-xs leading-relaxed text-zinc-200 ring-1 ring-zinc-800" {
                    div id="log" sse-swap=[following.then_some("log")] hx-swap="beforeend" {
                        @for line in log.lines() { (log_line(line)) }
                        @if !following && log.is_empty() {
                            p class="text-zinc-500" { "No log for this deployment." }
                        }
                    }
                }
            }
        }
    };
    let title = format!("Deployment · {}", service.name);
    let mut crumbs = crumbs(&project, Some(&environment));
    crumbs.push((service.name.as_str(), format!("/services/{}", service.id)));
    Ok(layout::page(signed, &title, &crumbs, content))
}

/// The status, what is deployed, the actions and the history. While in progress it reloads
/// itself on each `status` event from the stream.
fn summary_section(signed: &Signed, deployment: &Deployment, role: Role, events: &[DeploymentEvent]) -> Markup {
    let following = deployment.status.is_in_progress();
    let reload = format!("/deployments/{}/summary", deployment.id);
    let failure = events.iter().rev().find_map(|event| event.message.as_deref()).filter(|_| deployment.reason.is_some());
    let developer = role >= Role::Developer;
    html! {
        section id="summary" class="mb-8 space-y-6" hx-get=[following.then_some(&reload)] hx-trigger=[following.then_some("sse:status")]
            hx-swap="outerHTML" {
            div class={ (CARD) " p-5" } {
                div class="flex flex-wrap items-center gap-3" {
                    (status_badge(deployment.status))
                    @if let Some(reason) = deployment.reason {
                        span class="text-sm text-zinc-500 dark:text-zinc-400" { (reason.as_str()) }
                    }
                    div class="ml-auto flex gap-2" {
                        @if developer && following {
                            form method="post" action={ "/deployments/" (deployment.id) "/cancel" } hx-confirm="Cancel this deployment?" {
                                (csrf_field(signed))
                                button type="submit" class=(BUTTON_SECONDARY) { "Cancel" }
                            }
                        }
                        @if developer && can_roll_back_to(deployment) {
                            form method="post" action={ "/services/" (deployment.service_id) "/rollback" }
                                hx-confirm="Roll back to this deployment? It replaces what runs now." {
                                (csrf_field(signed))
                                input type="hidden" name="deployment_id" value=(deployment.id);
                                button type="submit" class=(BUTTON_SECONDARY) { "Roll back to this" }
                            }
                        }
                    }
                }
                @if let Some(failure) = failure {
                    p class="mt-4 rounded-md bg-red-50 px-3 py-2 text-sm text-red-700 dark:bg-red-500/10 dark:text-red-400" { (failure) }
                }
                dl class="mt-5 grid gap-x-6 gap-y-3 text-sm sm:grid-cols-2" {
                    (fact("Runs", html! { span class="font-mono break-all" { (what_runs(deployment)) } }))
                    @if let (Some(_), Some(image)) = (&deployment.commit_sha, &deployment.image) {
                        (fact("Image", html! { span class="font-mono break-all" { (image) } }))
                    }
                    (fact("Trigger", html! { (deployment.trigger.as_str()) }))
                    (fact("Strategy", html! { (deployment.strategy.as_str()) }))
                    @if let Some(target) = deployment.rollback_of {
                        (fact("Rolls back to", html! { a href={ "/deployments/" (target) } class={ (LINK) " font-mono" } { (target) } }))
                    }
                    (fact("Server", html! { span class="font-mono" { (deployment.server_id) } }))
                    (fact("Created", layout::time(deployment.created_at)))
                    (fact("Updated", layout::time(deployment.updated_at)))
                }
            }
            div {
                h2 class="mb-3 font-semibold" { "History" }
                ol class="space-y-2 text-sm" {
                    @for event in events {
                        li class="flex flex-wrap items-baseline gap-x-3" {
                            span class="w-44 shrink-0 text-zinc-500 dark:text-zinc-400" { (layout::time(event.created_at)) }
                            span class="font-medium" { (event.to.as_str()) }
                            @if let Some(reason) = event.reason { span class="text-zinc-500 dark:text-zinc-400" { (reason.as_str()) } }
                        }
                    }
                }
            }
        }
    }
}

fn fact(term: &str, value: Markup) -> Markup {
    html! {
        div {
            dt class="text-zinc-500 dark:text-zinc-400" { (term) }
            dd class="mt-0.5" { (value) }
        }
    }
}

/// A log line, `<RFC 3339 time> <text>`: the time of day, dimmed, then the text, with
/// `==> phase` lines standing out.
fn log_line(line: &str) -> Markup {
    let (time, text) = match line.split_once(' ') {
        Some((time, text)) if time.parse::<jiff::Timestamp>().is_ok() => (time.get(11..19), text),
        _ => (None, line),
    };
    let phase = text.strip_prefix("==> ");
    html! {
        div class="flex gap-3 whitespace-pre-wrap break-all" {
            span class="shrink-0 select-none text-zinc-500" { (time.unwrap_or("")) }
            @match phase {
                Some(phase) => span class="font-semibold text-indigo-300" { "==> " (phase) },
                None => span { (text) },
            }
        }
    }
}
