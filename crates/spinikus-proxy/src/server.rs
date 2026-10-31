//! Serving requests: look up the host, then redirect, or forward to the app and stream its
//! response back. Upgraded connections (WebSockets) are spliced together once both sides
//! have switched.

use std::{
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
use spinikus_core::route::{Route, RouteAction};
use tokio::net::TcpListener;

use crate::{
    Routes, Upstreams,
    forward::{is_upgrade, strip_hop_by_hop, to_upstream},
    routes::host_name,
};

/// How long connecting to an app may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long open requests get to finish when the proxy stops.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// How long to wait before accepting again after accepting failed, such as when out of
/// file descriptors.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

type Body = BoxBody<Bytes, hyper::Error>;

/// The proxy's state: the current routes and a client for the apps. Share it with `Arc`.
pub struct Proxy {
    routes: RwLock<Arc<Routes>>,
    upstreams: Arc<dyn Upstreams>,
    client: Client<HttpConnector, Incoming>,
}

impl Proxy {
    pub fn new(upstreams: Arc<dyn Upstreams>) -> Arc<Self> {
        let mut connector = HttpConnector::new();
        connector.set_connect_timeout(Some(CONNECT_TIMEOUT));
        connector.set_nodelay(true);
        let client = Client::builder(TokioExecutor::new()).build(connector);
        Arc::new(Self { routes: RwLock::default(), upstreams, client })
    }

    /// Replaces the route table. Requests already forwarded finish where they are.
    pub fn set_routes(&self, routes: &[Route]) {
        let routes = Arc::new(Routes::new(routes));
        *self.routes.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = routes;
    }

    fn routes(&self) -> Arc<Routes> {
        self.routes.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    /// Answers one request from `peer`.
    pub async fn handle(&self, mut request: Request<Incoming>, peer: SocketAddr) -> Response<Body> {
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
        let routes = self.routes();
        let target = match routes.lookup(host_str) {
            None => return text(StatusCode::NOT_FOUND, &format!("no app is served at {}", host_name(host_str))),
            Some(RouteAction::Redirect { to }) => return redirect(host_str, to.as_str(), &request),
            Some(RouteAction::Proxy(target)) => target.clone(),
        };
        let Some(upstream) = self.upstreams.resolve(&target).await else {
            return text(StatusCode::SERVICE_UNAVAILABLE, "the app is not running");
        };

        let upgrade = is_upgrade(request.headers());
        let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut request));
        to_upstream(&mut request, &host, upstream, peer, upgrade);
        let mut response = match self.client.request(request).await {
            Ok(response) => response,
            Err(error) => {
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
}

/// Serves HTTP/1 and HTTP/2 (prior knowledge) on `listener` until `shutdown` resolves,
/// then gives open requests a few seconds to finish.
pub async fn serve(proxy: Arc<Proxy>, listener: TcpListener, shutdown: impl Future<Output = ()>) {
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
        let proxy = proxy.clone();
        let service = service_fn(move |request| {
            let proxy = proxy.clone();
            async move { Ok::<_, Infallible>(proxy.handle(request, peer).await) }
        });
        let connection = builder.serve_connection_with_upgrades(TokioIo::new(stream), service).into_owned();
        let connection = graceful.watch(connection);
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::debug!(%error, %peer, "a proxy connection failed");
            }
        });
    }
    if tokio::time::timeout(SHUTDOWN_GRACE, graceful.shutdown()).await.is_err() {
        tracing::debug!("closed proxy connections that were still open");
    }
}

fn empty() -> Body {
    Full::new(Bytes::new()).map_err(|never| match never {}).boxed()
}

fn text(status: StatusCode, message: &str) -> Response<Body> {
    let mut response = Response::new(Full::new(Bytes::from(format!("{message}\n"))).map_err(|never| match never {}).boxed());
    *response.status_mut() = status;
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    response
}

/// A permanent redirect to `to`, keeping the port, path and query.
fn redirect<B>(host: &str, to: &str, request: &Request<B>) -> Response<Body> {
    let port = host.strip_prefix(host_name(host)).unwrap_or("");
    let path = request.uri().path_and_query().map_or("/", |path| path.as_str());
    let location = format!("http://{to}{port}{path}");
    let mut response = text(StatusCode::PERMANENT_REDIRECT, &format!("moved to {location}"));
    if let Ok(location) = HeaderValue::from_str(&location) {
        response.headers_mut().insert(header::LOCATION, location);
    }
    response
}
