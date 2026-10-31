//! The control stream, from the agent's side: connect, `Hello`, wait for `Welcome`, then
//! heartbeats and the control plane's messages until the connection drops, after which
//! the agent reconnects with backoff. The certificate is renewed on the open connection
//! once two thirds of its lifetime has passed, and the agent reconnects with the new one.
//! While connected, the reporter keeps the control plane's view of this server's
//! containers current, and commands run as they arrive. The proxy, when there is one,
//! serves for the agent's whole run, from the saved routes and certificates until new
//! ones arrive.

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use jiff::Timestamp;
use rcgen::{CertificateParams, KeyPair};
use spinikus_core::secret::Secret;
use spinikus_proto::agent::v1::{
    self as pb, AgentMessage, ControlMessage, agent_message, agent_service_client::AgentServiceClient, control_message,
};
use tokio::{
    net::TcpListener,
    sync::{mpsc, watch},
    time::timeout,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    Code, Status,
    transport::{Certificate, Channel, ClientTlsConfig, Identity},
};

use crate::{
    AgentError, AgentFiles, Backoff, Credentials,
    certificate::{renewal_due, until, validity},
    commands::{CommandEvent, Commands},
    enroll::channel,
    proxy::{ProxyState, RuntimeUpstreams},
    report::report,
    runtime::ContainerRuntime,
};

/// How long to wait for `Welcome` after sending `Hello`.
const WELCOME_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait before retrying a renewal that failed for a passing reason.
const RENEWAL_RETRY: Duration = Duration::from_secs(5 * 60);

/// How long to wait before listing the containers again after the runtime failed.
const RESYNC_DELAY: Duration = Duration::from_secs(5);

pub struct RunOptions {
    pub facts: pb::ServerFacts,
    pub backoff: Backoff,
    pub runtime: Arc<dyn ContainerRuntime>,
    /// Where the proxy serves apps, or `None` to run without one.
    pub proxy: Option<ProxyListeners>,
}

/// The proxy's listeners, bound by the caller.
pub struct ProxyListeners {
    pub http: TcpListener,
    /// Where HTTPS is served, or `None` for plain HTTP only.
    pub https: Option<TcpListener>,
}

/// Keeps the agent connected until `shutdown` resolves. Returns an error only for what
/// retrying cannot fix: the agent is not enrolled, its server was revoked, its certificate
/// expired, or the control plane does not support it.
pub async fn run(files: &AgentFiles, options: RunOptions, shutdown: impl Future<Output = ()>) -> Result<(), AgentError> {
    let RunOptions { facts, backoff, runtime, proxy } = options;
    let (stop_proxy, proxy_stopped) = watch::channel(false);
    let stopped = move || {
        let mut proxy_stopped = proxy_stopped.clone();
        async move {
            let _ = proxy_stopped.wait_for(|stopped| *stopped).await;
        }
    };
    let mut serving = Vec::new();
    let mut served = None;
    if let Some(ProxyListeners { http, https }) = proxy {
        let https_port = https.as_ref().and_then(|listener| listener.local_addr().ok()).map(|address| address.port());
        let proxy = spinikus_proxy::Proxy::new(Arc::new(RuntimeUpstreams::new(runtime.clone())), https_port);
        serving.push(tokio::spawn(spinikus_proxy::serve(proxy.clone(), http, stopped())));
        if let Some(https) = https {
            serving.push(tokio::spawn(spinikus_proxy::serve_tls(proxy.clone(), https, stopped())));
        }
        served = Some(proxy);
    }
    let mut proxy = ProxyState::load(files, served);
    let result = stay_connected(files, facts, backoff, runtime, &mut proxy, shutdown).await;
    let _ = stop_proxy.send(true);
    for serving in serving {
        let _ = serving.await;
    }
    result
}

