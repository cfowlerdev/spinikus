//! API tokens for the CLI and automation, and web sessions.
//!
//! Both are random secrets handed to the client once and stored only as a SHA-256 hash.
//! They are long and random, so a fast hash is enough: there is nothing to brute-force.

use jiff::Timestamp;
use serde::Serialize;

use crate::{
    id::{ApiTokenId, EnvironmentId, ProjectId, SessionId, UserId},
    membership::{MembershipScope, Role},
};

/// Prefix of every API token, so secret scanners and people can recognize a leaked one.
pub const API_TOKEN_PREFIX: &str = "spk_";

/// A user's API token. It acts as its user, limited to its role and scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ApiToken {
    pub id: ApiTokenId,
    pub user_id: UserId,
    /// What the user calls it, such as "GitHub Actions".
    pub name: String,
    /// The most the token may do; it never exceeds what its user may do.
    pub role: Role,
    /// Limits the token to one project or environment; `None` means wherever its user has access.
    pub scope: Option<MembershipScope>,
    pub expires_at: Option<Timestamp>,
    /// Updated at most once a minute.
    pub last_used_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub revoked_at: Option<Timestamp>,
}

impl ApiToken {
    /// What the token may do in `environment` of `project`, given its user's role there (see
    /// `membership::effective_role`): the lower of the two, and nothing outside the token's scope.
    pub fn effective_role(
        &self,
        user_role: Option<Role>,
        project: ProjectId,
        environment: Option<EnvironmentId>,
    ) -> Option<Role> {
        let in_scope = match self.scope {
            None => true,
            Some(MembershipScope::Project(id)) => id == project,
            Some(MembershipScope::Environment(id)) => Some(id) == environment,
        };
        if !in_scope {
            return None;
        }
        user_role.map(|role| role.min(self.role))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewApiToken {
    pub user_id: UserId,
    pub name: String,
    pub role: Role,
    pub scope: Option<MembershipScope>,
    pub expires_at: Option<Timestamp>,
}

/// A signed-in web session, identified by a random cookie value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Session {
    pub id: SessionId,
    pub user_id: UserId,
    pub expires_at: Timestamp,
    /// Updated at most once a minute.
    pub last_seen_at: Timestamp,
    /// For the "active sessions" list, so users can recognize and end their sessions.
    pub user_agent: Option<String>,
    pub ip_address: Option<String>,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSession {
    pub user_id: UserId,
    pub expires_at: Timestamp,
    pub user_agent: Option<String>,
    pub ip_address: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(role: Role, scope: Option<MembershipScope>) -> ApiToken {
        ApiToken {
            id: ApiTokenId::new(),
            user_id: UserId::new(),
            name: "CI".to_string(),
            role,
            scope,
            expires_at: None,
            last_used_at: None,
            created_at: Timestamp::UNIX_EPOCH,
            revoked_at: None,
        }
    }

    #[test]
    fn never_exceeds_its_user_or_its_own_role() {
        let (project, staging) = (ProjectId::new(), EnvironmentId::new());
        let developer_token = token(Role::Developer, None);

        assert_eq!(developer_token.effective_role(Some(Role::Admin), project, Some(staging)), Some(Role::Developer));
        assert_eq!(developer_token.effective_role(Some(Role::Viewer), project, Some(staging)), Some(Role::Viewer));
        assert_eq!(developer_token.effective_role(None, project, Some(staging)), None);
    }

    #[test]
    fn stays_inside_its_scope() {
        let (project, staging, production) = (ProjectId::new(), EnvironmentId::new(), EnvironmentId::new());
        let project_token = token(Role::Admin, Some(MembershipScope::Project(project)));
        let staging_token = token(Role::Admin, Some(MembershipScope::Environment(staging)));

        assert_eq!(project_token.effective_role(Some(Role::Admin), project, Some(production)), Some(Role::Admin));
        assert_eq!(project_token.effective_role(Some(Role::Admin), ProjectId::new(), None), None);
        assert_eq!(staging_token.effective_role(Some(Role::Admin), project, Some(staging)), Some(Role::Admin));
        assert_eq!(staging_token.effective_role(Some(Role::Admin), project, Some(production)), None);
        assert_eq!(staging_token.effective_role(Some(Role::Admin), project, None), None);
    }
}
