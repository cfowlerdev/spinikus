//! A service's runtime logs, read from its container on the server that runs it. They stay
//! on the agent and are streamed on demand: the control plane sends an `OpenSession` command,
//! the agent opens a session stream for it ([`Control::attach_session`]), and its frames go
//! to the caller. When the caller stops reading, the stream is closed and the agent stops.

use std::{collections::HashMap, sync::Mutex, time::Duration};

use jiff::Timestamp;
use spinikus_core::{
    deployment::Deployment,
    id::{DeploymentId, ServerId, ServiceId},
    membership::Role,
    server::ObservedInstance,
};
use spinikus_proto::agent::v1::{self as pb, command::Kind, control_message, open_session, session_frame::Frame};
use spinikus_store::{DeploymentRepository, InstanceRepository, StoreError};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Status, Streaming};

use crate::{Control, ControlError, Principal, access::not_found};

/// How long the agent has to open a session once asked.
pub const SESSION_OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// The most lines a caller can ask for from the log already written.
pub const MAX_TAIL: u32 = 10_000;

/// Which logs to read.
#[derive(Debug, Clone, Copy, Default)]
pub struct LogQuery {
    /// The deployment whose container to read; its live one if `None`.
    pub deployment_id: Option<DeploymentId>,
    /// Only lines written at or after this time.
    pub since: Option<Timestamp>,
    /// At most this many of the lines already written (all of them if `None`).
    pub tail: Option<u32>,
    /// Keep going with new lines.
    pub follow: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerLogUpdate {
    Line {
        at: Option<Timestamp>,
        stderr: bool,
        text: String,
    },
    /// No more lines: the log ended (without `follow`), or the agent could not go on.
    End {
        error: Option<String>,
    },
}

/// Sessions asked of agents that have not opened them yet.
#[derive(Default)]
pub(crate) struct Sessions {
    pending: Mutex<HashMap<String, Pending>>,
}

struct Pending {
    server: ServerId,
    ready: oneshot::Sender<Attached>,
}

/// An open session: the agent's frames, and the sender for frames to the agent, which keeps
/// the agent's side open until dropped.
pub(crate) struct Attached {
    pub(crate) frames: Streaming<pb::SessionFrame>,
    pub(crate) outbound: mpsc::Sender<pb::SessionFrame>,
}

/// A refusal about the deployment.
fn rejected(deployment: &Deployment, problem: &str) -> ControlError {
    ControlError::Rejected { entity: "deployment", id: deployment.id.to_string(), problem: problem.to_string() }
}

impl Sessions {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Control {
    /// Reads the logs of the service's container, or of the deployment the query names.
    pub async fn service_logs(
        &self,
        principal: &Principal,
        service_id: ServiceId,
        query: LogQuery,
    ) -> Result<ReceiverStream<ContainerLogUpdate>, ControlError> {
        self.service_with_access(principal, service_id, Role::Viewer).await?;
        let (deployment, instance) = self.deployment_container(service_id, query.deployment_id).await?;
        let tail = pb::LogTailSession {
            container_id: instance.container_id,
            since: query
                .since
                .map(|since| prost_types::Timestamp { seconds: since.as_second(), nanos: since.subsec_nanosecond() }),
            tail: query.tail.unwrap_or(0).min(MAX_TAIL),
            follow: query.follow,
        };
        let attached = self.open_session(&deployment, open_session::Kind::LogTail(tail), "the log").await?;
        let (sender, receiver) = mpsc::channel(256);
        tokio::spawn(forward(attached, sender));
        Ok(ReceiverStream::new(receiver))
    }

    /// The container of the deployment named, or of the service's live one, as its server
    /// last reported it.
    pub(crate) async fn deployment_container(
        &self,
        service_id: ServiceId,
        deployment_id: Option<DeploymentId>,
    ) -> Result<(Deployment, ObservedInstance), ControlError> {
        let deployment = match deployment_id {
            Some(id) => match self.store().get_deployment(id).await {
                Ok(deployment) if deployment.service_id == service_id => deployment,
                Ok(_) | Err(StoreError::NotFound { .. }) => return Err(not_found("deployment", id)),
                Err(error) => return Err(error.into()),
            },
            None => self.store().live_deployment(service_id).await?.ok_or_else(|| ControlError::Invalid {
                field: "deployment_id",
                problem: "the service has nothing live; name a deployment".to_string(),
            })?,
        };
        let instances = self.store().list_server_instances(deployment.server_id).await?;
        let instance = instances.into_iter().find(|instance| instance.observed.deployment_id == deployment.id);
        match instance {
            Some(instance) => Ok((deployment, instance.observed)),
            None => Err(rejected(&deployment, "it has no container on its server")),
        }
    }

