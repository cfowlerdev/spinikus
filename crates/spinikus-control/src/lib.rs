//! The control plane. This crate holds the service layer that the API, the web UI, the CLI
//! and webhooks all go through, so they share one set of rules.

mod access;
pub mod agents;
mod audit;
pub mod auth;
pub mod ca;
pub mod environments;
pub mod error;
mod input;
pub mod page;
pub mod password;
pub mod projects;
pub mod servers;
pub mod services;
pub mod setup;
#[cfg(test)]
mod test_support;

use std::sync::Arc;

use agents::AgentHub;
pub use auth::{ClientInfo, Credential, Principal};
pub use environments::{EnvironmentChanges, NewEnvironment};
pub use error::ControlError;
pub use page::Page;
pub use projects::{NewProject, ProjectChanges};
pub use servers::{Enrolled, JoinToken, NewServer};
pub use services::{NewService, ServiceChanges};
use spinikus_store::SqliteStore;
use tokio::sync::OnceCell;

/// The service layer. Cheap to clone; clones share the database pool and the connected agents.
#[derive(Clone)]
pub struct Control {
    store: SqliteStore,
    agents: AgentHub,
    /// Loaded (or created) on first use.
    authority: Arc<OnceCell<ca::CertificateAuthority>>,
}

impl Control {
    pub fn new(store: SqliteStore) -> Self {
        Self { store, agents: AgentHub::default(), authority: Arc::default() }
    }

    pub fn store(&self) -> &SqliteStore {
        &self.store
    }

    /// The agents connected to this control plane.
    pub fn agents(&self) -> &AgentHub {
        &self.agents
    }
}
