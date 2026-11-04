//! The tonic service agents connect to.

use std::pin::Pin;

use spinikus_core::id::ServerId;
use spinikus_proto::agent::v1::{
    self as pb, AgentMessage, ControlMessage, agent_message, agent_service_server::AgentService, control_message,
};
use spinikus_store::{InstanceRepository, ServerRepository, StoreError};
use tokio::{
    sync::{mpsc, oneshot},
    time::timeout,
};
use tokio_stream::{Stream, StreamExt, wrappers::ReceiverStream};
use tonic::{Request, Response, Status, Streaming};

use super::{AgentIdentity, AgentTimings, MIN_PROTOCOL_VERSION, convert, hub::OUTBOUND_BUFFER};
use crate::{Control, ControlError};

type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

/// The `AgentService` implementation. Wrap it in `AgentServiceServer` behind the mTLS layer.
#[derive(Clone)]
pub struct AgentGrpc {
    control: Control,
    timings: AgentTimings,
}

impl AgentGrpc {
    pub fn new(control: Control) -> Self {
        Self { control, timings: AgentTimings::default() }
    }

    pub fn with_timings(mut self, timings: AgentTimings) -> Self {
        self.timings = timings;
        self
    }
}

#[tonic::async_trait]
impl AgentService for AgentGrpc {
    async fn enroll(&self, request: Request<pb::EnrollRequest>) -> Result<Response<pb::EnrollResponse>, Status> {
        let request = request.into_inner();
        let enrolled = self.control.enroll_agent(&request.join_token, &request.csr_der).await.map_err(status)?;
        Ok(Response::new(pb::EnrollResponse {
            server_id: enrolled.server_id.to_string(),
            certificate_chain_pem: enrolled.certificate_chain_pem,
            ca_certificate_pem: enrolled.ca_certificate_pem,
        }))
    }

    async fn renew_certificate(
        &self,
        request: Request<pb::RenewCertificateRequest>,
    ) -> Result<Response<pb::RenewCertificateResponse>, Status> {
        let AgentIdentity(server) = identity(&request)?;
        let certificate_chain_pem =
            self.control.renew_agent_certificate(server, &request.into_inner().csr_der).await.map_err(status)?;
        Ok(Response::new(pb::RenewCertificateResponse { certificate_chain_pem }))
    }

    type ConnectStream = BoxStream<ControlMessage>;

    async fn connect(&self, request: Request<Streaming<AgentMessage>>) -> Result<Response<Self::ConnectStream>, Status> {
        let AgentIdentity(server) = identity(&request)?;
        let mut inbound = request.into_inner();

        let hello = match timeout(self.timings.hello_timeout, inbound.message()).await {
            Ok(Ok(Some(AgentMessage { message: Some(agent_message::Message::Hello(hello)) }))) => hello,
            Ok(Ok(Some(_))) => return Err(Status::invalid_argument("the first message must be Hello")),
            Ok(Ok(None)) => return Err(Status::cancelled("the stream closed before Hello")),
            Ok(Err(status)) => return Err(status),
            Err(_) => return Err(Status::deadline_exceeded("no Hello in time")),
        };
        if hello.protocol_version < MIN_PROTOCOL_VERSION {
            tracing::warn!(server = %server, version = hello.protocol_version, "refused an agent with an old protocol version");
            return Err(Status::failed_precondition(format!(
                "protocol version {} is not supported; upgrade the agent to one that speaks at least {MIN_PROTOCOL_VERSION}",
                hello.protocol_version
            )));
        }
        let transition = self.control.agents().transition().await;
        match self.control.store().record_agent_hello(server, convert::agent_info(&hello)).await {
            Ok(()) => {}
            Err(StoreError::NotFound { .. }) => return Err(Status::permission_denied("the server is unknown or revoked")),
            Err(error) => {
                tracing::error!(server = %server, %error, "failed to record an agent's hello");
                return Err(Status::internal("internal error"));
            }
        }

        let (sender, receiver) = mpsc::channel(OUTBOUND_BUFFER);
        let welcome = pb::Welcome {
            server_id: server.to_string(),
            heartbeat_interval: prost_types::Duration::try_from(self.timings.heartbeat_interval).ok(),
        };
        sender
            .try_send(ControlMessage { message: Some(control_message::Message::Welcome(welcome)) })
            .expect("a new channel has room");
        let (connection, stopped) = self.control.agents().register(server, sender);
        drop(transition);
        tracing::info!(server = %server, agent_version = hello.agent_version, "agent connected");
        self.control.agent_connected(server, hello.routes_revision, hello.certificates_revision).await;

        let control = self.control.clone();
        let idle_timeout = self.timings.idle_timeout;
        tokio::spawn(async move {
            read(&control, server, inbound, stopped, idle_timeout).await;
            let _transition = control.agents().transition().await;
            if control.agents().unregister(server, connection) {
                tracing::info!(server = %server, "agent disconnected");
                if let Err(error) = control.store().record_disconnect(server).await {
                    tracing::error!(server = %server, %error, "failed to record an agent's disconnect");
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver).map(Ok))))
    }

    type OpenSessionStream = BoxStream<pb::SessionFrame>;

