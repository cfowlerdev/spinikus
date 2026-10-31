//! Environment variables on a project, an environment or a service. Developers of a scope
//! set and remove its variables, and viewers see them, but never a secret's value: reading
//! one back is a reveal, which needs the admin role and is audited. A value can refer to
//! another service's variable (`${{ postgres.PASSWORD }}`, see [`spinikus_core::reference`]),
//! which is resolved when a deployment is created ([`Control::resolve_references`]).

use std::collections::HashMap;

use serde_json::json;
use spinikus_core::{
    environment::Environment,
    id::{ProjectId, VariableId},
    membership::Role,
    reference::{Reference, Template},
    secret::Secret,
    service::Service,
    slug::Slug,
    variable::{ResolvedVariable, Variable, VariableAvailability, VariableFields, VariableName, VariableScope},
};
use spinikus_store::{EnvironmentRepository, ProjectRepository, ServiceRepository, VariableRepository};

use crate::{Control, ControlError, Principal, access::not_found, audit};

/// The longest value a variable can have.
pub const MAX_VALUE_LEN: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewVariable {
    pub name: String,
    pub value: Secret,
    pub availability: VariableAvailability,
    pub secret: bool,
}

/// Changes to a variable; `None` keeps the current value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VariableChanges {
    pub name: Option<String>,
    pub value: Option<Secret>,
    pub availability: Option<VariableAvailability>,
    /// A secret stays secret: `Some(false)` on one is refused.
    pub secret: Option<bool>,
}

impl Control {
    pub async fn create_variable(
        &self,
        principal: &Principal,
        scope: VariableScope,
        new: NewVariable,
    ) -> Result<Variable, ControlError> {
        let project = self.scope_with_access(principal, scope, Role::Developer).await?;
        let fields = VariableFields { name: name(&new.name)?, availability: new.availability, secret: new.secret };
        check_value(&new.value)?;
        let variable = self.store().create_variable(scope, fields, new.value).await?;
        let details = json!({ "name": variable.name, "scope": scope.level(), "secret": variable.secret });
        self.audit_by(principal, "variable.create", variable.id, Some(project), details).await?;
        Ok(variable)
    }

    /// The variables set on the scope itself, by name; inherited ones are listed on their own scope.
    pub async fn list_variables(&self, principal: &Principal, scope: VariableScope) -> Result<Vec<Variable>, ControlError> {
        self.scope_with_access(principal, scope, Role::Viewer).await?;
        Ok(self.store().list_variables(scope).await?)
    }

    pub async fn get_variable(&self, principal: &Principal, id: VariableId) -> Result<Variable, ControlError> {
        let (variable, _) = self.variable_with_access(principal, id, Role::Viewer).await?;
        Ok(variable)
    }

    /// Applies `changes` if the variable is still at `revision`.
    pub async fn update_variable(
        &self,
        principal: &Principal,
        id: VariableId,
        revision: i64,
        changes: VariableChanges,
    ) -> Result<Variable, ControlError> {
        let (current, project) = self.variable_with_access(principal, id, Role::Developer).await?;
        if current.secret && changes.secret == Some(false) {
            return Err(ControlError::Invalid {
                field: "secret",
                problem: "a secret stays secret; delete it and add a new variable instead".to_string(),
            });
        }
        let fields = VariableFields {
            name: match &changes.name {
                Some(value) => name(value)?,
                None => current.name,
            },
            availability: changes.availability.unwrap_or(current.availability),
            secret: changes.secret.unwrap_or(current.secret),
        };
        if let Some(value) = &changes.value {
            check_value(value)?;
        }
        let given = changes.value.is_some();
        let variable = self.store().update_variable(id, revision, fields, changes.value).await?;
        let changed = audit::changed(&[
            ("name", changes.name.is_some()),
            ("value", given),
            ("availability", changes.availability.is_some()),
            ("secret", changes.secret.is_some()),
        ]);
        self.audit_by(principal, "variable.update", id, Some(project), changed).await?;
        Ok(variable)
    }

