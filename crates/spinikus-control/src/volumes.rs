//! Persistent volumes of a service. Developers add and change them, and viewers see them;
//! deleting one takes the admin role, since its data goes with it. Changes apply from the
//! service's next deployment: a service with volumes is deployed with `recreate`, so two
//! containers never share one.
//!
//! A database's data volume stays at the path its image keeps its data in: moving it would
//! start the database empty, so that is refused (renaming it is fine).
//!
//! The data stays on the server, in a runtime volume named after the volume's ID, even once
//! the volume is deleted; removing it from the servers comes later.

use serde_json::json;
use spinikus_core::{
    database::DatabaseSpec,
    environment::Environment,
    id::{ServiceId, VolumeId},
    membership::Role,
    service::Service,
    slug::Slug,
    volume::{MountPath, Volume, VolumeFields},
};
use spinikus_store::VolumeRepository;

use crate::{Control, ControlError, Principal, access::not_found, audit};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewVolume {
    /// A slug, such as `data`.
    pub name: String,
    pub mount_path: String,
}

/// Changes to a volume; `None` keeps the current value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VolumeChanges {
    pub name: Option<String>,
    pub mount_path: Option<String>,
}

impl Control {
    pub async fn create_volume(
        &self,
        principal: &Principal,
        service_id: ServiceId,
        new: NewVolume,
    ) -> Result<Volume, ControlError> {
        let (_, environment) = self.service_with_access(principal, service_id, Role::Developer).await?;
        let fields = VolumeFields { name: name(&new.name)?, mount_path: mount_path(&new.mount_path)? };
        let volume = self.store().create_volume(service_id, fields).await?;
        let details = json!({ "name": volume.name, "mount_path": volume.mount_path, "service_id": service_id.to_string() });
        self.audit_by(principal, "volume.create", volume.id, Some(environment.project_id), details).await?;
        Ok(volume)
    }

    pub async fn list_volumes(&self, principal: &Principal, service_id: ServiceId) -> Result<Vec<Volume>, ControlError> {
        self.service_with_access(principal, service_id, Role::Viewer).await?;
        Ok(self.store().list_volumes(service_id).await?)
    }

    pub async fn get_volume(&self, principal: &Principal, id: VolumeId) -> Result<Volume, ControlError> {
        let (volume, _, _) = self.volume_with_access(principal, id, Role::Viewer).await?;
        Ok(volume)
    }

    /// Applies `changes` if the volume is still at `revision`. The data stays where it is.
    /// Moving a database's data volume is `Rejected`.
    pub async fn update_volume(
        &self,
        principal: &Principal,
        id: VolumeId,
        revision: i64,
        changes: VolumeChanges,
    ) -> Result<Volume, ControlError> {
        let (current, service, environment) = self.volume_with_access(principal, id, Role::Developer).await?;
        let fields = VolumeFields {
            name: match &changes.name {
                Some(value) => name(value)?,
                None => current.name.clone(),
            },
            mount_path: match &changes.mount_path {
                Some(path) => mount_path(path)?,
                None => current.mount_path.clone(),
            },
        };
        if fields.mount_path != current.mount_path && holds_database_data(&service, &current) {
            return Err(ControlError::Rejected {
                entity: "volume",
                id: id.to_string(),
                problem: format!(
                    "it holds the database's data, which its image keeps at {}; moving it would start the database empty",
                    current.mount_path
                ),
            });
        }
        let volume = self.store().update_volume(id, revision, fields).await?;
        let changed = audit::changed(&[("name", changes.name.is_some()), ("mount_path", changes.mount_path.is_some())]);
        self.audit_by(principal, "volume.update", id, Some(environment.project_id), changed).await?;
        Ok(volume)
    }

    /// Deletes the volume. Needs the admin role, since later deployments go without its data.
    pub async fn delete_volume(&self, principal: &Principal, id: VolumeId) -> Result<(), ControlError> {
        let (volume, _, environment) = self.volume_with_access(principal, id, Role::Admin).await?;
        self.store().delete_volume(id).await?;
        let details = json!({ "name": volume.name, "service_id": volume.service_id.to_string() });
        self.audit_by(principal, "volume.delete", id, Some(environment.project_id), details).await
    }

