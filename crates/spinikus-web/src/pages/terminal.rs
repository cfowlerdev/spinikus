//! A terminal in a service's running container: a shell, drawn by the vendored xterm.js and
//! driven by `terminal.js` over a WebSocket to `/services/{id}/terminal/socket`. Environment
//! admins only, like `Control::open_terminal`. The page is loaded in full rather than boosted,
//! so its scripts run and leaving it closes the socket, and it is the one page whose CSP allows
//! inline styles, which xterm.js sets as it draws.

use std::time::Duration;

use axum::{
    extract::{
        Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
};
use maud::html;
use serde::Deserialize;
use spinikus_control::{
    Control, ControlError,
    terminals::{Terminal, TerminalOptions, TerminalOutput},
};
use spinikus_core::{
    id::{DeploymentId, ServiceId},
    membership::Role,
    terminal::{ClientMessage, ServerMessage},
};
use tokio_stream::StreamExt;

use super::{Tab, crumbs, parse_id, refused, service_tabs};
use crate::{
    auth::{Signed, websocket_from_this_site},
    error::WebError,
    layout::{self, BUTTON_SECONDARY, Head, LINK},
};

/// The terminal page's CSP: the usual one, with inline styles, which xterm.js needs.
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
     img-src 'self' data:; connect-src 'self'; font-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

/// The largest message the browser can send: keystrokes and pastes come in small pieces.
const MAX_MESSAGE_SIZE: usize = 64 * 1024;

/// How often the server pings an idle socket, so proxies between keep it open.
const PING_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Deserialize, Default)]
pub struct TerminalParams {
    #[serde(default)]
    deployment_id: String,
    #[serde(default)]
    rows: Option<u16>,
    #[serde(default)]
    columns: Option<u16>,
}

impl TerminalParams {
    fn deployment(&self) -> Result<Option<DeploymentId>, WebError> {
        match self.deployment_id.trim() {
            "" => Ok(None),
            id => parse_id(id).map(Some),
        }
    }
}

/// The URL of the terminal page, for the service's live deployment or the one given.
pub fn url(service: ServiceId, deployment: Option<DeploymentId>) -> String {
    match deployment {
        Some(deployment) => format!("/services/{service}/terminal?deployment_id={deployment}"),
        None => format!("/services/{service}/terminal"),
    }
}

pub async fn terminal(
    State(control): State<Control>,
    signed: Signed,
    Path(id): Path<String>,
    Query(params): Query<TerminalParams>,
) -> Result<Response, WebError> {
    let principal = &signed.principal;
    let service = control.get_service(principal, parse_id(&id)?).await?;
    let deployment = params.deployment()?;
    let environment = control.get_environment(principal, service.environment_id).await?;
    let project = control.get_project(principal, environment.project_id).await?;
    let role = control.environment_role(principal, &environment).await?;
    if role < Role::Admin {
        return Err(ControlError::Forbidden.into());
    }
    let mut socket = format!("/services/{}/terminal/socket", service.id);
    if let Some(deployment) = deployment {
        socket.push_str(&format!("?deployment_id={deployment}"));
    }
    let content = html! {
        (layout::heading(&service.name, Some(html! { span class="font-mono" { (service.slug) } })))
        (service_tabs(&service, role, Tab::Terminal))
        div class="mb-3 flex flex-wrap items-baseline gap-x-4 gap-y-1 text-sm" {
            p class="text-zinc-500 dark:text-zinc-400" {
                @match deployment {
                    Some(deployment) => {
                        "A shell in the container of deployment "
                        a href={ "/deployments/" (deployment) } class={ (LINK) " font-mono" } { (deployment) }
                        ". "
                    }
                    None => { "A shell in the container that runs now. " }
                }
                "Changes to its files are lost when it is replaced; this is for looking and fixing, not for deploying."
            }
            p id="terminal-status" role="status" class="ml-auto font-medium text-zinc-600 dark:text-zinc-300" { "Connecting…" }
            button id="terminal-reconnect" type="button" hidden class=(BUTTON_SECONDARY) { "Open again" }
        }
        div id="terminal" data-socket=(socket)
            class="h-[32rem] overflow-hidden rounded-lg bg-zinc-950 p-2 ring-1 ring-zinc-800" {}
    };
    let mut crumbs = crumbs(&project, Some(&environment));
    crumbs.push((service.name.as_str(), format!("/services/{}", service.id)));
    let head = Head { boost: false, scripts: &["xterm.min.js", "xterm-addon-fit.min.js", "terminal.js"], styles: &["xterm.css"] };
    let page = layout::page_with(&signed, &format!("Terminal · {}", service.name), &crumbs, content, head);
    let mut response = page.into_response();
    response.headers_mut().insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CONTENT_SECURITY_POLICY));
    Ok(response)
}

/// The terminal's WebSocket. It opens the terminal once upgraded, so a refusal reaches the
/// page as a message rather than a failed handshake the browser can't explain.
pub async fn socket(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<TerminalParams>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, WebError> {
    if !websocket_from_this_site(&headers) {
        return Err(WebError::CrossSite);
    }
    let service_id: ServiceId = parse_id(&id)?;
    let options = TerminalOptions {
        deployment_id: params.deployment()?,
        command: Vec::new(),
        rows: params.rows.unwrap_or(0),
        columns: params.columns.unwrap_or(0),
    };
    Ok(upgrade.max_message_size(MAX_MESSAGE_SIZE).on_upgrade(move |mut socket| async move {
        match control.open_terminal(&signed.principal, service_id, options).await {
            Ok(terminal) => relay(socket, terminal).await,
            Err(error) => {
                let message = refused(&error).unwrap_or_else(|| match error {
                    ControlError::NotFound { .. } => "Not found.".to_string(),
                    ControlError::Forbidden => "Only the environment's admins may open a terminal.".to_string(),
                    error => {
                        tracing::error!(%error, "failed to open a terminal");
                        "Something went wrong.".to_string()
                    }
                });
                finish(&mut socket, ServerMessage::Error { message }).await;
            }
        }
    }))
}

/// Says how the terminal ended and closes the socket.
async fn finish(socket: &mut WebSocket, last: ServerMessage) {
    let text = serde_json::to_string(&last).expect("serializes");
    let _ = socket.send(Message::Text(text.into())).await;
    let _ = socket.send(Message::Close(None)).await;
}

/// Passes the socket's messages to the terminal and its output back, until either ends.
async fn relay(mut socket: WebSocket, terminal: Terminal) {
    let Terminal { input, mut output, .. } = terminal;
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await;
    loop {
        tokio::select! {
            item = output.next() => {
                let last = match item {
                    Some(TerminalOutput::Data(data)) => {
                        if socket.send(Message::Binary(data.into())).await.is_err() {
                            return;
                        }
                        continue;
                    }
                    Some(TerminalOutput::Exited(code)) => ServerMessage::Exit { code },
                    Some(TerminalOutput::Ended { error }) => ServerMessage::Error { message: error },
                    None => ServerMessage::Error { message: "the terminal ended".to_string() },
                };
                finish(&mut socket, last).await;
                return;
            }
            message = socket.recv() => match message {
                Some(Ok(Message::Binary(data))) => {
                    let _ = input.write(data.to_vec()).await;
                }
                Some(Ok(Message::Text(text))) => {
                    if let Ok(ClientMessage::Resize { rows, columns }) = serde_json::from_str(&text) {
                        let _ = input.resize(rows, columns).await;
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
            },
            _ = ping.tick() => {
                if socket.send(Message::Ping(Default::default())).await.is_err() {
                    return;
                }
            }
        }
    }
}
