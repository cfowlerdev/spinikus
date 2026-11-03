//! The reverse proxy that runs on every agent: it serves the apps on its server by
//! hostname, from the route table the control plane sends, and redirects hostnames that
//! redirect. It serves HTTPS with the certificates it is given, chosen by SNI, answers
//! ACME HTTP-01 challenges over HTTP, and redirects HTTP to HTTPS for hosts with a
//! certificate.
//!
//! The control plane's own server also serves the dashboard on its domain, if one is set.
//!
//! The proxy knows routes, not containers: [`Upstreams`] tells it where a deployment's
//! container listens, which the agent answers from the container runtime.

mod certificates;
mod forward;
mod routes;
mod server;

use std::net::SocketAddr;

use async_trait::async_trait;
pub use certificates::Certificates;
pub use forward::Scheme;
pub use routes::Routes;
pub use server::{Proxy, serve, serve_tls};
use spinikus_core::route::ProxyTarget;

/// Where the proxy finds the apps it routes to.
#[async_trait]
pub trait Upstreams: Send + Sync + 'static {
    /// The address the target's container listens on, or `None` if it is not running here.
    async fn resolve(&self, target: &ProxyTarget) -> Option<SocketAddr>;

    /// Connecting to `address` for the target failed, so it may have moved, such as after
    /// the container restarted.
    fn failed(&self, target: &ProxyTarget, address: SocketAddr);

    /// Where the control plane's dashboard listens, for dashboard routes: only on the
    /// control plane's own server.
    fn dashboard(&self) -> Option<SocketAddr> {
        None
    }
}
