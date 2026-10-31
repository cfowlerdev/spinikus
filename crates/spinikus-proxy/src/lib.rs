//! The reverse proxy that runs on every agent: it serves the apps on its server by
//! hostname, from the route table the control plane sends, and redirects hostnames that
//! redirect. Plain HTTP only so far; TLS comes with certificates.
//!
//! The proxy knows routes, not containers: [`Upstreams`] tells it where a deployment's
//! container listens, which the agent answers from the container runtime.

mod forward;
mod routes;
mod server;

use std::net::SocketAddr;

use async_trait::async_trait;
pub use routes::Routes;
pub use server::{Proxy, serve};
use spinikus_core::route::ProxyTarget;

/// Where the proxy finds the apps it routes to.
#[async_trait]
pub trait Upstreams: Send + Sync + 'static {
    /// The address the target's container listens on, or `None` if it is not running here.
    async fn resolve(&self, target: &ProxyTarget) -> Option<SocketAddr>;

    /// Connecting to `address` for the target failed, so it may have moved, such as after
    /// the container restarted.
    fn failed(&self, target: &ProxyTarget, address: SocketAddr);
}
