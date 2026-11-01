//! Builds with BuildKit secrets. bollard starts a BuildKit session of its own for every
//! BuildKit build, serving only registry credentials, and keeps that plumbing private, so a
//! build with secrets talks to Docker itself: it opens `/session`, an HTTP/1.1 connection
//! upgraded to h2c over which Docker calls back into the agent with gRPC, serves the secrets
//! (and health) services there, then sends `/build` naming that session and reads its output
//! as bollard would. Dockerfiles read the secrets with `RUN --mount=type=secret,id=<NAME>`;
//! they never become part of the image.
//!
//! This needs Docker on a Unix socket or plain TCP. Builds without secrets go through bollard.

use std::{
    collections::HashMap,
    path::PathBuf,
    pin::Pin,
    task::{Context, Poll},
};

use base64::Engine as _;
use bollard::models::{BuildInfo, BuildInfoAux};
use bollard_buildkit_proto::{
    health::{
        HealthCheckRequest, HealthCheckResponse, HealthListRequest, HealthListResponse,
        health_check_response::ServingStatus,
        health_server::{Health, HealthServer},
    },
    moby::buildkit::{
        secrets::v1::{
            GetSecretRequest, GetSecretResponse,
            secrets_server::{Secrets, SecretsServer},
        },
        v1::StatusResponse,
    },
};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty, StreamBody};
use hyper::{
    Request, StatusCode,
    body::Frame,
    header::{CONNECTION, CONTENT_TYPE, HOST, UPGRADE},
};
use hyper_util::rt::TokioIo;
use prost::Message;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_stream::StreamExt;

use super::{BuildSpec, RuntimeError};

/// The gRPC methods the session serves, which Docker needs to be told.
const METHODS: [&str; 2] = ["/moby.buildkit.secrets.v1.Secrets/GetSecret", "/grpc.health.v1.Health/Check"];

/// Where the Docker daemon listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Endpoint {
    Unix(PathBuf),
    /// `host:port`, plain HTTP.
    Tcp(String),
}

impl Endpoint {
    /// `DOCKER_HOST` if set, else the local socket the runtime would use.
    pub(super) fn from_env(local_socket: Option<String>) -> Result<Self, RuntimeError> {
        let host = std::env::var("DOCKER_HOST").ok().filter(|host| !host.is_empty());
        Self::parse(host.as_deref(), std::env::var_os("DOCKER_TLS_VERIFY").is_some(), local_socket)
    }

    fn parse(host: Option<&str>, tls: bool, local_socket: Option<String>) -> Result<Self, RuntimeError> {
        match host {
            None => Ok(Self::Unix(local_socket.unwrap_or_else(|| "/var/run/docker.sock".to_string()).into())),
            Some(host) if host.starts_with("unix://") => Ok(Self::Unix(host["unix://".len()..].into())),
            Some(host) if host.starts_with("tcp://") && !tls => {
                Ok(Self::Tcp(host["tcp://".len()..].trim_end_matches('/').to_string()))
            }
            Some(host) => Err(RuntimeError::Failed(format!(
                "builds with secret variables need Docker on a Unix socket or plain TCP, not `{host}`"
            ))),
        }
    }

    async fn connect(&self) -> Result<TokioIo<Pin<Box<dyn Io>>>, RuntimeError> {
        let unavailable = |error: std::io::Error| RuntimeError::Unavailable(error.to_string());
        let stream: Pin<Box<dyn Io>> = match self {
            Self::Unix(path) => Box::pin(tokio::net::UnixStream::connect(path).await.map_err(unavailable)?),
            Self::Tcp(address) => Box::pin(tokio::net::TcpStream::connect(address).await.map_err(unavailable)?),
        };
        Ok(TokioIo::new(stream))
    }
}

trait Io: AsyncRead + AsyncWrite + Send {}
impl<T: AsyncRead + AsyncWrite + Send> Io for T {}

fn failed(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Failed(error.to_string())
}

