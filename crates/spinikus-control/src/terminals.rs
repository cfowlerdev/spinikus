//! Terminals: a command run with a TTY in a service's running container, such as a shell, for
//! fixing what a deployment can't. Like runtime logs, it is a session on the agent of the
//! container's server ([`Control::open_session`]): keystrokes and window sizes go to it as
//! frames, and the terminal's output comes back, then how the command exited. Only the
//! environment's admins may open one, since it runs anything as the container's user, and each
//! one is audited (`service.terminal`, with the deployment and the program, never the rest of
//! the command, which may hold secrets).

use serde_json::json;
use spinikus_core::{
    deployment::Deployment,
    id::{DeploymentId, ServiceId},
    membership::Role,
    server::InstanceStatus,
};
use spinikus_proto::agent::v1::{self as pb, open_session, session_frame::Frame};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::{Control, ControlError, Principal, service_logs::Attached};

/// The most arguments a terminal's command can have.
pub const MAX_COMMAND_ARGS: usize = 64;

/// The most bytes a terminal's command can have, all arguments together.
pub const MAX_COMMAND_LEN: usize = 4096;

/// The largest terminal, each way; larger sizes are reduced to it.
pub const MAX_ROWS: u16 = 500;
pub const MAX_COLUMNS: u16 = 1000;

/// What to open.
#[derive(Debug, Clone, Default)]
pub struct TerminalOptions {
    /// The deployment whose container to open it in; the live one if `None`.
    pub deployment_id: Option<DeploymentId>,
    /// The command to run; the container's shell (bash, else sh) if empty.
    pub command: Vec<String>,
    /// The terminal's size; zero means the default, 24 rows by 80 columns.
    pub rows: u16,
    pub columns: u16,
}

/// An open terminal.
pub struct Terminal {
    /// The deployment whose container it runs in.
    pub deployment: Deployment,
    /// Its input. Dropping it closes the command's input.
    pub input: TerminalInput,
    /// Its output, ending with [`TerminalOutput::Exited`] or [`TerminalOutput::Ended`].
    pub output: ReceiverStream<TerminalOutput>,
}

/// What the terminal writes, and how it ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalOutput {
    Data(Vec<u8>),
    /// The command exited with this code.
    Exited(i64),
    /// The terminal ended without an exit code: the agent could not go on.
    Ended {
        error: String,
    },
}

/// The terminal's input side.
#[derive(Clone)]
pub struct TerminalInput {
    sender: mpsc::Sender<pb::SessionFrame>,
}

/// The terminal has ended; its input goes nowhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the terminal has ended")]
pub struct TerminalEnded;

impl TerminalInput {
    /// Types into the terminal.
    pub async fn write(&self, data: Vec<u8>) -> Result<(), TerminalEnded> {
        self.send(Frame::Data(data)).await
    }

    /// Changes the terminal's size.
    pub async fn resize(&self, rows: u16, columns: u16) -> Result<(), TerminalEnded> {
        let (rows, columns) = bounded(rows, columns);
        self.send(Frame::Resize(pb::TerminalSize { rows, columns })).await
    }

    async fn send(&self, frame: Frame) -> Result<(), TerminalEnded> {
        let frame = pb::SessionFrame { session_id: String::new(), frame: Some(frame) };
        self.sender.send(frame).await.map_err(|_| TerminalEnded)
    }
}

impl Control {
    /// Opens a terminal in the service's running container. Environment admins only.
    pub async fn open_terminal(
        &self,
        principal: &Principal,
        service_id: ServiceId,
        options: TerminalOptions,
    ) -> Result<Terminal, ControlError> {
        let (_, environment) = self.service_with_access(principal, service_id, Role::Admin).await?;
        check_command(&options.command)?;
        let (deployment, instance) = self.deployment_container(service_id, options.deployment_id).await?;
        if instance.status != InstanceStatus::Running {
            return Err(ControlError::Rejected {
                entity: "deployment",
                id: deployment.id.to_string(),
                problem: "its container is not running; read its logs, or restart or redeploy the service".to_string(),
            });
        }
        let (rows, columns) = bounded(options.rows, options.columns);
        let program = options.command.first().cloned();
        let session = pb::TerminalSession { container_id: instance.container_id, rows, columns, command: options.command };
        let Attached { frames, outbound } =
            self.open_session(&deployment, open_session::Kind::Terminal(session), "the terminal").await?;

        let details = json!({ "deployment_id": deployment.id.to_string(), "program": program });
        self.audit_by(principal, "service.terminal", service_id, Some(environment.project_id), details).await?;
        let (sender, receiver) = mpsc::channel(64);
        tokio::spawn(forward(frames, sender));
        Ok(Terminal { deployment, input: TerminalInput { sender: outbound }, output: ReceiverStream::new(receiver) })
    }
}

fn check_command(command: &[String]) -> Result<(), ControlError> {
    let invalid = |problem: &str| Err(ControlError::Invalid { field: "command", problem: problem.to_string() });
    if command.len() > MAX_COMMAND_ARGS {
        return invalid(&format!("it can have at most {MAX_COMMAND_ARGS} arguments"));
    }
    if command.iter().map(String::len).sum::<usize>() > MAX_COMMAND_LEN {
        return invalid(&format!("it can be at most {MAX_COMMAND_LEN} bytes long"));
    }
    if command.iter().any(|argument| argument.contains('\0')) {
        return invalid("it can't contain NUL characters");
    }
    if command.first().is_some_and(|program| program.trim().is_empty()) {
        return invalid("its program can't be empty");
    }
    Ok(())
}

/// The size within bounds, zeros kept to mean the default.
fn bounded(rows: u16, columns: u16) -> (u32, u32) {
    (u32::from(rows.min(MAX_ROWS)), u32::from(columns.min(MAX_COLUMNS)))
}

/// Passes the agent's frames on until the terminal ends or nobody reads it.
async fn forward(mut frames: tonic::Streaming<pb::SessionFrame>, sender: mpsc::Sender<TerminalOutput>) {
    loop {
        let frame = tokio::select! {
            () = sender.closed() => return,
            frame = frames.message() => frame,
        };
        let output = match frame {
            Ok(Some(pb::SessionFrame { frame: Some(Frame::Data(data)), .. })) => TerminalOutput::Data(data),
            Ok(Some(pb::SessionFrame { frame: Some(Frame::Closed(closed)), .. })) => match closed.exit_code {
                Some(code) => TerminalOutput::Exited(code),
                None if closed.error.is_empty() => TerminalOutput::Ended { error: "the terminal ended".to_string() },
                None => TerminalOutput::Ended { error: closed.error },
            },
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => TerminalOutput::Ended { error: "the agent's stream ended".to_string() },
        };
        let last = !matches!(output, TerminalOutput::Data(_));
        if sender.send(output).await.is_err() || last {
            return;
        }
    }
}

#[cfg(test)]
mod tests;
