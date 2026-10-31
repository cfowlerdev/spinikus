//! The agent: what runs on each server and dials out to the control plane.
//!
//! So far it enrolls ([`join`]) and holds the control stream open ([`run`]): hello,
//! heartbeats, reconnects with backoff, and certificate renewal. The container runtime,
//! builder and proxy come next, behind the same connection.

mod backoff;
mod certificate;
mod connection;
mod enroll;
mod error;
mod facts;
mod files;

pub use backoff::Backoff;
pub use connection::{RunOptions, run};
pub use enroll::join;
pub use error::AgentError;
pub use facts::server_facts;
pub use files::{AgentFiles, Credentials};

/// This agent's version, sent in `Hello`.
pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");
