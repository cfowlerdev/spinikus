//! Services. Developers in an environment create and edit its services; admins delete them.

use serde_json::json;
use spinikus_core::{
    environment::Environment,
    id::{EnvironmentId, ServiceId},
    membership::Role,
    service::{Service, ServiceConfig, ServiceFields, ServiceKind},
};
use spinikus_store::{EnvironmentRepository, ServiceRepository};

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
}

/// Changes to a service. Fields left as `None` keep their current value; a new config
/// replaces the whole config. The kind is fixed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceChanges {
    pub name: Option<String>,
    pub slug: Option<String>,
    pub config: Option<ServiceConfig>,
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
        let fields = ServiceFields { name, slug, config: new.config };
        let service = self.store().create_service(environment_id, new.kind, fields).await?;
        let details = json!({ "slug": service.slug, "kind": service.kind, "environment_id": environment_id.to_string() });
        self.audit_by(principal, "service.create", service.id, Some(environment.project_id), details).await?;
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
        let service = self.store().update_service(id, revision, ServiceFields { name, slug, config }).await?;
        let changed =
            audit::changed(&[("name", changes.name.is_some()), ("slug", changes.slug.is_some()), ("config", config_changed)]);
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

    /// The service and its environment, if the principal has `needed` there. A service in an
    /// environment the principal cannot see is `NotFound`.
    async fn service_with_access(
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
        NewService { name: name.to_string(), slug: None, kind: ServiceKind::App, config }
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
