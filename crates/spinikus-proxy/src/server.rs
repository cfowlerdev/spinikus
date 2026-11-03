//! Serving requests: look up the host, then redirect, or forward to the app and stream its
//! response back. Upgraded connections (WebSockets) are spliced together once both sides
//! have switched.
//!
//! Over plain HTTP, ACME HTTP-01 challenges are answered first, for any host, and a host
//! with a certificate is redirected to HTTPS.

use std::{
    collections::HashMap,
    convert::Infallible,
    future::Future,
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::Duration,
};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::{
    Request, Response, StatusCode,
    body::Incoming,
    header::{self, HeaderValue},
    service::service_fn,
};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioIo},
    server::{conn::auto, graceful::GracefulShutdown},
};
use spinikus_core::{
    certificate::CertificateBundle,
    route::{Route, RouteAction},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpListener,
};
use tokio_rustls::TlsAcceptor;

use crate::{
    Certificates, Routes, Upstreams,
    certificates::Resolver,
    forward::{Scheme, is_upgrade, strip_hop_by_hop, to_upstream},
    routes::host_name,
};

/// How long connecting to an app may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a client gets to finish the TLS handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long open requests get to finish when the proxy stops.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// How long to wait before accepting again after accepting failed, such as when out of
/// file descriptors.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

const CHALLENGE_PREFIX: &str = "/.well-known/acme-challenge/";

type Body = BoxBody<Bytes, hyper::Error>;

/// The proxy's state: the current routes, certificates and ACME challenges, and a client
/// for the apps. Share it with `Arc`.
pub struct Proxy {
    routes: RwLock<Arc<Routes>>,
    certificates: Arc<RwLock<Arc<Certificates>>>,
    /// Key authorizations by token.
    challenges: RwLock<Arc<HashMap<String, String>>>,
    /// The port HTTPS is served on, for redirects; `None` without HTTPS.
    https_port: Option<u16>,
    upstreams: Arc<dyn Upstreams>,
    client: Client<HttpConnector, Incoming>,
}

impl Proxy {
    /// A proxy that redirects to HTTPS on `https_port` for hosts with a certificate, or
    /// never redirects if `None`.
    pub fn new(upstreams: Arc<dyn Upstreams>, https_port: Option<u16>) -> Arc<Self> {
        let mut connector = HttpConnector::new();
        connector.set_connect_timeout(Some(CONNECT_TIMEOUT));
        connector.set_nodelay(true);
        let client = Client::builder(TokioExecutor::new()).build(connector);
        Arc::new(Self {
            routes: RwLock::default(),
            certificates: Arc::default(),
            challenges: RwLock::default(),
            https_port,
            upstreams,
            client,
        })
    }

    /// Replaces the route table. Requests already forwarded finish where they are.
    pub fn set_routes(&self, routes: &[Route]) {
        *write(&self.routes) = Arc::new(Routes::new(routes));
    }

    /// Replaces the certificates, returning why any could not be loaded. New handshakes
    /// use the new set.
    pub fn set_certificates(&self, bundles: &[CertificateBundle]) -> Vec<String> {
        let (certificates, problems) = Certificates::new(bundles);
        *write(&self.certificates) = Arc::new(certificates);
        problems
    }

    /// Replaces the ACME HTTP-01 challenges: key authorizations by token.
    pub fn set_challenges(&self, challenges: HashMap<String, String>) {
        *write(&self.challenges) = Arc::new(challenges);
    }

