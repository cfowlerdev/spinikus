//! The agent listener: gRPC over TLS, separate from the HTTP listener. Agents present a
//! client certificate from the internal CA, whose subject names their server. A client
//! without one can only `Enroll`; every other call needs the identity.

use std::future::Future;

use jiff::Timestamp;
use spinikus_proto::agent::v1::agent_service_server::AgentServiceServer;
use spinikus_store::ServerRepository;
use tokio::{net::TcpListener, sync::oneshot};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{
    Request, Status,
    transport::{Certificate, Identity, Server, ServerTlsConfig},
};

use super::{AgentGrpc, AgentIdentity};
use crate::{Control, ControlError, ca::server_in_certificate};

pub type ListenerError = Box<dyn std::error::Error + Send + Sync>;

/// How long shutdown waits for agents to close their connections.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Serves the agent service on `listener` until `shutdown` resolves. The listener's
/// certificate is issued by the internal CA on every start.
pub async fn serve(control: Control, listener: TcpListener, shutdown: impl Future<Output = ()>) -> Result<(), ListenerError> {
    let authority = control.certificate_authority().await?;
    let (certificate_pem, key_pem) = authority.issue_listener_certificate(Timestamp::now())?;
    // The CA goes in the chain too, so a joining agent can find it and check it against the
    // fingerprint from its join command before trusting it.
    let chain = format!("{certificate_pem}\n{}", authority.certificate_pem());
    let tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(chain, key_pem.expose()))
        .client_ca_root(Certificate::from_pem(authority.certificate_pem()))
        .client_auth_optional(true);
    tracing::info!(address = %listener.local_addr()?, ca_fingerprint = authority.fingerprint(), "Agent listener started");
    // Nothing can be connected to a control plane that is only starting, whatever the
    // database says after a crash.
    control.store().record_all_disconnected().await.map_err(ControlError::from)?;

    let service = AgentServiceServer::with_interceptor(AgentGrpc::new(control.clone()), identify);
    // Agents' streams never end on their own, so on shutdown close them; the agents then
    // close their side and the connections drain. Give up waiting after a grace period.
    let (stopping, stopped) = oneshot::channel();
    let agents = control.agents().clone();
    let serving = Server::builder().tls_config(tls)?.add_service(service).serve_with_incoming_shutdown(
        TcpListenerStream::new(listener),
        async move {
            shutdown.await;
            agents.disconnect_all();
            let _ = stopping.send(());
        },
    );
    tokio::select! {
        result = serving => result?,
        () = async {
            if stopped.await.is_ok() {
                tokio::time::sleep(SHUTDOWN_GRACE).await;
            } else {
                std::future::pending::<()>().await;
            }
        } => tracing::warn!("agent connections did not close in time"),
    }
    Ok(())
}

/// Sets the request's [`AgentIdentity`] from its client certificate, which TLS has already
/// verified against the internal CA.
fn identify(mut request: Request<()>) -> Result<Request<()>, Status> {
    let server = request.peer_certs().and_then(|certificates| server_in_certificate(certificates.first()?.as_ref()));
    if let Some(server) = server {
        request.extensions_mut().insert(AgentIdentity(server));
    }
    Ok(request)
}
