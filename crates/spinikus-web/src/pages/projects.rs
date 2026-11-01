//! A project with its environments, an environment with its services, and a service.

use axum::extract::{Path, State};
use maud::{Markup, html};
use spinikus_control::Control;
use spinikus_core::{environment::Environment, project::Project, service::Service};

use super::parse_id;
use crate::{
    auth::Signed,
    error::WebError,
    layout::{self, CARD, LINK, environment_badge, service_badge},
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
    let content = html! {
        (layout::heading(&project.name, Some(html! { span class="font-mono" { (project.slug) } })))
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
    let content = html! {
        (layout::heading(&environment.name, Some(subtitle)))
        (service_list(&services.items, "No services in this environment yet."))
        (layout::next_page(services.has_more, services.items.last().map(|service| service.id.to_string())))
    };
    let title = format!("{} · {}", environment.name, project.name);
    Ok(layout::page(&signed, &title, &crumbs(&project, None), content))
}

pub async fn service(State(control): State<Control>, signed: Signed, Path(id): Path<String>) -> Result<Markup, WebError> {
    let service = control.get_service(&signed.principal, parse_id(&id)?).await?;
    let environment = control.get_environment(&signed.principal, service.environment_id).await?;
    let project = control.get_project(&signed.principal, environment.project_id).await?;
    let placement = match service.server_id {
        Some(server) => html! { span class="font-mono" { (server) } },
        None => html! { "This machine (the embedded server)" },
    };
    let details: [(&str, Markup); 5] = [
        ("Kind", service_badge(service.kind)),
        ("Slug", html! { span class="font-mono" { (service.slug) } }),
        ("Runs on", placement),
        ("Created", layout::time(service.created_at)),
        ("Updated", layout::time(service.updated_at)),
    ];
    let content = html! {
        (layout::heading(&service.name, None))
        dl class={ (CARD) " divide-y divide-zinc-200 dark:divide-zinc-800" } {
            @for (term, value) in details {
                div class="grid grid-cols-3 gap-4 px-5 py-3 text-sm" {
                    dt class="text-zinc-500 dark:text-zinc-400" { (term) }
                    dd class="col-span-2" { (value) }
                }
            }
        }
    };
    let title = format!("{} · {}", service.name, environment.name);
    Ok(layout::page(&signed, &title, &crumbs(&project, Some(&environment)), content))
}

/// Projects, then the project, then the environment if given.
fn crumbs<'a>(project: &'a Project, environment: Option<&'a Environment>) -> Vec<(&'a str, String)> {
    let mut crumbs = vec![("Projects", "/".to_string()), (project.name.as_str(), format!("/projects/{}", project.id))];
    if let Some(environment) = environment {
        crumbs.push((environment.name.as_str(), format!("/environments/{}", environment.id)));
    }
    crumbs
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
