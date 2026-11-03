//! HTTPS through a real control plane and agent: the certificate manager orders a
//! certificate from a fake CA, which checks the agent's proxy answers the HTTP-01 challenge
//! as a CA would, and the proxy then serves the domain over HTTPS and redirects HTTP there.
//! The embedded server's proxy serves the dashboard on its domain the same way.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

mod common;

use async_trait::async_trait;
use common::ControlPlane;
use rustls::pki_types::{CertificateDer, ServerName, pem::PemObject};
use serde_json::json;
use spinikus_agent::{AgentFiles, Backoff, ProxyListeners, RunOptions, runtime::MemoryRuntime};
use spinikus_control::{
    CertificateTimings, NewDomain,
    certificates::acme::{AcmeClient, AcmeError, ChallengeResponder, Issued},
};
use spinikus_core::{deployment::DeploymentStatus, domain::Hostname, secret::Secret, server::ServerStatus};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

/// A CA that validates the challenge over HTTP at the proxy, as if DNS pointed there, and
/// issues self-signed certificates.
#[derive(Default)]
struct FakeCa {
    /// The proxy's HTTP address.
    proxy: Mutex<Option<SocketAddr>>,
    /// What the proxy answered for each challenge.
    validated: Mutex<Vec<(u16, String)>>,
    /// The certificates issued, to trust in the test.
    issued: Mutex<Vec<CertificateDer<'static>>>,
}

#[async_trait]
impl AcmeClient for FakeCa {
    async fn create_account(&self, _directory_url: &str, _email: &str) -> Result<Secret, AcmeError> {
        Ok(Secret::new("account"))
    }

    async fn issue(
        &self,
        _credentials: &Secret,
        hostname: &Hostname,
        responder: &dyn ChallengeResponder,
    ) -> Result<Issued, AcmeError> {
        let token = "fake-token";
        responder.publish(token, "fake-token.thumbprint").await;
        let proxy = self.proxy.lock().unwrap().expect("the proxy is set");
        let answer = get(proxy, hostname.as_str(), &format!("/.well-known/acme-challenge/{token}")).await;
        responder.withdraw(token).await;
        let valid = answer == (200, "fake-token.thumbprint".to_string());
        self.validated.lock().unwrap().push(answer);
        if !valid {
            return Err(AcmeError("the challenge was not served".to_string()));
        }
        let certified = rcgen::generate_simple_self_signed(vec![hostname.to_string()]).unwrap();
        self.issued.lock().unwrap().push(certified.cert.der().clone());
        Ok(Issued { chain_pem: certified.cert.pem(), private_key_pem: Secret::new(certified.signing_key.serialize_pem()) })
    }
}

/// A raw HTTP/1.1 GET, returning the status and the body, or the `Location` for a redirect.
async fn get(address: SocketAddr, host: &str, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(address).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    parse(&response)
}

fn parse(response: &str) -> (u16, String) {
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    let status = head[9..12].parse().unwrap();
    let location = head.lines().find_map(|line| line.strip_prefix("location: "));
    (status, location.unwrap_or(body).to_string())
}

/// A GET over TLS with SNI `host`, trusting only `root`.
async fn get_tls(address: SocketAddr, host: &str, root: &CertificateDer<'static>) -> std::io::Result<(u16, String)> {
    let request = format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    send_tls(address, host, root, &request).await
}

/// A raw HTTP/1.1 request over TLS with SNI `host`, trusting only `root`.
async fn send_tls(
    address: SocketAddr,
    host: &str,
    root: &CertificateDer<'static>,
    request: &str,
) -> std::io::Result<(u16, String)> {
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
    stream.write_all(request.as_bytes()).await?;
    let mut response = String::new();
    // Servers may close without close_notify; what was read still counts.
    let _ = stream.read_to_string(&mut response).await;
    Ok(parse(&response))
}

/// An enrolled agent with a proxy on free ports, which the fake CA validates at.
struct Agent {
    _dir: tempfile::TempDir,
    files: AgentFiles,
    http: SocketAddr,
    https: SocketAddr,
    _stop: oneshot::Sender<()>,
}

