//! One-click databases. Creating one makes an ordinary service of kind `database` from a
//! [`DatabaseSpec`]: the engine's official image as its source, a `data` volume where the
//! image keeps its data, its runtime config, and variables with a generated password, which
//! apps of the environment reference (`${{ postgres.DATABASE_URL }}`). Then it is deployed,
//! unless `deploy` is false: its settings can then be changed before its first deployment.
//! Each part is made through the same operations as by hand, so the same permissions and
//! audit entries apply: developers of the environment create databases.
//!
//! A database can start from a backup of another (`from_backup`): it gets the backup's
//! engine, at the same version or a newer one, and a restore of the backup that waits for
//! its first deployment to go live. There is nothing to protect yet, so no safety backup.
//! Copying a database's data takes the admin role in its environment, as restoring does.

use serde::Serialize;
use serde_json::json;
use spinikus_core::{
    backup::{Backup, BackupStatus, NewRestore, Restore},
    database::{DatabaseSpec, Engine},
    deployment::Deployment,
    id::{BackupId, EnvironmentId, ServerId},
    membership::Role,
    secret::Secret,
    service::{Service, ServiceKind},
    source::{ImageSource, SourceSpec},
    variable::{VariableAvailability, VariableScope},
};
use spinikus_store::{EnvironmentRepository, RestoreRepository, ServiceRepository};

use crate::{Control, ControlError, NewService, NewVariable, NewVolume, Principal};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDatabase {
    pub name: String,
    /// Made from the name when not given. It is the name apps reach the database by.
    pub slug: Option<String>,
    /// Required unless it starts from a backup, whose engine it then must be.
    pub engine: Option<Engine>,
    /// A major version: the engine's newest if `None`, or the backup's database's version.
    pub version: Option<String>,
    /// The server to run on; `None` means the embedded server.
    pub server_id: Option<ServerId>,
    /// A backup of another database to restore into this one once it runs.
    pub from_backup: Option<BackupId>,
    /// Deploy it right away. A database started from a backup always is, to restore it.
    pub deploy: bool,
}

/// A database just created, and its first deployment, unless it was created without one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CreatedDatabase {
    pub service: Service,
    pub deployment: Option<Deployment>,
    /// The restore of the backup it starts from, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restore: Option<Restore>,
}

impl Control {
    pub async fn create_database(
        &self,
        principal: &Principal,
        environment_id: EnvironmentId,
        new: NewDatabase,
    ) -> Result<CreatedDatabase, ControlError> {
        if !new.deploy && new.from_backup.is_some() {
            let problem = "a database started from a backup is deployed right away, to restore the backup into it";
            return Err(ControlError::Invalid { field: "deploy", problem: problem.to_string() });
        }
        let source = match new.from_backup {
            Some(id) => Some(self.backup_to_copy(principal, id).await?),
            None => None,
        };
        let engine = match (new.engine, &source) {
            (Some(engine), Some((_, from))) if engine != from.engine() => {
                let problem = format!("the backup is of a {} database", from.engine());
                return Err(ControlError::Invalid { field: "engine", problem });
            }
            (Some(engine), _) => engine,
            (None, Some((_, from))) => from.engine(),
            (None, None) => return Err(ControlError::Invalid { field: "engine", problem: "choose an engine".to_string() }),
        };
        let version = new.version.as_deref().or(source.as_ref().map(|(_, from)| from.version()));
        let spec = DatabaseSpec::new(engine, version)
            .map_err(|error| ControlError::Invalid { field: "version", problem: error.to_string() })?;
        if let Some((_, from)) = &source
            && !from.restores_into(&spec)
        {
            let problem = format!("the backup is of {engine} {}, which doesn't restore into an older version", from.version());
            return Err(ControlError::Invalid { field: "version", problem });
        }
        let service = NewService {
            name: new.name,
            slug: new.slug,
            kind: ServiceKind::Database,
            config: spec.config(),
            server_id: new.server_id,
        };
        let service = self.create_service(principal, environment_id, service).await?;
        let backup = source.map(|(backup, _)| backup);
        match self.set_up_database(principal, &service, &spec, backup.as_ref(), new.deploy).await {
            Ok((deployment, restore)) => Ok(CreatedDatabase { service, deployment, restore }),
            Err(error) => {
                // Don't leave half a database behind.
                if let Err(cleanup) = self.store().delete_service(service.id).await {
                    tracing::warn!(service = %service.id, error = %cleanup, "could not delete a database that failed to set up");
                }
                Err(error)
            }
        }
    }

    /// A backup that can be copied into a new database, and its database's spec.
    async fn backup_to_copy(&self, principal: &Principal, id: BackupId) -> Result<(Backup, DatabaseSpec), ControlError> {
        let (backup, _) = self.backup_with_access(principal, id, Role::Admin).await?;
        let rejected =
            |problem: &str| ControlError::Rejected { entity: "backup", id: id.to_string(), problem: problem.to_string() };
        if backup.status != BackupStatus::Succeeded {
            return Err(rejected("only a backup that succeeded can be restored"));
        }
        let database = self.store().get_service(backup.service_id).await?;
        let spec = DatabaseSpec::of(&database.config).ok_or_else(|| rejected("its database names no version Spinikus knows"))?;
        Ok((backup, spec))
    }

