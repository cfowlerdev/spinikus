//! The agent: what runs on each server and dials out to the control plane.
//!
//! It enrolls ([`join`]) and holds the control stream open ([`run`]): hello, heartbeats,
//! reconnects with backoff, and certificate renewal. Over that stream it reports the
//! containers Spinikus manages on this server, through the container [`runtime`], and
//! runs commands: pulling an image, starting a deployment and verifying it with its health
//! check, and stopping and restarting containers. It applies the route tables it is sent
//! to its proxy ([`spinikus_proxy`]), which serves the apps on this server. The builder,
//! release commands and TLS come next.

mod backoff;
mod build;
mod certificate;
mod commands;
mod connection;
mod deploy;
mod enroll;
mod error;
mod facts;
mod files;
mod health;
mod proxy;
mod report;
pub mod runtime;

pub use backoff::Backoff;
pub use build::BuildOptions;
pub use connection::{ProxyListeners, RunOptions, run};
pub use enroll::join;
pub use error::AgentError;
pub use facts::server_facts;
pub use files::{AgentFiles, Credentials};

/// This agent's version, sent in `Hello`.
pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");
