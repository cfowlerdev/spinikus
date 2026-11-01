//! The overview: the projects the user can see, with their environments.

use axum::extract::{Query, State};
use maud::{Markup, html};
use serde::Deserialize;
use spinikus_control::Control;
use spinikus_core::id::ProjectId;

use super::parse_id;
use crate::{
    auth::Signed,
    error::WebError,
    layout::{self, BUTTON_PRIMARY, CARD, environment_badge},
};

/// How many projects a page shows, and how many environments each.
const PROJECTS_PER_PAGE: u32 = 50;
const ENVIRONMENTS_SHOWN: u32 = 20;

#[derive(Deserialize)]
pub struct After {
    after: Option<String>,
}

pub async fn overview(
    State(control): State<Control>,
    signed: Signed,
    Query(After { after }): Query<After>,
) -> Result<Markup, WebError> {
    let after: Option<ProjectId> = after.as_deref().map(parse_id).transpose()?;
    let projects = control.list_projects(&signed.principal, after, PROJECTS_PER_PAGE).await?;
    let mut rows = Vec::with_capacity(projects.items.len());
    for project in &projects.items {
        let environments = control.list_environments(&signed.principal, project.id, None, ENVIRONMENTS_SHOWN).await?;
        rows.push((project, environments.items));
    }
    let empty = match signed.principal.is_instance_admin() {
        true => "No projects yet. Create one to start deploying.",
        false => "You don't have access to any projects yet. An admin can add you to one.",
    };
    let content = html! {
        (layout::header("Projects", None, html! {
            @if signed.principal.is_instance_admin() { a href="/projects/new" class=(BUTTON_PRIMARY) { "New project" } }
        }))
        @if rows.is_empty() {
            (layout::empty(empty))
        } @else {
            ul class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3" {
                @for (project, environments) in &rows {
                    li class={ (CARD) " flex flex-col p-5 hover:border-zinc-300 dark:hover:border-zinc-700" } {
                        a href={ "/projects/" (project.id) } class="font-semibold hover:text-indigo-600 dark:hover:text-indigo-400" {
                            (project.name)
                        }
                        p class="mt-0.5 font-mono text-xs text-zinc-500 dark:text-zinc-400" { (project.slug) }
                        div class="mt-4 flex flex-wrap gap-2" {
                            @for environment in environments {
                                a href={ "/environments/" (environment.id) } class="inline-flex items-center gap-1.5 text-sm hover:underline" {
                                    (environment.name) (environment_badge(environment.kind))
                                }
                            }
                            @if environments.is_empty() {
                                span class="text-sm text-zinc-500 dark:text-zinc-400" { "No environments" }
                            }
                        }
                    }
                }
            }
            (layout::next_page(projects.has_more, projects.items.last().map(|project| project.id.to_string())))
        }
    };
    Ok(layout::page(&signed, "Projects", &[], content))
}
