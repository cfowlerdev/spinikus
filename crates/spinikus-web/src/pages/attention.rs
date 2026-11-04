//! The overview's "Needs attention" section: what [`Control::attention`] gives the user,
//! each item with what it is, where, why, and a link to where it can be fixed.
//!
//! [`Control::attention`]: spinikus_control::Control::attention

use jiff::Timestamp;
use maud::{Markup, html};
use spinikus_core::{
    attention::{AttentionItem, ServiceRef},
    crash::CrashReason,
    service::ServiceKind,
};

use crate::layout::{self, BADGE, CARD, LINK};

/// Error messages are shortened to this many characters; the linked page has them whole.
const MAX_ERROR_LEN: usize = 160;

#[derive(Clone, Copy)]
enum Severity {
    /// Down or broken now.
    Critical,
    /// Will be, or needs looking at.
    Warning,
}

/// One row: a status word, what happened, where, why, when, and the link.
struct Row<'a> {
    severity: Severity,
    status: &'static str,
    title: String,
    service: Option<&'a ServiceRef>,
    detail: Option<String>,
    when: Option<(&'static str, Timestamp)>,
    href: String,
    action: &'static str,
}

/// The section, or nothing when nothing needs attention.
pub fn section(items: &[AttentionItem]) -> Markup {
    if items.is_empty() {
        return html! {};
    }
    html! {
        section aria-labelledby="attention-heading" class="mb-10" {
            h2 id="attention-heading" class="mb-3 text-lg font-semibold tracking-tight" { "Needs attention" }
            ul id="attention" class={ (CARD) " divide-y divide-zinc-200 dark:divide-zinc-800" } {
                @for item in items { (row(&describe(item))) }
            }
        }
    }
}

fn row(row: &Row) -> Markup {
    let colors = match row.severity {
        Severity::Critical => "bg-red-50 text-red-700 ring-red-600/20 dark:bg-red-500/10 dark:text-red-400 dark:ring-red-500/20",
        Severity::Warning => {
            "bg-amber-50 text-amber-700 ring-amber-600/20 dark:bg-amber-500/10 dark:text-amber-400 dark:ring-amber-500/20"
        }
    };
    html! {
        li class="flex flex-col gap-2 px-4 py-3 sm:flex-row sm:items-start sm:gap-4" {
            span class={ (BADGE) " " (colors) " w-fit shrink-0 justify-center sm:mt-0.5 sm:w-28" } { (row.status) }
            div class="min-w-0 flex-1" {
                p class="text-sm font-medium" { (row.title) }
                @if let Some(service) = row.service {
                    p class="mt-0.5 text-xs text-zinc-500 dark:text-zinc-400" {
                        a href={ "/projects/" (service.project_id) } class="hover:underline" { (service.project_name) }
                        " / "
                        a href={ "/environments/" (service.environment_id) } class="hover:underline" { (service.environment_name) }
                        " / "
                        a href={ "/services/" (service.id) } class="hover:underline" { (service.name) }
                    }
                }
                @if let Some(detail) = &row.detail {
                    p class="mt-1 text-sm break-words text-zinc-600 dark:text-zinc-300" { (detail) }
                }
                @if let Some((label, at)) = row.when {
                    p class="mt-1 text-xs text-zinc-500 dark:text-zinc-400" { (label) " " (layout::time(at)) }
                }
            }
            a href=(row.href) class={ (LINK) " shrink-0 text-sm" } { (row.action) }
        }
    }
}

