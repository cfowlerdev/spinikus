//! The control stream over a real loopback gRPC connection. An interceptor stands in for
//! the mTLS layer and sets the agent's identity.

use std::time::Duration;

use spinikus_core::{
    id::{DeploymentId, ServerId, ServiceId},
    server::{ServerFields, ServerStatus},
};
use spinikus_proto::agent::v1::{
    self as pb, AgentMessage, ControlMessage, agent_message, agent_service_client::AgentServiceClient,
    agent_service_server::AgentServiceServer, control_message,
};
use spinikus_store::{InstanceRepository, ServerRepository};
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Code, Streaming, transport::Channel};

use super::{AgentGrpc, AgentIdentity, AgentTimings};
use crate::{Control, test_support::control};

/// Serves the agent service on a free port. With `identity`, every request is from that server.
async fn serve(control: &Control, identity: Option<ServerId>, timings: AgentTimings) -> AgentServiceClient<Channel> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let service = AgentServiceServer::with_interceptor(
        AgentGrpc::new(control.clone()).with_timings(timings),
        move |mut request: tonic::Request<()>| {
            if let Some(server) = identity {
                request.extensions_mut().insert(AgentIdentity(server));
            }
            Ok(request)
        },
    );
    let incoming = TcpListenerStream::new(listener);
    tokio::spawn(tonic::transport::Server::builder().add_service(service).serve_with_incoming(incoming));
    let channel = Channel::from_shared(format!("http://{address}")).unwrap().connect().await.unwrap();
    AgentServiceClient::new(channel)
}

/// An agent's side of the control stream.
struct Agent {
    outbound: mpsc::Sender<AgentMessage>,
    inbound: Streaming<ControlMessage>,
}

impl Agent {
    /// Opens the control stream, sending `first` before anything else.
    async fn connect(client: &mut AgentServiceClient<Channel>, first: agent_message::Message) -> Result<Self, tonic::Status> {
        let (outbound, receiver) = mpsc::channel(8);
        outbound.send(AgentMessage { message: Some(first) }).await.unwrap();
        let inbound = client.connect(ReceiverStream::new(receiver)).await?.into_inner();
        Ok(Self { outbound, inbound })
    }

    async fn send(&self, message: agent_message::Message) {
        self.outbound.send(AgentMessage { message: Some(message) }).await.unwrap();
    }

    async fn receive(&mut self) -> Option<control_message::Message> {
        tokio::time::timeout(Duration::from_secs(5), self.inbound.message()).await.unwrap().ok().flatten()?.message
    }
}

fn hello(protocol_version: u32) -> agent_message::Message {
    agent_message::Message::Hello(pb::Hello {
        protocol_version,
        agent_version: "0.1.0".to_string(),
        facts: Some(pb::ServerFacts { hostname: "web-1".to_string(), cpu_count: 4, ..Default::default() }),
        ..Default::default()
    })
}

fn container(id: &str) -> pb::Instance {
    pb::Instance {
        container_id: id.to_string(),
        service_id: ServiceId::new().to_string(),
        deployment_id: DeploymentId::new().to_string(),
        status: pb::InstanceStatus::Running.into(),
        health: pb::InstanceHealth::None.into(),
        ..Default::default()
    }
}

async fn server(control: &Control) -> ServerId {
    control.store().create_server(ServerFields { name: "web-1".to_string() }).await.unwrap().id
}

