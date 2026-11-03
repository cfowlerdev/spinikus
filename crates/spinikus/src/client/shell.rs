//! `spinikus shell`: a terminal in a service's running container, over the API's WebSocket
//! (`GET /services/{id}/terminal`). Binary messages are the terminal's bytes both ways; the
//! server ends with `{"type":"exit","code"}` or `{"type":"error","message"}`. From a terminal,
//! ours goes into raw mode, so keys such as Ctrl+C reach the command, and its window changes
//! are sent along. With stdin from a pipe, the input is sent as typed, then Ctrl+D, which a
//! shell reads as the end; the remote side always has a terminal, so it echoes the input. The
//! CLI exits with the command's status.

use std::io::{IsTerminal, Read, Write};

use anyhow::Context;
use futures_util::SinkExt;
use spinikus_core::terminal::{ClientMessage, ServerMessage};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_tungstenite::tungstenite::Message;

use super::{
    Session,
    args::ShellArgs,
    error::{CommandExited, Exit, fail},
};

/// The end of input, as a terminal reads it.
const END_OF_INPUT: u8 = 0x04;

pub async fn shell(session: &Session, args: ShellArgs) -> anyhow::Result<()> {
    let service = session.service(args.service.as_deref()).await?;
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let mut query = Vec::new();
    if let Some((rows, columns)) = window_size() {
        query.extend([("rows", rows.to_string()), ("columns", columns.to_string())]);
    }
    query.extend(args.deployment.map(|deployment| ("deployment_id", deployment)));
    if !args.command.is_empty() {
        query.push(("command", serde_json::to_string(&args.command)?));
    }
    let mut socket = session.api.websocket(&format!("/services/{}/terminal", service.id), &query).await?;
    let raw = match interactive {
        true => Some(raw::enable().context("Couldn't put the terminal in raw mode")?),
        false => None,
    };
    let ended = relay(&mut socket, interactive).await;
    drop(raw);
    match ended? {
        0 => Ok(()),
        code => Err(CommandExited(u8::try_from(code).unwrap_or(Exit::Failure as u8)).into()),
    }
}

/// Passes stdin to the socket and its output to stdout until the command exits, and returns
/// its exit code.
async fn relay(socket: &mut super::api::WebSocket, interactive: bool) -> anyhow::Result<i64> {
    let mut input = read_stdin();
    let mut resized = window_changes();
    let mut stdout = std::io::stdout();
    let mut stdin_open = true;
    loop {
        tokio::select! {
            chunk = input.recv(), if stdin_open => {
                let data = match chunk {
                    Some(chunk) => chunk,
                    None => {
                        stdin_open = false;
                        if interactive {
                            continue;
                        }
                        vec![END_OF_INPUT]
                    }
                };
                socket.send(Message::Binary(data.into())).await.context("The connection broke off")?;
            }
            Some(()) = resized.recv() => {
                if let Some((rows, columns)) = window_size() {
                    let resize = serde_json::to_string(&ClientMessage::Resize { rows, columns })?;
                    socket.send(Message::Text(resize.into())).await.context("The connection broke off")?;
                }
            }
            message = socket.next() => match message {
                Some(Ok(Message::Binary(data))) => {
                    stdout.write_all(&data)?;
                    stdout.flush()?;
                }
                Some(Ok(Message::Text(text))) => match serde_json::from_str(&text) {
                    Ok(ServerMessage::Exit { code }) => return Ok(code),
                    Ok(ServerMessage::Error { message }) => return Err(fail(Exit::Failure, message)),
                    Err(_) => {}
                },
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error).context("The connection broke off"),
                None => anyhow::bail!("The connection closed before the command exited"),
            },
        }
    }
}

/// Reads stdin on its own thread, which may stay blocked in a read when the command exits; the
/// process ends without waiting for it.
fn read_stdin() -> mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel(16);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buffer = [0; 4096];
        loop {
            match stdin.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(read) => {
                    if sender.blocking_send(buffer[..read].to_vec()).is_err() {
                        return;
                    }
                }
            }
        }
    });
    receiver
}

/// The terminal's size in rows and columns, if stdout is one.
#[cfg(unix)]
fn window_size() -> Option<(u16, u16)> {
    let stdout = std::io::stdout();
    if !stdout.is_terminal() {
        return None;
    }
    let size = rustix::termios::tcgetwinsize(&stdout).ok()?;
    (size.ws_row > 0 && size.ws_col > 0).then_some((size.ws_row, size.ws_col))
}

#[cfg(not(unix))]
fn window_size() -> Option<(u16, u16)> {
    None
}

/// A message each time the window changes size.
#[cfg(unix)]
fn window_changes() -> mpsc::Receiver<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let (sender, receiver) = mpsc::channel(1);
    if let Ok(mut changes) = signal(SignalKind::window_change()) {
        tokio::spawn(async move {
            while changes.recv().await.is_some() {
                // A change already waiting to be sent covers this one.
                let _ = sender.try_send(());
            }
        });
    }
    receiver
}

#[cfg(not(unix))]
fn window_changes() -> mpsc::Receiver<()> {
    mpsc::channel(1).1
}

#[cfg(unix)]
mod raw {
    use rustix::termios::{OptionalActions, Termios, tcgetattr, tcsetattr};

    /// Raw mode on stdin until dropped, which puts the terminal back as it was.
    pub struct Raw(Termios);

    pub fn enable() -> std::io::Result<Raw> {
        let stdin = std::io::stdin();
        let original = tcgetattr(&stdin)?;
        let mut raw = original.clone();
        raw.make_raw();
        tcsetattr(&stdin, OptionalActions::Now, &raw)?;
        Ok(Raw(original))
    }

    impl Drop for Raw {
        fn drop(&mut self) {
            let _ = tcsetattr(std::io::stdin(), OptionalActions::Now, &self.0);
        }
    }
}

#[cfg(not(unix))]
mod raw {
    pub struct Raw;

    pub fn enable() -> std::io::Result<Raw> {
        Ok(Raw)
    }
}
