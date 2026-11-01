//! Environments. A project's admins create and delete them; an environment's admins rename it.

use serde_json::json;
use spinikus_core::{
    environment::{Environment, EnvironmentFields, EnvironmentKind},
    id::{EnvironmentId, ProjectId},
    membership::Role,
};
use spinikus_store::{EnvironmentRepository, ProjectRepository, StoreError};

use crate::{
    Control, ControlError, Principal,
    access::not_found,
    audit, input,
    page::{self, Page},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEnvironment {
    pub name: String,
    /// Made from the name when not given.
    pub slug: Option<String>,
    pub kind: EnvironmentKind,
}

/// Changes to an environment. Fields left as `None` keep their current value. The kind is fixed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvironmentChanges {
    pub name: Option<String>,
    pub slug: Option<String>,
}

impl Control {
    pub async fn create_environment(
        &self,
        principal: &Principal,
        project_id: ProjectId,
        new: NewEnvironment,
    ) -> Result<Environment, ControlError> {
        let project = self.store().get_project(project_id).await?;
        self.access(principal).await?.check_project(&project, Role::Admin)?;
        let name = input::name(&new.name)?;
        let slug = input::slug(new.slug.as_deref(), &name)?;
        let environment = self.store().create_environment(project_id, new.kind, EnvironmentFields { name, slug }).await?;
        let kind = environment.kind;
        let details = json!({ "slug": environment.slug, "kind": kind.as_str(), "pr_number": kind.pr_number() });
        self.audit_by(principal, "environment.create", environment.id, Some(project_id), details).await?;
        Ok(environment)
    }

