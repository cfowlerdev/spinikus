use async_trait::async_trait;
use spinikus_core::{
    id::{ServiceId, VariableId},
    secret::Secret,
    variable::{self, ResolvedVariable, Variable, VariableFields, VariableScope}
};

use crate::{
    EnvironmentRepository, Keyring, ProjectRepository, Sealed, ServiceRepository, SqliteStore, StoreError,
    error::parse_column
};

const ENTITY: &str = "variable";

#[async_trait]
pub trait VariableRepository: Send + Sync {
    /// Fails with `NotFound` if the project, environment or service does not exist or is deleted.
    async fn create_variable(&self, scope: VariableScope, fields: VariableFields, value: Secret) -> Result<Variable, StoreError>;

    async fn get_variable(&self, id: VariableId) -> Result<Variable, StoreError>;

    /// The variables set on this scope itself (not inherited ones), by name.
    async fn list_variables(&self, scope: VariableScope) -> Result<Vec<Variable>, StoreError>;

    /// Replaces the variable's fields if it is still at `revision`. A `value` of `None` keeps the
    /// current value, since a secret's value cannot be read back to send it again.
    /// A secret cannot be made non-secret.
    async fn update_variable(
        &self,
        id: VariableId,
        revision: i64,
        fields: VariableFields,
        value: Option<Secret>
    ) -> Result<Variable, StoreError>;

    async fn delete_variable(&self, id: VariableId) -> Result<(), StoreError>;

    /// Decrypts a variable's value, secret or not. Callers check permissions and audit the reveal.
    async fn reveal_variable(&self, id: VariableId) -> Result<Secret, StoreError>;

    /// Every variable that applies to the service, from its project, environment and itself,
    /// with the most specific winning for each name. Sorted by name.
    async fn resolve_service_variables(&self, service_id: ServiceId) -> Result<Vec<ResolvedVariable>, StoreError>;
}

macro_rules! columns {
    () => {
        "id, project_id, environment_id, service_id, name, value, data_key_id, availability, secret, revision, created_at, updated_at"
    };
}

/// Inserts a variable only if its parent row (in `$parent`) exists and is not deleted.
macro_rules! insert_under {
    ($parent:literal) => {
        concat!(
            "INSERT INTO variables (id, project_id, environment_id, service_id, name, value, data_key_id, availability, secret)
             SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?
             WHERE EXISTS (SELECT 1 FROM ", $parent, " WHERE id = ? AND deleted_at IS NULL)
             RETURNING ",
            columns!()
        )
    };
}

macro_rules! select_by {
    ($column:literal) => {
        concat!("SELECT ", columns!(), " FROM variables WHERE ", $column, " = ? ORDER BY name")
    };
}

#[derive(sqlx::FromRow)]
struct VariableRow {
    id: String,
    project_id: Option<String>,
    environment_id: Option<String>,
    service_id: Option<String>,
    name: String,
    value: Vec<u8>,
    data_key_id: String,
    availability: String,
    secret: bool,
    revision: i64,
    created_at: String,
    updated_at: String
}

impl VariableRow {
    fn id(&self) -> Result<VariableId, StoreError> {
        parse_column(ENTITY, "id", &self.id)
    }

    fn scope(&self) -> Result<VariableScope, StoreError> {
        match (&self.project_id, &self.environment_id, &self.service_id) {
            (Some(id), None, None) => Ok(VariableScope::Project(parse_column(ENTITY, "project_id", id)?)),
            (None, Some(id), None) => Ok(VariableScope::Environment(parse_column(ENTITY, "environment_id", id)?)),
            (None, None, Some(id)) => Ok(VariableScope::Service(parse_column(ENTITY, "service_id", id)?)),
            _ => Err(StoreError::InvalidRow { entity: ENTITY, column: "scope", problem: "not exactly one scope".to_string() })
        }
    }

