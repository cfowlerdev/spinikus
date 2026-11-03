//! The proxy over real connections, in front of a small HTTP backend.

use std::{
    collections::HashMap,
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::Full;
use hyper::{Request, Response, StatusCode, body::Incoming, header, service::service_fn};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::pki_types::{CertificateDer, ServerName};
use spinikus_core::{
    certificate::CertificateBundle,
    id::{CertificateId, DeploymentId, ServiceId},
    route::{ProxyTarget, Route, RouteAction},
    secret::Secret,
};
use spinikus_proxy::{Proxy, Upstreams, serve, serve_tls};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

#[derive(Default)]
struct FixedUpstreams {
    addresses: Mutex<HashMap<DeploymentId, SocketAddr>>,
    failed: Mutex<Vec<SocketAddr>>,
    dashboard: Mutex<Option<SocketAddr>>,
}

#[async_trait]
impl Upstreams for FixedUpstreams {
    async fn resolve(&self, target: &ProxyTarget) -> Option<SocketAddr> {
        self.addresses.lock().unwrap().get(&target.deployment_id).copied()
    }

    fn failed(&self, _: &ProxyTarget, address: SocketAddr) {
        self.failed.lock().unwrap().push(address);
    }

    fn dashboard(&self) -> Option<SocketAddr> {
        *self.dashboard.lock().unwrap()
    }
}

/// Answers with the request's path and forwarding headers, and switches `/echo` to an
/// echoing protocol.
async fn backend() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let service = service_fn(|mut request: Request<Incoming>| async move {
                if request.uri().path() == "/echo" {
                    let upgrade = hyper::upgrade::on(&mut request);
                    tokio::spawn(async move {
                        let mut io = TokioIo::new(upgrade.await.unwrap());
                        let mut buffer = [0; 64];
                        let read = io.read(&mut buffer).await.unwrap();
                        io.write_all(&buffer[..read]).await.unwrap();
                    });
                    let response = Response::builder()
                        .status(StatusCode::SWITCHING_PROTOCOLS)
                        .header(header::CONNECTION, "upgrade")
                        .header(header::UPGRADE, "echo")
                        .body(Full::new(Bytes::new()))
                        .unwrap();
                    return Ok::<_, Infallible>(response);
                }
                let header =
                    |name: &str| request.headers().get(name).map(|value| value.to_str().unwrap().to_string()).unwrap_or_default();
                let body = format!(
                    "path={} host={} xff={} xfh={} xfp={} te={}",
                    request.uri().path_and_query().unwrap(),
                    header("host"),
                    header("x-forwarded-for"),
                    header("x-forwarded-host"),
                    header("x-forwarded-proto"),
                    header("te"),
                );
                Ok(Response::builder().header("keep-alive", "timeout=5").body(Full::new(Bytes::from(body))).unwrap())
            });
            tokio::spawn(async move {
                let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                    .serve_connection_with_upgrades(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    address
}

struct Running {
    proxy: Arc<Proxy>,
    upstreams: Arc<FixedUpstreams>,
    address: SocketAddr,
    https: SocketAddr,
    _stop: (oneshot::Sender<()>, oneshot::Sender<()>),
}

/// The proxy on free ports, serving HTTP and HTTPS.
async fn start() -> Running {
    let upstreams = Arc::new(FixedUpstreams::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let tls_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let https = tls_listener.local_addr().unwrap();
    let proxy = Proxy::new(upstreams.clone(), Some(https.port()));
    let (stop, stopped) = oneshot::channel::<()>();
    tokio::spawn(serve(proxy.clone(), listener, async move {
        let _ = stopped.await;
    }));
    let (stop_tls, tls_stopped) = oneshot::channel::<()>();
    tokio::spawn(serve_tls(proxy.clone(), tls_listener, async move {
        let _ = tls_stopped.await;
    }));
    Running { proxy, upstreams, address, https, _stop: (stop, stop_tls) }
}

/// A self-signed certificate for `hostnames`, and the bundle to serve it.
fn self_signed(hostnames: &[&str]) -> (CertificateDer<'static>, CertificateBundle) {
    let names: Vec<String> = hostnames.iter().map(|name| name.to_string()).collect();
    let certified = rcgen::generate_simple_self_signed(names).unwrap();
    let bundle = CertificateBundle {
        id: CertificateId::new(),
        hostnames: hostnames.iter().map(|name| name.parse().unwrap()).collect(),
        chain_pem: certified.cert.pem(),
        private_key_pem: Secret::new(certified.signing_key.serialize_pem()),
    };
    (certified.cert.der().clone(), bundle)
}

/// A raw HTTP/1.1 GET over TLS with SNI `host`, trusting only `root`.
async fn get_tls(address: SocketAddr, host: &str, root: &CertificateDer<'static>) -> std::io::Result<(u16, String)> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(root.clone()).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let stream = TcpStream::connect(address).await?;
    let mut stream = connector.connect(ServerName::try_from(host.to_string()).unwrap(), stream).await?;
    let request = format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    let mut response = String::new();
    // Servers may close without close_notify; what was read still counts.
    let _ = stream.read_to_string(&mut response).await;
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    Ok((head[9..12].parse().unwrap(), body.to_string()))
}

fn proxy_route(hostname: &str) -> (Route, ProxyTarget) {
    let target = ProxyTarget { service_id: ServiceId::new(), deployment_id: DeploymentId::new(), port: 80 };
    (Route { hostname: hostname.parse().unwrap(), action: RouteAction::Proxy(target.clone()) }, target)
}

/// Sends a raw HTTP/1.1 request and returns the status, headers and body.
async fn get(address: SocketAddr, host: &str, path: &str, extra: &str) -> (u16, String, String) {
    let mut stream = TcpStream::connect(address).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n{extra}\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    let status = head[9..12].parse().unwrap();
    (status, head.to_ascii_lowercase(), body.to_string())
}

#[tokio::test]
async fn forwards_by_host_with_forwarding_headers() {
    let running = start().await;
    let backend = backend().await;
    let (route, target) = proxy_route("app.example.com");
    running.upstreams.addresses.lock().unwrap().insert(target.deployment_id, backend);
    running.proxy.set_routes(&[route]);

    let (status, head, body) = get(running.address, "App.Example.com:8000", "/a?b=c", "TE: trailers\r\n").await;

    assert_eq!(status, 200);
    assert_eq!(body, "path=/a?b=c host=App.Example.com:8000 xff=127.0.0.1 xfh=App.Example.com:8000 xfp=http te=");
    assert!(!head.contains("keep-alive: timeout"), "{head}");
}

#[tokio::test]
async fn forwards_the_dashboard_only_where_it_listens() {
    let running = start().await;
    let backend = backend().await;
    let route = Route { hostname: "spinikus.example.com".parse().unwrap(), action: RouteAction::Dashboard };
    running.proxy.set_routes(&[route]);

    let (elsewhere, _, message) = get(running.address, "spinikus.example.com", "/", "").await;
    *running.upstreams.dashboard.lock().unwrap() = Some(backend);
    let (status, _, body) = get(running.address, "spinikus.example.com", "/login", "").await;
    *running.upstreams.dashboard.lock().unwrap() = Some(TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap());
    let (down, _, _) = get(running.address, "spinikus.example.com", "/", "").await;

    assert_eq!((elsewhere, message.as_str()), (404, "the dashboard is not served at spinikus.example.com on this server\n"));
    assert_eq!(status, 200);
    assert_eq!(body, "path=/login host=spinikus.example.com xff=127.0.0.1 xfh=spinikus.example.com xfp=http te=");
    assert_eq!(down, 502);
    assert!(running.upstreams.failed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn redirects_unknown_hosts_and_missing_apps() {
    let running = start().await;
    let (route, _) = proxy_route("app.example.com");
    let (stopped, _) = proxy_route("stopped.example.com");
    let (gone, gone_target) = proxy_route("gone.example.com");
    // Nothing listens on a port taken from a listener that is dropped.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
    running.upstreams.addresses.lock().unwrap().insert(gone_target.deployment_id, closed);
    let redirect = Route {
        hostname: "www.example.com".parse().unwrap(),
        action: RouteAction::Redirect { to: "app.example.com".parse().unwrap() },
    };
    running.proxy.set_routes(&[route, stopped, gone, redirect]);

    let (moved, head, _) = get(running.address, "www.example.com:8000", "/x?y=1", "").await;
    let (unknown, _, unknown_body) = get(running.address, "nope.example.com", "/", "").await;
    let (not_running, _, _) = get(running.address, "stopped.example.com", "/", "").await;
    let (unreachable, _, _) = get(running.address, "gone.example.com", "/", "").await;

    assert_eq!(moved, 308);
    assert!(head.contains("location: http://app.example.com:8000/x?y=1"), "{head}");
    assert_eq!((unknown, unknown_body.trim()), (404, "no app is served at nope.example.com"));
    assert_eq!(not_running, 503);
    assert_eq!(unreachable, 502);
    assert_eq!(*running.upstreams.failed.lock().unwrap(), [closed]);
}

#[tokio::test]
async fn new_routes_replace_the_old() {
    let running = start().await;
    let backend = backend().await;
    let (old, old_target) = proxy_route("old.example.com");
    let (new, new_target) = proxy_route("new.example.com");
    for target in [&old_target, &new_target] {
        running.upstreams.addresses.lock().unwrap().insert(target.deployment_id, backend);
    }
    running.proxy.set_routes(&[old]);
    let (before, _, _) = get(running.address, "old.example.com", "/", "").await;

    running.proxy.set_routes(&[new]);

    assert_eq!(before, 200);
    assert_eq!(get(running.address, "old.example.com", "/", "").await.0, 404);
    assert_eq!(get(running.address, "new.example.com", "/", "").await.0, 200);
}

#[tokio::test]
async fn splices_upgraded_connections() {
    let running = start().await;
    let backend = backend().await;
    let (route, target) = proxy_route("app.example.com");
    running.upstreams.addresses.lock().unwrap().insert(target.deployment_id, backend);
    running.proxy.set_routes(&[route]);

    let mut stream = TcpStream::connect(running.address).await.unwrap();
    let request = "GET /echo HTTP/1.1\r\nHost: app.example.com\r\nConnection: Upgrade\r\nUpgrade: echo\r\n\r\n";
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(stream.read_u8().await.unwrap());
    }
    stream.write_all(b"hello").await.unwrap();
    let mut echoed = [0; 5];
    stream.read_exact(&mut echoed).await.unwrap();

    let head = String::from_utf8(head).unwrap().to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 101"), "{head}");
    assert!(head.contains("upgrade: echo"), "{head}");
    assert_eq!(&echoed, b"hello");
}

#[tokio::test]
async fn serves_https_by_sni_and_redirects_http_there() {
    let running = start().await;
    let backend = backend().await;
    let (route, target) = proxy_route("app.example.com");
    let (plain, plain_target) = proxy_route("plain.example.com");
    for target in [&target, &plain_target] {
        running.upstreams.addresses.lock().unwrap().insert(target.deployment_id, backend);
    }
    running.proxy.set_routes(&[route, plain]);
    let (root, bundle) = self_signed(&["app.example.com"]);
    let (_, mut broken) = self_signed(&["broken.example.com"]);
    broken.private_key_pem = Secret::new("not a key");

    let problems = running.proxy.set_certificates(&[bundle, broken]);
    let (status, body) = get_tls(running.https, "app.example.com", &root).await.unwrap();
    let (moved, head, _) = get(running.address, "app.example.com:8000", "/a?b=1", "").await;
    let (plain_status, _, plain_body) = get(running.address, "plain.example.com", "/", "").await;
    let unknown = get_tls(running.https, "plain.example.com", &root).await;

    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("unreadable private key"), "{problems:?}");
    assert_eq!(status, 200);
    assert!(body.contains("xfp=https"), "{body}");
    assert_eq!(moved, 308);
    assert!(head.contains(&format!("location: https://app.example.com:{}/a?b=1", running.https.port())), "{head}");
    // Without a certificate, HTTP keeps serving.
    assert_eq!(plain_status, 200);
    assert!(plain_body.contains("xfp=http "), "{plain_body}");
    assert!(unknown.is_err(), "no certificate for the name fails the handshake");
}

#[tokio::test]
async fn answers_acme_challenges_over_http_for_any_host() {
    let running = start().await;
    let (_, bundle) = self_signed(&["app.example.com"]);
    running.proxy.set_certificates(&[bundle]);
    running.proxy.set_challenges(HashMap::from([("token-1".to_string(), "token-1.thumbprint".to_string())]));

    let (status, _, body) = get(running.address, "app.example.com", "/.well-known/acme-challenge/token-1", "").await;
    let (unknown, _, _) = get(running.address, "new.example.com", "/.well-known/acme-challenge/token-2", "").await;
    running.proxy.set_challenges(HashMap::new());
    let (withdrawn, _, _) = get(running.address, "app.example.com", "/.well-known/acme-challenge/token-1", "").await;

    // Not redirected, though the host has a certificate.
    assert_eq!((status, body.as_str()), (200, "token-1.thumbprint"));
    assert_eq!(unknown, 404);
    assert_eq!(withdrawn, 404);
}