    /// The project's environments: all of them with a role in the project, otherwise those
    /// the principal has a role in.
    pub async fn list_environments(
        &self,
        principal: &Principal,
        project_id: ProjectId,
        after: Option<EnvironmentId>,
        limit: u32,
    ) -> Result<Page<Environment>, ControlError> {
        let limit = page::limit(limit);
        self.store().get_project(project_id).await?;
        let access = self.access(principal).await?;
        if !access.can_see_project(project_id) {
            return Err(not_found("project", project_id));
        }
        let items = match access.visible_environments(project_id) {
            None => self.store().list_environments(project_id, after, limit + 1).await?,
            Some(ids) => {
                let mut items = Vec::new();
                for id in ids.into_iter().filter(|id| after.is_none_or(|after| *id > after)) {
                    if items.len() > limit as usize {
                        break;
                    }
                    match self.store().get_environment(id).await {
                        Ok(environment) => items.push(environment),
                        Err(StoreError::NotFound { .. }) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                items
            }
        };
        Ok(Page::from_overfetch(items, limit))
    }

    pub async fn get_environment(&self, principal: &Principal, id: EnvironmentId) -> Result<Environment, ControlError> {
        let environment = self.store().get_environment(id).await?;
        self.access(principal).await?.check_environment(&environment, Role::Viewer)?;
        Ok(environment)
    }

    /// The principal's role in the environment, so a client can offer only what they may do
    /// (each operation still checks). `NotFound` like `get_environment` if they have none.
    pub async fn environment_role(&self, principal: &Principal, environment: &Environment) -> Result<Role, ControlError> {
        let access = self.access(principal).await?;
        access.role_in(environment.project_id, Some(environment.id)).ok_or_else(|| not_found("environment", environment.id))
    }

    /// Applies `changes` if the environment is still at `revision`. Needs the admin role in it.
    pub async fn update_environment(
        &self,
        principal: &Principal,
        id: EnvironmentId,
        revision: i64,
        changes: EnvironmentChanges,
    ) -> Result<Environment, ControlError> {
        let current = self.store().get_environment(id).await?;
        self.access(principal).await?.check_environment(&current, Role::Admin)?;
        let name = match &changes.name {
            Some(name) => input::name(name)?,
            None => current.name,
        };
        let slug = match changes.slug.as_deref() {
            Some(slug) => input::slug(Some(slug), &name)?,
            None => current.slug,
        };
        let environment = self.store().update_environment(id, revision, EnvironmentFields { name, slug }).await?;
        let changed = audit::changed(&[("name", changes.name.is_some()), ("slug", changes.slug.is_some())]);
        self.audit_by(principal, "environment.update", id, Some(current.project_id), changed).await?;
        Ok(environment)
    }

    /// Deletes the environment. Needs the admin role in the project, since that is where
    /// environments are added, and fails while the environment has services.
    pub async fn delete_environment(&self, principal: &Principal, id: EnvironmentId) -> Result<(), ControlError> {
        let environment = self.store().get_environment(id).await?;
        let access = self.access(principal).await?;
        access.check_environment(&environment, Role::Viewer)?;
        let project = self.store().get_project(environment.project_id).await?;
        access.check_project(&project, Role::Admin)?;
        self.store().delete_environment(id).await?;
        let details = json!({ "slug": environment.slug });
        self.audit_by(principal, "environment.delete", id, Some(environment.project_id), details).await
    }
}

#[cfg(test)]
mod tests {
    use spinikus_core::{membership::MembershipScope, project::Project};

    use super::*;
    use crate::{
        NewProject,
        test_support::{control, grant, user},
    };

    fn new(name: &str, kind: EnvironmentKind) -> NewEnvironment {
        NewEnvironment { name: name.to_string(), slug: None, kind }
    }

    async fn project(control: &Control, admin: &Principal) -> Project {
        control.create_project(admin, NewProject { name: "Shop".to_string(), slug: None }).await.unwrap()
    }

    #[tokio::test]
    async fn project_admins_create_environments() {
        let (_dir, control) = control().await;
        let admin = user(&control, "admin@example.com", true).await;
        let developer = user(&control, "ada@example.com", false).await;
        let project = project(&control, &admin).await;
        grant(&control, &developer, MembershipScope::Project(project.id), Role::Developer).await;

        let production = control.create_environment(&admin, project.id, new("Production", EnvironmentKind::Production)).await;
        let by_developer = control.create_environment(&developer, project.id, new("Staging", EnvironmentKind::Staging)).await;
        let preview =
            control.create_environment(&admin, project.id, new("PR 7", EnvironmentKind::Preview { pr_number: 7 })).await;

        assert_eq!(production.unwrap().slug.as_str(), "production");
        assert!(matches!(by_developer, Err(ControlError::Forbidden)), "{by_developer:?}");
        assert_eq!(preview.unwrap().slug.as_str(), "pr-7");
    }

    #[tokio::test]
    async fn tells_the_role_a_principal_has_in_an_environment() {
        let (_dir, control) = control().await;
        let admin = user(&control, "admin@example.com", true).await;
        let developer = user(&control, "ada@example.com", false).await;
        let viewer = user(&control, "grace@example.com", false).await;
        let project = project(&control, &admin).await;
        let production =
            control.create_environment(&admin, project.id, new("Production", EnvironmentKind::Production)).await.unwrap();
        let staging = control.create_environment(&admin, project.id, new("Staging", EnvironmentKind::Staging)).await.unwrap();
        grant(&control, &developer, MembershipScope::Project(project.id), Role::Viewer).await;
        grant(&control, &developer, MembershipScope::Environment(staging.id), Role::Developer).await;
        grant(&control, &viewer, MembershipScope::Environment(staging.id), Role::Viewer).await;

        assert_eq!(control.environment_role(&admin, &production).await.unwrap(), Role::Admin);
        assert_eq!(control.project_role(&admin, &project).await.unwrap(), Some(Role::Admin));
        assert_eq!(control.project_role(&developer, &project).await.unwrap(), Some(Role::Viewer));
        assert_eq!(control.project_role(&viewer, &project).await.unwrap(), None, "sees it only through staging");
        assert_eq!(control.environment_role(&developer, &production).await.unwrap(), Role::Viewer);
        assert_eq!(control.environment_role(&developer, &staging).await.unwrap(), Role::Developer);
        let hidden = control.environment_role(&viewer, &production).await;
        assert!(matches!(hidden, Err(ControlError::NotFound { .. })), "{hidden:?}");
    }

    #[tokio::test]
    async fn environment_members_see_only_their_environments() {
        let (_dir, control) = control().await;
        let admin = user(&control, "admin@example.com", true).await;
        let contractor = user(&control, "ada@example.com", false).await;
        let project = project(&control, &admin).await;
        let production =
            control.create_environment(&admin, project.id, new("Production", EnvironmentKind::Production)).await.unwrap();
        let staging = control.create_environment(&admin, project.id, new("Staging", EnvironmentKind::Staging)).await.unwrap();
        grant(&control, &contractor, MembershipScope::Environment(staging.id), Role::Admin).await;

        let listed = control.list_environments(&contractor, project.id, None, 50).await.unwrap();
        let all = control.list_environments(&admin, project.id, None, 1).await.unwrap();
        let hidden = control.get_environment(&contractor, production.id).await;
        let renamed = control.update_environment(
            &contractor,
            staging.id,
            1,
            EnvironmentChanges { name: Some("QA".to_string()), ..Default::default() },
        );
        let renamed = renamed.await.unwrap();
        let delete = control.delete_environment(&contractor, staging.id).await;
        let delete_hidden = control.delete_environment(&contractor, production.id).await;

        assert_eq!(listed.items.iter().map(|environment| environment.id).collect::<Vec<_>>(), [staging.id]);
        assert_eq!((all.items.len(), all.has_more), (1, true));
        assert!(matches!(hidden, Err(ControlError::NotFound { entity: "environment", .. })), "{hidden:?}");
        assert_eq!((renamed.name.as_str(), renamed.slug.as_str()), ("QA", "staging"));
        assert!(matches!(delete, Err(ControlError::Forbidden)), "{delete:?}");
        assert!(matches!(delete_hidden, Err(ControlError::NotFound { .. })), "{delete_hidden:?}");
    }

    #[tokio::test]
    async fn strangers_cannot_see_a_project() {
        let (_dir, control) = control().await;
        let admin = user(&control, "admin@example.com", true).await;
        let stranger = user(&control, "eve@example.com", false).await;
        let project = project(&control, &admin).await;

        let listed = control.list_environments(&stranger, project.id, None, 50).await;
        let created = control.create_environment(&stranger, project.id, new("Staging", EnvironmentKind::Staging)).await;

        assert!(matches!(listed, Err(ControlError::NotFound { entity: "project", .. })), "{listed:?}");
        assert!(matches!(created, Err(ControlError::NotFound { entity: "project", .. })), "{created:?}");
    }
}