/// Waits up to a few seconds for the server to reach `status`.
async fn wait_for_status(control: &Control, server: ServerId, status: ServerStatus) {
    for _ in 0..100 {
        if control.store().get_server(server).await.unwrap().status == status {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("server never became {status}");
}

async fn wait_for_containers(control: &Control, server: ServerId, expected: &[&str]) {
    for _ in 0..100 {
        let mut containers: Vec<_> = control
            .store()
            .list_server_instances(server)
            .await
            .unwrap()
            .into_iter()
            .map(|instance| instance.observed.container_id)
            .collect();
        containers.sort();
        if containers == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the server's containers never became {expected:?}");
}

#[tokio::test]
async fn an_agent_connects_reports_and_disconnects() {
    let (_dir, control) = control().await;
    let id = server(&control).await;
    let mut client = serve(&control, Some(id), AgentTimings::default()).await;

    let mut agent = Agent::connect(&mut client, hello(pb::PROTOCOL_VERSION)).await.unwrap();
    let Some(control_message::Message::Welcome(welcome)) = agent.receive().await else { panic!("expected Welcome") };
    let stored = control.store().get_server(id).await.unwrap();

    assert_eq!(welcome.server_id, id.to_string());
    assert_eq!(welcome.heartbeat_interval.unwrap().seconds, 10);
    assert_eq!(stored.status, ServerStatus::Connected);
    assert_eq!(stored.agent.unwrap().facts.hostname, "web-1");
    assert!(control.agents().is_connected(id));

    let malformed = pb::Instance { service_id: "web".to_string(), ..container("bad") };
    let full = pb::StateReport { full: true, instances: vec![container("a"), container("b"), malformed], ..Default::default() };
    agent.send(agent_message::Message::StateReport(full)).await;
    wait_for_containers(&control, id, &["a", "b"]).await;
    let delta = pb::StateReport { full: false, instances: vec![container("c")], removed_container_ids: vec!["a".to_string()] };
    agent.send(agent_message::Message::StateReport(delta)).await;
    wait_for_containers(&control, id, &["b", "c"]).await;
    agent.send(agent_message::Message::Heartbeat(pb::Heartbeat::default())).await;

    let restart = pb::Command {
        command_id: "cmd_1".to_string(),
        kind: Some(pb::command::Kind::RestartInstance(pb::RestartInstance { container_id: "b".to_string() })),
    };
    control
        .agents()
        .send(id, ControlMessage { message: Some(control_message::Message::Command(restart.clone())) })
        .await
        .unwrap();
    assert_eq!(agent.receive().await, Some(control_message::Message::Command(restart)));

    drop(agent);
    wait_for_status(&control, id, ServerStatus::Disconnected).await;
    assert!(!control.agents().is_connected(id));
    assert!(control.agents().send(id, ControlMessage::default()).await.is_err());
}

#[tokio::test]
async fn refuses_agents_it_should_not_talk_to() {
    let (_dir, control) = control().await;
    let id = server(&control).await;
    let revoked = server(&control).await;
    control.store().revoke_server(revoked).await.unwrap();
    let timings = AgentTimings::default();

    let anonymous = Agent::connect(&mut serve(&control, None, timings).await, hello(1)).await.err().unwrap();
    let old = Agent::connect(&mut serve(&control, Some(id), timings).await, hello(0)).await.err().unwrap();
    let heartbeat_first = agent_message::Message::Heartbeat(pb::Heartbeat::default());
    let rude = Agent::connect(&mut serve(&control, Some(id), timings).await, heartbeat_first).await.err().unwrap();
    let blocked = Agent::connect(&mut serve(&control, Some(revoked), timings).await, hello(1)).await.err().unwrap();

    assert_eq!(anonymous.code(), Code::Unauthenticated);
    assert_eq!(old.code(), Code::FailedPrecondition);
    assert_eq!(rude.code(), Code::InvalidArgument);
    assert_eq!(blocked.code(), Code::PermissionDenied);
    assert_eq!(control.store().get_server(id).await.unwrap().status, ServerStatus::Pending);
    assert!(control.agents().connected_servers().is_empty());
}

#[tokio::test]
async fn a_reconnect_replaces_the_old_connection() {
    let (_dir, control) = control().await;
    let id = server(&control).await;
    let mut client = serve(&control, Some(id), AgentTimings::default()).await;

    let mut first = Agent::connect(&mut client, hello(1)).await.unwrap();
    first.receive().await.unwrap();
    let mut second = Agent::connect(&mut client, hello(1)).await.unwrap();
    second.receive().await.unwrap();

    assert_eq!(first.receive().await, None, "the old stream is closed");
    drop(first);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(control.store().get_server(id).await.unwrap().status, ServerStatus::Connected);
    assert_eq!(control.agents().connected_servers(), [id]);

    drop(second);
    wait_for_status(&control, id, ServerStatus::Disconnected).await;
}

#[tokio::test]
async fn a_quiet_agent_is_disconnected() {
    let (_dir, control) = control().await;
    let id = server(&control).await;
    let timings = AgentTimings { idle_timeout: Duration::from_millis(200), ..AgentTimings::default() };
    let mut client = serve(&control, Some(id), timings).await;

    let mut agent = Agent::connect(&mut client, hello(1)).await.unwrap();
    agent.receive().await.unwrap();

    assert_eq!(agent.receive().await, None);
    wait_for_status(&control, id, ServerStatus::Disconnected).await;
}