    /// Answers one request from `peer`, which came in over `scheme`.
    pub async fn handle(&self, mut request: Request<Incoming>, peer: SocketAddr, scheme: Scheme) -> Response<Body> {
        if scheme == Scheme::Http
            && let Some(token) = request.uri().path().strip_prefix(CHALLENGE_PREFIX)
        {
            let challenges = read(&self.challenges);
            return match challenges.get(token) {
                // Exactly the key authorization: CAs need not ignore a trailing newline.
                Some(key_authorization) => plain(StatusCode::OK, key_authorization.clone()),
                None => text(StatusCode::NOT_FOUND, "no such challenge"),
            };
        }
        // HTTP/1.1 has `Host`; HTTP/2 has the URI's authority.
        let host = match request.headers().get(header::HOST).cloned() {
            Some(host) => host,
            None => match request.uri().authority().and_then(|authority| HeaderValue::from_str(authority.as_str()).ok()) {
                Some(host) => host,
                None => return text(StatusCode::BAD_REQUEST, "the request names no host"),
            },
        };
        let Ok(host_str) = host.to_str() else {
            return text(StatusCode::BAD_REQUEST, "the host is not valid");
        };
        let name = host_name(host_str);
        if scheme == Scheme::Http
            && let Some(port) = self.https_port
            && read(&self.certificates).get(name).is_some()
        {
            let authority = if port == 443 { name.to_string() } else { format!("{name}:{port}") };
            return redirect(&format!("https://{authority}"), &request);
        }
        let routes = read(&self.routes);
        let target = match routes.lookup(host_str) {
            None => return text(StatusCode::NOT_FOUND, &format!("no app is served at {name}")),
            Some(RouteAction::Redirect { to }) => {
                let port = host_str.strip_prefix(name).unwrap_or("");
                return redirect(&format!("{}://{to}{port}", scheme.as_str()), &request);
            }
            Some(RouteAction::Proxy(target)) => Some(target.clone()),
            Some(RouteAction::Dashboard) => None,
        };
        drop(routes);
        let upstream = match &target {
            Some(target) => match self.upstreams.resolve(target).await {
                Some(upstream) => upstream,
                None => return text(StatusCode::SERVICE_UNAVAILABLE, "the app is not running"),
            },
            None => match self.upstreams.dashboard() {
                Some(upstream) => upstream,
                None => return text(StatusCode::NOT_FOUND, &format!("the dashboard is not served at {name} on this server")),
            },
        };

        let upgrade = is_upgrade(request.headers());
        let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut request));
        to_upstream(&mut request, &host, upstream, peer, scheme, upgrade);
        let mut response = match self.client.request(request).await {
            Ok(response) => response,
            Err(error) => {
                let Some(target) = target else {
                    tracing::warn!(%error, %upstream, "the dashboard did not answer");
                    return text(StatusCode::BAD_GATEWAY, "the dashboard is not reachable");
                };
                if error.is_connect() {
                    self.upstreams.failed(&target, upstream);
                }
                tracing::debug!(%error, deployment = %target.deployment_id, %upstream, "the app did not answer");
                return text(StatusCode::BAD_GATEWAY, "the app is not reachable");
            }
        };

        if response.status() == StatusCode::SWITCHING_PROTOCOLS
            && let Some(client_upgrade) = client_upgrade
        {
            let app_upgrade = hyper::upgrade::on(&mut response);
            tokio::spawn(async move {
                let (client, app) = match tokio::try_join!(client_upgrade, app_upgrade) {
                    Ok(upgraded) => upgraded,
                    Err(error) => return tracing::debug!(%error, "an upgrade failed"),
                };
                let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(client), &mut TokioIo::new(app)).await;
            });
            let (mut parts, _) = response.into_parts();
            strip_hop_by_hop(&mut parts.headers, true);
            return Response::from_parts(parts, empty());
        }
        let (mut parts, body) = response.into_parts();
        strip_hop_by_hop(&mut parts.headers, false);
        Response::from_parts(parts, body.boxed())
    }

    /// The TLS configuration: certificates by SNI, HTTP/2 and HTTP/1.1 by ALPN.
    fn tls_acceptor(&self) -> TlsAcceptor {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("the ring provider supports the default protocol versions")
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(Resolver(self.certificates.clone())));
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        TlsAcceptor::from(Arc::new(config))
    }
}

fn read<T>(lock: &RwLock<Arc<T>>) -> Arc<T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
}