impl Agent {
    async fn start(
        control_plane: &ControlPlane,
        ca: &FakeCa,
        token: &Secret,
        runtime: Arc<MemoryRuntime>,
        dashboard: Option<SocketAddr>,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let files = AgentFiles::new(dir.path().join("agent"));
        let credentials =
            spinikus_agent::join(&control_plane.url, token, &control_plane.fingerprint, spinikus_agent::server_facts())
                .await
                .unwrap();
        files.save(&credentials).unwrap();
        let (http, https) = (TcpListener::bind("127.0.0.1:0").await.unwrap(), TcpListener::bind("127.0.0.1:0").await.unwrap());
        let (http_address, https_address) = (http.local_addr().unwrap(), https.local_addr().unwrap());
        *ca.proxy.lock().unwrap() = Some(http_address);
        let options = RunOptions {
            facts: spinikus_agent::server_facts(),
            backoff: Backoff::new(Duration::from_millis(50), Duration::from_millis(200)),
            runtime,
            proxy: Some(ProxyListeners { http, https: Some(https), dashboard }),
            local_repositories: false,
            backups: Default::default(),
        };
        let (stop, stopped) = oneshot::channel::<()>();
        let agent_files = files.clone();
        tokio::spawn(async move {
            spinikus_agent::run(&agent_files, options, async move {
                let _ = stopped.await;
            })
            .await
        });
        Self { _dir: dir, files, http: http_address, https: https_address, _stop: stop }
    }
}

/// Waits for the CA's first certificate and an answer from `host` over HTTPS, returning both.
async fn served_over_https(ca: &FakeCa, https: SocketAddr, host: &str) -> (CertificateDer<'static>, (u16, String)) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let root = ca.issued.lock().unwrap().first().cloned();
        if let Some(root) = root
            && let Ok(response) = get_tls(https, host, &root).await
        {
            return (root, response);
        }
        assert!(tokio::time::Instant::now() < deadline, "no HTTPS in time; the CA saw {:?}", ca.validated.lock().unwrap());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_domain_gets_a_certificate_and_is_served_over_https() {
    let ca = Arc::new(FakeCa::default());
    let timings = CertificateTimings { tick: Duration::from_millis(100), ..Default::default() };
    let control_plane = ControlPlane::start_with(|control| control.with_acme_client(ca.clone(), timings)).await;
    let control = &control_plane.control;
    let (server, token) = control_plane.join_token().await;
    let runtime = Arc::new(MemoryRuntime::new());
    runtime.add_image("ghcr.io/acme/web:1", "ghcr.io/acme/web@sha256:01");
    let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
    runtime.set_endpoint(app.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(app, axum::Router::new().route("/", axum::routing::get(|| async { "hello" }))).await });
    let agent = Agent::start(&control_plane, &ca, &token, runtime, None).await;
    let (files, http_address, https_address) = (&agent.files, agent.http, agent.https);
    control_plane.wait_for_status(server, ServerStatus::Connected).await;

    let service = control_plane
        .service(server, "ghcr.io/acme/web:1", json!({ "port": 3000, "healthcheck": { "start_period": "0s" } }))
        .await;
    let domain = NewDomain { hostname: "shop.example.com".to_string(), redirect_to: None };
    control.create_domain(&control_plane.admin, service, domain).await.unwrap();
    let deployment = control.create_deployment(&control_plane.admin, service).await.unwrap();
    control_plane.wait_for_deployment(deployment.id, DeploymentStatus::Live, Duration::from_secs(10)).await;
    let before = get(http_address, "shop.example.com", "/").await;
    control.set_setting(&control_plane.admin, "acme_email", 0, json!("ops@example.com")).await.unwrap();

    let (_, served) = served_over_https(&ca, https_address, "shop.example.com").await;
    let redirected = get(http_address, "shop.example.com", "/cart?id=1").await;
    let localhost = get(http_address, "web-production-shop.localhost", "/").await;

    assert_eq!(before, (200, "hello".to_string()));
    assert_eq!(*ca.validated.lock().unwrap(), [(200, "fake-token.thumbprint".to_string())]);
    assert_eq!(served, (200, "hello".to_string()));
    let port = https_address.port();
    assert_eq!(redirected, (308, format!("https://shop.example.com:{port}/cart?id=1")));
    // No certificate for a name that cannot be public, so plain HTTP keeps working.
    assert_eq!(localhost, (200, "hello".to_string()));
    let (revision, saved) = files.load_certificates().unwrap();
    assert!(revision > 0);
    assert_eq!(saved.iter().map(|bundle| bundle.hostnames[0].as_str()).collect::<Vec<_>>(), ["shop.example.com"]);
    let listed = control.list_domains(&control_plane.admin, service).await.unwrap();
    assert!(listed.certificates[0].certificate_id.is_some(), "{listed:?}");
    let _ = CertificateDer::from_pem_slice(saved[0].chain_pem.as_bytes()).unwrap();
}

