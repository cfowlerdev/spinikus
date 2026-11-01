//! Services. Developers in an environment create and edit its services (including where
//! they run and their runtime settings); admins delete them.

use serde_json::json;
use spinikus_core::{
    environment::Environment,
    id::{EnvironmentId, ServerId, ServiceId},
    membership::Role,
    runtime::RuntimeSettings,
    service::{Service, ServiceConfig, ServiceFields, ServiceKind},
    source::SourceSpec,
};
use spinikus_store::{EnvironmentRepository, ServerRepository, ServiceRepository, StoreError};

use crate::{
    Control, ControlError, Principal,
    access::not_found,
    audit, input,
    page::{self, Page},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewService {
    pub name: String,
    /// Made from the name when not given.
    pub slug: Option<String>,
    pub kind: ServiceKind,
    pub config: ServiceConfig,
    /// The server to run on; `None` means the embedded server.
    pub server_id: Option<ServerId>,
}

/// Changes to a service. Fields left as `None` keep their current value; a new config
/// replaces the whole config. The kind is fixed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceChanges {
    pub name: Option<String>,
    pub slug: Option<String>,
    pub config: Option<ServiceConfig>,
    /// `Some(None)` moves the service back to the embedded server.
    pub server_id: Option<Option<ServerId>>,
}

impl Control {
    pub async fn create_service(
        &self,
        principal: &Principal,
        environment_id: EnvironmentId,
        new: NewService,
    ) -> Result<Service, ControlError> {
        let environment = self.store().get_environment(environment_id).await?;
        self.access(principal).await?.check_environment(&environment, Role::Developer)?;
        let name = input::name(&new.name)?;
        let slug = input::slug(new.slug.as_deref(), &name)?;
        check_config(&new.config)?;
        self.check_placement(new.server_id).await?;
        let fields = ServiceFields { name, slug, config: new.config, server_id: new.server_id };
        let service = self.store().create_service(environment_id, new.kind, fields).await?;
        let details = json!({ "slug": service.slug, "kind": service.kind, "environment_id": environment_id.to_string() });
        self.audit_by(principal, "service.create", service.id, Some(environment.project_id), details).await?;
        Ok(service)
    }

    /// Creates a service with its source, as one step: the source is checked first, and
    /// if it can't be created after all, neither is the service. Needs the developer role.
    pub async fn create_service_with_source(
        &self,
        principal: &Principal,
        environment_id: EnvironmentId,
        new: NewService,
        source: SourceSpec,
    ) -> Result<Service, ControlError> {
        crate::sources::check(&source)?;
        let service = self.create_service(principal, environment_id, new).await?;
        if let Err(error) = self.create_source(principal, service.id, source).await {
            // The service was created moments ago, by this call; nothing else refers to it.
            if let Err(cleanup) = self.store().delete_service(service.id).await {
                tracing::warn!(service = %service.id, error = %cleanup, "could not remove a service whose source failed");
            }
            return Err(error);
        }
        Ok(service)
    }

    pub async fn list_services(
        &self,
        principal: &Principal,
        environment_id: EnvironmentId,
        after: Option<ServiceId>,
        limit: u32,
    ) -> Result<Page<Service>, ControlError> {
        let limit = page::limit(limit);
        let environment = self.store().get_environment(environment_id).await?;
        self.access(principal).await?.check_environment(&environment, Role::Viewer)?;
        let items = self.store().list_services(environment_id, after, limit + 1).await?;
        Ok(Page::from_overfetch(items, limit))
    }

    pub async fn get_service(&self, principal: &Principal, id: ServiceId) -> Result<Service, ControlError> {
        let (service, _) = self.service_with_access(principal, id, Role::Viewer).await?;
        Ok(service)
    }

    /// Applies `changes` if the service is still at `revision`. Needs the developer role.
    pub async fn update_service(
        &self,
        principal: &Principal,
        id: ServiceId,
        revision: i64,
        changes: ServiceChanges,
    ) -> Result<Service, ControlError> {
        let (current, environment) = self.service_with_access(principal, id, Role::Developer).await?;
        let name = match &changes.name {
            Some(name) => input::name(name)?,
            None => current.name,
        };
        let slug = match changes.slug.as_deref() {
            Some(slug) => input::slug(Some(slug), &name)?,
            None => current.slug,
        };
        let config_changed = changes.config.is_some();
        let config = changes.config.unwrap_or(current.config);
        check_config(&config)?;
        let placement_changed = changes.server_id.is_some();
        let server_id = changes.server_id.unwrap_or(current.server_id);
        if placement_changed {
            self.check_placement(server_id).await?;
        }
        let service = self.store().update_service(id, revision, ServiceFields { name, slug, config, server_id }).await?;
        let changed = audit::changed(&[
            ("name", changes.name.is_some()),
            ("slug", changes.slug.is_some()),
            ("config", config_changed),
            ("server_id", placement_changed),
        ]);
        self.audit_by(principal, "service.update", id, Some(environment.project_id), changed).await?;
        Ok(service)
    }

