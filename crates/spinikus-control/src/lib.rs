//! The control plane. This crate holds the service layer that the API, the web UI, the CLI
//! and webhooks all go through, so they share one set of rules.

mod access;
mod audit;
pub mod auth;
pub mod environments;
pub mod error;
mod input;
pub mod page;
pub mod password;
pub mod projects;
pub mod services;
pub mod setup;
#[cfg(test)]
mod test_support;

pub use auth::{ClientInfo, Credential, Principal};
pub use environments::{EnvironmentChanges, NewEnvironment};
pub use error::ControlError;
pub use page::Page;
pub use projects::{NewProject, ProjectChanges};
pub use services::{NewService, ServiceChanges};
use spinikus_store::SqliteStore;

/// The service layer. Cheap to clone; clones share the database pool.
#[derive(Clone)]
pub struct Control {
    store: SqliteStore,
}

impl Control {
    pub fn new(store: SqliteStore) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &SqliteStore {
        &self.store
    }
}
