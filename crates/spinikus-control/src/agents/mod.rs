//! The control plane's side of the agent protocol (`spinikus.agent.v1`).
//!
//! Agents dial out and hold a `Connect` stream open; the control plane never calls an
//! agent. The first message on the stream must be `Hello`, answered with `Welcome`. After
//! that the agent sends heartbeats and state reports, and the control plane queues
//! messages for it through the [`AgentHub`].
//!
//! The server an agent speaks for comes from an [`AgentIdentity`] in the request
//! extensions, which the mTLS layer sets from the client certificate. Without one, a
//! connection is refused.

mod convert;
mod hub;
mod service;
#[cfg(test)]
mod tests;

use std::time::Duration;

pub use hub::{AgentHub, NotConnected};
pub use service::AgentGrpc;
use spinikus_core::id::ServerId;

/// The oldest protocol version agents may speak. Agents send theirs in `Hello`.
pub const MIN_PROTOCOL_VERSION: u32 = 1;

/// The server a connection is for, proven by its client certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentIdentity(pub ServerId);

/// Timing of the control stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentTimings {
    /// How often agents send `Heartbeat`, told to them in `Welcome`.
    pub heartbeat_interval: Duration,
    /// How long the control plane waits for `Hello` after the stream opens.
    pub hello_timeout: Duration,
    /// How long a connection may go without any message before it is closed.
    pub idle_timeout: Duration,
}

impl Default for AgentTimings {
    fn default() -> Self {
        Self {
            heartbeat_interval: Duration::from_secs(10),
            hello_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(30),
        }
    }
}