    async fn set_up_database(
        &self,
        principal: &Principal,
        service: &Service,
        spec: &DatabaseSpec,
        from_backup: Option<&Backup>,
        deploy: bool,
    ) -> Result<(Option<Deployment>, Option<Restore>), ControlError> {
        let image = SourceSpec::Image(ImageSource { image: spec.image() });
        self.create_source(principal, service.id, image).await?;
        let volume = NewVolume { name: "data".to_string(), mount_path: spec.data_path().to_string() };
        self.create_volume(principal, service.id, volume).await?;
        let password: String = rand::random::<[u8; 24]>().iter().map(|byte| format!("{byte:02x}")).collect();
        for variable in spec.variables(&service.slug, &password) {
            let new = NewVariable {
                name: variable.name.to_string(),
                value: Secret::new(variable.value),
                availability: VariableAvailability::Runtime,
                secret: variable.secret,
            };
            self.create_variable(principal, VariableScope::Service(service.id), new).await?;
        }
        if !deploy {
            return Ok((None, None));
        }
        let deployment = Some(self.create_deployment(principal, service.id).await?);
        let Some(backup) = from_backup else {
            return Ok((deployment, None));
        };
        let new = NewRestore { service_id: service.id, backup_id: backup.id, safety_backup_id: None };
        let restore = self.store().create_restore(new).await?;
        let environment = self.store().get_environment(service.environment_id).await?;
        let details = json!({ "service_id": service.id.to_string(), "backup_id": backup.id.to_string(), "new_database": true });
        self.audit_by(principal, "backup.restore", restore.id, Some(environment.project_id), details).await?;
        self.wake_backups();
        Ok((deployment, Some(restore)))
    }
}

#[cfg(test)]
mod tests {
    use spinikus_core::{
        deployment::{DeploymentStatus, DeploymentStrategy},
        membership::{MembershipScope, Role},
    };
    use spinikus_store::{DeploymentRepository, SourceRepository, VariableRepository, VolumeRepository};

    use super::*;
    use crate::{
        DeploymentTimings,
        deployments::tests::setup,
        test_support::{grant, user},
    };

    fn new(name: &str, engine: Engine, version: Option<&str>) -> NewDatabase {
        NewDatabase {
            name: name.to_string(),
            slug: None,
            engine: Some(engine),
            version: version.map(str::to_string),
            server_id: None,
            from_backup: None,
            deploy: true,
        }
    }

    #[tokio::test]
    async fn creates_and_deploys_a_database_that_apps_can_reference() {
        let setup = setup(DeploymentTimings::default()).await;
        let (control, admin, web) = (&setup.control, &setup.admin, &setup.service);
        let environment = web.environment_id;
        let developer = user(control, "ada@example.com", false).await;
        let viewer = user(control, "vic@example.com", false).await;
        grant(control, &developer, MembershipScope::Environment(environment), Role::Developer).await;
        grant(control, &viewer, MembershipScope::Environment(environment), Role::Viewer).await;
        let mut postgres = new("Postgres", Engine::Postgres, Some("17"));
        postgres.server_id = Some(setup.server.id);

        let created = control.create_database(&developer, environment, postgres).await.unwrap();
        let by_viewer = control.create_database(&viewer, environment, new("Cache", Engine::Redis, None)).await;
        let bad_version = control.create_database(&developer, environment, new("Old", Engine::Postgres, Some("9.6"))).await;
        let url = NewVariable {
            name: "DATABASE_URL".to_string(),
            value: Secret::new("${{ postgres.DATABASE_URL }}"),
            availability: VariableAvailability::Runtime,
            secret: false,
        };
        control.create_variable(admin, VariableScope::Service(web.id), url).await.unwrap();
        let web_deployment = control.create_deployment(admin, web.id).await.unwrap();

        let (service, deployment) = (&created.service, created.deployment.as_ref().unwrap());
        assert_eq!((service.kind, service.slug.as_str()), (ServiceKind::Database, "postgres"));
        let source = control.store().get_source(service.id).await.unwrap();
        assert_eq!(source.spec, SourceSpec::Image(ImageSource { image: "postgres:17".to_string() }));
        let volumes = control.store().list_volumes(service.id).await.unwrap();
        assert_eq!((volumes[0].name.as_str(), volumes[0].mount_path.as_str()), ("data", "/var/lib/postgresql/data"));
        assert_eq!((deployment.status, deployment.strategy), (DeploymentStatus::Queued, DeploymentStrategy::Recreate));
        assert_eq!(deployment.server_id, setup.server.id);
        let variables = control.store().list_variables(VariableScope::Service(service.id)).await.unwrap();
        let password = variables.iter().find(|variable| variable.name.as_str() == "POSTGRES_PASSWORD").unwrap();
        let password = control.store().reveal_variable(password.id).await.unwrap();
        assert_eq!(password.expose().len(), 48);
        let resolved = control.store().deployment_variables(web_deployment.id).await.unwrap();
        let url = resolved.iter().find(|variable| variable.name.as_str() == "DATABASE_URL").unwrap();
        assert_eq!(url.value.expose(), format!("postgres://app:{}@postgres:5432/app", password.expose()));
        assert!(url.secret);
        assert!(matches!(by_viewer, Err(ControlError::Forbidden)), "{by_viewer:?}");
        assert!(matches!(bad_version, Err(ControlError::Invalid { field: "version", .. })), "{bad_version:?}");
    }

