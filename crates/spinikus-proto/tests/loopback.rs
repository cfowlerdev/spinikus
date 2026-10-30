//! The generated client and server talk to each other over a real (plaintext, loopback)
//! connection, and messages survive an encode/decode round-trip.

use std::pin::Pin;

use prost::Message;
use spinikus_proto::agent::v1::{
    AgentMessage, Command, ControlMessage, EnrollRequest, EnrollResponse, Hello, PushStatsResponse, RenewCertificateRequest,
    RenewCertificateResponse, ServerFacts, SessionFrame, StatsBatch, StopDeployment, Welcome, agent_message,
    agent_service_client::AgentServiceClient,
    agent_service_server::{AgentService, AgentServiceServer},
    command, control_message,
};
use tokio_stream::{Stream, StreamExt, wrappers::TcpListenerStream};
use tonic::{Request, Response, Status, Streaming, transport::Server};

type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

/// Answers each `Hello` with a `Welcome` naming the agent's hostname as its server.
struct Greeter;

#[tonic::async_trait]
impl AgentService for Greeter {
    async fn enroll(&self, _: Request<EnrollRequest>) -> Result<Response<EnrollResponse>, Status> {
        Err(Status::unimplemented("enroll"))
    }

    async fn renew_certificate(&self, _: Request<RenewCertificateRequest>) -> Result<Response<RenewCertificateResponse>, Status> {
        Err(Status::unimplemented("renew_certificate"))
    }

    type ConnectStream = BoxStream<ControlMessage>;

    async fn connect(&self, request: Request<Streaming<AgentMessage>>) -> Result<Response<Self::ConnectStream>, Status> {
        let replies = request.into_inner().filter_map(|message| match message {
            Ok(AgentMessage { message: Some(agent_message::Message::Hello(hello)) }) => Some(Ok(ControlMessage {
                message: Some(control_message::Message::Welcome(Welcome {
                    server_id: format!("srv_{}", hello.facts.unwrap_or_default().hostname),
                    heartbeat_interval: Some(prost_types::Duration { seconds: 15, nanos: 0 }),
                })),
            })),
            Ok(_) => None,
            Err(status) => Some(Err(status)),
        });
        Ok(Response::new(Box::pin(replies)))
    }

    type OpenSessionStream = BoxStream<SessionFrame>;

    async fn open_session(&self, _: Request<Streaming<SessionFrame>>) -> Result<Response<Self::OpenSessionStream>, Status> {
        Err(Status::unimplemented("open_session"))
    }

    async fn push_stats(&self, _: Request<Streaming<StatsBatch>>) -> Result<Response<PushStatsResponse>, Status> {
        Err(Status::unimplemented("push_stats"))
    }
}

fn hello() -> AgentMessage {
    AgentMessage {
        message: Some(agent_message::Message::Hello(Hello {
            protocol_version: spinikus_proto::agent::v1::PROTOCOL_VERSION,
            agent_version: "0.1.0".to_string(),
            facts: Some(ServerFacts { hostname: "web-1".to_string(), cpu_count: 4, ..Default::default() }),
            in_flight_command_ids: vec!["cmd_1".to_string()],
            ..Default::default()
        })),
    }
}

#[tokio::test]
async fn connects_over_loopback() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(
        Server::builder().add_service(AgentServiceServer::new(Greeter)).serve_with_incoming(TcpListenerStream::new(listener)),
    );
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}")).unwrap().connect().await.unwrap();
    let mut client = AgentServiceClient::new(channel);

    let mut replies = client.connect(tokio_stream::iter([hello()])).await.unwrap().into_inner();
    let welcome = replies.next().await.unwrap().unwrap();
    let unimplemented = client.enroll(EnrollRequest::default()).await.unwrap_err();

    let Some(control_message::Message::Welcome(welcome)) = welcome.message else { panic!("expected Welcome, got {welcome:?}") };
    assert_eq!(welcome.server_id, "srv_web-1");
    assert_eq!(welcome.heartbeat_interval.unwrap().seconds, 15);
    assert!(replies.next().await.is_none(), "the stream ends when the agent's side does");
    assert_eq!(unimplemented.code(), tonic::Code::Unimplemented);
}

#[test]
fn messages_round_trip() {
    let command = ControlMessage {
        message: Some(control_message::Message::Command(Command {
            command_id: "cmd_01".to_string(),
            kind: Some(command::Kind::StopDeployment(StopDeployment {
                deployment_id: "dep_01".to_string(),
                grace_period: Some(prost_types::Duration { seconds: 30, nanos: 0 }),
            })),
        })),
    };

    assert_eq!(ControlMessage::decode(command.encode_to_vec().as_slice()).unwrap(), command);
    assert_eq!(AgentMessage::decode(hello().encode_to_vec().as_slice()).unwrap(), hello());
}

#[test]
fn unknown_fields_and_variants_are_tolerated() {
    // An older peer reading a message from a newer one: an unknown oneof variant decodes
    // as `None` instead of failing, so additive changes within v1 stay compatible.
    let mut bytes = Vec::new();
    prost::encoding::string::encode(99, &"from the future".to_string(), &mut bytes);

    assert_eq!(ControlMessage::decode(bytes.as_slice()).unwrap(), ControlMessage { message: None });
}