    pub async fn delete_variable(&self, principal: &Principal, id: VariableId) -> Result<(), ControlError> {
        let (variable, project) = self.variable_with_access(principal, id, Role::Developer).await?;
        self.store().delete_variable(id).await?;
        let details = json!({ "name": variable.name, "scope": variable.scope.level() });
        self.audit_by(principal, "variable.delete", id, Some(project), details).await
    }

    /// A variable's value, secret or not. Needs the admin role in its scope, and is audited.
    pub async fn reveal_variable(&self, principal: &Principal, id: VariableId) -> Result<Secret, ControlError> {
        let (variable, project) = self.variable_with_access(principal, id, Role::Viewer).await?;
        self.scope_with_access(principal, variable.scope, Role::Admin).await?;
        let value = self.store().reveal_variable(id).await?;
        self.audit_by(principal, "variable.reveal", id, Some(project), json!({ "name": variable.name })).await?;
        Ok(value)
    }

    /// Replaces the references in the service's variables with the values they name, from the
    /// variables the named services get. A reference to a secret makes the variable secret.
    /// `Invalid` if a reference names no service of the environment, or no variable it gets.
    pub(crate) async fn resolve_references(
        &self,
        service: &Service,
        variables: Vec<ResolvedVariable>,
    ) -> Result<Vec<ResolvedVariable>, ControlError> {
        // Each named service's variables as it gets them, references left as they are.
        let mut named: HashMap<Slug, HashMap<String, ResolvedVariable>> = HashMap::new();
        let by_name = |variables: &[ResolvedVariable]| {
            variables.iter().map(|variable| (variable.name.to_string(), variable.clone())).collect::<HashMap<_, _>>()
        };
        named.insert(service.slug.clone(), by_name(&variables));
        let mut resolved = Vec::with_capacity(variables.len());
        for mut variable in variables {
            let template =
                Template::parse(variable.value.expose()).map_err(|error| invalid(&variable.name, error.to_string()))?;
            for reference in template.references() {
                if named.contains_key(&reference.service) {
                    continue;
                }
                let Some(other) = self.store().find_service_by_slug(service.environment_id, &reference.service).await? else {
                    return Err(invalid(&variable.name, format!("`{reference}` names no service of this environment")));
                };
                let theirs = self.store().resolve_service_variables(other.id).await?;
                named.insert(reference.service.clone(), by_name(&theirs));
            }
            let mut secret = variable.secret;
            let value = template.render(|reference: &Reference| {
                let found = named.get(&reference.service).and_then(|theirs| theirs.get(&reference.name)).ok_or_else(|| {
                    invalid(
                        &variable.name,
                        format!("`{reference}`: `{}` gets no variable `{}`", reference.service, reference.name),
                    )
                })?;
                secret |= found.secret;
                Ok::<_, ControlError>(found.value.expose().to_string())
            })?;
            variable.value = Secret::new(value);
            variable.secret = secret;
            resolved.push(variable);
        }
        Ok(resolved)
    }

    /// The project the scope belongs to, if the principal has `needed` there.
    async fn scope_with_access(
        &self,
        principal: &Principal,
        scope: VariableScope,
        needed: Role,
    ) -> Result<ProjectId, ControlError> {
        match scope {
            VariableScope::Project(id) => {
                let project = self.store().get_project(id).await?;
                self.access(principal).await?.check_project(&project, needed)?;
                Ok(id)
            }
            VariableScope::Environment(id) => {
                let environment: Environment = self.store().get_environment(id).await?;
                self.access(principal).await?.check_environment(&environment, needed)?;
                Ok(environment.project_id)
            }
            VariableScope::Service(id) => {
                let (_, environment) = self.service_with_access(principal, id, needed).await?;
                Ok(environment.project_id)
            }
        }
    }