    #[tokio::test]
    async fn the_data_volume_can_be_renamed_but_not_moved_or_deleted() {
        let setup = setup(DeploymentTimings::default()).await;
        let (control, admin) = (&setup.control, &setup.admin);
        let environment = setup.service.environment_id;
        let mut postgres = new("Postgres", Engine::Postgres, None);
        postgres.server_id = Some(setup.server.id);
        let created = control.create_database(admin, environment, postgres).await.unwrap();
        let data = control.list_volumes(admin, created.service.id).await.unwrap().remove(0);
        let other = NewVolume { name: "scratch".to_string(), mount_path: "/scratch".to_string() };
        let other = control.create_volume(admin, created.service.id, other).await.unwrap();

        let to = |path: &str| crate::VolumeChanges { mount_path: Some(path.to_string()), ..Default::default() };
        let moved = control.update_volume(admin, data.id, data.revision, to("/elsewhere")).await;
        let same_path = control.update_volume(admin, data.id, data.revision, to(data.mount_path.as_str())).await.unwrap();
        let renamed = crate::VolumeChanges { name: Some("pgdata".to_string()), ..Default::default() };
        let renamed = control.update_volume(admin, data.id, same_path.revision, renamed).await.unwrap();
        let other_moved = control.update_volume(admin, other.id, other.revision, to("/tmp/scratch")).await.unwrap();
        let deleted = control.delete_volume(admin, data.id).await;
        control.delete_volume(admin, other.id).await.unwrap();

        assert!(matches!(&moved, Err(ControlError::Rejected { entity: "volume", .. })), "{moved:?}");
        assert_eq!((renamed.name.as_str(), renamed.mount_path.as_str()), ("pgdata", "/var/lib/postgresql"));
        assert_eq!(other_moved.mount_path.as_str(), "/tmp/scratch", "only the data volume is fixed");
        assert!(matches!(&deleted, Err(ControlError::Rejected { entity: "volume", .. })), "{deleted:?}");
        assert_eq!(control.list_volumes(admin, created.service.id).await.unwrap(), [renamed]);
    }

    #[tokio::test]
    async fn a_database_can_wait_to_be_deployed() {
        let setup = setup(DeploymentTimings::default()).await;
        let (control, admin) = (&setup.control, &setup.admin);
        let environment = setup.service.environment_id;
        let mut postgres = new("Postgres", Engine::Postgres, None);
        postgres.server_id = Some(setup.server.id);
        postgres.deploy = false;
        let mut from_backup = new("Copy", Engine::Postgres, None);
        from_backup.deploy = false;
        from_backup.from_backup = Some(BackupId::new());

        let created = control.create_database(admin, environment, postgres).await.unwrap();
        let refused = control.create_database(admin, environment, from_backup).await;
        let deployments = control.store().list_deployments(created.service.id, None, 10).await.unwrap();
        let variables = control.store().list_variables(VariableScope::Service(created.service.id)).await.unwrap();
        let later = control.create_deployment(admin, created.service.id).await.unwrap();

        assert_eq!(created.deployment, None);
        assert!(deployments.is_empty(), "{deployments:?}");
        assert!(variables.iter().any(|variable| variable.name.as_str() == "DATABASE_URL"), "set up all the same");
        assert!(matches!(refused, Err(ControlError::Invalid { field: "deploy", .. })), "{refused:?}");
        assert_eq!(later.strategy, DeploymentStrategy::Recreate);
    }

    #[tokio::test]
    async fn two_databases_get_different_passwords_and_slugs_must_be_free() {
        let setup = setup(DeploymentTimings::default()).await;
        let (control, admin) = (&setup.control, &setup.admin);
        let environment = setup.service.environment_id;
        let mut cache = new("Cache", Engine::Redis, None);
        cache.server_id = Some(setup.server.id);
        let mut sessions = cache.clone();
        sessions.name = "Sessions".to_string();
        let mut taken = cache.clone();
        taken.slug = Some("web".to_string());

        let first = control.create_database(admin, environment, cache).await.unwrap();
        let second = control.create_database(admin, environment, sessions).await.unwrap();
        let conflict = control.create_database(admin, environment, taken).await;

        let password = |service| async move {
            let variables = control.store().list_variables(VariableScope::Service(service)).await.unwrap();
            control
                .store()
                .reveal_variable(variables.iter().find(|v| v.name.as_str() == "REDIS_PASSWORD").unwrap().id)
                .await
                .unwrap()
        };
        assert_ne!(password(first.service.id).await.expose(), password(second.service.id).await.expose());
        assert!(matches!(conflict, Err(ControlError::Conflict { .. })), "{conflict:?}");
    }
}
