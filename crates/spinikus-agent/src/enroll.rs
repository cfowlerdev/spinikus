//! Joining: trading a join token and a fresh key's CSR for a client certificate.
//!
//! The agent does not know the control plane's CA yet, only its fingerprint from the join
//! command. So it first opens a TLS connection that accepts the control plane only if the
//! presented chain contains a CA with that fingerprint which signed the listener's
//! certificate. That CA is then the only root for the real, normally verified connection
//! that carries the join token, so the token never reaches anyone else.

use std::sync::{Arc, Mutex};

use rcgen::{CertificateParams, KeyPair};
use rustls::{
    ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
    client::{
        WebPkiServerVerifier,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use spinikus_core::{enrollment::CONTROL_PLANE_SERVER_NAME, secret::Secret};
use spinikus_proto::agent::v1::{self as pb, agent_service_client::AgentServiceClient};
use tokio::net::TcpStream;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Uri};

use crate::{
    AgentError, Credentials,
    certificate::{der_to_pem, fingerprint, parse_fingerprint},
};

/// Enrolls this server's agent with the control plane at `control_plane`
/// (`https://host:port`), checking its CA against `ca_fingerprint` (`sha256:…`).
pub async fn join(
    control_plane: &str,
    join_token: &Secret,
    ca_fingerprint: &str,
    facts: pb::ServerFacts,
) -> Result<Credentials, AgentError> {
    let expected = parse_fingerprint(ca_fingerprint).ok_or(AgentError::InvalidFingerprint)?;
    let ca_pem = fetch_pinned_ca(control_plane, &expected).await?;

    let key = KeyPair::generate().expect("generating a key pair does not fail");
    let csr = CertificateParams::new(Vec::<String>::new())
        .and_then(|params| params.serialize_request(&key))
        .expect("a CSR without names always serializes");
    let channel = channel(control_plane, ClientTlsConfig::new().ca_certificate(Certificate::from_pem(&ca_pem))).await?;
    let request = pb::EnrollRequest {
        join_token: join_token.expose().to_string(),
        csr_der: csr.der().to_vec(),
        agent_version: crate::AGENT_VERSION.to_string(),
        facts: Some(facts),
    };
    let enrolled = AgentServiceClient::new(channel).enroll(request).await.map_err(AgentError::Refused)?.into_inner();

    Ok(Credentials {
        control_plane: control_plane.to_string(),
        server_id: enrolled
            .server_id
            .parse()
            .map_err(|_| AgentError::Connect(format!("the control plane sent an invalid server ID `{}`", enrolled.server_id)))?,
        ca_pem,
        certificate_pem: enrolled.certificate_chain_pem,
        key_pem: Secret::new(key.serialize_pem()),
    })
}

/// A TLS channel to the control plane, which is always verified as [`CONTROL_PLANE_SERVER_NAME`].
pub(crate) async fn channel(control_plane: &str, tls: ClientTlsConfig) -> Result<Channel, AgentError> {
    let tls = tls.domain_name(CONTROL_PLANE_SERVER_NAME);
    Channel::from_shared(control_plane.to_string())
        .map_err(|_| AgentError::InvalidUrl(control_plane.to_string()))?
        .connect_timeout(std::time::Duration::from_secs(10))
        .tls_config(tls)
        .map_err(|error| AgentError::Connect(error.to_string()))?
        .connect()
        .await
        .map_err(|error| AgentError::Connect(source_chain(&error)))
}

/// Connects once to find the CA with the `expected` fingerprint in the control plane's chain,
/// and returns it as PEM.
async fn fetch_pinned_ca(control_plane: &str, expected: &str) -> Result<String, AgentError> {
    let uri: Uri = control_plane.parse().map_err(|_| AgentError::InvalidUrl(control_plane.to_string()))?;
    let (Some("https"), Some(host)) = (uri.scheme_str(), uri.host()) else {
        return Err(AgentError::InvalidUrl(control_plane.to_string()));
    };
    let port = uri.port_u16().unwrap_or(443);

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinnedCa { expected: expected.to_string(), provider: provider.clone(), found: Mutex::default() });
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("the ring provider supports the default versions")
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_no_client_auth();
    let stream = TcpStream::connect((host.trim_matches(['[', ']']), port))
        .await
        .map_err(|error| AgentError::Connect(format!("{host}:{port}: {error}")))?;
    let name = ServerName::try_from(CONTROL_PLANE_SERVER_NAME).expect("a valid DNS name");
    let handshake = tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, stream).await;

    let found = verifier.found.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
    match (handshake, found) {
        (Ok(_), Some(ca)) => Ok(der_to_pem(&ca)),
        _ => Err(AgentError::FingerprintMismatch { expected: expected.to_string() }),
    }
}

/// Accepts the server only if its chain includes a CA with the expected fingerprint and the
/// server's certificate verifies against that CA, and remembers the CA.
#[derive(Debug)]
struct PinnedCa {
    expected: String,
    provider: Arc<CryptoProvider>,
    found: Mutex<Option<Vec<u8>>>,
}

impl ServerCertVerifier for PinnedCa {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let rejected = || rustls::Error::General("no CA with the expected fingerprint".to_string());
        let ca = intermediates.iter().find(|certificate| fingerprint(certificate) == self.expected).ok_or_else(rejected)?;
        let mut roots = RootCertStore::empty();
        roots.add(ca.clone()).map_err(|_| rejected())?;
        let verifier = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), self.provider.clone())
            .build()
            .map_err(|error| rustls::Error::General(error.to_string()))?;
        verifier.verify_server_cert(end_entity, &[], server_name, ocsp_response, now)?;
        *self.found.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(ca.to_vec());
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, certificate, signature, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, certificate, signature, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// An error with its sources, since transport errors hide the useful part in them.
pub(crate) fn source_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(next) = source {
        message.push_str(": ");
        message.push_str(&next.to_string());
        source = next.source();
    }
    message
}
