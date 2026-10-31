//! The agent listener: gRPC over TLS, separate from the HTTP listener. Agents present a
//! client certificate from the internal CA, whose subject names their server. A client
//! without one can only `Enroll`; every other call needs the identity.

use std::future::Future;

use jiff::Timestamp;
use spinikus_proto::agent::v1::agent_service_server::AgentServiceServer;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{
    Request, Status,
    transport::{Certificate, Identity, Server, ServerTlsConfig},
};

use super::{AgentGrpc, AgentIdentity};
use crate::{Control, ca::server_in_certificate};

pub type ListenerError = Box<dyn std::error::Error + Send + Sync>;

/// Serves the agent service on `listener` until `shutdown` resolves. The listener's
/// certificate is issued by the internal CA on every start.
pub async fn serve(control: Control, listener: TcpListener, shutdown: impl Future<Output = ()>) -> Result<(), ListenerError> {
    let authority = control.certificate_authority().await?;
    let (certificate_pem, key_pem) = authority.issue_listener_certificate(Timestamp::now())?;
    let tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(certificate_pem, key_pem.expose()))
        .client_ca_root(Certificate::from_pem(authority.certificate_pem()))
        .client_auth_optional(true);
    tracing::info!(address = %listener.local_addr()?, ca_fingerprint = authority.fingerprint(), "Agent listener started");
    let service = AgentServiceServer::with_interceptor(AgentGrpc::new(control.clone()), identify);
    Server::builder()
        .tls_config(tls)?
        .add_service(service)
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), shutdown)
        .await?;
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