    fn decrypt(&self, keyring: &Keyring) -> Result<Secret, StoreError> {
        let sealed = Sealed { key_id: parse_column(ENTITY, "data_key_id", &self.data_key_id)?, ciphertext: self.value.clone() };
        let plaintext = keyring.open(&sealed, self.id.as_bytes())?;
        let value = std::str::from_utf8(&plaintext)
            .map_err(|_| StoreError::InvalidRow { entity: ENTITY, column: "value", problem: "not UTF-8".to_string() })?;
        Ok(Secret::new(value))
    }

    fn into_variable(self, keyring: &Keyring) -> Result<Variable, StoreError> {
        let value = if self.secret { None } else { Some(self.decrypt(keyring)?.expose().to_string()) };
        Ok(Variable {
            id: self.id()?,
            scope: self.scope()?,
            name: parse_column(ENTITY, "name", &self.name)?,
            value,
            availability: parse_column(ENTITY, "availability", &self.availability)?,
            secret: self.secret,
            revision: self.revision,
            created_at: parse_column(ENTITY, "created_at", &self.created_at)?,
            updated_at: parse_column(ENTITY, "updated_at", &self.updated_at)?
        })
    }

    fn into_resolved(self, keyring: &Keyring) -> Result<ResolvedVariable, StoreError> {
        Ok(ResolvedVariable {
            value: self.decrypt(keyring)?,
            level: self.scope()?.level(),
            variable_id: self.id()?,
            name: parse_column(ENTITY, "name", &self.name)?,
            availability: parse_column(ENTITY, "availability", &self.availability)?,
            secret: self.secret
        })
    }
}

/// The scope as the `project_id`, `environment_id` and `service_id` columns, one of them set.
fn scope_columns(scope: VariableScope) -> [Option<String>; 3] {
    match scope {
        VariableScope::Project(id) => [Some(id.to_string()), None, None],
        VariableScope::Environment(id) => [None, Some(id.to_string()), None],
        VariableScope::Service(id) => [None, None, Some(id.to_string())]
    }
}

fn name_conflict(fields: &VariableFields) -> impl FnOnce(&str) -> StoreError {
    let value = fields.name.to_string();
    move |_| StoreError::Conflict { entity: ENTITY, field: "name", value }
}

fn scope_not_found(scope: VariableScope) -> StoreError {
    let id = match scope {
        VariableScope::Project(id) => id.to_string(),
        VariableScope::Environment(id) => id.to_string(),
        VariableScope::Service(id) => id.to_string()
    };
    StoreError::NotFound { entity: scope.level().as_str(), id }
}

fn not_found(id: VariableId) -> StoreError {
    StoreError::NotFound { entity: ENTITY, id: id.to_string() }
}

fn seal(keyring: &Keyring, id: VariableId, value: &Secret) -> Sealed {
    keyring.seal(value.expose().as_bytes(), id.to_string().as_bytes())
}

impl SqliteStore {
    /// Fails with `NotFound` unless the scope's project, environment or service exists and is not deleted.
    async fn check_scope_active(&self, scope: VariableScope) -> Result<(), StoreError> {
        match scope {
            VariableScope::Project(id) => self.get_project(id).await.map(drop),
            VariableScope::Environment(id) => self.get_environment(id).await.map(drop),
            VariableScope::Service(id) => self.get_service(id).await.map(drop)
        }
    }

    async fn fetch_variable(&self, id: VariableId) -> Result<VariableRow, StoreError> {
        let row: Option<VariableRow> = sqlx::query_as(concat!("SELECT ", columns!(), " FROM variables WHERE id = ?"))
            .bind(id.to_string())
            .fetch_optional(self.pool())
            .await?;
        row.ok_or_else(|| not_found(id))
    }
}