#[tokio::test]
async fn the_embedded_server_serves_the_dashboard_on_its_domain_over_https() {
    let ca = Arc::new(FakeCa::default());
    let timings = CertificateTimings { tick: Duration::from_millis(100), ..Default::default() };
    let control_plane = ControlPlane::start_with(|control| control.with_acme_client(ca.clone(), timings)).await;
    let control = &control_plane.control;
    let dashboard = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dashboard_address = dashboard.local_addr().unwrap();
    let router = spinikus_api::router(control.clone()).merge(spinikus_web::router(control.clone()));
    tokio::spawn(async move { axum::serve(dashboard, router.into_make_service_with_connect_info::<SocketAddr>()).await });
    let embedded = control.embedded_server("local").await.unwrap();
    let join = control.create_embedded_join_token(embedded.id).await.unwrap();
    let agent = Agent::start(&control_plane, &ca, &join.secret, Arc::new(MemoryRuntime::new()), Some(dashboard_address)).await;
    control_plane.wait_for_status(embedded.id, ServerStatus::Connected).await;

    let host = "spinikus.example.com";
    let before = get(agent.http, host, "/login").await;
    let admin = &control_plane.admin;
    control.set_setting(admin, "dashboard_domain", 0, json!(host)).await.unwrap();
    control.set_setting(admin, "acme_email", 0, json!("ops@example.com")).await.unwrap();
    let (root, _) = served_over_https(&ca, agent.https, host).await;
    let page = |path: &str| format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    let login = send_tls(agent.https, host, &root, &page("/login")).await.unwrap();
    let api = send_tls(agent.https, host, &root, &page("/api/v1/me")).await.unwrap();
    let sign_in = |origin: &str| {
        let body = "email=nobody%40example.com&password=wrong";
        format!(
            "POST /login HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    let same_site = send_tls(agent.https, host, &root, &sign_in("https://spinikus.example.com")).await.unwrap();
    let cross_site = send_tls(agent.https, host, &root, &sign_in("https://evil.example")).await.unwrap();
    let redirected = get(agent.http, host, "/login").await;

    assert_eq!(before.0, 404, "no dashboard domain yet: {before:?}");
    assert_eq!(login.0, 200);
    assert!(login.1.contains("Sign in"), "{}", login.1);
    assert_eq!(api.0, 401, "the API is served too: {api:?}");
    // The dashboard sees the browser's host, so a form from its own page passes the origin check.
    assert_eq!(same_site.0, 401, "{same_site:?}");
    assert_eq!(cross_site.0, 403, "{cross_site:?}");
    assert_eq!(redirected, (308, format!("https://{host}:{}/login", agent.https.port())));
    let (_, saved) = agent.files.load_certificates().unwrap();
    assert_eq!(saved.iter().map(|bundle| bundle.hostnames[0].as_str()).collect::<Vec<_>>(), [host]);
}