    /// Deletes the service. Needs the admin role, and fails while the service has domains.
    pub async fn delete_service(&self, principal: &Principal, id: ServiceId) -> Result<(), ControlError> {
        let (service, environment) = self.service_with_access(principal, id, Role::Admin).await?;
        self.store().delete_service(id).await?;
        let details = json!({ "slug": service.slug, "environment_id": environment.id.to_string() });
        self.audit_by(principal, "service.delete", id, Some(environment.project_id), details).await
    }

    /// A service can be placed on any server that is not revoked.
    async fn check_placement(&self, server_id: Option<ServerId>) -> Result<(), ControlError> {
        let Some(server_id) = server_id else {
            return Ok(());
        };
        let unknown = || ControlError::Invalid { field: "server_id", problem: format!("no server `{server_id}`") };
        match self.store().get_server(server_id).await {
            Ok(server) if server.revoked_at.is_none() => Ok(()),
            Ok(_) | Err(StoreError::NotFound { .. }) => Err(unknown()),
            Err(error) => Err(error.into()),
        }
    }

    /// The service and its environment, if the principal has `needed` there. A service in an
    /// environment the principal cannot see is `NotFound`.
    pub(crate) async fn service_with_access(
        &self,
        principal: &Principal,
        id: ServiceId,
        needed: Role,
    ) -> Result<(Service, Environment), ControlError> {
        let service = self.store().get_service(id).await?;
        let environment = self.store().get_environment(service.environment_id).await?;
        match self.access(principal).await?.check_environment(&environment, needed) {
            Err(ControlError::NotFound { .. }) => Err(not_found("service", id)),
            result => result.map(|()| (service, environment)),
        }
    }
}

/// The runtime settings must be ones Spinikus can apply; the rest of the config is free-form.
fn check_config(config: &ServiceConfig) -> Result<(), ControlError> {
    RuntimeSettings::from_config(config)
        .map(|_| ())
        .map_err(|error| ControlError::Invalid { field: "config.runtime", problem: error.to_string() })
}

#[cfg(test)]
mod tests {
    use spinikus_core::{environment::EnvironmentKind, membership::MembershipScope};

    use super::*;
    use crate::{
        NewEnvironment, NewProject,
        test_support::{control, grant, user, with_token},
    };

    fn new(name: &str) -> NewService {
        let config = json!({ "port": 8080 }).as_object().cloned().unwrap();
        NewService { name: name.to_string(), slug: None, kind: ServiceKind::App, config, server_id: None }
    }

    async fn environments(control: &Control, admin: &Principal) -> (Environment, Environment) {
        let project = control.create_project(admin, NewProject { name: "Shop".to_string(), slug: None }).await.unwrap();
        let mut created = Vec::new();
        for (name, kind) in [("Production", EnvironmentKind::Production), ("Staging", EnvironmentKind::Staging)] {
            let new = NewEnvironment { name: name.to_string(), slug: None, kind };
            created.push(control.create_environment(admin, project.id, new).await.unwrap());
        }
        let staging = created.pop().unwrap();
        (created.pop().unwrap(), staging)
    }

    #[tokio::test]
    async fn creates_a_service_with_its_source_or_neither() {
        let (_dir, control) = control().await;
        let admin = user(&control, "admin@example.com", true).await;
        let (production, _) = environments(&control, &admin).await;
        let image = |image: &str| SourceSpec::Image(spinikus_core::source::ImageSource { image: image.to_string() });

        let created = control.create_service_with_source(&admin, production.id, new("Web"), image("nginx:1")).await.unwrap();
        let bad_source = control.create_service_with_source(&admin, production.id, new("Api"), image(" ")).await;
        let taken = control.create_service_with_source(&admin, production.id, new("Web"), image("nginx:1")).await;

        assert_eq!(control.get_source(&admin, created.id).await.unwrap().spec, image("nginx:1"));
        assert!(matches!(bad_source, Err(ControlError::Invalid { .. })), "{bad_source:?}");
        assert!(matches!(taken, Err(ControlError::Conflict { .. })), "{taken:?}");
        let services = control.list_services(&admin, production.id, None, 50).await.unwrap().items;
        assert_eq!(services.iter().map(|service| service.name.as_str()).collect::<Vec<_>>(), ["Web"]);
    }

    #[tokio::test]
    async fn developers_create_and_edit_services_and_admins_delete_them() {
        let (_dir, control) = control().await;
        let admin = user(&control, "admin@example.com", true).await;
        let developer = user(&control, "ada@example.com", false).await;
        let (production, _) = environments(&control, &admin).await;
        grant(&control, &developer, MembershipScope::Environment(production.id), Role::Developer).await;

        let service = control.create_service(&developer, production.id, new("Web App")).await.unwrap();
        let config = json!({ "port": 3000 }).as_object().cloned().unwrap();
        let changes = ServiceChanges { config: Some(config.clone()), ..Default::default() };
        let updated = control.update_service(&developer, service.id, 1, changes).await.unwrap();
        let listed = control.list_services(&developer, production.id, None, 50).await.unwrap();
        let delete = control.delete_service(&developer, service.id).await;

        assert_eq!((service.slug.as_str(), service.kind), ("web-app", ServiceKind::App));
        assert_eq!((updated.config, updated.name.as_str(), updated.revision), (config, "Web App", 2));
        assert_eq!(listed.items.len(), 1);
        assert!(matches!(delete, Err(ControlError::Forbidden)), "{delete:?}");
        control.delete_service(&admin, service.id).await.unwrap();
        assert!(matches!(control.get_service(&admin, service.id).await, Err(ControlError::NotFound { .. })));
    }