async fn stay_connected(
    files: &AgentFiles,
    facts: pb::ServerFacts,
    mut backoff: Backoff,
    runtime: Arc<dyn ContainerRuntime>,
    proxy: &mut ProxyState,
    shutdown: impl Future<Output = ()>,
) -> Result<(), AgentError> {
    let mut shutdown = std::pin::pin!(shutdown);
    // Outlives connections: a command that finishes while disconnected reports on the next.
    let mut commands = Commands::new(runtime.clone());
    loop {
        let credentials = files.load()?;
        let invalid =
            || AgentError::InvalidFile { path: files.dir().join("identity.json"), problem: "unreadable certificate".to_string() };
        let (_, not_after) = validity(&credentials.certificate_pem).ok_or_else(invalid)?;
        if not_after <= Timestamp::now() {
            return Err(AgentError::CertificateExpired(not_after));
        }

        let ended = session(files, &credentials, &facts, &*runtime, &mut commands, proxy, shutdown.as_mut()).await?;
        let reason = match ended {
            Ended::Shutdown => return Ok(()),
            Ended::Renewed => {
                backoff.reset();
                continue;
            }
            Ended::Lost { welcomed, reason } => {
                if welcomed {
                    backoff.reset();
                }
                reason
            }
        };
        let delay = backoff.next_delay();
        tracing::warn!(reason, retry_in = ?delay, "lost the connection to the control plane");
        tokio::select! {
            () = shutdown.as_mut() => return Ok(()),
            () = tokio::time::sleep(delay) => {}
        }
    }
}

enum Ended {
    Shutdown,
    /// The certificate was renewed; reconnect with the new one.
    Renewed,
    /// The connection failed or dropped; retry. `welcomed` if it had been accepted.
    Lost {
        welcomed: bool,
        reason: String,
    },
}

fn lost(welcomed: bool, reason: impl ToString) -> Ended {
    Ended::Lost { welcomed, reason: reason.to_string() }
}

/// What a refusal from the control plane means: fatal for revoked or unsupported agents,
/// otherwise worth retrying.
fn refused(status: Status, welcomed: bool) -> Result<Ended, AgentError> {
    match status.code() {
        Code::PermissionDenied => Err(AgentError::Revoked),
        Code::FailedPrecondition => Err(AgentError::Unsupported(status.message().to_string())),
        _ => Ok(lost(welcomed, format!("{:?}: {}", status.code(), status.message()))),
    }
}

/// One connection, from connecting until it ends.
async fn session(
    files: &AgentFiles,
    credentials: &Credentials,
    facts: &pb::ServerFacts,
    runtime: &dyn ContainerRuntime,
    commands: &mut Commands,
    proxy: &mut ProxyState,
    mut shutdown: Pin<&mut impl Future<Output = ()>>,
) -> Result<Ended, AgentError> {
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(&credentials.ca_pem))
        .identity(Identity::from_pem(&credentials.certificate_pem, credentials.key_pem.expose()));
    let channel = tokio::select! {
        () = shutdown.as_mut() => return Ok(Ended::Shutdown),
        channel = channel(&credentials.control_plane, tls) => channel,
    };
    let channel = match channel {
        Ok(channel) => channel,
        Err(AgentError::Connect(reason)) => return Ok(lost(false, reason)),
        Err(error) => return Err(error),
    };
    let mut client = AgentServiceClient::new(channel);

    let (outbound, receiver) = mpsc::channel(32);
    let hello = pb::Hello {
        protocol_version: pb::PROTOCOL_VERSION,
        agent_version: crate::AGENT_VERSION.to_string(),
        facts: Some(facts.clone()),
        in_flight_command_ids: commands.in_flight(),
        routes_revision: proxy.routes_revision(),
        certificates_revision: proxy.certificates_revision(),
    };
    outbound.send(message(agent_message::Message::Hello(hello))).await.expect("the receiver is held below");
    let response = tokio::select! {
        () = shutdown.as_mut() => return Ok(Ended::Shutdown),
        response = client.connect(ReceiverStream::new(receiver)) => response,
    };
    let mut inbound = match response {
        Ok(response) => response.into_inner(),
        Err(status) => return refused(status, false),
    };
    let welcome = tokio::select! {
        () = shutdown.as_mut() => return Ok(Ended::Shutdown),
        welcome = timeout(WELCOME_TIMEOUT, inbound.message()) => welcome,
    };
    let welcome = match welcome {
        Ok(Ok(Some(ControlMessage { message: Some(control_message::Message::Welcome(welcome)) }))) => welcome,
        Ok(Err(status)) => return refused(status, false),
        Ok(_) => return Ok(lost(false, "the control plane did not answer Hello with Welcome")),
        Err(_) => return Ok(lost(false, "no Welcome in time")),
    };
    tracing::info!(server = welcome.server_id, "connected to the control plane");

    let interval = welcome
        .heartbeat_interval
        .and_then(|interval| Duration::try_from(interval).ok())
        .unwrap_or(Duration::from_secs(10))
        .max(Duration::from_secs(1));
    let mut heartbeats = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    let (not_before, not_after) = validity(&credentials.certificate_pem).expect("checked before connecting");
    let renewal = tokio::time::sleep(until(renewal_due(not_before, not_after)));
    let mut renewal = std::pin::pin!(renewal);
    let reporter = report(runtime, &outbound, RESYNC_DELAY);
    let mut reporter = std::pin::pin!(reporter);

    loop {
        tokio::select! {
            () = shutdown.as_mut() => return Ok(Ended::Shutdown),
            _ = heartbeats.tick() => {
                let heartbeat = pb::Heartbeat { sent_at: Some(std::time::SystemTime::now().into()) };
                if outbound.send(message(agent_message::Message::Heartbeat(heartbeat))).await.is_err() {
                    return Ok(lost(true, "the stream closed"));
                }
            }
            () = renewal.as_mut() => match renew(&mut client, files).await {
                Ok(()) => return Ok(Ended::Renewed),
                Err(status) if status.code() == Code::PermissionDenied => return Err(AgentError::Revoked),
                Err(status) => {
                    tracing::warn!(%status, "failed to renew the certificate, will retry");
                    renewal.as_mut().reset(tokio::time::Instant::now() + RENEWAL_RETRY);
                }
            },
            // Returns only once the stream has closed.
            () = reporter.as_mut() => return Ok(lost(true, "the stream closed")),
            event = commands.next(), if commands.busy() => {
                let event = match event {
                    CommandEvent::Progress(progress) => agent_message::Message::CommandProgress(progress),
                    CommandEvent::Finished(result) => agent_message::Message::CommandResult(result),
                };
                if outbound.send(message(event)).await.is_err() {
                    return Ok(lost(true, "the stream closed"));
                }
            }
            received = inbound.message() => match received {
                Ok(Some(received)) => handle(received, &outbound, commands, proxy).await,
                Ok(None) => return Ok(lost(true, "the control plane closed the connection")),
                Err(status) => return refused(status, true),
            },
        }
    }
}

