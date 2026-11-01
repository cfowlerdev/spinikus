//! The control plane. This crate holds the service layer that the API, the web UI, the CLI
//! and webhooks all go through, so they share one set of rules.

mod access;
pub mod agents;
mod audit;
pub mod auth;
pub mod backups;
pub mod ca;
pub mod certificates;
pub mod databases;
pub mod deployments;
pub mod domains;
pub mod environments;
pub mod error;
mod input;
pub mod page;
pub mod password;
pub mod projects;
pub mod servers;
pub mod service_logs;
pub mod services;
pub mod settings;
pub mod setup;
pub mod sources;
#[cfg(test)]
mod test_support;
pub mod tokens;
pub mod variables;
pub mod volumes;
pub mod webhooks;

use std::sync::Arc;

use agents::AgentHub;
pub use auth::{ClientInfo, Credential, Principal};
pub use backups::{BackupScheduleChanges, BackupTimings, NewBackupSchedule, NewStorageDestination, StorageDestinationChanges};
pub use certificates::{
    CertificateTimings,
    acme::{AcmeClient, InstantAcme},
};
pub use databases::{CreatedDatabase, NewDatabase};
pub use deployments::DeploymentTimings;
pub use domains::{DomainChanges, NewDomain, ServiceDomains};
pub use environments::{EnvironmentChanges, NewEnvironment};
pub use error::ControlError;
pub use page::Page;
pub use projects::{NewProject, ProjectChanges};
pub use servers::{Enrolled, JoinToken, NewServer};
pub use services::{NewService, ServiceChanges};
use spinikus_store::SqliteStore;
pub use tokens::NewToken;
use tokio::sync::OnceCell;
pub use variables::{NewVariable, VariableChanges};
pub use volumes::{NewVolume, VolumeChanges};

/// The service layer. Cheap to clone; clones share the database pool and the connected agents.
#[derive(Clone)]
pub struct Control {
    store: SqliteStore,
    agents: AgentHub,
    /// Loaded (or created) on first use.
    authority: Arc<OnceCell<ca::CertificateAuthority>>,
    runner: Arc<deployments::Runner>,
    certificates: Arc<certificates::Certificates>,
    logs: Arc<deployments::logs::DeploymentLogs>,
    sessions: Arc<service_logs::Sessions>,
    backups: Arc<backups::Backups>,
}

impl Control {
    pub fn new(store: SqliteStore) -> Self {
        let runner = Arc::new(deployments::Runner::new(DeploymentTimings::default()));
        let certificates = Arc::new(certificates::Certificates::new(Arc::new(InstantAcme::new()), CertificateTimings::default()));
        let logs = Arc::new(deployments::logs::DeploymentLogs::new(None));
        let sessions = Arc::default();
        let backups = Arc::new(backups::Backups::new(BackupTimings::default(), Default::default()));
        Self { store, agents: AgentHub::default(), authority: Arc::default(), runner, certificates, logs, sessions, backups }
    }

    /// Replaces the ACME client and the certificate manager's timings, such as to use a
    /// test CA. Call it before the certificate manager starts.
    pub fn with_acme_client(mut self, acme: Arc<dyn AcmeClient>, timings: CertificateTimings) -> Self {
        self.certificates = Arc::new(certificates::Certificates::new(acme, timings));
        self
    }

    /// Replaces the deployment runner's timings, such as to shorten them in tests. Call it
    /// before the runner starts.
    pub fn with_deployment_timings(mut self, timings: DeploymentTimings) -> Self {
        self.runner = Arc::new(deployments::Runner::new(timings));
        self
    }

    /// Replaces the backup runner's timings, such as to shorten them in tests. Call it before
    /// the runner starts.
    pub fn with_backup_timings(mut self, timings: BackupTimings) -> Self {
        self.backups = Arc::new(backups::Backups::new(timings, self.backups.stores.clone()));
        self
    }

    /// Reaches every storage destination at `store` instead of its bucket, for tests. Call it
    /// before the backup runner starts.
    pub fn with_backup_store(mut self, store: Arc<dyn object_store::ObjectStore>) -> Self {
        let timings = self.backups.timings();
        self.backups = Arc::new(backups::Backups::new(timings, backups::storage::BackupStores::Fixed(store)));
        self
    }

    pub(crate) fn runner(&self) -> &deployments::Runner {
        &self.runner
    }

    pub fn store(&self) -> &SqliteStore {
        &self.store
    }

    /// The agents connected to this control plane.
    pub fn agents(&self) -> &AgentHub {
        &self.agents
    }
}