    async fn variable_with_access(
        &self,
        principal: &Principal,
        id: VariableId,
        needed: Role,
    ) -> Result<(Variable, ProjectId), ControlError> {
        let variable = self.store().get_variable(id).await?;
        match self.scope_with_access(principal, variable.scope, needed).await {
            Err(ControlError::NotFound { .. }) => Err(not_found("variable", id)),
            Err(error) => Err(error),
            Ok(project) => Ok((variable, project)),
        }
    }
}

fn name(value: &str) -> Result<VariableName, ControlError> {
    value.trim().parse().map_err(|error: spinikus_core::variable::VariableNameError| ControlError::Invalid {
        field: "name",
        problem: error.to_string(),
    })
}

/// Values must fit an environment, and their references must be well-formed (whether they
/// name anything is only known when deploying).
fn check_value(value: &Secret) -> Result<(), ControlError> {
    let invalid = |problem: String| ControlError::Invalid { field: "value", problem };
    let value = value.expose();
    if value.len() > MAX_VALUE_LEN {
        return Err(invalid("it is longer than 64 KiB".to_string()));
    }
    if value.contains('\0') {
        return Err(invalid("it cannot contain NUL characters".to_string()));
    }
    Template::parse(value).map(|_| ()).map_err(|error| invalid(error.to_string()))
}

fn invalid(name: &VariableName, problem: String) -> ControlError {
    ControlError::Invalid { field: "variables", problem: format!("{name}: {problem}") }
}

#[cfg(test)]
mod tests {
    use spinikus_core::{membership::MembershipScope, service::ServiceKind};
    use spinikus_store::{AuditLogRepository, DeploymentRepository};

    use super::*;
    use crate::{
        DeploymentTimings, NewService,
        deployments::tests::setup,
        test_support::{grant, user},
    };

    fn new(name: &str, value: &str, secret: bool) -> NewVariable {
        NewVariable { name: name.to_string(), value: Secret::new(value), availability: VariableAvailability::Runtime, secret }
    }

    #[tokio::test]
    async fn developers_set_variables_viewers_see_them_and_admins_reveal_secrets() {
        let setup = setup(DeploymentTimings::default()).await;
        let (control, admin, service) = (&setup.control, &setup.admin, setup.service.id);
        let environment = setup.service.environment_id;
        let developer = user(control, "ada@example.com", false).await;
        let viewer = user(control, "vic@example.com", false).await;
        grant(control, &developer, MembershipScope::Environment(environment), Role::Developer).await;
        grant(control, &viewer, MembershipScope::Environment(environment), Role::Viewer).await;
        let project = control.store().get_environment(environment).await.unwrap().project_id;

        let key =
            control.create_variable(&developer, VariableScope::Service(service), new("API_KEY", "s3cret", true)).await.unwrap();
        let level = control
            .create_variable(&developer, VariableScope::Environment(environment), new("LOG", "info", false))
            .await
            .unwrap();
        let on_project = control.create_variable(&developer, VariableScope::Project(project), new("X", "1", false)).await;
        let by_viewer = control.create_variable(&viewer, VariableScope::Service(service), new("Y", "1", false)).await;
        let bad_name = control.create_variable(admin, VariableScope::Service(service), new("MY-VAR", "1", false)).await;
        let bad_reference =
            control.create_variable(admin, VariableScope::Service(service), new("URL", "${{ db.URL", false)).await;
        let listed = control.list_variables(&viewer, VariableScope::Service(service)).await.unwrap();
        let unsecret = VariableChanges { secret: Some(false), ..Default::default() };
        let unsecret = control.update_variable(&developer, key.id, key.revision, unsecret).await;
        let changes = VariableChanges { value: Some(Secret::new("debug")), ..Default::default() };
        let changed = control.update_variable(&developer, level.id, level.revision, changes).await.unwrap();
        let revealed_by_developer = control.reveal_variable(&developer, key.id).await;
        let revealed = control.reveal_variable(admin, key.id).await.unwrap();
        control.delete_variable(&developer, level.id).await.unwrap();
        let audit = control.store().list_audit_entries(None, None, None, 50).await.unwrap();

        assert_eq!((key.value, listed[0].value.clone()), (None, None));
        assert!(matches!(on_project, Err(ControlError::Forbidden)), "{on_project:?}");
        assert!(matches!(by_viewer, Err(ControlError::Forbidden)), "{by_viewer:?}");
        assert!(matches!(bad_name, Err(ControlError::Invalid { field: "name", .. })), "{bad_name:?}");
        assert!(matches!(bad_reference, Err(ControlError::Invalid { field: "value", .. })), "{bad_reference:?}");
        assert!(matches!(unsecret, Err(ControlError::Invalid { field: "secret", .. })), "{unsecret:?}");
        assert_eq!(changed.value.as_deref(), Some("debug"));
        assert!(matches!(revealed_by_developer, Err(ControlError::Forbidden)), "{revealed_by_developer:?}");
        assert_eq!(revealed.expose(), "s3cret");
        let actions: Vec<_> = audit.iter().map(|entry| entry.action.to_string()).collect();
        assert!(actions.iter().any(|action| action == "variable.reveal"), "{actions:?}");
        assert!(audit.iter().all(|entry| !serde_json::to_string(&entry.details).unwrap().contains("s3cret")));
    }

