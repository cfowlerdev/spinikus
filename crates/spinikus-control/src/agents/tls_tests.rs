//! Enrollment and identity over the real TLS listener, as an agent sees them.

use std::time::Duration;

use rcgen::{CertificateParams, KeyPair};
use spinikus_core::{enrollment::CONTROL_PLANE_SERVER_NAME, id::ServerId};
use spinikus_proto::agent::v1::{
    self as pb, AgentMessage, agent_message, agent_service_client::AgentServiceClient, control_message,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    Code,
    transport::{Certificate, Channel, ClientTlsConfig, Identity},
};

use crate::{
    Control, NewServer, Principal,
    ca::server_in_certificate,
    test_support::{control, user},
};

struct Setup {
    _dir: tempfile::TempDir,
    control: Control,
    admin: Principal,
    address: String,
    ca_pem: String,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

async fn setup() -> Setup {
    let (dir, control) = control().await;
    let admin = user(&control, "admin@example.com", true).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("https://{}", listener.local_addr().unwrap());
    let (shutdown, stopped) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(super::serve(control.clone(), listener, async move {
        let _ = stopped.await;
    }));
    let ca_pem = control.certificate_authority().await.unwrap().certificate_pem().to_string();
    Setup { _dir: dir, control, admin, address, ca_pem, _shutdown: shutdown }
}

impl Setup {
    /// A client that trusts only the control plane's CA, with `identity` as its client certificate if given.
    async fn client(&self, identity: Option<(&str, &KeyPair)>) -> AgentServiceClient<Channel> {
        let mut tls =
            ClientTlsConfig::new().ca_certificate(Certificate::from_pem(&self.ca_pem)).domain_name(CONTROL_PLANE_SERVER_NAME);
        if let Some((chain, key)) = identity {
            tls = tls.identity(Identity::from_pem(chain, key.serialize_pem()));
        }
        let endpoint = Channel::from_shared(self.address.clone()).unwrap().tls_config(tls).unwrap();
        let mut last_error = None;
        for _ in 0..50 {
            match endpoint.connect().await {
                Ok(channel) => return AgentServiceClient::new(channel),
                Err(error) => last_error = Some(error),
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("could not connect: {last_error:?}");
    }

    async fn join_token(&self) -> (ServerId, String) {
        let server = self.control.create_server(&self.admin, NewServer { name: "web-1".to_string() }).await.unwrap();
        let join = self.control.create_join_token(&self.admin, server.id).await.unwrap();
        (server.id, join.secret.expose().to_string())
    }
}

fn csr(key: &KeyPair) -> Vec<u8> {
    CertificateParams::new(Vec::<String>::new()).unwrap().serialize_request(key).unwrap().der().to_vec()
}

fn enroll_request(join_token: &str, csr_der: Vec<u8>) -> pb::EnrollRequest {
    pb::EnrollRequest { join_token: join_token.to_string(), csr_der, ..Default::default() }
}

/// The agent's sender (which keeps its side open while held) and the control plane's stream.
type Connection = (mpsc::Sender<AgentMessage>, tonic::Streaming<pb::ControlMessage>);

async fn connect(client: &mut AgentServiceClient<Channel>) -> Result<Connection, tonic::Status> {
    let (sender, receiver) = mpsc::channel(4);
    let hello = pb::Hello { protocol_version: pb::PROTOCOL_VERSION, ..Default::default() };
    sender.send(AgentMessage { message: Some(agent_message::Message::Hello(hello)) }).await.unwrap();
    let stream = client.connect(ReceiverStream::new(receiver)).await?.into_inner();
    Ok((sender, stream))
}

#[tokio::test]
async fn an_agent_enrolls_then_connects_with_its_certificate() {
    let setup = setup().await;
    let (server, join_token) = setup.join_token().await;
    let key = KeyPair::generate().unwrap();
    let mut anonymous = setup.client(None).await;

    let enrolled = anonymous.enroll(enroll_request(&join_token, csr(&key))).await.unwrap().into_inner();
    let reused = anonymous.enroll(enroll_request(&join_token, csr(&key))).await.unwrap_err();
    let without_certificate = connect(&mut anonymous).await.err().unwrap();

    assert_eq!(enrolled.server_id, server.to_string());
    assert_eq!(enrolled.ca_certificate_pem, setup.ca_pem);
    let certificate = crate::ca::certificate_der(&enrolled.certificate_chain_pem).unwrap();
    assert_eq!(server_in_certificate(&certificate), Some(server));
    assert_eq!(reused.code(), Code::Unauthenticated);
    assert_eq!(without_certificate.code(), Code::Unauthenticated);

    let mut agent = setup.client(Some((&enrolled.certificate_chain_pem, &key))).await;
    let (_sender, mut stream) = connect(&mut agent).await.unwrap();
    let Some(control_message::Message::Welcome(welcome)) = stream.message().await.unwrap().unwrap().message else {
        panic!("expected Welcome")
    };
    let new_key = KeyPair::generate().unwrap();
    let renewed = agent.renew_certificate(pb::RenewCertificateRequest { csr_der: csr(&new_key) }).await.unwrap().into_inner();

    assert_eq!(welcome.server_id, server.to_string());
    let renewed = crate::ca::certificate_der(&renewed.certificate_chain_pem).unwrap();
    assert_eq!(server_in_certificate(&renewed), Some(server));
    assert_ne!(renewed, certificate);
}

#[tokio::test]
async fn a_bad_csr_leaves_the_join_token_usable() {
    let setup = setup().await;
    let (_, join_token) = setup.join_token().await;
    let mut anonymous = setup.client(None).await;

    let bad = anonymous.enroll(enroll_request(&join_token, b"not a csr".to_vec())).await.unwrap_err();
    let good = anonymous.enroll(enroll_request(&join_token, csr(&KeyPair::generate().unwrap()))).await;

    assert_eq!(bad.code(), Code::InvalidArgument);
    assert!(good.is_ok(), "{good:?}");
}

#[tokio::test]
async fn revoking_a_server_cuts_off_its_agent() {
    let setup = setup().await;
    let (server, join_token) = setup.join_token().await;
    let key = KeyPair::generate().unwrap();
    let enrolled = setup.client(None).await.enroll(enroll_request(&join_token, csr(&key))).await.unwrap().into_inner();
    let mut agent = setup.client(Some((&enrolled.certificate_chain_pem, &key))).await;
    let (_sender, mut stream) = connect(&mut agent).await.unwrap();
    stream.message().await.unwrap().unwrap();

    setup.control.revoke_server(&setup.admin, server).await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(5), stream.message()).await.unwrap();
    let reconnect = connect(&mut agent).await.err().unwrap();
    let renew = agent.renew_certificate(pb::RenewCertificateRequest { csr_der: csr(&key) }).await.unwrap_err();

    assert!(matches!(ended, Ok(None)), "{ended:?}");
    assert_eq!(reconnect.code(), Code::PermissionDenied);
    assert_eq!(renew.code(), Code::PermissionDenied);
    assert!(!setup.control.agents().is_connected(server));
}