fn message(message: agent_message::Message) -> AgentMessage {
    AgentMessage { message: Some(message) }
}

/// Gets a certificate for a fresh key and saves both.
async fn renew(client: &mut AgentServiceClient<Channel>, files: &AgentFiles) -> Result<(), Status> {
    let key = KeyPair::generate().expect("generating a key pair does not fail");
    let csr = CertificateParams::new(Vec::<String>::new())
        .and_then(|params| params.serialize_request(&key))
        .expect("a CSR without names always serializes");
    let response = client.renew_certificate(pb::RenewCertificateRequest { csr_der: csr.der().to_vec() }).await?.into_inner();
    files
        .save_identity(&response.certificate_chain_pem, &Secret::new(key.serialize_pem()))
        .map_err(|error| Status::internal(error.to_string()))?;
    tracing::info!("renewed the agent's certificate");
    Ok(())
}

async fn handle(
    received: ControlMessage,
    outbound: &mpsc::Sender<AgentMessage>,
    commands: &mut Commands,
    proxy: &mut ProxyState,
) {
    match received.message {
        Some(control_message::Message::Command(command)) => {
            if let Some(result) = commands.start(command) {
                let _ = outbound.send(message(agent_message::Message::CommandResult(result))).await;
            }
        }
        // Unacknowledged snapshots are sent again later.
        Some(control_message::Message::SetRoutes(set)) => {
            let revision = set.revision;
            match proxy.apply_routes(set) {
                Ok(()) => applied(outbound, pb::SnapshotKind::Routes, revision).await,
                Err(error) => tracing::error!(%error, "failed to apply routes"),
            }
        }
        Some(control_message::Message::SetCertificates(set)) => {
            let revision = set.revision;
            match proxy.apply_certificates(set) {
                Ok(()) => applied(outbound, pb::SnapshotKind::Certificates, revision).await,
                Err(error) => tracing::error!(%error, "failed to apply certificates"),
            }
        }
        Some(control_message::Message::AcmeChallenges(set)) => {
            let revision = set.revision;
            proxy.apply_challenges(set);
            applied(outbound, pb::SnapshotKind::AcmeChallenges, revision).await;
        }
        Some(control_message::Message::Welcome(_)) | None => {}
    }
}

async fn applied(outbound: &mpsc::Sender<AgentMessage>, kind: pb::SnapshotKind, revision: u64) {
    let applied = pb::SnapshotApplied { kind: kind.into(), revision };
    let _ = outbound.send(message(agent_message::Message::SnapshotApplied(applied))).await;
}
