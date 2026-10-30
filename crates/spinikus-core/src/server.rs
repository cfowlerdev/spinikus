//! Servers that run an agent, and the containers (instances) those agents report.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::{
    id::{DeploymentId, InstanceId, ServerId, ServiceId},
    text::impl_text,
};

/// A machine managed by Spinikus. Its name is set by users; everything under
/// `agent` and `last_seen_at` is reported by the agent running on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Server {
    pub id: ServerId,
    pub name: String,
    pub status: ServerStatus,
    /// What the agent said about itself in its last `Hello`.
    pub agent: Option<AgentInfo>,
    pub last_seen_at: Option<Timestamp>,
    pub revision: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub revoked_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerStatus {
    /// No agent has connected yet.
    Pending,
    Connected,
    /// The agent connected before but is not connected now.
    Disconnected,
    /// Blocked: its agent can no longer connect, even with an unexpired certificate.
    Revoked,
}

impl ServerStatus {
    pub const ALL: [ServerStatus; 4] = [Self::Pending, Self::Connected, Self::Disconnected, Self::Revoked];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Connected => "connected",
            Self::Disconnected => "disconnected",
            Self::Revoked => "revoked",
        }
    }

    /// Works out the status from what is stored: whether the server is revoked, and
    /// whether an agent has connected (`None` if never) and is connected now.
    pub fn derive(revoked: bool, connected: Option<bool>) -> Self {
        match (revoked, connected) {
            (true, _) => Self::Revoked,
            (false, None) => Self::Pending,
            (false, Some(true)) => Self::Connected,
            (false, Some(false)) => Self::Disconnected,
        }
    }
}

/// The fields a user sets when creating a server, or replaces when updating one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerFields {
    pub name: String,
}

/// Sent by the agent in `Hello` each time it connects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub agent_version: String,
    pub protocol_version: u32,
    pub facts: ServerFacts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerFacts {
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub cpu_count: u32,
    pub memory_bytes: u64,
}

/// A container as last reported by the agent on its server. Observed state only:
/// what should run comes from deployments, and the reconciler compares the two.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Instance {
    pub id: InstanceId,
    pub server_id: ServerId,
    #[serde(flatten)]
    pub observed: ObservedInstance,
    /// When the agent last reported this container.
    pub reported_at: Timestamp,
}

/// One container in an agent's `StateReport`. The service and deployment come from
/// labels Spinikus puts on the containers it starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedInstance {
    pub service_id: ServiceId,
    pub deployment_id: DeploymentId,
    /// The container runtime's ID, unique on its server.
    pub container_id: String,
    pub status: InstanceStatus,
    pub health: InstanceHealth,
    /// How often the runtime has restarted the container, for spotting crash loops.
    pub restart_count: u32,
    pub started_at: Option<Timestamp>,
}

/// Container states, following Docker's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceStatus {
    Created,
    Running,
    Paused,
    Restarting,
    Removing,
    Exited,
    Dead,
}

impl InstanceStatus {
    pub const ALL: [InstanceStatus; 7] =
        [Self::Created, Self::Running, Self::Paused, Self::Restarting, Self::Removing, Self::Exited, Self::Dead];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Restarting => "restarting",
            Self::Removing => "removing",
            Self::Exited => "exited",
            Self::Dead => "dead",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceHealth {
    /// The container has no health check.
    None,
    Starting,
    Healthy,
    Unhealthy,
}

impl InstanceHealth {
    pub const ALL: [InstanceHealth; 4] = [Self::None, Self::Starting, Self::Healthy, Self::Unhealthy];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Starting => "starting",
            Self::Healthy => "healthy",
            Self::Unhealthy => "unhealthy",
        }
    }
}

impl_text!(ServerStatus, "server status");
impl_text!(InstanceStatus, "instance status");
impl_text!(InstanceHealth, "instance health");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_server_status() {
        assert_eq!(ServerStatus::derive(false, None), ServerStatus::Pending);
        assert_eq!(ServerStatus::derive(false, Some(true)), ServerStatus::Connected);
        assert_eq!(ServerStatus::derive(false, Some(false)), ServerStatus::Disconnected);
        assert_eq!(ServerStatus::derive(true, Some(true)), ServerStatus::Revoked);
    }

    #[test]
    fn text_round_trips() {
        for status in ServerStatus::ALL {
            assert_eq!(status.to_string().parse(), Ok(status));
            assert_eq!(serde_json::to_string(&status).unwrap(), format!("\"{status}\""));
        }
        for status in InstanceStatus::ALL {
            assert_eq!(status.to_string().parse(), Ok(status));
            assert_eq!(serde_json::to_string(&status).unwrap(), format!("\"{status}\""));
        }
        for health in InstanceHealth::ALL {
            assert_eq!(health.to_string().parse(), Ok(health));
            assert_eq!(serde_json::to_string(&health).unwrap(), format!("\"{health}\""));
        }
    }
}