    async fn volume_with_access(
        &self,
        principal: &Principal,
        id: VolumeId,
        needed: Role,
    ) -> Result<(Volume, Service, Environment), ControlError> {
        let volume = self.store().get_volume(id).await?;
        match self.service_with_access(principal, volume.service_id, needed).await {
            Err(ControlError::NotFound { .. }) => Err(not_found("volume", id)),
            Err(error) => Err(error),
            Ok((service, environment)) => Ok((volume, service, environment)),
        }
    }
}

/// Whether `volume` is where `service`, a database, keeps its data.
fn holds_database_data(service: &Service, volume: &Volume) -> bool {
    DatabaseSpec::of(&service.config).is_some_and(|spec| spec.data_path() == volume.mount_path)
}

fn name(value: &str) -> Result<Slug, ControlError> {
    value
        .trim()
        .parse()
        .map_err(|error: spinikus_core::slug::SlugError| ControlError::Invalid { field: "name", problem: error.to_string() })
}

fn mount_path(value: &str) -> Result<MountPath, ControlError> {
    value.trim().parse().map_err(|error: spinikus_core::volume::MountPathError| ControlError::Invalid {
        field: "mount_path",
        problem: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use spinikus_core::{deployment::DeploymentStrategy, membership::MembershipScope};

    use super::*;
    use crate::{
        DeploymentTimings,
        deployments::tests::setup,
        test_support::{grant, user},
    };

    fn new(name: &str, mount_path: &str) -> NewVolume {
        NewVolume { name: name.to_string(), mount_path: mount_path.to_string() }
    }

    #[tokio::test]
    async fn developers_add_volumes_and_admins_delete_them() {
        let setup = setup(DeploymentTimings::default()).await;
        let (control, admin, service) = (&setup.control, &setup.admin, setup.service.id);
        let developer = user(control, "ada@example.com", false).await;
        let viewer = user(control, "vic@example.com", false).await;
        let environment = MembershipScope::Environment(setup.service.environment_id);
        grant(control, &developer, environment, Role::Developer).await;
        grant(control, &viewer, environment, Role::Viewer).await;

        let without = control.create_deployment(admin, service).await.unwrap();
        let data = control.create_volume(&developer, service, new("data", "/var/lib/data/")).await.unwrap();
        let by_viewer = control.create_volume(&viewer, service, new("cache", "/cache")).await;
        let bad_name = control.create_volume(&developer, service, new("My Data", "/x")).await;
        let bad_path = control.create_volume(&developer, service, new("x", "relative")).await;
        let same_path = control.create_volume(&developer, service, new("other", "/var/lib/data")).await;
        let changes = VolumeChanges { mount_path: Some("/data".to_string()), ..Default::default() };
        let moved = control.update_volume(&developer, data.id, data.revision, changes).await.unwrap();
        let listed = control.list_volumes(&viewer, service).await.unwrap();
        let with = control.create_deployment(admin, service).await.unwrap();
        let deleted_by_developer = control.delete_volume(&developer, data.id).await;
        control.delete_volume(admin, data.id).await.unwrap();

        assert_eq!(data.mount_path.as_str(), "/var/lib/data");
        assert!(matches!(by_viewer, Err(ControlError::Forbidden)), "{by_viewer:?}");
        assert!(matches!(bad_name, Err(ControlError::Invalid { field: "name", .. })), "{bad_name:?}");
        assert!(matches!(bad_path, Err(ControlError::Invalid { field: "mount_path", .. })), "{bad_path:?}");
        assert!(matches!(same_path, Err(ControlError::Conflict { .. })), "{same_path:?}");
        assert_eq!(moved.mount_path.as_str(), "/data");
        assert_eq!(listed, [moved]);
        assert_eq!((without.strategy, with.strategy), (DeploymentStrategy::BlueGreen, DeploymentStrategy::Recreate));
        assert!(matches!(deleted_by_developer, Err(ControlError::Forbidden)), "{deleted_by_developer:?}");
        assert!(control.list_volumes(admin, service).await.unwrap().is_empty());
    }
}
