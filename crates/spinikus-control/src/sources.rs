//! A service's source: the Git repository it builds from, or the image it runs. Viewers
//! read it; developers set it.

use serde_json::json;
use spinikus_core::{
    id::ServiceId,
    membership::Role,
    source::{Source, SourceSpec},
};
use spinikus_store::SourceRepository;

use crate::{Control, ControlError, Principal};

impl Control {
    pub async fn get_source(&self, principal: &Principal, service_id: ServiceId) -> Result<Source, ControlError> {
        self.service_with_access(principal, service_id, Role::Viewer).await?;
        Ok(self.store().get_source(service_id).await?)
    }

    /// Gives a service its source. A `Conflict` if it already has one: replace that with
    /// [`Control::replace_source`].
    pub async fn create_source(
        &self,
        principal: &Principal,
        service_id: ServiceId,
        spec: SourceSpec,
    ) -> Result<Source, ControlError> {
        let (_, environment) = self.service_with_access(principal, service_id, Role::Developer).await?;
        check(&spec)?;
        let source = self.store().create_source(service_id, spec).await?;
        let details = json!({ "service_id": service_id.to_string(), "type": source.spec.type_name() });
        self.audit_by(principal, "source.create", source.id, Some(environment.project_id), details).await?;
        Ok(source)
    }

    /// Replaces the service's source if it is still at `revision`, such as to switch from an
    /// image to a Git repository. Deployments already created keep the source they had.
    pub async fn replace_source(
        &self,
        principal: &Principal,
        service_id: ServiceId,
        revision: i64,
        spec: SourceSpec,
    ) -> Result<Source, ControlError> {
        let (_, environment) = self.service_with_access(principal, service_id, Role::Developer).await?;
        check(&spec)?;
        let source = self.store().update_source(service_id, revision, spec).await?;
        let details = json!({ "service_id": service_id.to_string(), "type": source.spec.type_name() });
        self.audit_by(principal, "source.update", source.id, Some(environment.project_id), details).await?;
        Ok(source)
    }
}

fn check(spec: &SourceSpec) -> Result<(), ControlError> {
    spec.validate().map_err(|error| ControlError::Invalid { field: error.field, problem: error.to_string() })
}

#[cfg(test)]
mod tests {
    use spinikus_core::{
        environment::EnvironmentKind,
        membership::MembershipScope,
        service::ServiceKind,
        source::{GitSource, ImageSource},
    };

    use super::*;
    use crate::{
        NewEnvironment, NewProject, NewService,
        test_support::{control, grant, user},
    };

    fn image(image: &str) -> SourceSpec {
        SourceSpec::Image(ImageSource { image: image.to_string() })
    }

    #[tokio::test]
    async fn developers_set_the_source_and_viewers_read_it() {
        let (_dir, control) = control().await;
        let admin = user(&control, "admin@example.com", true).await;
        let developer = user(&control, "ada@example.com", false).await;
        let viewer = user(&control, "vic@example.com", false).await;
        let project = control.create_project(&admin, NewProject { name: "Shop".to_string(), slug: None }).await.unwrap();
        let new = NewEnvironment { name: "Production".to_string(), slug: None, kind: EnvironmentKind::Production };
        let environment = control.create_environment(&admin, project.id, new).await.unwrap();
        grant(&control, &developer, MembershipScope::Environment(environment.id), Role::Developer).await;
        grant(&control, &viewer, MembershipScope::Environment(environment.id), Role::Viewer).await;
        let new = NewService {
            name: "Web".to_string(),
            slug: None,
            kind: ServiceKind::App,
            config: Default::default(),
            server_id: None,
        };
        let service = control.create_service(&admin, environment.id, new).await.unwrap();

        let created = control.create_source(&developer, service.id, image("ghcr.io/acme/web:1")).await.unwrap();
        let again = control.create_source(&developer, service.id, image("ghcr.io/acme/web:2")).await;
        let by_viewer = control.create_source(&viewer, service.id, image("ghcr.io/acme/web:2")).await;
        let invalid = control.replace_source(&developer, service.id, created.revision, image("not an image")).await;
        let git = SourceSpec::Git(GitSource {
            repo_url: "https://github.com/acme/web".to_string(),
            branch: "main".to_string(),
            root_directory: None,
        });
        let replaced = control.replace_source(&developer, service.id, created.revision, git.clone()).await.unwrap();
        let stale = control.replace_source(&developer, service.id, created.revision, image("ghcr.io/acme/web:3")).await;

        assert_eq!(control.get_source(&viewer, service.id).await.unwrap().spec, git);
        assert_eq!(replaced.revision, created.revision + 1);
        assert!(matches!(again, Err(ControlError::Conflict { .. })), "{again:?}");
        assert!(matches!(by_viewer, Err(ControlError::Forbidden)), "{by_viewer:?}");
        assert!(matches!(invalid, Err(ControlError::Invalid { field: "image", .. })), "{invalid:?}");
        assert!(matches!(stale, Err(ControlError::RevisionMismatch { .. })), "{stale:?}");
    }
}
