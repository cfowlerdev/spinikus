//! Restoring a backup into its database, replacing the data there. Admins of the database's
//! environment restore; viewers see restores. A restore first queues a safety backup of the
//! current data to the backup's destination, and the runner sends the restore only once that
//! succeeded, so a mistaken restore can be undone by restoring the safety backup. One restore
//! runs at a time per database.

use serde_json::json;
use spinikus_core::{
    backup::{BackupStatus, NewRestore, Restore},
    id::{BackupId, RestoreId, ServiceId},
    membership::Role,
};
use spinikus_store::{
    BackupRepository, DeploymentRepository, RestoreRepository, ServiceRepository, StorageDestinationRepository, StoreError,
};

use super::database_engine;
use crate::{
    Control, ControlError, Principal,
    access::not_found,
    page::{self, Page},
};

impl Control {
    /// Queues a restore of the backup into its database, after a safety backup of what is
    /// there now. The database must be live, and have no restore in progress.
    pub async fn restore_backup(&self, principal: &Principal, backup_id: BackupId) -> Result<Restore, ControlError> {
        let (backup, environment) = self.backup_with_access(principal, backup_id, Role::Admin).await?;
        let rejected = |problem: String| ControlError::Rejected { entity: "backup", id: backup_id.to_string(), problem };
        if backup.status != BackupStatus::Succeeded {
            return Err(rejected("only a backup that succeeded can be restored".to_string()));
        }
        let service = self.store().get_service(backup.service_id).await?;
        database_engine(&service)?;
        if self.store().live_deployment(service.id).await?.is_none() {
            return Err(rejected("the database has nothing live to restore into".to_string()));
        }
        let restoring = self.store().list_active_restores().await?;
        if restoring.iter().any(|restore| restore.service_id == service.id) {
            return Err(rejected("a restore of this database is in progress".to_string()));
        }
        // The safety backup goes where the backup is.
        match self.store().get_storage_destination(backup.destination_id).await {
            Ok(_) => {}
            Err(StoreError::NotFound { .. }) => {
                return Err(rejected("its storage destination was deleted, so there is nowhere for a safety backup".to_string()));
            }
            Err(error) => return Err(error.into()),
        }
        let safety = self.queue_backup(&service, backup.destination_id, None).await?;
        let new = NewRestore { service_id: service.id, backup_id, safety_backup_id: Some(safety.id) };
        let restore = match self.store().create_restore(new).await {
            Ok(restore) => restore,
            Err(error) => {
                // Another restore started meanwhile: its own safety backup covers the data.
                let _ = self.store().finish_backup(safety.id, Err("its restore was not created".to_string())).await;
                return Err(match error {
                    StoreError::Conflict { .. } => rejected("a restore of this database is in progress".to_string()),
                    error => error.into(),
                });
            }
        };
        let details = json!({
            "service_id": service.id.to_string(),
            "backup_id": backup_id.to_string(),
            "safety_backup_id": safety.id.to_string(),
        });
        self.audit_by(principal, "backup.restore", restore.id, Some(environment.project_id), details).await?;
        self.wake_backups();
        Ok(restore)
    }

    /// The database's restores, newest first.
    pub async fn list_restores(
        &self,
        principal: &Principal,
        service_id: ServiceId,
        after: Option<RestoreId>,
        limit: u32,
    ) -> Result<Page<Restore>, ControlError> {
        self.service_with_access(principal, service_id, Role::Viewer).await?;
        let limit = page::limit(limit);
        let items = self.store().list_restores(service_id, after, limit + 1).await?;
        Ok(Page::from_overfetch(items, limit))
    }

    pub async fn get_restore(&self, principal: &Principal, id: RestoreId) -> Result<Restore, ControlError> {
        let restore = self.store().get_restore(id).await?;
        match self.service_with_access(principal, restore.service_id, Role::Viewer).await {
            Err(ControlError::NotFound { .. }) => Err(not_found("restore", id)),
            Err(error) => Err(error),
            Ok(_) => Ok(restore),
        }
    }
}