    #[tokio::test]
    async fn deployments_resolve_references_to_other_services() {
        let setup = setup(DeploymentTimings::default()).await;
        let (control, admin, web) = (&setup.control, &setup.admin, &setup.service);
        let new_db = NewService {
            name: "Postgres".to_string(),
            slug: None,
            kind: ServiceKind::Database,
            config: Default::default(),
            server_id: None,
        };
        let db = control.create_service(admin, web.environment_id, new_db).await.unwrap();
        let set = |scope, name: &str, value: &str, secret| control.create_variable(admin, scope, new(name, value, secret));
        set(VariableScope::Service(db.id), "PASSWORD", "hunter2", true).await.unwrap();
        set(VariableScope::Environment(web.environment_id), "DB_NAME", "shop", false).await.unwrap();
        let url = "postgres://app:${{ postgres.PASSWORD }}@postgres:5432/${{ postgres.DB_NAME }}";
        set(VariableScope::Service(web.id), "DATABASE_URL", url, false).await.unwrap();
        set(VariableScope::Service(web.id), "MIRROR", "${{ web.DB_NAME }}-copy", false).await.unwrap();
        set(VariableScope::Service(web.id), "LITERAL", "$${{ not.A_REFERENCE }}", false).await.unwrap();

        let deployment = control.create_deployment(admin, web.id).await.unwrap();
        let variables = control.store().deployment_variables(deployment.id).await.unwrap();
        let value = |name: &str| {
            let variable = variables.iter().find(|variable| variable.name.as_str() == name).unwrap();
            (variable.value.expose().to_string(), variable.secret)
        };

        assert_eq!(value("DATABASE_URL"), ("postgres://app:hunter2@postgres:5432/shop".to_string(), true));
        assert_eq!(value("MIRROR"), ("shop-copy".to_string(), false));
        assert_eq!(value("LITERAL"), ("${{ not.A_REFERENCE }}".to_string(), false));

        let missing = set(VariableScope::Service(web.id), "CACHE", "${{ redis.URL }}", false).await.unwrap();
        let no_service = control.create_deployment(admin, web.id).await;
        let changes = VariableChanges { value: Some(Secret::new("${{ postgres.NOPE }}")), ..Default::default() };
        control.update_variable(admin, missing.id, missing.revision, changes).await.unwrap();
        let no_variable = control.create_deployment(admin, web.id).await;

        let problem = |result: Result<_, ControlError>| match result {
            Err(ControlError::Invalid { field: "variables", problem }) => problem,
            other => panic!("{other:?}"),
        };
        assert!(problem(no_service).contains("`${{ redis.URL }}` names no service"));
        assert!(problem(no_variable).contains("`postgres` gets no variable `NOPE`"));
    }
}