fn describe(item: &AttentionItem) -> Row<'_> {
    match item {
        AttentionItem::ServerDisconnected { server_id, server_name, last_seen_at } => Row {
            severity: Severity::Critical,
            status: "disconnected",
            title: format!("Server {server_name} is disconnected"),
            service: None,
            detail: Some("Its agent isn't connected, so nothing on it can be deployed, restarted or backed up.".to_string()),
            when: last_seen_at.map(|at| ("Last seen", at)),
            href: format!("/settings/servers/{server_id}"),
            action: "View server",
        },
        AttentionItem::ServerLowOnDisk { server_id, server_name } => Row {
            severity: Severity::Warning,
            status: "low disk",
            title: format!("Server {server_name} is low on disk"),
            service: None,
            detail: Some("Its disk is 90% full or more.".to_string()),
            when: None,
            href: format!("/settings/servers/{server_id}"),
            action: "View server",
        },
        AttentionItem::ServiceCrashing { service, crash_loop } => Row {
            severity: Severity::Critical,
            status: "crashing",
            title: format!("{} is crashing", service.name),
            service: Some(service),
            detail: Some(match crash_loop.reason {
                CrashReason::Restarting => format!("Its container was restarted {} times.", crash_loop.restarts),
                CrashReason::NotRunning => "Its container isn't running.".to_string(),
            }),
            when: Some(("Since", crash_loop.started_at)),
            href: format!("/services/{}/logs?deployment_id={}", service.id, crash_loop.deployment_id),
            action: "View logs",
        },
        AttentionItem::DeploymentFailed { service, deployment_id, reason, message, at } => Row {
            severity: Severity::Critical,
            status: "failed",
            title: format!("{}'s latest deployment failed", service.name),
            service: Some(service),
            detail: message.as_deref().map(shorten).or_else(|| reason.map(|reason| reason.as_str().to_string())),
            when: Some(("At", *at)),
            href: format!("/deployments/{deployment_id}"),
            action: "View deployment",
        },
        AttentionItem::BackupFailed { service, error, at, .. } => Row {
            severity: Severity::Warning,
            status: "failed",
            title: format!("{}'s latest backup failed", service.name),
            service: Some(service),
            detail: error.as_deref().map(shorten),
            when: Some(("At", *at)),
            href: format!("/services/{}/backups", service.id),
            action: "View backups",
        },
        AttentionItem::CertificateFailed { service, hostname, error, expires_at, .. } => Row {
            severity: match expires_at {
                None => Severity::Critical,
                Some(_) => Severity::Warning,
            },
            status: "https",
            title: match expires_at {
                None => format!("No HTTPS certificate for {hostname}"),
                Some(_) => format!("Renewing the HTTPS certificate for {hostname} failed"),
            },
            service: Some(service),
            detail: error.as_deref().map(shorten),
            when: expires_at.map(|at| ("The current one expires", at)),
            href: domains_href(service),
            action: "View domains",
        },
        AttentionItem::CertificateExpiring { service, hostname, expires_at } => Row {
            severity: Severity::Warning,
            status: "https",
            title: format!("The HTTPS certificate for {hostname} expires soon"),
            service: Some(service),
            detail: None,
            when: Some(("Expires", *expires_at)),
            href: domains_href(service),
            action: "View domains",
        },
        AttentionItem::InstanceBackupFailed { error, at, .. } => Row {
            severity: Severity::Warning,
            status: "failed",
            title: "The latest backup of Spinikus failed".to_string(),
            service: None,
            detail: error.as_deref().map(shorten),
            when: Some(("At", *at)),
            href: "/settings/backups".to_string(),
            action: "View backups",
        },
        AttentionItem::NotificationsFailing { channel_id, channel_name, error, at, .. } => Row {
            severity: Severity::Warning,
            status: "failing",
            title: format!("Notifications to {channel_name} can't be sent"),
            service: None,
            detail: error.as_deref().map(shorten),
            when: Some(("Latest", *at)),
            href: format!("/settings/notifications/{channel_id}"),
            action: "View channel",
        },
    }
}

/// Apps have a Domains tab; anything else's hostnames are only its generated one, shown on
/// its page.
fn domains_href(service: &ServiceRef) -> String {
    match service.kind {
        ServiceKind::App => format!("/services/{}/domains", service.id),
        _ => format!("/services/{}", service.id),
    }
}

fn shorten(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(MAX_ERROR_LEN) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::shorten;

    #[test]
    fn shortens_long_errors_on_character_boundaries() {
        assert_eq!(shorten(" short \n"), "short");
        let long = "é".repeat(200);
        assert_eq!(shorten(&long), format!("{}…", "é".repeat(160)));
    }
}
