//! The agent: what runs on each server and dials out to the control plane.
//!
//! It enrolls ([`join`]) and holds the control stream open ([`run`]): hello, heartbeats,
//! reconnects with backoff, and certificate renewal. Over that stream it reports the
//! containers Spinikus manages on this server, through the container [`runtime`], and
//! runs the commands that stop and restart them. The builder and proxy come next.

mod backoff;
mod certificate;
mod commands;
mod connection;
mod enroll;
mod error;
mod facts;
mod files;
mod report;
pub mod runtime;

pub use backoff::Backoff;
pub use connection::{RunOptions, run};
pub use enroll::join;
pub use error::AgentError;
pub use facts::server_facts;
pub use files::{AgentFiles, Credentials};

/// This agent's version, sent in `Hello`.
pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");
