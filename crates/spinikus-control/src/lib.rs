//! The control plane. This crate holds the service layer that the API, the web UI, the CLI
//! and webhooks all go through, so they share one set of rules.

pub mod auth;
pub mod error;
pub mod password;
pub mod setup;
#[cfg(test)]
mod test_support;

pub use auth::{ClientInfo, Credential, Principal};
pub use error::ControlError;
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