    #[tokio::test]
    async fn checks_placement_and_runtime_settings() {
        let (_dir, control) = control().await;
        let admin = user(&control, "admin@example.com", true).await;
        let (production, _) = environments(&control, &admin).await;
        let server = control.create_server(&admin, crate::NewServer { name: "web-1".to_string() }).await.unwrap();
        let revoked = control.create_server(&admin, crate::NewServer { name: "old".to_string() }).await.unwrap();
        control.revoke_server(&admin, revoked.id).await.unwrap();
        let runtime = |runtime: serde_json::Value| json!({ "runtime": runtime }).as_object().cloned().unwrap();

        let placed = NewService { server_id: Some(server.id), ..new("Web") };
        let service = control.create_service(&admin, production.id, placed).await.unwrap();
        let on_revoked = NewService { server_id: Some(revoked.id), ..new("Worker") };
        let on_revoked = control.create_service(&admin, production.id, on_revoked).await;
        let unknown = NewService { config: runtime(json!({ "release": "migrate" })), ..new("Worker") };
        let unknown = control.create_service(&admin, production.id, unknown).await;
        let moved =
            ServiceChanges { server_id: Some(None), config: Some(runtime(json!({ "port": 3000 }))), ..Default::default() };
        let moved = control.update_service(&admin, service.id, service.revision, moved).await.unwrap();
        let path_without_port =
            ServiceChanges { config: Some(runtime(json!({ "healthcheck": { "path": "/up" } }))), ..Default::default() };
        let path_without_port = control.update_service(&admin, service.id, moved.revision, path_without_port).await;

        assert_eq!(service.server_id, Some(server.id));
        assert!(matches!(on_revoked, Err(ControlError::Invalid { field: "server_id", .. })), "{on_revoked:?}");
        assert!(matches!(unknown, Err(ControlError::Invalid { field: "config.runtime", .. })), "{unknown:?}");
        assert_eq!(moved.server_id, None);
        assert!(matches!(path_without_port, Err(ControlError::Invalid { field: "config.runtime", .. })), "{path_without_port:?}");
    }

    #[tokio::test]
    async fn services_in_hidden_environments_are_not_found() {
        let (_dir, control) = control().await;
        let admin = user(&control, "admin@example.com", true).await;
        let contractor = user(&control, "ada@example.com", false).await;
        let (production, staging) = environments(&control, &admin).await;
        grant(&control, &contractor, MembershipScope::Environment(staging.id), Role::Developer).await;
        let service = control.create_service(&admin, production.id, new("Web")).await.unwrap();

        let get = control.get_service(&contractor, service.id).await;
        let create = control.create_service(&contractor, production.id, new("Worker")).await;
        let list = control.list_services(&contractor, production.id, None, 50).await;

        assert!(matches!(get, Err(ControlError::NotFound { entity: "service", .. })), "{get:?}");
        assert!(matches!(create, Err(ControlError::NotFound { entity: "environment", .. })), "{create:?}");
        assert!(matches!(list, Err(ControlError::NotFound { entity: "environment", .. })), "{list:?}");
    }

    #[tokio::test]
    async fn tokens_cap_the_role() {
        let (_dir, control) = control().await;
        let admin = user(&control, "admin@example.com", true).await;
        let developer = user(&control, "ada@example.com", false).await;
        let (production, staging) = environments(&control, &admin).await;
        grant(&control, &developer, MembershipScope::Environment(production.id), Role::Developer).await;
        let read_only = with_token(&control, &developer, Role::Viewer, None).await;
        let staging_only = with_token(&control, &admin, Role::Admin, Some(MembershipScope::Environment(staging.id))).await;

        let by_viewer_token = control.create_service(&read_only, production.id, new("Web")).await;
        let outside_scope = control.create_service(&staging_only, production.id, new("Web")).await;
        let inside_scope = control.create_service(&staging_only, staging.id, new("Web")).await;
        let listed = control.list_projects(&staging_only, None, 50).await.unwrap();

        assert!(matches!(by_viewer_token, Err(ControlError::Forbidden)), "{by_viewer_token:?}");
        assert!(matches!(outside_scope, Err(ControlError::NotFound { .. })), "{outside_scope:?}");
        assert!(inside_scope.is_ok(), "{inside_scope:?}");
        assert_eq!(listed.items.len(), 1);
    }
}
