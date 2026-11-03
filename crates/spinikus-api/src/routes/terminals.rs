use std::time::Duration;

use axum::{
    extract::{
        State,
        ws::{Message, WebSocket},
    },
    response::Response,
};
use serde::Deserialize;
use spinikus_control::{
    Control,
    terminals::{Terminal, TerminalOptions, TerminalOutput},
};
use spinikus_core::{
    id::{DeploymentId, ServiceId},
    terminal::{ClientMessage, ServerMessage},
};
use tokio_stream::StreamExt;

use crate::{
    error::ApiError,
    extract::{self, Authenticated, Path, Query},
};

/// The largest message a client can send: keystrokes and pastes come in small pieces.
const MAX_MESSAGE_SIZE: usize = 64 * 1024;

/// How often the server pings an idle socket, so proxies between keep it open.
const PING_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct TerminalParams {
    /// The deployment whose container to open it in; the live one by default.
    deployment_id: Option<DeploymentId>,
    /// The command to run, as a JSON array of strings, such as `["psql","-U","app"]`; the
    /// container's shell (bash, else sh) by default.
    command: Option<String>,
    /// The terminal's size in characters; 24 by 80 by default.
    rows: Option<u16>,
    columns: Option<u16>,
}

/// Opens a terminal in the service's running container, as a WebSocket. Binary messages are
/// the terminal's bytes, both ways; the client sends `{"type":"resize","rows","columns"}` as
/// text when its window changes, and the server ends with `{"type":"exit","code"}` or
/// `{"type":"error","message"}` before closing. Closing the socket closes the command's
/// input. Environment admins only; each terminal is recorded in the audit log.
#[utoipa::path(
    get,
    path = "/services/{id}/terminal",
    operation_id = "terminals_open",
    tag = "terminals",
    params(("id" = ServiceId, Path), TerminalParams),
    responses((status = 101, description = "Switched to the terminal's WebSocket.")),
)]
pub async fn open(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
    Query(params): Query<TerminalParams>,
    extract::WebSocket(upgrade): extract::WebSocket,
) -> Result<Response, ApiError> {
    let command = match params.command.as_deref() {
        Some(command) => {
            serde_json::from_str(command).map_err(|_| ApiError::invalid("command", "it must be a JSON array of strings"))?
        }
        None => Vec::new(),
    };
    let options = TerminalOptions {
        deployment_id: params.deployment_id,
        command,
        rows: params.rows.unwrap_or(0),
        columns: params.columns.unwrap_or(0),
    };
    let terminal = control.open_terminal(&principal, service_id, options).await?;
    Ok(upgrade.max_message_size(MAX_MESSAGE_SIZE).on_upgrade(move |socket| relay(socket, terminal)))
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
                let text = serde_json::to_string(&last).expect("serializes");
                let _ = socket.send(Message::Text(text.into())).await;
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
            message = socket.recv() => match message {
                Some(Ok(Message::Binary(data))) => {
                    // Ended: the output says how, next.
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
