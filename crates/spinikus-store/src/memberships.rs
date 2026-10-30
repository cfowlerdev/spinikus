use async_trait::async_trait;
use spinikus_core::{
    id::{MembershipId, UserId},
    membership::{Membership, MembershipScope, Role},
};

use crate::{EnvironmentRepository, ProjectRepository, SqliteStore, StoreError, UserRepository, error::parse_column};

const ENTITY: &str = "membership";

#[async_trait]
pub trait MembershipRepository: Send + Sync {
    /// Fails with `NotFound` if the user does not exist or the project or environment does not
    /// exist or is deleted, and `Conflict` if the user already has a membership in the scope.
    async fn create_membership(&self, user_id: UserId, scope: MembershipScope, role: Role) -> Result<Membership, StoreError>;

    async fn get_membership(&self, id: MembershipId) -> Result<Membership, StoreError>;

    /// The members of a project or environment (not inherited ones), in creation order.
    async fn list_memberships(&self, scope: MembershipScope) -> Result<Vec<Membership>, StoreError>;

    /// All of a user's memberships, for working out their role with `membership::effective_role`.
    async fn list_user_memberships(&self, user_id: UserId) -> Result<Vec<Membership>, StoreError>;

    /// Changes the role if the membership is still at `revision`.
    async fn update_membership(&self, id: MembershipId, revision: i64, role: Role) -> Result<Membership, StoreError>;

    async fn delete_membership(&self, id: MembershipId) -> Result<(), StoreError>;
}

macro_rules! columns {
    () => {
        "id, user_id, project_id, environment_id, role, revision, created_at, updated_at"
    };
}

/// Inserts a membership only if its project or environment (in `$parent`) exists and is not deleted.
macro_rules! insert_under {
    ($parent:literal) => {
        concat!(
            "INSERT INTO memberships (id, user_id, project_id, environment_id, role)
             SELECT ?, ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM ",
            $parent,
            " WHERE id = ? AND deleted_at IS NULL)
             RETURNING ",
            columns!()
        )
    };
}

macro_rules! select_by {
    ($column:literal) => {
        concat!("SELECT ", columns!(), " FROM memberships WHERE ", $column, " = ? ORDER BY id")
    };
}

#[derive(sqlx::FromRow)]
struct MembershipRow {
    id: String,
    user_id: String,
    project_id: Option<String>,
    environment_id: Option<String>,
    role: String,
    revision: i64,
    created_at: String,
    updated_at: String,
}

impl TryFrom<MembershipRow> for Membership {
    type Error = StoreError;

    fn try_from(row: MembershipRow) -> Result<Self, Self::Error> {
        let scope = match (&row.project_id, &row.environment_id) {
            (Some(id), None) => MembershipScope::Project(parse_column(ENTITY, "project_id", id)?),
            (None, Some(id)) => MembershipScope::Environment(parse_column(ENTITY, "environment_id", id)?),
            _ => {
                return Err(StoreError::InvalidRow {
                    entity: ENTITY,
                    column: "scope",
                    problem: "not exactly one scope".to_string(),
                });
            }
        };
        Ok(Membership {
            id: parse_column(ENTITY, "id", &row.id)?,
            user_id: parse_column(ENTITY, "user_id", &row.user_id)?,
            scope,
            role: parse_column(ENTITY, "role", &row.role)?,
            revision: row.revision,
            created_at: parse_column(ENTITY, "created_at", &row.created_at)?,
            updated_at: parse_column(ENTITY, "updated_at", &row.updated_at)?,
        })
    }
}

/// The scope as `(project_id, environment_id)` columns, one of them set, and the one that is.
fn scope_columns(scope: MembershipScope) -> (Option<String>, Option<String>, String) {
    match scope {
        MembershipScope::Project(id) => (Some(id.to_string()), None, id.to_string()),
        MembershipScope::Environment(id) => (None, Some(id.to_string()), id.to_string()),
    }
}

fn not_found(id: MembershipId) -> StoreError {
    StoreError::NotFound { entity: ENTITY, id: id.to_string() }
}

impl SqliteStore {
    /// Fails with `NotFound` unless the scope's project or environment exists and is not deleted.
    pub(crate) async fn check_membership_scope(&self, scope: MembershipScope) -> Result<(), StoreError> {
        match scope {
            MembershipScope::Project(id) => self.get_project(id).await.map(drop),
            MembershipScope::Environment(id) => self.get_environment(id).await.map(drop),
        }
    }
}