/// An open session; dropping it ends it.
pub(super) struct Session {
    pub(super) id: String,
    serving: tokio::task::JoinHandle<()>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

/// Opens a session that gives BuildKit `secrets` by their IDs.
pub(super) async fn open(endpoint: &Endpoint, secrets: HashMap<String, Vec<u8>>) -> Result<Session, RuntimeError> {
    let (mut sender, connection) = hyper::client::conn::http1::handshake(endpoint.connect().await?)
        .await
        .map_err(|error| RuntimeError::Unavailable(error.to_string()))?;
    tokio::spawn(async move {
        if let Err(error) = connection.with_upgrades().await {
            tracing::debug!(%error, "the BuildKit session's connection ended");
        }
    });
    let id = format!("spinikus-{:032x}", rand::random::<u128>());
    let mut request = Request::post("/session")
        .header(HOST, "docker")
        .header(CONNECTION, "Upgrade")
        .header(UPGRADE, "h2c")
        .header("X-Docker-Expose-Session-Uuid", &id)
        .header("X-Docker-Expose-Session-Name", "spinikus");
    for method in METHODS {
        request = request.header("X-Docker-Expose-Session-Grpc-Method", method);
    }
    let response = sender.send_request(request.body(Empty::<Bytes>::new()).map_err(failed)?).await.map_err(failed)?;
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        let status = response.status();
        let body = response.into_body().collect().await.map(|body| body.to_bytes()).unwrap_or_default();
        return Err(RuntimeError::Failed(format!("Docker refused a BuildKit session ({status}): {}", message(&body))));
    }
    let upgraded = hyper::upgrade::on(response).await.map_err(failed)?;
    let transport = Transport(TokioIo::new(upgraded));
    let server = tonic::transport::Server::builder()
        .add_service(HealthServer::new(Healthy))
        .add_service(SecretsServer::new(SecretStore(secrets)));
    let serving = tokio::spawn(async move {
        let incoming = tokio_stream::once(Ok::<_, std::io::Error>(transport));
        if let Err(error) = server.serve_with_incoming(incoming).await {
            tracing::debug!(%error, "the BuildKit session ended");
        }
    });
    Ok(Session { id, serving })
}

/// Runs a BuildKit build of `context` (a tar stream) in `session`, giving `show` its output.
/// A failed build is `Failed`, with the builder's error.
pub(super) async fn build(
    endpoint: &Endpoint,
    session: &Session,
    spec: &BuildSpec,
    context: tokio::io::DuplexStream,
    show: &mut (dyn FnMut(BuildInfo) + Send),
) -> Result<(), RuntimeError> {
    let (mut sender, connection) = hyper::client::conn::http1::handshake(endpoint.connect().await?)
        .await
        .map_err(|error| RuntimeError::Unavailable(error.to_string()))?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::debug!(%error, "the build's connection ended");
        }
    });
    let args: HashMap<&str, &str> = spec.args.iter().map(|(name, value)| (name.as_str(), value.as_str())).collect();
    let mut query = vec![
        ("version", "2".to_string()),
        ("session", session.id.clone()),
        ("dockerfile", spec.dockerfile.clone()),
        ("t", spec.tag.clone()),
        ("rm", "1".to_string()),
        ("buildargs", serde_json::to_string(&args).expect("strings serialize")),
    ];
    if let Some(target) = &spec.target {
        query.push(("target", target.clone()));
    }
    let query: Vec<String> = query.iter().map(|(name, value)| format!("{name}={}", encode(value))).collect();
    let body = StreamBody::new(tokio_util::io::ReaderStream::new(context).map(|chunk| chunk.map(Frame::data)));
    let request = Request::post(format!("/build?{}", query.join("&")))
        .header(HOST, "docker")
        .header(CONTENT_TYPE, "application/x-tar")
        .body(body)
        .map_err(failed)?;
    let response = sender.send_request(request).await.map_err(failed)?;
    let status = response.status();
    let mut body = response.into_body();
    if !status.is_success() {
        let body = body.collect().await.map(|body| body.to_bytes()).unwrap_or_default();
        return Err(RuntimeError::Failed(message(&body)));
    }
    let mut pending = Vec::new();
    while let Some(frame) = body.frame().await {
        let Ok(data) = frame.map_err(failed)?.into_data() else { continue };
        pending.extend_from_slice(&data);
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            handle(&line, show)?;
        }
    }
    handle(&pending, show)
}

/// One line of the build's output: progress for `show`, or the build's error.
fn handle(line: &[u8], show: &mut (dyn FnMut(BuildInfo) + Send)) -> Result<(), RuntimeError> {
    let line = line.trim_ascii();
    if line.is_empty() {
        return Ok(());
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
        return Ok(());
    };
    if let Some(error) = value.get("errorDetail").and_then(|detail| detail.get("message")).or(value.get("error")) {
        return Err(RuntimeError::Failed(error.as_str().unwrap_or_default().to_string()));
    }
    if value.get("id").and_then(serde_json::Value::as_str) == Some("moby.buildkit.trace") {
        let status = value
            .get("aux")
            .and_then(serde_json::Value::as_str)
            .and_then(|aux| base64::engine::general_purpose::STANDARD.decode(aux).ok())
            .and_then(|bytes| StatusResponse::decode(bytes.as_slice()).ok());
        if let Some(status) = status {
            show(BuildInfo { aux: Some(BuildInfoAux::BuildKit(status)), ..Default::default() });
        }
        return Ok(());
    }
    if let Some(stream) = value.get("stream").and_then(serde_json::Value::as_str) {
        show(BuildInfo { stream: Some(stream.to_string()), ..Default::default() });
    }
    Ok(())
}

