//! Projects. Instance admins create them; a project's admins manage it.

use serde_json::json;
use spinikus_core::{
    id::ProjectId,
    membership::Role,
    project::{Project, ProjectFields},
};
use spinikus_store::{ProjectRepository, StoreError};

use crate::{
    Control, ControlError, Principal,
    access::not_found,
    audit, input,
    page::{self, Page},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProject {
    pub name: String,
    /// Made from the name when not given.
    pub slug: Option<String>,
}

/// Changes to a project. Fields left as `None` keep their current value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectChanges {
    pub name: Option<String>,
    pub slug: Option<String>,
}

impl Control {
    pub async fn create_project(&self, principal: &Principal, new: NewProject) -> Result<Project, ControlError> {
        if !principal.is_instance_admin() {
            return Err(ControlError::Forbidden);
        }
        let name = input::name(&new.name)?;
        let slug = input::slug(new.slug.as_deref(), &name)?;
        let project = self.store().create_project(ProjectFields { name, slug }).await?;
        self.audit_by(principal, "project.create", project.id, Some(project.id), json!({ "slug": project.slug })).await?;
        Ok(project)
    }

    /// The projects the principal has a role in, or in one of whose environments they have a role.
    pub async fn list_projects(
        &self,
        principal: &Principal,
        after: Option<ProjectId>,
        limit: u32,
    ) -> Result<Page<Project>, ControlError> {
        let limit = page::limit(limit);
        let items = match self.access(principal).await?.visible_projects() {
            None => self.store().list_projects(after, limit + 1).await?,
            Some(ids) => {
                let mut items = Vec::new();
                for id in ids.into_iter().filter(|id| after.is_none_or(|after| *id > after)) {
                    if items.len() > limit as usize {
                        break;
                    }
                    match self.store().get_project(id).await {
                        Ok(project) => items.push(project),
                        Err(StoreError::NotFound { .. }) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                items
            }
        };
        Ok(Page::from_overfetch(items, limit))
    }

    pub async fn get_project(&self, principal: &Principal, id: ProjectId) -> Result<Project, ControlError> {
        let project = self.store().get_project(id).await?;
        if !self.access(principal).await?.can_see_project(id) {
            return Err(not_found("project", id));
        }
        Ok(project)
    }

    /// The principal's role in the project itself, so a client can offer only what they may
    /// do (each operation still checks): `None` for someone who sees the project only
    /// through a role in one of its environments.
    pub async fn project_role(&self, principal: &Principal, project: &Project) -> Result<Option<Role>, ControlError> {
        let access = self.access(principal).await?;
        if !access.can_see_project(project.id) {
            return Err(not_found("project", project.id));
        }
        Ok(access.role_in(project.id, None))
    }

    /// Applies `changes` if the project is still at `revision`. Needs the admin role in the project.
    pub async fn update_project(
        &self,
        principal: &Principal,
        id: ProjectId,
        revision: i64,
        changes: ProjectChanges,
    ) -> Result<Project, ControlError> {
        let current = self.store().get_project(id).await?;
        self.access(principal).await?.check_project(&current, Role::Admin)?;
        let name = match &changes.name {
            Some(name) => input::name(name)?,
            None => current.name,
        };
        let slug = match changes.slug.as_deref() {
            Some(slug) => input::slug(Some(slug), &name)?,
            None => current.slug,
        };
        let project = self.store().update_project(id, revision, ProjectFields { name, slug }).await?;
        let changed = audit::changed(&[("name", changes.name.is_some()), ("slug", changes.slug.is_some())]);
        self.audit_by(principal, "project.update", id, Some(id), changed).await?;
        Ok(project)
    }

    /// Deletes the project. Needs the admin role in it, and fails while it has environments.
    pub async fn delete_project(&self, principal: &Principal, id: ProjectId) -> Result<(), ControlError> {
        let project = self.store().get_project(id).await?;
        self.access(principal).await?.check_project(&project, Role::Admin)?;
        self.store().delete_project(id).await?;
        self.audit_by(principal, "project.delete", id, Some(id), json!({ "slug": project.slug })).await
    }
}

#[cfg(test)]
mod tests {
    use spinikus_core::{environment::EnvironmentKind, membership::MembershipScope};
    use spinikus_store::AuditLogRepository;

    use super::*;
    use crate::{
        NewEnvironment,
        test_support::{control, grant, user, with_token},
    };

    fn new(name: &str) -> NewProject {
        NewProject { name: name.to_string(), slug: None }
    }

    #[tokio::test]
    async fn only_instance_admins_create_projects() {
        let (_dir, control) = control().await;
        let admin = user(&control, "admin@example.com", true).await;
        let member = user(&control, "ada@example.com", false).await;

        let project = control.create_project(&admin, new(" Web Shop ")).await.unwrap();
        let by_member = control.create_project(&member, new("Other")).await;
        let by_viewer_token = control.create_project(&with_token(&control, &admin, Role::Viewer, None).await, new("Other")).await;

        assert_eq!((project.name.as_str(), project.slug.as_str()), ("Web Shop", "web-shop"));
        assert!(matches!(by_member, Err(ControlError::Forbidden)), "{by_member:?}");
        assert!(matches!(by_viewer_token, Err(ControlError::Forbidden)), "{by_viewer_token:?}");
        let audit = control.store().list_audit_entries(None, None, None, 10).await.unwrap();
        assert_eq!(audit[0].action.as_str(), "project.create");
        assert_eq!(audit[0].project_id, Some(project.id));
    }

    #[tokio::test]
    async fn members_see_only_their_projects() {
        let (_dir, control) = control().await;
        let admin = user(&control, "admin@example.com", true).await;
        let member = user(&control, "ada@example.com", false).await;
        let mut projects = Vec::new();
        for name in ["one", "two", "three", "four"] {
            projects.push(control.create_project(&admin, new(name)).await.unwrap());
        }
        let staging = NewEnvironment { name: "Staging".to_string(), slug: None, kind: EnvironmentKind::Staging };
        let environment = control.create_environment(&admin, projects[2].id, staging).await.unwrap();
        grant(&control, &member, MembershipScope::Project(projects[0].id), Role::Viewer).await;
        grant(&control, &member, MembershipScope::Project(projects[1].id), Role::Viewer).await;
        grant(&control, &member, MembershipScope::Environment(environment.id), Role::Developer).await;

        let all = control.list_projects(&admin, None, 50).await.unwrap();
        let first = control.list_projects(&member, None, 2).await.unwrap();
        let rest = control.list_projects(&member, Some(first.items[1].id), 2).await.unwrap();
        let hidden = control.get_project(&member, projects[3].id).await;

        assert_eq!(all.items.len(), 4);
        assert!(!all.has_more);
        assert_eq!(first.items.iter().map(|project| project.id).collect::<Vec<_>>(), [projects[0].id, projects[1].id]);
        assert!(first.has_more);
        assert_eq!(rest.items.iter().map(|project| project.id).collect::<Vec<_>>(), [projects[2].id]);
        assert!(!rest.has_more);
        assert!(matches!(hidden, Err(ControlError::NotFound { entity: "project", .. })), "{hidden:?}");
        control.get_project(&member, projects[2].id).await.unwrap();
    }

    #[tokio::test]
    async fn project_admins_update_and_delete() {
        let (_dir, control) = control().await;
        let admin = user(&control, "admin@example.com", true).await;
        let owner = user(&control, "ada@example.com", false).await;
        let viewer = user(&control, "bob@example.com", false).await;
        let project = control.create_project(&admin, new("Shop")).await.unwrap();
        grant(&control, &owner, MembershipScope::Project(project.id), Role::Admin).await;
        grant(&control, &viewer, MembershipScope::Project(project.id), Role::Viewer).await;
        let rename = ProjectChanges { name: Some("Web Shop".to_string()), slug: None };

        let by_viewer = control.update_project(&viewer, project.id, 1, rename.clone()).await;
        let renamed = control.update_project(&owner, project.id, 1, rename.clone()).await.unwrap();
        let stale = control.update_project(&owner, project.id, 1, rename).await;
        let bad_slug = ProjectChanges { slug: Some("Web Shop".to_string()), ..Default::default() };
        let invalid = control.update_project(&owner, project.id, 2, bad_slug).await;

        assert!(matches!(by_viewer, Err(ControlError::Forbidden)), "{by_viewer:?}");
        assert_eq!((renamed.name.as_str(), renamed.slug.as_str(), renamed.revision), ("Web Shop", "shop", 2));
        assert!(matches!(stale, Err(ControlError::RevisionMismatch { actual: 2, .. })), "{stale:?}");
        assert!(matches!(invalid, Err(ControlError::Invalid { field: "slug", .. })), "{invalid:?}");
        let audit = control.store().list_audit_entries(None, None, None, 10).await.unwrap();
        assert_eq!(audit[0].action.as_str(), "project.update");
        assert_eq!(audit[0].details["changed"], serde_json::json!(["name"]));

        let environment = NewEnvironment { name: "Production".to_string(), slug: None, kind: EnvironmentKind::Production };
        let environment = control.create_environment(&owner, project.id, environment).await.unwrap();
        let with_children = control.delete_project(&owner, project.id).await;
        control.delete_environment(&owner, environment.id).await.unwrap();
        let by_viewer = control.delete_project(&viewer, project.id).await;
        control.delete_project(&owner, project.id).await.unwrap();

        assert!(matches!(with_children, Err(ControlError::HasChildren { .. })), "{with_children:?}");
        assert!(matches!(by_viewer, Err(ControlError::Forbidden)), "{by_viewer:?}");
        assert!(matches!(control.get_project(&admin, project.id).await, Err(ControlError::NotFound { .. })));
    }
}
