//! A service: what runs now, its deployments, and deploying, restarting or rolling it back.

use axum::{
    Form,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use serde::Deserialize;
use spinikus_control::{Control, ControlError};
use spinikus_core::{
    database::Engine,
    deployment::{Deployment, DeploymentStatus},
    id::{DeploymentId, ServiceId},
    membership::Role,
    service::Service,
    source::SourceSpec,
};

use super::{Tab, crumbs, parse_id, redirect, refused, service_tabs};
use crate::{
    auth::Signed,
    error::WebError,
    layout::{self, BUTTON_PRIMARY, BUTTON_SECONDARY, CARD, LINK, csrf_field, service_badge, status_badge},
};

/// How many deployments the history shows at a time.
const DEPLOYMENTS_PER_PAGE: u32 = 20;
/// How often the history refreshes while a deployment is in progress.
const POLL_EVERY: &str = "every 2s";

#[derive(Deserialize)]
pub struct After {
    after: Option<String>,
    /// Set after a restart, to say it happened.
    restarted: Option<String>,
}

pub async fn service(
    State(control): State<Control>,
    signed: Signed,
    Path(id): Path<String>,
    Query(After { after, restarted }): Query<After>,
) -> Result<Response, WebError> {
    let after = after.as_deref().map(parse_id).transpose()?;
    let shown = restarted.is_some().then_some(Shown::Notice("Restarted."));
    Ok(service_page(&control, &signed, parse_id(&id)?, after, shown).await?.into_response())
}

/// The history alone, for refreshing it while a deployment runs.
pub async fn deployments(State(control): State<Control>, signed: Signed, Path(id): Path<String>) -> Result<Markup, WebError> {
    let service = control.get_service(&signed.principal, parse_id(&id)?).await?;
    let environment = control.get_environment(&signed.principal, service.environment_id).await?;
    let role = control.environment_role(&signed.principal, &environment).await?;
    let page = control.list_deployments(&signed.principal, service.id, None, DEPLOYMENTS_PER_PAGE).await?;
    Ok(history(&signed, &service, role, &page.items, page.has_more, None))
}

/// Deploys the service as it is now, then shows the deployment.
pub async fn deploy(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, WebError> {
    let service_id = parse_id(&id)?;
    match control.create_deployment(&signed.principal, service_id).await {
        Ok(deployment) => Ok(redirect(&headers, &format!("/deployments/{}", deployment.id))),
        Err(error) => match refused(&error) {
            Some(message) => {
                let page = service_page(&control, &signed, service_id, None, Some(Shown::Alert(&message))).await?;
                Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
            }
            None => Err(error.into()),
        },
    }
}

#[derive(Deserialize)]
pub struct Rollback {
    /// The deployment to roll back to; the last one replaced if empty.
    deployment_id: Option<String>,
}

/// Rolls the service back to an earlier deployment, then shows the rollback.
pub async fn rollback(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<Rollback>,
) -> Result<Response, WebError> {
    let service_id = parse_id(&id)?;
    let target: Option<DeploymentId> = form.deployment_id.as_deref().filter(|id| !id.is_empty()).map(parse_id).transpose()?;
    match control.rollback_service(&signed.principal, service_id, target).await {
        Ok(deployment) => Ok(redirect(&headers, &format!("/deployments/{}", deployment.id))),
        Err(error) => match refused(&error) {
            Some(message) => {
                let page = service_page(&control, &signed, service_id, None, Some(Shown::Alert(&message))).await?;
                Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
            }
            None => Err(error.into()),
        },
    }
}

/// Restarts the service's running container, then shows the service.
pub async fn restart(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, WebError> {
    let service_id = parse_id(&id)?;
    match control.restart_service(&signed.principal, service_id).await {
        Ok(_) => Ok(redirect(&headers, &format!("/services/{service_id}?restarted=1"))),
        Err(error) => match refused(&error) {
            Some(message) => {
                let page = service_page(&control, &signed, service_id, None, Some(Shown::Alert(&message))).await?;
                Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
            }
            None => Err(error.into()),
        },
    }
}

/// A message above the service page.
#[derive(Clone, Copy)]
enum Shown<'a> {
    Alert(&'a str),
    Notice(&'a str),
}

async fn service_page(
    control: &Control,
    signed: &Signed,
    id: ServiceId,
    after: Option<DeploymentId>,
    shown: Option<Shown<'_>>,
) -> Result<Markup, ControlError> {
    let principal = &signed.principal;
    let service = control.get_service(principal, id).await?;
    let environment = control.get_environment(principal, service.environment_id).await?;
    let project = control.get_project(principal, environment.project_id).await?;
    let role = control.environment_role(principal, &environment).await?;
    let source = match control.get_source(principal, id).await {
        Ok(source) => Some(source.spec),
        Err(ControlError::NotFound { .. }) => None,
        Err(error) => return Err(error),
    };
    let history_page = control.list_deployments(principal, id, after, DEPLOYMENTS_PER_PAGE).await?;
    // What runs now is on the first page unless the history is long and busy; look further if not.
    let live = match history_page.items.iter().find(|deployment| deployment.status == DeploymentStatus::Live) {
        Some(live) => Some(live.clone()),
        None => find_live(control, signed, id).await?,
    };
    let can_deploy = role >= Role::Developer;
    let placement = match service.server_id {
        Some(server) => html! { span class="font-mono" { (server) } },
        None => html! { "This machine (the embedded server)" },
    };
    let content = html! {
        @match shown {
            Some(Shown::Alert(message)) => (layout::alert(message)),
            Some(Shown::Notice(message)) => (layout::notice(message)),
            None => {}
        }
        div class="mb-6 flex flex-wrap items-start gap-4" {
            div {
                h1 class="flex items-center gap-3 text-2xl font-semibold tracking-tight" { (service.name) (service_badge(service.kind)) }
                p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400" { (source_summary(source.as_ref())) }
            }
            @if can_deploy {
                div class="ml-auto flex gap-2" {
                    @if live.is_some() {
                        form method="post" action={ "/services/" (service.id) "/restart" }
                            hx-confirm="Restart the running container? Requests to it fail until it is back up." {
                            (csrf_field(signed))
                            button type="submit" class=(BUTTON_SECONDARY) { "Restart" }
                        }
                    }
                    form method="post" action={ "/services/" (service.id) "/deploy" } {
                        (csrf_field(signed))
                        button type="submit" class=(BUTTON_PRIMARY) disabled[source.is_none()] { "Deploy" }
                    }
                }
            }
        }
        (service_tabs(&service, role, Tab::Main))
        @if let Some(engine) = Engine::of(&service.config) { (super::databases::connection(&service, engine)) }
        section class="mb-8" {
            h2 class="mb-3 font-semibold" { "Running now" }
            (current(live.as_ref()))
        }
        section class="mb-8" {
            h2 class="mb-3 font-semibold" { "Deployments" }
            (history(signed, &service, role, &history_page.items, history_page.has_more, after))
        }
        section {
            h2 class="mb-3 font-semibold" { "Details" }
            dl class={ (CARD) " divide-y divide-zinc-200 dark:divide-zinc-800" } {
                (detail("Slug", html! { span class="font-mono" { (service.slug) } }))
                (detail("Runs on", placement))
                (detail("Created", layout::time(service.created_at)))
                (detail("Updated", layout::time(service.updated_at)))
            }
        }
    };
    let title = format!("{} · {}", service.name, environment.name);
    Ok(layout::page(signed, &title, &crumbs(&project, Some(&environment)), content))
}

/// The live deployment beyond the first page of the history, if there is one.
async fn find_live(control: &Control, signed: &Signed, id: ServiceId) -> Result<Option<Deployment>, ControlError> {
    let mut after = None;
    loop {
        let page = control.list_deployments(&signed.principal, id, after, spinikus_control::page::MAX_PAGE_SIZE).await?;
        if let Some(live) = page.items.iter().find(|deployment| deployment.status == DeploymentStatus::Live) {
            return Ok(Some(live.clone()));
        }
        // A replaced deployment is older than the live one, so past one there is none.
        if !page.has_more || page.items.iter().any(|deployment| deployment.status == DeploymentStatus::Superseded) {
            return Ok(None);
        }
        after = page.items.last().map(|deployment| deployment.id);
    }
}

fn source_summary(source: Option<&SourceSpec>) -> Markup {
    match source {
        Some(SourceSpec::Git(git)) => html! {
            span class="font-mono" { (git.repo_url) }
            " on " span class="font-mono" { (git.branch) }
            @if let Some(root) = &git.root_directory { " in " span class="font-mono" { (root) } }
        },
        Some(SourceSpec::Image(image)) => html! { "Image " span class="font-mono" { (image.image) } },
        None => html! { "No source yet, so nothing to deploy." },
    }
}

fn current(live: Option<&Deployment>) -> Markup {
    match live {
        None => layout::empty("Nothing is running. Deploy the service to start it."),
        Some(deployment) => html! {
            a href={ "/deployments/" (deployment.id) } class={ (CARD) " flex flex-wrap items-center gap-x-6 gap-y-2 p-5 hover:border-zinc-300 dark:hover:border-zinc-700" } {
                (status_badge(deployment.status))
                span class="font-mono text-sm" { (what_runs_short(deployment)) }
                span class="text-sm text-zinc-500 dark:text-zinc-400" { (deployment.trigger.as_str()) " · " (layout::time(deployment.created_at)) }
                span class="ml-auto text-sm text-zinc-500 dark:text-zinc-400" { (deployment.id) }
            }
        },
    }
}

/// The commit, else the image, of a deployment.
pub fn what_runs(deployment: &Deployment) -> String {
    match (&deployment.commit_sha, &deployment.image) {
        (Some(commit), _) => commit.chars().take(12).collect(),
        (None, Some(image)) => image.clone(),
        (None, None) => "—".to_string(),
    }
}

/// [`what_runs`], with an image's digest cut to 12 characters, for lists; the whole of it
/// shows on hover.
fn what_runs_short(deployment: &Deployment) -> Markup {
    let full = what_runs(deployment);
    let short = match full.split_once("@sha256:") {
        Some((name, digest)) if digest.len() > 12 => format!("{name}@sha256:{}", &digest[..12]),
        _ => full.clone(),
    };
    html! { span title=(full) { (short) } }
}

/// Whether the service can be rolled back to this deployment: it was live, was replaced, and
/// its image is still on its server.
pub fn can_roll_back_to(deployment: &Deployment) -> bool {
    deployment.status == DeploymentStatus::Superseded && deployment.image.is_some() && deployment.cleaned_at.is_none()
}

/// The deployment history; on its first page it refreshes itself while one is in progress.
fn history(
    signed: &Signed,
    service: &Service,
    role: Role,
    deployments: &[Deployment],
    has_more: bool,
    after: Option<DeploymentId>,
) -> Markup {
    let busy = after.is_none() && deployments.iter().any(|deployment| deployment.status.is_in_progress());
    let url = format!("/services/{}/deployments", service.id);
    html! {
        div id="deployments" hx-get=[busy.then_some(&url)] hx-trigger=[busy.then_some(POLL_EVERY)] hx-swap="outerHTML" {
            @if deployments.is_empty() {
                (layout::empty("No deployments yet."))
            } @else {
                ul class={ (CARD) " divide-y divide-zinc-200 dark:divide-zinc-800" } {
                    @for deployment in deployments {
                        li class="flex flex-wrap items-center gap-x-4 gap-y-2 px-5 py-3 text-sm" {
                            span class="w-28" { (status_badge(deployment.status)) }
                            a href={ "/deployments/" (deployment.id) } class="font-mono hover:text-indigo-600 dark:hover:text-indigo-400" {
                                (what_runs_short(deployment))
                            }
                            span class="text-zinc-500 dark:text-zinc-400" { (deployment.trigger.as_str()) }
                            span class="ml-auto text-zinc-500 dark:text-zinc-400" { (layout::time(deployment.created_at)) }
                            @if role >= Role::Developer && can_roll_back_to(deployment) {
                                form method="post" action={ "/services/" (service.id) "/rollback" }
                                    hx-confirm="Roll back to this deployment? It replaces what runs now." {
                                    (csrf_field(signed))
                                    input type="hidden" name="deployment_id" value=(deployment.id);
                                    button type="submit" class={ (BUTTON_SECONDARY) " py-1" } { "Roll back" }
                                }
                            }
                        }
                    }
                }
                @if let (true, Some(last)) = (has_more, deployments.last()) {
                    div class="mt-3 text-sm" { a href={ "/services/" (service.id) "?after=" (last.id) } class=(LINK) { "Older deployments" } }
                }
            }
        }
    }
}

fn detail(term: &str, value: Markup) -> Markup {
    html! {
        div class="grid grid-cols-3 gap-4 px-5 py-3 text-sm" {
            dt class="text-zinc-500 dark:text-zinc-400" { (term) }
            dd class="col-span-2" { (value) }
        }
    }
}
