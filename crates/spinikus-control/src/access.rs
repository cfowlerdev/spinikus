//! What a principal may see and do in projects and environments.
//!
//! Something the principal has no role in is reported as `NotFound`, so its existence is not
//! revealed. Something they can see but lack the role for is `Forbidden`.

use std::collections::BTreeSet;

use spinikus_core::{
    environment::Environment,
    id::{EnvironmentId, ProjectId},
    membership::{Membership, MembershipScope, Role},
    project::Project,
};
use spinikus_store::{EnvironmentRepository, MembershipRepository, StoreError};

use crate::{Control, ControlError, Credential, Principal};

/// A principal with their memberships loaded, for checking access to projects and environments.
pub(crate) struct Access<'a> {
    principal: &'a Principal,
    memberships: Vec<Membership>,
    /// Each scope the user has a membership in or the token is limited to, as a project
    /// and, for an environment scope, the environment.
    grants: Vec<(ProjectId, Option<EnvironmentId>)>,
    /// Instance admins see every project, unless their token is scoped.
    sees_everything: bool,
}

impl Control {
    pub(crate) async fn access<'a>(&self, principal: &'a Principal) -> Result<Access<'a>, ControlError> {
        let memberships = self.store().list_user_memberships(principal.user.id).await?;
        let token_scope = match &principal.credential {
            Credential::ApiToken(token) => token.scope,
            Credential::Session(_) => None,
        };
        let mut grants = Vec::new();
        for scope in memberships.iter().map(|membership| membership.scope).chain(token_scope) {
            match scope {
                MembershipScope::Project(project) => grants.push((project, None)),
                MembershipScope::Environment(environment) => match self.store().get_environment(environment).await {
                    Ok(found) => grants.push((found.project_id, Some(environment))),
                    Err(StoreError::NotFound { .. }) => {}
                    Err(error) => return Err(error.into()),
                },
            }
        }
        let sees_everything = principal.user.admin && token_scope.is_none();
        Ok(Access { principal, memberships, grants, sees_everything })
    }
}

impl Access<'_> {
    pub fn role_in(&self, project: ProjectId, environment: Option<EnvironmentId>) -> Option<Role> {
        self.principal.role_in(&self.memberships, project, environment)
    }

    /// The projects the principal can see, or `None` for all of them.
    pub fn visible_projects(&self) -> Option<BTreeSet<ProjectId>> {
        if self.sees_everything {
            return None;
        }
        let visible = self.grants.iter().filter(|(project, environment)| self.role_in(*project, *environment).is_some());
        Some(visible.map(|(project, _)| *project).collect())
    }

    /// The project's environments the principal can see, or `None` for all of them.
    pub fn visible_environments(&self, project: ProjectId) -> Option<BTreeSet<EnvironmentId>> {
        if self.role_in(project, None).is_some() {
            return None;
        }
        let visible = self.grants.iter().filter_map(|(granted, environment)| match environment {
            Some(environment) if *granted == project && self.role_in(project, Some(*environment)).is_some() => Some(*environment),
            _ => None,
        });
        Some(visible.collect())
    }

    /// Whether the principal has a role in the project or in any of its environments.
    pub fn can_see_project(&self, project: ProjectId) -> bool {
        self.visible_environments(project).is_none_or(|environments| !environments.is_empty())
    }

    pub fn check_project(&self, project: &Project, needed: Role) -> Result<(), ControlError> {
        match self.role_in(project.id, None) {
            Some(role) if role >= needed => Ok(()),
            _ if self.can_see_project(project.id) => Err(ControlError::Forbidden),
            _ => Err(not_found("project", project.id)),
        }
    }

    pub fn check_environment(&self, environment: &Environment, needed: Role) -> Result<(), ControlError> {
        match self.role_in(environment.project_id, Some(environment.id)) {
            Some(role) if role >= needed => Ok(()),
            Some(_) => Err(ControlError::Forbidden),
            None => Err(not_found("environment", environment.id)),
        }
    }
}

pub(crate) fn not_found(entity: &'static str, id: impl ToString) -> ControlError {
    ControlError::NotFound { entity, id: id.to_string() }
}