fn write<T>(lock: &RwLock<Arc<T>>) -> std::sync::RwLockWriteGuard<'_, Arc<T>> {
    lock.write().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Serves plain HTTP/1 and HTTP/2 (prior knowledge) on `listener` until `shutdown`
/// resolves, then gives open requests a few seconds to finish.
pub async fn serve(proxy: Arc<Proxy>, listener: TcpListener, shutdown: impl Future<Output = ()>) {
    accept(proxy, listener, None, shutdown).await
}

/// Serves HTTPS on `listener` until `shutdown` resolves, like [`serve`].
pub async fn serve_tls(proxy: Arc<Proxy>, listener: TcpListener, shutdown: impl Future<Output = ()>) {
    let acceptor = proxy.tls_acceptor();
    accept(proxy, listener, Some(acceptor), shutdown).await
}

async fn accept(proxy: Arc<Proxy>, listener: TcpListener, tls: Option<TlsAcceptor>, shutdown: impl Future<Output = ()>) {
    let mut shutdown = std::pin::pin!(shutdown);
    let graceful = GracefulShutdown::new();
    let builder = auto::Builder::new(TokioExecutor::new());
    loop {
        let (stream, peer) = tokio::select! {
            () = shutdown.as_mut() => break,
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(error) => {
                    tracing::warn!(%error, "the proxy failed to accept a connection");
                    tokio::time::sleep(ACCEPT_RETRY).await;
                    continue;
                }
            },
        };
        let _ = stream.set_nodelay(true);
        let (proxy, builder, watcher) = (proxy.clone(), builder.clone(), graceful.watcher());
        let tls = tls.clone();
        tokio::spawn(async move {
            let result = match tls {
                None => watcher.watch(connection(builder, stream, proxy, peer, Scheme::Http)).await,
                Some(acceptor) => {
                    let stream = match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                        Ok(Ok(stream)) => stream,
                        Ok(Err(error)) => return tracing::debug!(%error, %peer, "a TLS handshake failed"),
                        Err(_) => return tracing::debug!(%peer, "a TLS handshake took too long"),
                    };
                    watcher.watch(connection(builder, stream, proxy, peer, Scheme::Https)).await
                }
            };
            if let Err(error) = result {
                tracing::debug!(%error, %peer, "a proxy connection failed");
            }
        });
    }
    if tokio::time::timeout(SHUTDOWN_GRACE, graceful.shutdown()).await.is_err() {
        tracing::debug!("closed proxy connections that were still open");
    }
}

/// One client connection, which may serve several requests.
fn connection<S>(
    builder: auto::Builder<TokioExecutor>,
    stream: S,
    proxy: Arc<Proxy>,
    peer: SocketAddr,
    scheme: Scheme,
) -> impl hyper_util::server::graceful::GracefulConnection<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = service_fn(move |request| {
        let proxy = proxy.clone();
        async move { Ok::<_, Infallible>(proxy.handle(request, peer, scheme).await) }
    });
    builder.serve_connection_with_upgrades(TokioIo::new(stream), service).into_owned()
}

fn empty() -> Body {
    Full::new(Bytes::new()).map_err(|never| match never {}).boxed()
}

fn text(status: StatusCode, message: &str) -> Response<Body> {
    plain(status, format!("{message}\n"))
}

fn plain(status: StatusCode, content: String) -> Response<Body> {
    let mut response = Response::new(Full::new(Bytes::from(content)).map_err(|never| match never {}).boxed());
    *response.status_mut() = status;
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    response
}

/// A permanent redirect to `base` (scheme and authority), keeping the path and query.
fn redirect<B>(base: &str, request: &Request<B>) -> Response<Body> {
    let path = request.uri().path_and_query().map_or("/", |path| path.as_str());
    let location = format!("{base}{path}");
    let mut response = text(StatusCode::PERMANENT_REDIRECT, &format!("moved to {location}"));
    if let Ok(location) = HeaderValue::from_str(&location) {
        response.headers_mut().insert(header::LOCATION, location);
    }
    response
}
