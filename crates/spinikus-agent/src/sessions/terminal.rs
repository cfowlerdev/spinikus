//! Terminal sessions: a command run in a container with a TTY. The control plane's `data`
//! frames are the terminal's input and its `resize` frames change its size; the terminal's
//! output goes back as `data`, then `closed` with the command's exit code. When the control
//! plane closes its side, the terminal gets Ctrl+D, which ends a shell at its prompt, and then
//! its input closes. Docker has no hangup for an exec, so a program that ignores Ctrl+D keeps
//! running until it exits or the container goes.

use std::time::Duration;

use spinikus_proto::agent::v1::{self as pb, session_frame::Frame};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::{StreamExt, wrappers::ReceiverStream};
use tonic::Streaming;

use super::{CLOSE_GRACE, closed};
use crate::runtime::{ContainerRuntime, ExecOutput, TerminalInput, TerminalSize};

/// What runs when the control plane names no command: bash if the image has it, else sh.
pub(crate) const SHELL: [&str; 3] = ["/bin/sh", "-c", "if command -v bash >/dev/null 2>&1; then exec bash; fi; exec sh"];

/// Ctrl+D: the end of input, as a terminal reads it.
const END_OF_INPUT: u8 = 0x04;

/// The size of a terminal whose size was not given.
const DEFAULT_SIZE: TerminalSize = TerminalSize { rows: 24, columns: 80 };

/// The largest size a terminal can have, each way.
const MAX_ROWS: u32 = 500;
const MAX_COLUMNS: u32 = 1000;

/// How long a command whose input was closed has to exit before the session stops reading it.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// Serves the terminal until its command exits or the control plane goes. Takes the inbound
/// stream, and waits for the control plane to close it itself.
pub(super) async fn serve(
    runtime: &dyn ContainerRuntime,
    terminal: pb::TerminalSession,
    sender: &mpsc::Sender<pb::SessionFrame>,
    inbound: Streaming<pb::SessionFrame>,
) -> Option<Streaming<pb::SessionFrame>> {
    match runtime.inspect(&terminal.container_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            let _ = sender.send(closed(format!("no container `{}` managed by Spinikus", terminal.container_id))).await;
            return Some(inbound);
        }
        Err(error) => {
            let _ = sender.send(closed(error.to_string())).await;
            return Some(inbound);
        }
    }
    let command = match terminal.command.is_empty() {
        true => SHELL.iter().map(ToString::to_string).collect(),
        false => terminal.command,
    };
    let (input, inputs) = mpsc::channel(64);
    let (gone, mut left) = oneshot::channel();
    let reader = tokio::spawn(read(inbound, input, gone));
    let size = size(terminal.rows, terminal.columns);
    let mut output = runtime.terminal(&terminal.container_id, command, size, Box::pin(ReceiverStream::new(inputs)));

    // Once the control plane has gone, nobody reads the output: wait a little for the exit,
    // without sending anything.
    let mut deadline = None;
    loop {
        let item = tokio::select! {
            item = output.next() => item,
            _ = &mut left, if deadline.is_none() => {
                deadline = Some(tokio::time::Instant::now() + EXIT_GRACE);
                continue;
            }
            () = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)), if deadline.is_some() => break,
        };
        if deadline.is_some() {
            match item {
                Some(Ok(ExecOutput::Stdout(_) | ExecOutput::Stderr(_))) => continue,
                _ => break,
            }
        }
        let frame = match item {
            Some(Ok(ExecOutput::Stdout(data) | ExecOutput::Stderr(data))) => Frame::Data(data.into()),
            Some(Ok(ExecOutput::Exited(code))) => {
                Frame::Closed(pb::SessionClosed { error: String::new(), exit_code: Some(code) })
            }
            Some(Err(error)) => Frame::Closed(pb::SessionClosed { error: error.to_string(), exit_code: None }),
            None => Frame::Closed(pb::SessionClosed { error: "the terminal ended".to_string(), exit_code: None }),
        };
        let last = matches!(frame, Frame::Closed(_));
        if sender.send(pb::SessionFrame { session_id: String::new(), frame: Some(frame) }).await.is_err() || last {
            break;
        }
    }
    drop(output);
    // The reader drains the inbound stream until the control plane closes its side.
    let mut reader = AbortOnDrop(reader);
    let _ = tokio::time::timeout(CLOSE_GRACE, &mut reader.0).await;
    None
}

/// Turns the control plane's frames into the terminal's input until it closes its side, then
/// types Ctrl+D (see the module's comment) and says so. Frames that come after the command
/// stopped reading are dropped.
async fn read(mut inbound: Streaming<pb::SessionFrame>, input: mpsc::Sender<TerminalInput>, gone: oneshot::Sender<()>) {
    let mut input = Some(input);
    while let Ok(Some(frame)) = inbound.message().await {
        let item = match frame.frame {
            Some(Frame::Data(data)) => TerminalInput::Data(data.into()),
            Some(Frame::Resize(resize)) => TerminalInput::Resize(size(resize.rows, resize.columns)),
            _ => continue,
        };
        if let Some(sender) = &input
            && sender.send(item).await.is_err()
        {
            input = None;
        }
    }
    if let Some(input) = input {
        let _ = input.send(TerminalInput::Data(bytes::Bytes::from_static(&[END_OF_INPUT]))).await;
    }
    let _ = gone.send(());
}

/// A size within bounds; a zero means the default.
fn size(rows: u32, columns: u32) -> TerminalSize {
    let clamp = |value: u32, max: u32, default: u16| match value {
        0 => default,
        value => value.min(max) as u16,
    };
    TerminalSize { rows: clamp(rows, MAX_ROWS, DEFAULT_SIZE.rows), columns: clamp(columns, MAX_COLUMNS, DEFAULT_SIZE.columns) }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_bounded_and_default() {
        assert_eq!(size(0, 0), DEFAULT_SIZE);
        assert_eq!(size(40, 120), TerminalSize { rows: 40, columns: 120 });
        assert_eq!(size(100_000, 100_000), TerminalSize { rows: 500, columns: 1000 });
    }
}