#[async_trait]
impl MembershipRepository for SqliteStore {
    async fn create_membership(&self, user_id: UserId, scope: MembershipScope, role: Role) -> Result<Membership, StoreError> {
        let query = match scope {
            MembershipScope::Project(_) => insert_under!("projects"),
            MembershipScope::Environment(_) => insert_under!("environments"),
        };
        let (project_id, environment_id, parent) = scope_columns(scope);

        let result: Result<Option<MembershipRow>, sqlx::Error> = sqlx::query_as(query)
            .bind(MembershipId::new().to_string())
            .bind(user_id.to_string())
            .bind(project_id)
            .bind(environment_id)
            .bind(role.as_str())
            .bind(&parent)
            .fetch_optional(self.pool())
            .await;

        match result {
            Ok(Some(row)) => row.try_into(),
            Ok(None) => {
                let entity = match scope {
                    MembershipScope::Project(_) => "project",
                    MembershipScope::Environment(_) => "environment",
                };
                Err(StoreError::NotFound { entity, id: parent })
            }
            Err(error) if error.as_database_error().is_some_and(|db| db.is_foreign_key_violation()) => {
                // The scope was checked by the insert, so the user is what is missing.
                Err(StoreError::NotFound { entity: "user", id: user_id.to_string() })
            }
            Err(error) => Err(StoreError::on_unique_violation(error, |_| StoreError::Conflict {
                entity: ENTITY,
                field: "scope",
                value: format!("user {user_id} is already a member"),
            })),
        }
    }

    async fn get_membership(&self, id: MembershipId) -> Result<Membership, StoreError> {
        let row: Option<MembershipRow> = sqlx::query_as(concat!("SELECT ", columns!(), " FROM memberships WHERE id = ?"))
            .bind(id.to_string())
            .fetch_optional(self.pool())
            .await?;
        row.ok_or_else(|| not_found(id))?.try_into()
    }

    async fn list_memberships(&self, scope: MembershipScope) -> Result<Vec<Membership>, StoreError> {
        let query = match scope {
            MembershipScope::Project(_) => select_by!("project_id"),
            MembershipScope::Environment(_) => select_by!("environment_id"),
        };
        let (_, _, parent_id) = scope_columns(scope);
        let rows: Vec<MembershipRow> = sqlx::query_as(query).bind(parent_id).fetch_all(self.pool()).await?;

        if rows.is_empty() {
            // Tell an empty list apart from a missing or deleted scope.
            self.check_membership_scope(scope).await?;
        }
        rows.into_iter().map(Membership::try_from).collect()
    }

    async fn list_user_memberships(&self, user_id: UserId) -> Result<Vec<Membership>, StoreError> {
        let rows: Vec<MembershipRow> =
            sqlx::query_as(select_by!("user_id")).bind(user_id.to_string()).fetch_all(self.pool()).await?;

        if rows.is_empty() {
            // Tell an empty list apart from a missing user.
            self.get_user(user_id).await?;
        }
        rows.into_iter().map(Membership::try_from).collect()
    }

    async fn update_membership(&self, id: MembershipId, revision: i64, role: Role) -> Result<Membership, StoreError> {
        let row: Option<MembershipRow> = sqlx::query_as(concat!(
            "UPDATE memberships
             SET role = ?, revision = revision + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ? AND revision = ?
             RETURNING ",
            columns!()
        ))
        .bind(role.as_str())
        .bind(id.to_string())
        .bind(revision)
        .fetch_optional(self.pool())
        .await?;

        match row {
            Some(row) => row.try_into(),
            // Nothing matched: either the membership is gone or it has moved on to a newer revision.
            None => {
                let current = self.get_membership(id).await?;
                Err(StoreError::RevisionMismatch {
                    entity: ENTITY,
                    id: id.to_string(),
                    expected: revision,
                    actual: current.revision,
                })
            }
        }
    }