#[async_trait]
impl VariableRepository for SqliteStore {
    async fn create_variable(&self, scope: VariableScope, fields: VariableFields, value: Secret) -> Result<Variable, StoreError> {
        let query = match scope {
            VariableScope::Project(_) => insert_under!("projects"),
            VariableScope::Environment(_) => insert_under!("environments"),
            VariableScope::Service(_) => insert_under!("services")
        };
        let id = VariableId::new();
        let sealed = seal(self.keyring(), id, &value);
        let [project_id, environment_id, service_id] = scope_columns(scope);
        let parent_id = project_id.clone().or(environment_id.clone()).or(service_id.clone());

        let row: Option<VariableRow> = sqlx::query_as(query)
            .bind(id.to_string())
            .bind(project_id)
            .bind(environment_id)
            .bind(service_id)
            .bind(fields.name.as_str())
            .bind(sealed.ciphertext)
            .bind(sealed.key_id.to_string())
            .bind(fields.availability.as_str())
            .bind(fields.secret)
            .bind(parent_id)
            .fetch_optional(self.pool())
            .await
            .map_err(|error| StoreError::on_unique_violation(error, name_conflict(&fields)))?;

        match row {
            Some(row) => row.into_variable(self.keyring()),
            None => Err(scope_not_found(scope))
        }
    }

    async fn get_variable(&self, id: VariableId) -> Result<Variable, StoreError> {
        self.fetch_variable(id).await?.into_variable(self.keyring())
    }

    async fn list_variables(&self, scope: VariableScope) -> Result<Vec<Variable>, StoreError> {
        let query = match scope {
            VariableScope::Project(_) => select_by!("project_id"),
            VariableScope::Environment(_) => select_by!("environment_id"),
            VariableScope::Service(_) => select_by!("service_id")
        };
        let [project_id, environment_id, service_id] = scope_columns(scope);
        let rows: Vec<VariableRow> = sqlx::query_as(query)
            .bind(project_id.or(environment_id).or(service_id))
            .fetch_all(self.pool())
            .await?;

        if rows.is_empty() {
            // Tell an empty list apart from a missing or deleted scope.
            self.check_scope_active(scope).await?;
        }
        rows.into_iter().map(|row| row.into_variable(self.keyring())).collect()
    }

    async fn update_variable(
        &self,
        id: VariableId,
        revision: i64,
        fields: VariableFields,
        value: Option<Secret>
    ) -> Result<Variable, StoreError> {
        let mut tx = self.begin_write().await?;
        let current: Option<(i64, bool)> = sqlx::query_as("SELECT revision, secret FROM variables WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        let (current_revision, was_secret) = current.ok_or_else(|| not_found(id))?;
        if current_revision != revision {
            return Err(StoreError::RevisionMismatch { entity: ENTITY, id: id.to_string(), expected: revision, actual: current_revision });
        }
        if was_secret && !fields.secret {
            return Err(StoreError::Rejected { entity: ENTITY, id: id.to_string(), problem: "a secret cannot be made non-secret" });
        }

        let sealed = value.map(|value| seal(self.keyring(), id, &value));
        let row: VariableRow = sqlx::query_as(concat!(
            "UPDATE variables
             SET name = ?, availability = ?, secret = ?, value = coalesce(?, value), data_key_id = coalesce(?, data_key_id),
                 revision = revision + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?
             RETURNING ",
            columns!()
        ))
        .bind(fields.name.as_str())
        .bind(fields.availability.as_str())
        .bind(fields.secret)
        .bind(sealed.as_ref().map(|sealed| sealed.ciphertext.clone()))
        .bind(sealed.as_ref().map(|sealed| sealed.key_id.to_string()))
        .bind(id.to_string())
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| StoreError::on_unique_violation(error, name_conflict(&fields)))?;

        tx.commit().await?;
        row.into_variable(self.keyring())
    }

    async fn delete_variable(&self, id: VariableId) -> Result<(), StoreError> {
        let result = sqlx::query("DELETE FROM variables WHERE id = ?").bind(id.to_string()).execute(self.pool()).await?;

        if result.rows_affected() == 0 {
            return Err(not_found(id));
        }
        Ok(())
    }

    async fn reveal_variable(&self, id: VariableId) -> Result<Secret, StoreError> {
        self.fetch_variable(id).await?.decrypt(self.keyring())
    }

