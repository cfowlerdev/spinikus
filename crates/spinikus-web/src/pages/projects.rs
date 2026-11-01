//! A project with its environments, and an environment with its services.

use axum::extract::{Path, State};
use maud::{Markup, html};
use spinikus_control::Control;
use spinikus_core::{membership::Role, service::Service};

use super::{Tab, crumbs, environment_tabs, parse_id, project_tabs};
use crate::{
    auth::Signed,
    error::WebError,
    layout::{self, BUTTON_PRIMARY, CARD, LINK, environment_badge, service_badge},
};

/// How many services an environment shows; more are on the environment's page.
const SERVICES_SHOWN: u32 = 100;
const ENVIRONMENTS_SHOWN: u32 = 100;

pub async fn project(State(control): State<Control>, signed: Signed, Path(id): Path<String>) -> Result<Markup, WebError> {
    let project = control.get_project(&signed.principal, parse_id(&id)?).await?;
    let environments = control.list_environments(&signed.principal, project.id, None, ENVIRONMENTS_SHOWN).await?;
    let mut sections = Vec::with_capacity(environments.items.len());
    for environment in environments.items {
        let services = control.list_services(&signed.principal, environment.id, None, SERVICES_SHOWN).await?;
        sections.push((environment, services.items, services.has_more));
    }
    let role = control.project_role(&signed.principal, &project).await?;
    let actions = html! {
        @if role == Some(Role::Admin) {
            a href={ "/projects/" (project.id) "/environments/new" } class=(BUTTON_PRIMARY) { "New environment" }
        }
    };
    let content = html! {
        (layout::header(&project.name, Some(html! { span class="font-mono" { (project.slug) } }), actions))
        (project_tabs(&project, role, Tab::Main))
        @if sections.is_empty() {
            (layout::empty("This project has no environments yet."))
        }
        div class="space-y-8" {
            @for (environment, services, more) in &sections {
                section {
                    div class="mb-3 flex items-center gap-2" {
                        h2 class="font-semibold" {
                            a href={ "/environments/" (environment.id) } class="hover:text-indigo-600 dark:hover:text-indigo-400" { (environment.name) }
                        }
                        (environment_badge(environment.kind))
                    }
                    (service_list(services, "No services in this environment yet."))
                    @if *more {
                        div class="mt-3 text-sm" { a href={ "/environments/" (environment.id) } class=(LINK) { "All services" } }
                    }
                }
            }
        }
    };
    Ok(layout::page(&signed, &project.name, &[("Projects", "/".to_string())], content))
}

pub async fn environment(State(control): State<Control>, signed: Signed, Path(id): Path<String>) -> Result<Markup, WebError> {
    let environment = control.get_environment(&signed.principal, parse_id(&id)?).await?;
    let project = control.get_project(&signed.principal, environment.project_id).await?;
    let services = control.list_services(&signed.principal, environment.id, None, SERVICES_SHOWN).await?;
    let subtitle = html! { span class="inline-flex items-center gap-2" { span class="font-mono" { (environment.slug) } (environment_badge(environment.kind)) } };
    let role = control.environment_role(&signed.principal, &environment).await?;
    let actions = html! {
        @if role >= Role::Developer { a href={ "/environments/" (environment.id) "/services/new" } class=(BUTTON_PRIMARY) { "New service" } }
    };
    let content = html! {
        (layout::header(&environment.name, Some(subtitle), actions))
        (environment_tabs(&environment, role, Tab::Main))
        (service_list(&services.items, "No services in this environment yet."))
        (layout::next_page(services.has_more, services.items.last().map(|service| service.id.to_string())))
    };
    let title = format!("{} · {}", environment.name, project.name);
    Ok(layout::page(&signed, &title, &crumbs(&project, None), content))
}

fn service_list(services: &[Service], empty: &str) -> Markup {
    html! {
        @if services.is_empty() {
            (layout::empty(empty))
        } @else {
            ul class={ (CARD) " divide-y divide-zinc-200 dark:divide-zinc-800" } {
                @for service in services {
                    li {
                        a href={ "/services/" (service.id) } class="flex items-center gap-3 px-5 py-3 hover:bg-zinc-50 dark:hover:bg-zinc-800/50" {
                            span class="font-medium" { (service.name) }
                            span class="font-mono text-xs text-zinc-500 dark:text-zinc-400" { (service.slug) }
                            span class="ml-auto" { (service_badge(service.kind)) }
                        }
                    }
                }
            }
        }
    }
}
