//! The audit log: all of it in the instance settings for instance admins, and a project's
//! part on its Audit log tab for its admins. Newest first, a page at a time.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use maud::{Markup, html};
use serde::Deserialize;
use serde_json::Value;
use spinikus_control::{AuditPage, Control, ControlError};
use spinikus_core::{
    actor::Actor,
    audit::AuditEntry,
    id::{AuditEntryId, ProjectId, ServerId, UserId},
    membership::Role,
    project::Project,
};

use super::{
    Tab, crumbs, parse_id, project_tabs,
    settings::{SettingsTab, require_admin, tabs},
};
use crate::{
    auth::Signed,
    error::WebError,
    layout::{self, CARD, LINK},
};

const PAGE_SIZE: u32 = 50;

/// Longest a detail value is shown.
const MAX_DETAIL_LEN: usize = 120;

#[derive(Deserialize)]
pub struct Params {
    /// The cursor: entries older than this one. Ignored if it doesn't parse.
    #[serde(default)]
    before: String,
}

impl Params {
    fn before(&self) -> Option<AuditEntryId> {
        self.before.parse().ok()
    }
}

pub async fn instance(State(control): State<Control>, signed: Signed, Query(params): Query<Params>) -> Result<Markup, WebError> {
    require_admin(&signed)?;
    let page = control.list_audit_entries(&signed.principal, None, params.before(), PAGE_SIZE).await?;
    let names = names(&control, &signed, &page).await?;
    let content = html! {
        (tabs(&signed, SettingsTab::AuditLog))
        p class="mb-3 text-sm text-zinc-500 dark:text-zinc-400" { "Everything done in this instance, newest first. Entries can't be changed or removed." }
        (entries(&signed, &page, &names, "/settings/audit-log"))
    };
    Ok(layout::page(&signed, "Audit log · Settings", &[], content))
}

pub async fn project(
    State(control): State<Control>,
    signed: Signed,
    Path(id): Path<String>,
    Query(params): Query<Params>,
) -> Result<Markup, WebError> {
    let id: ProjectId = parse_id(&id)?;
    let project: Project = control.get_project(&signed.principal, id).await?;
    let page = control.list_audit_entries(&signed.principal, Some(project.id), params.before(), PAGE_SIZE).await?;
    let names = names(&control, &signed, &page).await?;
    let base = format!("/projects/{}/audit-log", project.id);
    let content = html! {
        (layout::heading(&project.name, Some(html! { span class="font-mono" { (project.slug) } })))
        (project_tabs(&project, Some(Role::Admin), Tab::AuditLog))
        p class="mb-3 text-sm text-zinc-500 dark:text-zinc-400" { "What was done in this project, newest first." }
        (entries(&signed, &page, &names, &base))
    };
    let crumbs = &crumbs(&project, None)[..1];
    Ok(layout::page(&signed, &format!("Audit log · {}", project.name), crumbs, content))
}

/// What to call each actor: users by name, servers by name.
async fn names(control: &Control, signed: &Signed, page: &AuditPage) -> Result<HashMap<String, String>, ControlError> {
    let mut names: HashMap<String, String> =
        page.users.iter().map(|user| (user.id.to_string(), format!("{} <{}>", user.name, user.email.as_str()))).collect();
    let mut servers: Vec<ServerId> = page
        .entries
        .items
        .iter()
        .filter_map(|entry| match entry.actor {
            Actor::Server(id) => Some(id),
            _ => None,
        })
        .collect();
    servers.sort();
    servers.dedup();
    for id in servers {
        match control.get_server(&signed.principal, id).await {
            Ok(server) => {
                names.insert(id.to_string(), format!("server {}", server.name));
            }
            Err(ControlError::NotFound { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(names)
}

fn entries(signed: &Signed, page: &AuditPage, names: &HashMap<String, String>, base: &str) -> Markup {
    let entries = &page.entries;
    html! {
        @if entries.items.is_empty() {
            (layout::empty("Nothing yet."))
        } @else {
            ol class={ (CARD) " divide-y divide-zinc-200 dark:divide-zinc-800" } {
                @for entry in &entries.items { (row(signed, entry, names)) }
            }
            @if let (true, Some(last)) = (entries.has_more, entries.items.last()) {
                div class="mt-4" { a href={ (base) "?before=" (last.id) } class=(LINK) { "Older entries" } }
            }
        }
    }
}

fn actor(entry: &AuditEntry, names: &HashMap<String, String>) -> String {
    let named = |id: String, fallback: &str| names.get(&id).cloned().unwrap_or_else(|| format!("{fallback} {id}"));
    match entry.actor {
        Actor::System => "Spinikus".to_string(),
        Actor::User(id) => named(id.to_string(), "user"),
        Actor::Server(id) => named(id.to_string(), "server"),
        Actor::Webhook(_) => "a Git webhook".to_string(),
    }
}

/// A link to what an entry is about, for the kinds of IDs that have a page the reader may open.
fn target_href(signed: &Signed, target: &str) -> Option<String> {
    let (prefix, _) = target.split_once('_')?;
    let admin = signed.principal.is_instance_admin();
    match prefix {
        "prj" => Some(format!("/projects/{target}")),
        "env" => Some(format!("/environments/{target}")),
        "svc" => Some(format!("/services/{target}")),
        "dep" => Some(format!("/deployments/{target}")),
        "usr" if admin => target.parse::<UserId>().ok().map(|id| format!("/settings/users/{id}")),
        "srv" if admin => Some(format!("/settings/servers/{target}")),
        "dst" if admin => Some(format!("/settings/storage/{target}")),
        _ => None,
    }
}

/// A detail's value: strings as they are, everything else as JSON, shortened.
fn detail(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        value => value.to_string(),
    };
    match text.char_indices().nth(MAX_DETAIL_LEN) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text,
    }
}

fn row(signed: &Signed, entry: &AuditEntry, names: &HashMap<String, String>) -> Markup {
    html! {
        li class="grid gap-x-4 gap-y-1 px-5 py-3 text-sm sm:grid-cols-[11rem_1fr]" {
            span class="text-xs text-zinc-500 sm:text-sm dark:text-zinc-400" { (layout::time(entry.created_at)) }
            div class="min-w-0" {
                p class="flex flex-wrap items-baseline gap-x-2" {
                    span class="font-mono font-medium" { (entry.action.as_str()) }
                    @if let Some(target) = &entry.target_id {
                        @match target_href(signed, target) {
                            Some(href) => a href=(href) class={ (LINK) " font-mono text-xs break-all" } { (target) },
                            None => span class="font-mono text-xs break-all text-zinc-500 dark:text-zinc-400" { (target) },
                        }
                    }
                }
                p class="text-xs text-zinc-500 dark:text-zinc-400" {
                    "by " (actor(entry, names))
                    @if entry.token_id.is_some() { " with an API token" }
                }
                @if !entry.details.is_empty() {
                    p class="mt-1 font-mono text-xs break-all text-zinc-600 dark:text-zinc-300" {
                        @for (index, (key, value)) in entry.details.iter().enumerate() {
                            @if index > 0 { " · " }
                            span class="text-zinc-400 dark:text-zinc-500" { (key) "=" } (detail(value))
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortens_long_details_on_character_boundaries() {
        assert_eq!(detail(&Value::String("é".repeat(MAX_DETAIL_LEN + 5))), format!("{}…", "é".repeat(MAX_DETAIL_LEN)));
        assert_eq!(detail(&serde_json::json!(["name"])), r#"["name"]"#);
        assert_eq!(detail(&serde_json::json!("plain")), "plain");
    }
}