/// The `message` of an Engine API error body, or the body.
fn message(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("message").and_then(serde_json::Value::as_str).map(str::to_string))
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string())
}

/// Percent-encodes a query value.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (byte as char).to_string(),
            byte => format!("%{byte:02X}"),
        })
        .collect()
}

struct SecretStore(HashMap<String, Vec<u8>>);

#[tonic::async_trait]
impl Secrets for SecretStore {
    async fn get_secret(
        &self,
        request: tonic::Request<GetSecretRequest>,
    ) -> Result<tonic::Response<GetSecretResponse>, tonic::Status> {
        let id = &request.get_ref().id;
        match self.0.get(id) {
            Some(data) => Ok(tonic::Response::new(GetSecretResponse { data: data.clone() })),
            None => Err(tonic::Status::not_found(format!("there is no build secret `{id}`"))),
        }
    }
}

struct Healthy;

#[tonic::async_trait]
impl Health for Healthy {
    async fn check(
        &self,
        _request: tonic::Request<HealthCheckRequest>,
    ) -> Result<tonic::Response<HealthCheckResponse>, tonic::Status> {
        Ok(tonic::Response::new(HealthCheckResponse { status: ServingStatus::Serving.into() }))
    }

    type WatchStream = Pin<Box<dyn tokio_stream::Stream<Item = Result<HealthCheckResponse, tonic::Status>> + Send>>;

    async fn watch(
        &self,
        _request: tonic::Request<HealthCheckRequest>,
    ) -> Result<tonic::Response<Self::WatchStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("watch"))
    }

    async fn list(
        &self,
        _request: tonic::Request<HealthListRequest>,
    ) -> Result<tonic::Response<HealthListResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("list"))
    }
}

/// The upgraded connection, as a connection tonic serves.
struct Transport(TokioIo<hyper::upgrade::Upgraded>);

impl tonic::transport::server::Connected for Transport {
    type ConnectInfo = ();

    fn connect_info(&self) -> Self::ConnectInfo {}
}

impl AsyncRead for Transport {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for Transport {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_docker_where_the_runtime_does() {
        let local = Some("/home/me/.docker/run/docker.sock".to_string());

        assert_eq!(Endpoint::parse(None, false, local.clone()), Ok(Endpoint::Unix("/home/me/.docker/run/docker.sock".into())));
        assert_eq!(Endpoint::parse(None, false, None), Ok(Endpoint::Unix("/var/run/docker.sock".into())));
        assert_eq!(Endpoint::parse(Some("unix:///run/docker.sock"), false, None), Ok(Endpoint::Unix("/run/docker.sock".into())));
        assert_eq!(Endpoint::parse(Some("tcp://10.0.0.2:2375/"), false, None), Ok(Endpoint::Tcp("10.0.0.2:2375".to_string())));
        assert!(Endpoint::parse(Some("tcp://10.0.0.2:2376"), true, None).is_err(), "TLS is not supported");
        assert!(Endpoint::parse(Some("ssh://me@host"), false, None).is_err());
    }

    #[test]
    fn reads_progress_and_errors_from_the_build_output() {
        let mut shown = Vec::new();
        let status = StatusResponse::default().encode_to_vec();
        let trace =
            serde_json::json!({ "id": "moby.buildkit.trace", "aux": base64::engine::general_purpose::STANDARD.encode(status) });

        handle(trace.to_string().as_bytes(), &mut |info| shown.push(info)).unwrap();
        handle(br#"{"stream":"Step 1/2"}"#, &mut |info| shown.push(info)).unwrap();
        handle(b"  \r\n", &mut |info| shown.push(info)).unwrap();
        let error = handle(br#"{"errorDetail":{"message":"exit code: 1"},"error":"exit code: 1"}"#, &mut |_| {}).unwrap_err();

        assert!(matches!(shown[0].aux, Some(BuildInfoAux::BuildKit(_))));
        assert_eq!(shown[1].stream.as_deref(), Some("Step 1/2"));
        assert_eq!(shown.len(), 2);
        assert_eq!(error, RuntimeError::Failed("exit code: 1".to_string()));
        assert_eq!(encode(r#"{"A":"b c"}"#), "%7B%22A%22%3A%22b%20c%22%7D");
    }
}