    async fn resolve_service_variables(&self, service_id: ServiceId) -> Result<Vec<ResolvedVariable>, StoreError> {
        let service = self.get_service(service_id).await?;
        let environment = self.get_environment(service.environment_id).await?;

        let rows: Vec<VariableRow> = sqlx::query_as(concat!(
            "SELECT ", columns!(), " FROM variables WHERE service_id = ?
             UNION ALL SELECT ", columns!(), " FROM variables WHERE environment_id = ?
             UNION ALL SELECT ", columns!(), " FROM variables WHERE project_id = ?"
        ))
        .bind(service_id.to_string())
        .bind(environment.id.to_string())
        .bind(environment.project_id.to_string())
        .fetch_all(self.pool())
        .await?;

        let candidates = rows.into_iter().map(|row| row.into_resolved(self.keyring())).collect::<Result<Vec<_>, _>>()?;
        Ok(variable::resolve(candidates))
    }
}

#[cfg(test)]
mod tests {
    use spinikus_core::{
        environment::{EnvironmentFields, EnvironmentKind},
        id::{EnvironmentId, ProjectId},
        project::ProjectFields,
        service::{ServiceFields, ServiceKind},
        variable::{ScopeLevel, VariableAvailability}
    };

    use super::*;
    use crate::test_support::store;

    struct Tree {
        project: ProjectId,
        environment: EnvironmentId,
        service: ServiceId
    }

    async fn tree(store: &SqliteStore) -> Tree {
        let project = store
            .create_project(ProjectFields { name: "Web".to_string(), slug: "web".parse().unwrap() })
            .await
            .unwrap()
            .id;
        let environment = store
            .create_environment(
                project,
                EnvironmentKind::Production,
                EnvironmentFields { name: "Production".to_string(), slug: "production".parse().unwrap() }
            )
            .await
            .unwrap()
            .id;
        let service = store
            .create_service(
                environment,
                ServiceKind::App,
                ServiceFields { name: "App".to_string(), slug: "app".parse().unwrap(), config: Default::default() }
            )
            .await
            .unwrap()
            .id;
        Tree { project, environment, service }
    }

    fn fields(name: &str, secret: bool) -> VariableFields {
        VariableFields { name: name.parse().unwrap(), availability: VariableAvailability::Runtime, secret }
    }

    #[tokio::test]
    async fn creates_and_reads_a_plain_variable() {
        let (_dir, store) = store().await;
        let tree = tree(&store).await;

        let created = store
            .create_variable(VariableScope::Service(tree.service), fields("PORT", false), Secret::new("8080"))
            .await
            .unwrap();
        let fetched = store.get_variable(created.id).await.unwrap();

        assert_eq!(fetched, created);
        assert_eq!(created.value.as_deref(), Some("8080"));
        assert_eq!(created.scope, VariableScope::Service(tree.service));
        assert_eq!(created.revision, 1);
    }

    #[tokio::test]
    async fn secrets_are_write_only_and_encrypted_at_rest() {
        let (_dir, store) = store().await;
        let tree = tree(&store).await;

        let created = store
            .create_variable(VariableScope::Service(tree.service), fields("API_KEY", true), Secret::new("hunter2"))
            .await
            .unwrap();

        assert_eq!(created.value, None);
        assert_eq!(store.get_variable(created.id).await.unwrap().value, None);
        assert_eq!(store.reveal_variable(created.id).await.unwrap().expose(), "hunter2");
        let stored: Vec<u8> = sqlx::query_scalar("SELECT value FROM variables").fetch_one(store.pool()).await.unwrap();
        assert!(!stored.windows(7).any(|window| window == b"hunter2"));
    }