    async fn open_session(
        &self,
        request: Request<Streaming<pb::SessionFrame>>,
    ) -> Result<Response<Self::OpenSessionStream>, Status> {
        let AgentIdentity(server) = identity(&request)?;
        let frames = self.control.attach_session(server, request.into_inner()).await?;
        Ok(Response::new(Box::pin(frames.map(Ok))))
    }

    async fn push_stats(&self, request: Request<Streaming<pb::StatsBatch>>) -> Result<Response<pb::PushStatsResponse>, Status> {
        let AgentIdentity(server) = identity(&request)?;
        // Only from an agent that is connected, so never from a revoked server's.
        if !self.control.agents().is_connected(server) {
            return Err(Status::failed_precondition("connect first"));
        }
        let mut batches = request.into_inner();
        let mut points = Vec::new();
        while let Some(batch) = batches.message().await? {
            points.extend(batch.samples.into_iter().filter_map(convert::stat_sample));
            if points.len() > crate::metrics::MAX_POINTS_PER_PUSH {
                return Err(Status::resource_exhausted("too many samples in one push"));
            }
        }
        self.control.record_stats(server, points).await.map_err(status)?;
        Ok(Response::new(pb::PushStatsResponse {}))
    }
}

/// The server the request is from, proven by its client certificate.
fn identity<T>(request: &Request<T>) -> Result<AgentIdentity, Status> {
    request.extensions().get::<AgentIdentity>().copied().ok_or_else(|| Status::unauthenticated("no agent identity"))
}

/// A gRPC status for a service layer error. Internal details stay in the log.
fn status(error: ControlError) -> Status {
    match error {
        ControlError::Invalid { .. } => Status::invalid_argument(error.to_string()),
        ControlError::InvalidCredentials => Status::unauthenticated("the join token is invalid, used or expired"),
        ControlError::NotFound { .. } | ControlError::Forbidden => Status::permission_denied("the server is unknown or revoked"),
        _ => Status::internal("internal error"),
    }
}

/// Handles the agent's messages until its stream ends, goes idle, or the connection is
/// replaced or disconnected.
async fn read(
    control: &Control,
    server: ServerId,
    mut inbound: Streaming<AgentMessage>,
    mut stopped: oneshot::Receiver<()>,
    idle_timeout: std::time::Duration,
) {
    loop {
        let next = tokio::select! {
            _ = &mut stopped => return,
            next = timeout(idle_timeout, inbound.message()) => next,
        };
        match next {
            Ok(Ok(Some(message))) => {
                if let Err(error) = handle(control, server, message).await {
                    tracing::error!(server = %server, %error, "failed to handle an agent message");
                }
            }
            Ok(Ok(None)) => return,
            Ok(Err(status)) => {
                tracing::debug!(server = %server, %status, "agent stream failed");
                return;
            }
            Err(_) => {
                tracing::warn!(server = %server, "agent went quiet, closing its connection");
                return;
            }
        }
    }
}

async fn handle(control: &Control, server: ServerId, message: AgentMessage) -> Result<(), StoreError> {
    let Some(message) = message.message else {
        tracing::debug!(server = %server, "ignored an agent message of an unknown kind");
        return Ok(());
    };
    match message {
        agent_message::Message::Heartbeat(_) => control.store().record_heartbeat(server).await,
        agent_message::Message::StateReport(report) => apply_state_report(control, server, report).await,
        agent_message::Message::Hello(_) => {
            tracing::warn!(server = %server, "ignored a second Hello");
            Ok(())
        }
        agent_message::Message::CommandProgress(progress) => {
            control.deployment_progress(server, progress).await;
            Ok(())
        }
        agent_message::Message::CommandResult(result) => {
            if crate::backups::is_backup_command(&result.command_id) {
                control.backup_result(server, result).await;
            } else if crate::restarts::is_restart_command(&result.command_id) {
                control.restart_result(server, result);
            } else {
                control.deployment_result(server, result).await;
            }
            Ok(())
        }
        agent_message::Message::SnapshotApplied(applied) => {
            match applied.kind() {
                pb::SnapshotKind::Routes => control.routes_applied(server, applied.revision).await,
                pb::SnapshotKind::Certificates => control.certificates_applied(server, applied.revision).await,
                pb::SnapshotKind::AcmeChallenges => control.challenges_applied(server, applied.revision),
                pb::SnapshotKind::Unspecified => tracing::debug!(server = %server, "ignored a snapshot of no kind"),
            }
            Ok(())
        }
    }
}

async fn apply_state_report(control: &Control, server: ServerId, report: pb::StateReport) -> Result<(), StoreError> {
    let instances = report.instances.into_iter().filter_map(|instance| {
        let container = instance.container_id.clone();
        convert::instance(instance)
            .inspect_err(|problem| tracing::warn!(server = %server, container, problem, "ignored a reported container"))
            .ok()
    });
    if report.full {
        control.store().replace_server_instances(server, instances.collect()).await?;
        return Ok(());
    }
    for instance in instances {
        control.store().upsert_instance(server, instance).await?;
    }
    for container in report.removed_container_ids {
        control.store().remove_instance(server, &container).await?;
    }
    Ok(())
}
