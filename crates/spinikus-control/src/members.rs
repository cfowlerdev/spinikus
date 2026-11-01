//! Who has a role in a project or environment. A scope's admins manage its members (instance
//! admins are admins everywhere); members are added by email, so the user must exist first.

use serde_json::json;
use spinikus_core::{
    id::{MembershipId, ProjectId},
    membership::{Membership, MembershipScope, Role},
    user::{Email, User},
};
use spinikus_store::{EnvironmentRepository, MembershipRepository, ProjectRepository, StoreError, UserRepository};

use crate::{Control, ControlError, Principal, access::not_found};

/// A membership with its user, for listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub membership: Membership,
    pub user: User,
}

impl Control {
    /// Checks that the principal is an admin of `scope`, and returns its project.
    async fn scope_as_admin(&self, principal: &Principal, scope: MembershipScope) -> Result<ProjectId, ControlError> {
        let access = self.access(principal).await?;
        match scope {
            MembershipScope::Project(id) => {
                let project = self.store().get_project(id).await?;
                access.check_project(&project, Role::Admin)?;
                Ok(project.id)
            }
            MembershipScope::Environment(id) => {
                let environment = self.store().get_environment(id).await?;
                access.check_environment(&environment, Role::Admin)?;
                Ok(environment.project_id)
            }
        }
    }

    /// The scope's own members (not those it inherits from its project), with their users.
    pub async fn list_members(&self, principal: &Principal, scope: MembershipScope) -> Result<Vec<Member>, ControlError> {
        self.scope_as_admin(principal, scope).await?;
        let mut members = Vec::new();
        for membership in self.store().list_memberships(scope).await? {
            let user = self.store().get_user(membership.user_id).await?;
            members.push(Member { membership, user });
        }
        Ok(members)
    }

    /// Gives the user with `email` a role in the scope.
    pub async fn add_member(
        &self,
        principal: &Principal,
        scope: MembershipScope,
        email: &str,
        role: Role,
    ) -> Result<Member, ControlError> {
        let project_id = self.scope_as_admin(principal, scope).await?;
        let invalid = |problem: &str| ControlError::Invalid { field: "email", problem: problem.to_string() };
        let email: Email = email.parse().map_err(|_| invalid("it is not an email address"))?;
        let user = self.store().find_user_by_email(&email).await?.ok_or_else(|| invalid("no user has this email"))?;
        if !user.is_active() {
            return Err(invalid("this user is disabled"));
        }
        let membership = match self.store().create_membership(user.id, scope, role).await {
            Ok(membership) => membership,
            Err(StoreError::Conflict { .. }) => {
                return Err(ControlError::Rejected {
                    entity: "user",
                    id: user.id.to_string(),
                    problem: "they are already a member here; change their role instead".to_string(),
                });
            }
            Err(error) => return Err(error.into()),
        };
        let details = json!({ "user_id": user.id, "role": role, "scope": scope });
        self.audit_by(principal, "membership.create", membership.id, Some(project_id), details).await?;
        Ok(Member { membership, user })
    }

    /// The membership, if the principal is an admin of its scope; `NotFound` if they can't see it.
    async fn membership_as_admin(
        &self,
        principal: &Principal,
        id: MembershipId,
    ) -> Result<(Membership, ProjectId), ControlError> {
        let membership = self.store().get_membership(id).await?;
        match self.scope_as_admin(principal, membership.scope).await {
            Ok(project_id) => Ok((membership, project_id)),
            Err(ControlError::NotFound { .. }) => Err(not_found("membership", id)),
            Err(error) => Err(error),
        }
    }

    pub async fn update_member(
        &self,
        principal: &Principal,
        id: MembershipId,
        revision: i64,
        role: Role,
    ) -> Result<Membership, ControlError> {
        let (_, project_id) = self.membership_as_admin(principal, id).await?;
        let membership = self.store().update_membership(id, revision, role).await?;
        self.audit_by(principal, "membership.update", id, Some(project_id), json!({ "role": role })).await?;
        Ok(membership)
    }

    pub async fn remove_member(&self, principal: &Principal, id: MembershipId) -> Result<(), ControlError> {
        let (membership, project_id) = self.membership_as_admin(principal, id).await?;
        self.store().delete_membership(id).await?;
        let details = json!({ "user_id": membership.user_id, "scope": membership.scope });
        self.audit_by(principal, "membership.delete", id, Some(project_id), details).await
    }
}

#[cfg(test)]
mod tests {
    use spinikus_core::{
        environment::{EnvironmentFields, EnvironmentKind},
        project::ProjectFields,
    };
    use spinikus_store::AuditLogRepository;

    use super::*;
    use crate::test_support::{control, grant, user};

    #[tokio::test]
    async fn scope_admins_manage_members_by_email() {
        let (_dir, control) = control().await;
        let owner = user(&control, "owner@example.com", false).await;
        let dev = user(&control, "dev@example.com", false).await;
        let grace = user(&control, "grace@example.com", false).await;
        let shop =
            control.store().create_project(ProjectFields { name: "Shop".into(), slug: "shop".parse().unwrap() }).await.unwrap();
        let staging = control
            .store()
            .create_environment(
                shop.id,
                EnvironmentKind::Staging,
                EnvironmentFields { name: "Staging".into(), slug: "staging".parse().unwrap() },
            )
            .await
            .unwrap();
        grant(&control, &owner, MembershipScope::Project(shop.id), Role::Admin).await;
        let project = MembershipScope::Project(shop.id);

        let added = control.add_member(&owner, project, "Dev@Example.com", Role::Developer).await.unwrap();
        let again = control.add_member(&owner, project, "dev@example.com", Role::Viewer).await;
        let nobody = control.add_member(&owner, project, "nobody@example.com", Role::Viewer).await;
        let by_dev = control.add_member(&dev, project, "grace@example.com", Role::Viewer).await;
        // A project admin is an admin of its environments too.
        let in_staging =
            control.add_member(&owner, MembershipScope::Environment(staging.id), "grace@example.com", Role::Viewer).await;

        assert_eq!((added.user.id, added.membership.role), (dev.user.id, Role::Developer));
        assert!(matches!(again, Err(ControlError::Rejected { .. })), "{again:?}");
        assert!(matches!(nobody, Err(ControlError::Invalid { field: "email", .. })), "{nobody:?}");
        assert!(matches!(by_dev, Err(ControlError::Forbidden)), "{by_dev:?}");
        assert!(in_staging.is_ok(), "{in_staging:?}");
        let members: Vec<_> =
            control.list_members(&owner, project).await.unwrap().into_iter().map(|member| member.user.id).collect();
        assert_eq!(members, [owner.user.id, dev.user.id]);
        assert!(matches!(control.list_members(&grace, project).await, Err(ControlError::Forbidden)), "grace is only in staging");

        let membership = added.membership;
        let promoted = control.update_member(&owner, membership.id, membership.revision, Role::Admin).await.unwrap();
        assert_eq!(promoted.role, Role::Admin);
        assert!(matches!(control.remove_member(&grace, membership.id).await, Err(ControlError::Forbidden)));
        control.remove_member(&owner, membership.id).await.unwrap();
        assert_eq!(control.list_members(&owner, project).await.unwrap().len(), 1);
        let audit = control.store().list_audit_entries(Some(shop.id), None, None, 10).await.unwrap();
        let actions: Vec<_> = audit.iter().map(|entry| entry.action.as_str()).collect();
        assert_eq!(actions, ["membership.delete", "membership.update", "membership.create", "membership.create"]);
    }
}