    #[tokio::test]
    async fn a_value_moved_to_another_row_does_not_decrypt() {
        let (_dir, store) = store().await;
        let tree = tree(&store).await;
        let scope = VariableScope::Service(tree.service);
        let secret = store.create_variable(scope, fields("API_KEY", true), Secret::new("hunter2")).await.unwrap();
        let plain = store.create_variable(scope, fields("PORT", false), Secret::new("8080")).await.unwrap();

        sqlx::query("UPDATE variables SET value = (SELECT value FROM variables WHERE id = ?) WHERE id = ?")
            .bind(secret.id.to_string())
            .bind(plain.id.to_string())
            .execute(store.pool())
            .await
            .unwrap();

        assert!(matches!(store.get_variable(plain.id).await, Err(StoreError::Secrets(_))));
    }

    #[tokio::test]
    async fn names_are_unique_per_scope() {
        let (_dir, store) = store().await;
        let tree = tree(&store).await;
        store.create_variable(VariableScope::Service(tree.service), fields("PORT", false), Secret::new("1")).await.unwrap();

        let duplicate = store.create_variable(VariableScope::Service(tree.service), fields("PORT", false), Secret::new("2")).await;
        let other_scope = store.create_variable(VariableScope::Environment(tree.environment), fields("PORT", false), Secret::new("3")).await;

        assert!(matches!(duplicate, Err(StoreError::Conflict { field: "name", .. })), "{duplicate:?}");
        other_scope.unwrap();
    }

    #[tokio::test]
    async fn scopes_must_exist_and_be_active() {
        let (_dir, store) = store().await;
        let tree = tree(&store).await;
        store.delete_service(tree.service).await.unwrap();

        let deleted = store.create_variable(VariableScope::Service(tree.service), fields("PORT", false), Secret::new("1")).await;
        let missing = store.create_variable(VariableScope::Project(ProjectId::new()), fields("PORT", false), Secret::new("1")).await;
        let listed = store.list_variables(VariableScope::Service(tree.service)).await;

        assert!(matches!(deleted, Err(StoreError::NotFound { entity: "service", .. })), "{deleted:?}");
        assert!(matches!(missing, Err(StoreError::NotFound { entity: "project", .. })), "{missing:?}");
        assert!(matches!(listed, Err(StoreError::NotFound { entity: "service", .. })), "{listed:?}");
    }