    /// Asks the agent of the deployment's server to open a session, and waits for it.
    /// `what` names it in the refusal when it doesn't.
    pub(crate) async fn open_session(
        &self,
        deployment: &Deployment,
        kind: open_session::Kind,
        what: &str,
    ) -> Result<Attached, ControlError> {
        if !self.agents().is_connected(deployment.server_id) {
            return Err(rejected(deployment, "the agent of its server is not connected"));
        }
        let session_id = format!("session-{}", rand::random::<u128>());
        let (ready, attached) = oneshot::channel();
        self.sessions.lock().insert(session_id.clone(), Pending { server: deployment.server_id, ready });
        let open = pb::OpenSession { session_id: session_id.clone(), kind: Some(kind) };
        let command = pb::Command { command_id: format!("session/{session_id}"), kind: Some(Kind::OpenSession(open)) };
        let message = pb::ControlMessage { message: Some(control_message::Message::Command(command)) };
        let sent = self.agents().send(deployment.server_id, message).await.is_ok();
        let attached = match sent {
            true => tokio::time::timeout(SESSION_OPEN_TIMEOUT, attached).await.ok().and_then(Result::ok),
            false => None,
        };
        attached.ok_or_else(|| {
            self.sessions.lock().remove(&session_id);
            rejected(deployment, &format!("the agent of its server did not open {what}"))
        })
    }

    /// Takes an agent's session stream, whose first frame names the session, and hands it to
    /// whoever asked for it. The returned stream is the control plane's side; it ends when the
    /// asker stops reading.
    pub(crate) async fn attach_session(
        &self,
        server: ServerId,
        mut frames: Streaming<pb::SessionFrame>,
    ) -> Result<ReceiverStream<pb::SessionFrame>, Status> {
        let first = match tokio::time::timeout(SESSION_OPEN_TIMEOUT, frames.message()).await {
            Ok(Ok(Some(first))) => first,
            _ => return Err(Status::invalid_argument("the first frame must name the session")),
        };
        let pending = self.sessions.lock().remove(&first.session_id);
        let Some(pending) = pending else {
            return Err(Status::not_found("no session is waiting with this ID"));
        };
        if pending.server != server {
            return Err(Status::permission_denied("the session is for another server"));
        }
        let (outbound, receiver) = mpsc::channel(64);
        pending
            .ready
            .send(Attached { frames, outbound })
            .map_err(|_| Status::cancelled("nobody is waiting for the session any more"))?;
        Ok(ReceiverStream::new(receiver))
    }
}

/// Passes the agent's frames on until the log ends or nobody reads it.
async fn forward(mut attached: Attached, sender: mpsc::Sender<ContainerLogUpdate>) {
    loop {
        let frame = tokio::select! {
            () = sender.closed() => return,
            frame = attached.frames.message() => frame,
        };
        let update = match frame {
            Ok(Some(pb::SessionFrame { frame: Some(Frame::LogLine(line)), .. })) => ContainerLogUpdate::Line {
                at: line.at.and_then(|at| Timestamp::new(at.seconds, at.nanos).ok()),
                stderr: line.stream() == pb::LogStream::Stderr,
                text: line.text,
            },
            Ok(Some(pb::SessionFrame { frame: Some(Frame::Closed(closed)), .. })) => {
                ContainerLogUpdate::End { error: Some(closed.error).filter(|error| !error.is_empty()) }
            }
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => ContainerLogUpdate::End { error: Some("the agent's stream ended".to_string()) },
        };
        let end = matches!(update, ContainerLogUpdate::End { .. });
        if sender.send(update).await.is_err() || end {
            return;
        }
    }
}
