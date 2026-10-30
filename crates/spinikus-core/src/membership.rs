//! Memberships: a user's role in a project or in one of its environments.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::{
    id::{EnvironmentId, MembershipId, ProjectId, UserId},
    text::impl_text,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Membership {
    pub id: MembershipId,
    pub user_id: UserId,
    #[serde(flatten)]
    pub scope: MembershipScope,
    pub role: Role,
    pub revision: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// What a membership grants access to. A user has at most one membership per scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "scope", content = "scope_id", rename_all = "snake_case")]
pub enum MembershipScope {
    /// The project and all of its environments.
    Project(ProjectId),
    /// One environment only, such as giving a contractor access to staging.
    Environment(EnvironmentId),
}

/// Roles, ordered from least to most access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Reads everything except secret values.
    Viewer,
    /// Also deploys, rolls back, and edits services, variables, domains and volumes.
    Developer,
    /// Also manages the scope itself and its members, and reveals secrets.
    Admin,
}

impl Role {
    pub const ALL: [Role; 3] = [Self::Viewer, Self::Developer, Self::Admin];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Developer => "developer",
            Self::Admin => "admin",
        }
    }
}

impl_text!(Role, "role");

/// The role a user has in `environment` of `project`, from their memberships: the higher of
/// their project role and their role in that environment. An environment membership can
/// only add access, never take away what the project membership gives. Instance admins
/// have every role regardless; callers check that first.
pub fn effective_role(memberships: &[Membership], project: ProjectId, environment: Option<EnvironmentId>) -> Option<Role> {
    memberships
        .iter()
        .filter(|membership| match membership.scope {
            MembershipScope::Project(id) => id == project,
            MembershipScope::Environment(id) => Some(id) == environment,
        })
        .map(|membership| membership.role)
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn membership(scope: MembershipScope, role: Role) -> Membership {
        Membership {
            id: MembershipId::new(),
            user_id: UserId::new(),
            scope,
            role,
            revision: 1,
            created_at: Timestamp::UNIX_EPOCH,
            updated_at: Timestamp::UNIX_EPOCH,
        }
    }

    #[test]
    fn the_higher_of_project_and_environment_roles_wins() {
        let (project, other_project) = (ProjectId::new(), ProjectId::new());
        let (staging, production) = (EnvironmentId::new(), EnvironmentId::new());
        let memberships = [
            membership(MembershipScope::Project(project), Role::Viewer),
            membership(MembershipScope::Environment(staging), Role::Developer),
            membership(MembershipScope::Project(other_project), Role::Admin),
        ];

        assert_eq!(effective_role(&memberships, project, Some(staging)), Some(Role::Developer));
        assert_eq!(effective_role(&memberships, project, Some(production)), Some(Role::Viewer));
        assert_eq!(effective_role(&memberships, project, None), Some(Role::Viewer));
        assert_eq!(effective_role(&memberships, other_project, Some(staging)), Some(Role::Admin));
        assert_eq!(effective_role(&memberships, ProjectId::new(), None), None);
    }

    #[test]
    fn an_environment_membership_alone_gives_no_project_wide_role() {
        let staging = EnvironmentId::new();
        let memberships = [membership(MembershipScope::Environment(staging), Role::Admin)];

        assert_eq!(effective_role(&memberships, ProjectId::new(), Some(staging)), Some(Role::Admin));
        assert_eq!(effective_role(&memberships, ProjectId::new(), None), None);
    }

    #[test]
    fn roles_are_ordered_and_round_trip() {
        assert!(Role::Viewer < Role::Developer && Role::Developer < Role::Admin);
        for role in Role::ALL {
            assert_eq!(role.to_string().parse(), Ok(role));
            assert_eq!(serde_json::to_string(&role).unwrap(), format!("\"{role}\""));
        }
    }
}