    #[tokio::test]
    async fn lists_a_scopes_own_variables_by_name() {
        let (_dir, store) = store().await;
        let tree = tree(&store).await;
        let scope = VariableScope::Environment(tree.environment);
        for name in ["ZED", "ALPHA"] {
            store.create_variable(scope, fields(name, false), Secret::new("x")).await.unwrap();
        }
        store.create_variable(VariableScope::Project(tree.project), fields("SHARED", false), Secret::new("x")).await.unwrap();

        let names: Vec<_> =
            store.list_variables(scope).await.unwrap().into_iter().map(|variable| variable.name.to_string()).collect();

        assert_eq!(names, vec!["ALPHA", "ZED"]);
        assert!(store.list_variables(VariableScope::Service(tree.service)).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn updates_keep_or_replace_the_value() {
        let (_dir, store) = store().await;
        let tree = tree(&store).await;
        let created = store
            .create_variable(VariableScope::Service(tree.service), fields("API_KEY", true), Secret::new("old"))
            .await
            .unwrap();

        let renamed = store.update_variable(created.id, 1, fields("TOKEN", true), None).await.unwrap();
        assert_eq!(renamed.name.as_str(), "TOKEN");
        assert_eq!(renamed.revision, 2);
        assert_eq!(store.reveal_variable(created.id).await.unwrap().expose(), "old");

        let mut build = fields("TOKEN", true);
        build.availability = VariableAvailability::Both;
        let replaced = store.update_variable(created.id, 2, build, Some(Secret::new("new"))).await.unwrap();
        assert_eq!(replaced.availability, VariableAvailability::Both);
        assert_eq!(store.reveal_variable(created.id).await.unwrap().expose(), "new");
    }

    #[tokio::test]
    async fn update_rejects_stale_revisions_unsecreting_and_name_conflicts() {
        let (_dir, store) = store().await;
        let tree = tree(&store).await;
        let scope = VariableScope::Service(tree.service);
        let secret = store.create_variable(scope, fields("API_KEY", true), Secret::new("x")).await.unwrap();
        store.create_variable(scope, fields("PORT", false), Secret::new("1")).await.unwrap();

        let stale = store.update_variable(secret.id, 2, fields("API_KEY", true), None).await;
        let unsecret = store.update_variable(secret.id, 1, fields("API_KEY", false), None).await;
        let conflict = store.update_variable(secret.id, 1, fields("PORT", true), None).await;
        let missing = store.update_variable(VariableId::new(), 1, fields("X", false), None).await;

        assert!(matches!(stale, Err(StoreError::RevisionMismatch { expected: 2, actual: 1, .. })), "{stale:?}");
        assert!(matches!(unsecret, Err(StoreError::Rejected { .. })), "{unsecret:?}");
        assert!(matches!(conflict, Err(StoreError::Conflict { field: "name", .. })), "{conflict:?}");
        assert!(matches!(missing, Err(StoreError::NotFound { .. })), "{missing:?}");
    }

    #[tokio::test]
    async fn a_plain_variable_can_become_secret() {
        let (_dir, store) = store().await;
        let tree = tree(&store).await;
        let created = store
            .create_variable(VariableScope::Service(tree.service), fields("API_KEY", false), Secret::new("x"))
            .await
            .unwrap();

        let updated = store.update_variable(created.id, 1, fields("API_KEY", true), None).await.unwrap();

        assert!(updated.secret);
        assert_eq!(updated.value, None);
    }

    #[tokio::test]
    async fn deletes_a_variable() {
        let (_dir, store) = store().await;
        let tree = tree(&store).await;
        let created = store
            .create_variable(VariableScope::Service(tree.service), fields("PORT", false), Secret::new("1"))
            .await
            .unwrap();

        store.delete_variable(created.id).await.unwrap();

        assert!(matches!(store.get_variable(created.id).await, Err(StoreError::NotFound { .. })));
        assert!(matches!(store.delete_variable(created.id).await, Err(StoreError::NotFound { .. })));
    }

    #[tokio::test]
    async fn resolves_all_three_layers_for_a_service() {
        let (_dir, store) = store().await;
        let tree = tree(&store).await;
        let other = tree_sibling_service(&store, tree.environment).await;
        let set = |scope, name: &'static str, value: &'static str| {
            let store = &store;
            async move { store.create_variable(scope, fields(name, false), Secret::new(value)).await.unwrap() }
        };
        set(VariableScope::Project(tree.project), "LOG_LEVEL", "info").await;
        set(VariableScope::Project(tree.project), "REGION", "eu").await;
        set(VariableScope::Environment(tree.environment), "LOG_LEVEL", "warn").await;
        set(VariableScope::Service(tree.service), "LOG_LEVEL", "debug").await;
        set(VariableScope::Service(other), "ONLY_OTHER", "x").await;
        store.create_variable(VariableScope::Service(tree.service), fields("API_KEY", true), Secret::new("hunter2")).await.unwrap();

        let resolved: Vec<_> = store
            .resolve_service_variables(tree.service)
            .await
            .unwrap()
            .into_iter()
            .map(|variable| (variable.name.to_string(), variable.value.expose().to_string(), variable.level))
            .collect();

        assert_eq!(
            resolved,
            vec![
                ("API_KEY".to_string(), "hunter2".to_string(), ScopeLevel::Service),
                ("LOG_LEVEL".to_string(), "debug".to_string(), ScopeLevel::Service),
                ("REGION".to_string(), "eu".to_string(), ScopeLevel::Project)
            ]
        );
        assert!(matches!(store.resolve_service_variables(ServiceId::new()).await, Err(StoreError::NotFound { .. })));
    }

    async fn tree_sibling_service(store: &SqliteStore, environment: EnvironmentId) -> ServiceId {
        store
            .create_service(
                environment,
                ServiceKind::App,
                ServiceFields { name: "Worker".to_string(), slug: "worker".parse().unwrap(), config: Default::default() }
            )
            .await
            .unwrap()
            .id
    }
}
