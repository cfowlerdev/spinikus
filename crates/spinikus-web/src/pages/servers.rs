//! Servers, in the instance settings: adding one and connecting its agent with a join
//! token, its health as the agent reports it, and revoking it. Instance admins only. Also
//! the server choice of the service and database forms, which anyone editing them sees.

use axum::{
    Form,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use serde::Deserialize;
use spinikus_control::{Control, ControlError, JoinToken, NewServer, page::MAX_PAGE_SIZE};
use spinikus_core::{
    id::ServerId,
    server::{Server, ServerStatus},
};

use super::{
    backups::human_size,
    parse_id, problem_message, redirect,
    settings::{SettingsTab, require_admin, tabs},
};
use crate::{
    auth::Signed,
    error::WebError,
    forms::{self, Input, Notice},
    layout::{self, BADGE, CARD, LINK},
};

const URL: &str = "/settings/servers";

/// The agent listener's port unless configured otherwise, for the join command.
const DEFAULT_AGENTS_PORT: u16 = 8443;

#[derive(Deserialize, Default)]
pub struct NewServerForm {
    #[serde(default)]
    name: String,
}

pub async fn servers(State(control): State<Control>, signed: Signed) -> Result<Markup, WebError> {
    require_admin(&signed)?;
    Ok(list_page(&control, &signed, &NewServerForm::default(), Notice::None).await?)
}

pub async fn create(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Form(form): Form<NewServerForm>,
) -> Result<Response, WebError> {
    require_admin(&signed)?;
    match control.create_server(&signed.principal, NewServer { name: form.name.clone() }).await {
        Ok(server) => Ok(redirect(&headers, &format!("{URL}/{}", server.id))),
        Err(error) => {
            let message = problem_message(error)?;
            let page = list_page(&control, &signed, &form, Notice::Problem(&message)).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
    }
}

async fn list_page(control: &Control, signed: &Signed, form: &NewServerForm, notice: Notice<'_>) -> Result<Markup, ControlError> {
    let servers = all_servers(control, signed).await?;
    let fields = html! {
        (Input::text("server-name", "name", "Name", &form.name).required().placeholder("web-2").render())
    };
    let content = html! {
        (tabs(signed, SettingsTab::Servers))
        div class="space-y-8" {
            section {
                h2 class="mb-1 font-semibold" { "Servers" }
                p class="mb-3 text-sm text-zinc-500 dark:text-zinc-400" {
                    "Machines whose agents run services. Each agent connects to the control plane; revoked servers are not listed."
                }
                @if servers.is_empty() {
                    (layout::empty("No servers yet."))
                } @else {
                    ul class={ (CARD) " divide-y divide-zinc-200 dark:divide-zinc-800" } {
                        @for server in &servers { (server_row(server)) }
                    }
                }
            }
            div class="max-w-2xl" {
                (forms::card(signed, "Add a server", Some("Then connect its agent with a join token from the server's page."), URL, notice, fields, "Add server"))
            }
        }
    };
    Ok(layout::page(signed, "Servers · Settings", &[], content))
}

/// Every server that is not revoked.
async fn all_servers(control: &Control, signed: &Signed) -> Result<Vec<Server>, ControlError> {
    let mut servers = Vec::new();
    let mut after = None;
    loop {
        let page = control.list_servers(&signed.principal, after, MAX_PAGE_SIZE).await?;
        after = page.items.last().map(|server| server.id);
        servers.extend(page.items);
        if !page.has_more {
            return Ok(servers);
        }
    }
}

fn server_row(server: &Server) -> Markup {
    html! {
        li class="flex flex-wrap items-center gap-x-4 gap-y-1 px-5 py-3 text-sm" {
            div class="min-w-0" {
                a href={ (URL) "/" (server.id) } class="font-medium hover:text-indigo-600 dark:hover:text-indigo-400" { (server.name) }
                @if server.embedded { " " span class="text-zinc-500 dark:text-zinc-400" { "(this control plane)" } }
                @if let Some(agent) = &server.agent {
                    p class="text-xs text-zinc-500 dark:text-zinc-400" {
                        (agent.facts.hostname) " · " (agent.facts.os) "/" (agent.facts.arch) " · agent " (agent.agent_version)
                    }
                }
            }
            div class="ml-auto flex items-center gap-3" {
                @if let Some(seen) = server.last_seen_at {
                    span class="text-xs text-zinc-500 dark:text-zinc-400" { "seen " (layout::time(seen)) }
                }
                (status_badge(server.status))
            }
        }
    }
}

pub fn status_badge(status: ServerStatus) -> Markup {
    let (label, colors) = match status {
        ServerStatus::Connected => (
            "connected",
            "bg-emerald-50 text-emerald-700 ring-emerald-600/20 dark:bg-emerald-500/10 dark:text-emerald-400 dark:ring-emerald-500/20",
        ),
        ServerStatus::Pending => (
            "waiting for its agent",
            "bg-sky-50 text-sky-700 ring-sky-600/20 dark:bg-sky-500/10 dark:text-sky-400 dark:ring-sky-500/20",
        ),
        ServerStatus::Disconnected => (
            "disconnected",
            "bg-amber-50 text-amber-700 ring-amber-600/20 dark:bg-amber-500/10 dark:text-amber-400 dark:ring-amber-500/20",
        ),
        ServerStatus::Revoked => {
            ("revoked", "bg-red-50 text-red-700 ring-red-600/20 dark:bg-red-500/10 dark:text-red-400 dark:ring-red-500/20")
        }
    };
    html! { span class={ (BADGE) " " (colors) } { (label) } }
}

pub async fn server(State(control): State<Control>, signed: Signed, Path(id): Path<String>) -> Result<Markup, WebError> {
    require_admin(&signed)?;
    let server = control.get_server(&signed.principal, parse_id(&id)?).await?;
    Ok(server_page(&signed, &server, None))
}

/// A join token for the server's agent, shown on the server's page this once.
pub async fn join_token(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, WebError> {
    require_admin(&signed)?;
    let server = control.get_server(&signed.principal, parse_id(&id)?).await?;
    let join = control.create_join_token(&signed.principal, server.id).await?;
    let page = server_page(&signed, &server, Some(Join { token: &join, host: host(&headers) }));
    // The page is the server's, whatever URL the button posted to.
    Ok((StatusCode::OK, [("hx-replace-url", format!("{URL}/{}", server.id))], page).into_response())
}

pub async fn revoke(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, WebError> {
    require_admin(&signed)?;
    let server = control.get_server(&signed.principal, parse_id(&id)?).await?;
    match control.revoke_server(&signed.principal, server.id).await {
        Ok(()) => Ok(redirect(&headers, &format!("{URL}/{}", server.id))),
        // Already revoked: the page says so.
        Err(ControlError::NotFound { .. }) => Ok(redirect(&headers, &format!("{URL}/{}", server.id))),
        Err(error) => Err(error.into()),
    }
}

/// The host the browser reached the dashboard at, without its port: most likely the host
/// agents reach the control plane at too.
fn host(headers: &HeaderMap) -> String {
    let host = headers.get(header::HOST).and_then(|value| value.to_str().ok()).unwrap_or("localhost");
    match host.rsplit_once(':') {
        // Not the colons of a bare IPv6 address.
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) && !name.ends_with(':') => name.to_string(),
        _ => host.to_string(),
    }
}

struct Join<'a> {
    token: &'a JoinToken,
    host: String,
}

fn server_page(signed: &Signed, server: &Server, join: Option<Join<'_>>) -> Markup {
    let base = format!("{URL}/{}", server.id);
    let content = html! {
        (tabs(signed, SettingsTab::Servers))
        div class="max-w-2xl space-y-6" {
            section class={ (CARD) " p-6" } {
                div class="flex flex-wrap items-center gap-3" {
                    h2 class="font-semibold" { (server.name) }
                    (status_badge(server.status))
                }
                dl class="mt-4 grid grid-cols-[auto_1fr] gap-x-6 gap-y-2 text-sm" {
                    dt class="text-zinc-500 dark:text-zinc-400" { "ID" }
                    dd class="font-mono break-all" { (server.id) }
                    @if let Some(agent) = &server.agent {
                        dt class="text-zinc-500 dark:text-zinc-400" { "Host" }
                        dd { (agent.facts.hostname) }
                        dt class="text-zinc-500 dark:text-zinc-400" { "System" }
                        dd { (agent.facts.os) "/" (agent.facts.arch) ", " (agent.facts.cpu_count) " CPUs, " (human_size(agent.facts.memory_bytes)) " memory" }
                        dt class="text-zinc-500 dark:text-zinc-400" { "Agent" }
                        dd { (agent.agent_version) " (protocol " (agent.protocol_version) ")" }
                    }
                    dt class="text-zinc-500 dark:text-zinc-400" { "Last seen" }
                    dd { @if let Some(seen) = server.last_seen_at { (layout::time(seen)) } @else { "never" } }
                    dt class="text-zinc-500 dark:text-zinc-400" { "Added" }
                    dd { (layout::time(server.created_at)) }
                    @if let Some(revoked) = server.revoked_at {
                        dt class="text-zinc-500 dark:text-zinc-400" { "Revoked" }
                        dd { (layout::time(revoked)) }
                    }
                }
            }
            @if server.embedded {
                p class="text-sm text-zinc-500 dark:text-zinc-400" {
                    "This is the control plane's own server: its agent runs in the control plane's process and enrolls itself. "
                    "Services and databases run here unless they name another server."
                }
            } @else if server.revoked_at.is_none() {
                @if let Some(join) = join {
                    (join_card(server, join))
                } @else {
                    section class={ (CARD) " p-6" } {
                        h2 class="font-semibold" { "Connect an agent" }
                        p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400" {
                            "Install Spinikus on the server, then enroll its agent with a join token. A token works once, for an hour."
                        }
                        form method="post" action={ (base) "/join-token" } class="mt-4" {
                            (layout::csrf_field(signed))
                            button type="submit" class=(layout::BUTTON_PRIMARY) { "Create a join token" }
                        }
                    }
                }
                (forms::danger(signed, "Revoke server",
                    "Its agent is disconnected and can never connect again, and its join tokens stop working. Deployments on it fail; containers already running there keep running.",
                    &format!("{base}/revoke"), "Revoke server", "Revoke this server? This can't be undone."))
            }
            div class="text-sm" { a href=(URL) class=(LINK) { "Back to servers" } }
        }
    };
    layout::page(signed, &format!("{} · Servers", server.name), &[], content)
}

fn join_card(server: &Server, join: Join<'_>) -> Markup {
    let command = format!(
        "SPINIKUS_JOIN_TOKEN={} spinikus agent join https://{}:{DEFAULT_AGENTS_PORT} --ca-fingerprint {}",
        join.token.secret.expose(),
        join.host,
        join.token.ca_fingerprint,
    );
    html! {
        section class={ (CARD) " p-6" } {
            h2 class="font-semibold" { "Connect an agent" }
            p role="status" class="mt-4 rounded-md bg-emerald-50 px-3 py-2 text-sm text-emerald-700 dark:bg-emerald-500/10 dark:text-emerald-400" {
                "Here is a join token for " (server.name) ". It is shown only this once and works until " (layout::time(join.token.token.expires_at)) "."
            }
            ol class="mt-4 list-decimal space-y-3 pl-5 text-sm" {
                li {
                    "On the server, in the directory with its " code class="font-mono" { "spinikus.toml" } ", enroll the agent:"
                    pre class="mt-2 whitespace-pre-wrap break-all rounded-md bg-zinc-100 p-3 font-mono text-xs select-all dark:bg-zinc-800" { (command) }
                    p class="mt-1 text-xs text-zinc-500 dark:text-zinc-400" {
                        "If the server reaches the control plane's agent listener (" code class="font-mono" { "[agents] listen" } ", port 8443 by default) at another address or port, use that."
                    }
                }
                li {
                    "Then run it:"
                    pre class="mt-2 rounded-md bg-zinc-100 p-3 font-mono text-xs select-all dark:bg-zinc-800" { "spinikus agent run" }
                }
            }
        }
    }
}

/// Where a service can run, for a `server_id` select: the default (empty, the control
/// plane's server), then the other servers, and `current` even if it was revoked. `None`
/// when there is nothing to choose.
pub(super) async fn choices(
    control: &Control,
    signed: &Signed,
    current: Option<ServerId>,
) -> Result<Option<Vec<(String, String)>>, ControlError> {
    let servers = all_servers(control, signed).await?;
    let embedded = servers.iter().find(|server| server.embedded);
    let default = match embedded {
        Some(server) => format!("{} (the control plane's server)", server.name),
        None => "The control plane's server".to_string(),
    };
    let mut choices = vec![(String::new(), default)];
    for server in servers.iter().filter(|server| !server.embedded || Some(server.id) == current) {
        choices.push((server.id.to_string(), server.name.clone()));
    }
    if let Some(current) = current
        && !servers.iter().any(|server| server.id == current)
    {
        let name = match control.get_server(&signed.principal, current).await {
            Ok(server) => format!("{} (revoked)", server.name),
            Err(ControlError::NotFound { .. }) => format!("{current} (gone)"),
            Err(error) => return Err(error),
        };
        choices.push((current.to_string(), name));
    }
    Ok((choices.len() > 1).then_some(choices))
}

/// The select for [`choices`].
pub(super) fn select(id: &str, choices: &[(String, String)], selected: &str) -> Markup {
    let options: Vec<(&str, &str)> = choices.iter().map(|(value, label)| (value.as_str(), label.as_str())).collect();
    forms::select(id, "server_id", "Server", &options, selected)
}

/// A `server_id` field: empty for the default.
pub(super) fn parse_choice(value: &str) -> Result<Option<ServerId>, ControlError> {
    match value.trim() {
        "" => Ok(None),
        value => value
            .parse()
            .map(Some)
            .map_err(|_| ControlError::Invalid { field: "server_id", problem: format!("no server `{value}`") }),
    }
}