    async fn delete_membership(&self, id: MembershipId) -> Result<(), StoreError> {
        let result = sqlx::query("DELETE FROM memberships WHERE id = ?").bind(id.to_string()).execute(self.pool()).await?;

        if result.rows_affected() == 0 {
            return Err(not_found(id));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use spinikus_core::{
        environment::{EnvironmentFields, EnvironmentKind},
        id::{EnvironmentId, ProjectId},
        membership::effective_role,
        project::ProjectFields,
        user::UserFields,
    };

    use super::*;
    use crate::test_support::store;

    async fn setup(store: &SqliteStore) -> (UserId, ProjectId, EnvironmentId) {
        let user = UserFields { email: "ada@example.com".parse().unwrap(), name: "Ada".to_string(), admin: false };
        let user_id = store.create_user(user, None).await.unwrap().id;
        let project = ProjectFields { name: "Web".to_string(), slug: "web".parse().unwrap() };
        let project_id = store.create_project(project).await.unwrap().id;
        let env = EnvironmentFields { name: "Staging".to_string(), slug: "staging".parse().unwrap() };
        let env_id = store.create_environment(project_id, EnvironmentKind::Staging, env).await.unwrap().id;
        (user_id, project_id, env_id)
    }

    #[tokio::test]
    async fn creates_and_lists_memberships() {
        let (_dir, store) = store().await;
        let (user, project, staging) = setup(&store).await;

        let in_project = store.create_membership(user, MembershipScope::Project(project), Role::Viewer).await.unwrap();
        let in_staging = store.create_membership(user, MembershipScope::Environment(staging), Role::Developer).await.unwrap();

        assert_eq!(store.get_membership(in_project.id).await.unwrap(), in_project);
        assert_eq!(store.list_memberships(MembershipScope::Project(project)).await.unwrap(), vec![in_project.clone()]);
        assert_eq!(store.list_memberships(MembershipScope::Environment(staging)).await.unwrap(), vec![in_staging.clone()]);
        let mine = store.list_user_memberships(user).await.unwrap();
        assert_eq!(mine, vec![in_project, in_staging]);
        assert_eq!(effective_role(&mine, project, Some(staging)), Some(Role::Developer));
    }

    #[tokio::test]
    async fn one_membership_per_user_and_scope() {
        let (_dir, store) = store().await;
        let (user, project, _) = setup(&store).await;
        store.create_membership(user, MembershipScope::Project(project), Role::Viewer).await.unwrap();

        let duplicate = store.create_membership(user, MembershipScope::Project(project), Role::Admin).await;

        assert!(matches!(duplicate, Err(StoreError::Conflict { field: "scope", .. })), "{duplicate:?}");
    }

    #[tokio::test]
    async fn requires_an_existing_user_and_active_scope() {
        let (_dir, store) = store().await;
        let (user, project, staging) = setup(&store).await;
        store.delete_environment(staging).await.unwrap();

        let no_user = store.create_membership(UserId::new(), MembershipScope::Project(project), Role::Viewer).await;
        let deleted_env = store.create_membership(user, MembershipScope::Environment(staging), Role::Viewer).await;
        let no_project = store.create_membership(user, MembershipScope::Project(ProjectId::new()), Role::Viewer).await;
        let listed = store.list_user_memberships(UserId::new()).await;

        assert!(matches!(no_user, Err(StoreError::NotFound { entity: "user", .. })), "{no_user:?}");
        assert!(matches!(deleted_env, Err(StoreError::NotFound { entity: "environment", .. })), "{deleted_env:?}");
        assert!(matches!(no_project, Err(StoreError::NotFound { entity: "project", .. })), "{no_project:?}");
        assert!(matches!(listed, Err(StoreError::NotFound { entity: "user", .. })), "{listed:?}");
        assert!(store.list_user_memberships(user).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn changes_roles_at_the_current_revision_and_deletes() {
        let (_dir, store) = store().await;
        let (user, project, _) = setup(&store).await;
        let membership = store.create_membership(user, MembershipScope::Project(project), Role::Viewer).await.unwrap();

        let promoted = store.update_membership(membership.id, 1, Role::Admin).await.unwrap();
        let stale = store.update_membership(membership.id, 1, Role::Viewer).await;
        store.delete_membership(membership.id).await.unwrap();

        assert_eq!((promoted.role, promoted.revision), (Role::Admin, 2));
        assert!(matches!(stale, Err(StoreError::RevisionMismatch { expected: 1, actual: 2, .. })), "{stale:?}");
        assert!(matches!(store.get_membership(membership.id).await, Err(StoreError::NotFound { .. })));
        assert!(matches!(store.delete_membership(membership.id).await, Err(StoreError::NotFound { .. })));
    }
}
