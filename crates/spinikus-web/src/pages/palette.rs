//! The command palette's index: every project, environment and service the user can see,
//! and the pages they may open, as links for `palette.js` to filter as they type. It is
//! fetched each time the palette opens, so it is never stale.

use maud::{Markup, html};
use spinikus_control::{Control, ControlError, page::MAX_PAGE_SIZE};
use spinikus_core::service::ServiceKind;

use axum::extract::State;

use crate::{auth::Signed, error::WebError};

/// At most this many entries, so a huge instance still opens the palette quickly; the
/// rest is a click away.
const MAX_ENTRIES: usize = 2_000;

struct Entry {
    title: String,
    /// Where it is, such as `Shop / Production`, and what it is.
    context: String,
    href: String,
}

pub async fn palette(State(control): State<Control>, signed: Signed) -> Result<Markup, WebError> {
    let entries = entries(&control, &signed).await?;
    Ok(html! {
        @for entry in &entries {
            li role="none" {
                a role="option" href=(entry.href) data-search=(format!("{} {}", entry.title, entry.context).to_lowercase())
                    class="flex items-baseline gap-3 rounded-md px-3 py-2 text-sm hover:bg-zinc-100 aria-selected:bg-indigo-50 aria-selected:text-indigo-900 dark:hover:bg-zinc-800 dark:aria-selected:bg-indigo-500/15 dark:aria-selected:text-indigo-100" {
                    span class="truncate font-medium" { (entry.title) }
                    span class="ml-auto shrink-0 text-xs text-zinc-500 dark:text-zinc-400" { (entry.context) }
                }
            }
        }
    })
}

async fn entries(control: &Control, signed: &Signed) -> Result<Vec<Entry>, ControlError> {
    let page = |title: &str, href: &str| Entry { title: title.to_string(), context: "page".to_string(), href: href.to_string() };
    let principal = &signed.principal;
    let mut entries = vec![page("Projects", "/")];
    if principal.is_instance_admin() {
        entries.push(page("New project", "/projects/new"));
        entries.push(page("General settings", "/settings/general"));
        entries.push(page("Servers", "/settings/servers"));
        entries.push(page("Storage destinations", "/settings/storage"));
        entries.push(page("Users", "/settings/users"));
        entries.push(page("Audit log", "/settings/audit-log"));
    }
    entries.push(page("Account", "/settings/account"));
    entries.push(page("API tokens", "/settings/tokens"));

    let mut after = None;
    'projects: loop {
        let projects = control.list_projects(principal, after, MAX_PAGE_SIZE).await?;
        after = projects.items.last().map(|project| project.id);
        for project in &projects.items {
            entries.push(Entry {
                title: project.name.clone(),
                context: "project".to_string(),
                href: format!("/projects/{}", project.id),
            });
            let mut after = None;
            loop {
                let environments = control.list_environments(principal, project.id, after, MAX_PAGE_SIZE).await?;
                after = environments.items.last().map(|environment| environment.id);
                for environment in &environments.items {
                    entries.push(Entry {
                        title: environment.name.clone(),
                        context: format!("{} · environment", project.name),
                        href: format!("/environments/{}", environment.id),
                    });
                    let mut after = None;
                    loop {
                        let services = control.list_services(principal, environment.id, after, MAX_PAGE_SIZE).await?;
                        after = services.items.last().map(|service| service.id);
                        for service in &services.items {
                            let kind = match service.kind {
                                ServiceKind::Database => "database",
                                _ => "service",
                            };
                            entries.push(Entry {
                                title: service.name.clone(),
                                context: format!("{} / {} · {kind}", project.name, environment.name),
                                href: format!("/services/{}", service.id),
                            });
                        }
                        if entries.len() >= MAX_ENTRIES {
                            break 'projects;
                        }
                        if !services.has_more {
                            break;
                        }
                    }
                }
                if !environments.has_more {
                    break;
                }
            }
        }
        if !projects.has_more {
            break;
        }
    }
    entries.truncate(MAX_ENTRIES);
    Ok(entries)
}
