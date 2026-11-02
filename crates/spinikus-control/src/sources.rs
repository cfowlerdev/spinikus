//! A service's source: the Git repository it builds from, or the image it runs. Viewers
//! read it; developers set it.
//!
//! A service cloned over SSH gets a deploy key: an Ed25519 key pair made when a Git source
//! with an SSH URL is set (or on request), whose public half the user adds to the
//! repository. The key belongs to the service, so it outlives source changes.

use serde_json::json;
use spinikus_core::{
    id::ServiceId,
    membership::Role,
    secret::Secret,
    source::{DeployKey, Source, SourceSpec},
};
use spinikus_store::{DeployKeyRepository, SourceRepository};
use ssh_key::{HashAlg, LineEnding, PrivateKey, private::Ed25519Keypair};

use crate::{Control, ControlError, Principal, access::not_found};

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
        self.ensure_deploy_key(&source).await?;
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
        self.ensure_deploy_key(&source).await?;
        Ok(source)
    }

    /// The service's deploy key. `NotFound` if it has none.
    pub async fn get_deploy_key(&self, principal: &Principal, service_id: ServiceId) -> Result<DeployKey, ControlError> {
        self.service_with_access(principal, service_id, Role::Viewer).await?;
        self.store().get_deploy_key(service_id).await?.ok_or_else(|| not_found("deploy key", service_id))
    }

    /// Gives the service a new deploy key, replacing any it had: the old one stops working
    /// once removed from the repository.
    pub async fn create_deploy_key(&self, principal: &Principal, service_id: ServiceId) -> Result<DeployKey, ControlError> {
        let (_, environment) = self.service_with_access(principal, service_id, Role::Developer).await?;
        let key = self.new_deploy_key(service_id).await?;
        let details = json!({ "fingerprint": key.fingerprint });
        self.audit_by(principal, "deploy_key.create", service_id, Some(environment.project_id), details).await?;
        Ok(key)
    }

    /// Makes a deploy key for a source cloned over SSH, unless the service has one.
    async fn ensure_deploy_key(&self, source: &Source) -> Result<(), ControlError> {
        if let SourceSpec::Git(git) = &source.spec
            && git.uses_ssh()
            && self.store().get_deploy_key(source.service_id).await?.is_none()
        {
            self.new_deploy_key(source.service_id).await?;
        }
        Ok(())
    }

    async fn new_deploy_key(&self, service_id: ServiceId) -> Result<DeployKey, ControlError> {
        let keypair = Ed25519Keypair::from_seed(&rand::random());
        let internal = |error: ssh_key::Error| ControlError::internal("making a deploy key", error);
        let private = PrivateKey::new(keypair.into(), "spinikus").map_err(internal)?;
        let public = private.public_key().to_openssh().map_err(internal)?;
        let fingerprint = private.public_key().fingerprint(HashAlg::Sha256).to_string();
        let private = Secret::new(private.to_openssh(LineEnding::LF).map_err(internal)?.as_str());
        Ok(self.store().set_deploy_key(service_id, &public, &fingerprint, &private).await?)
    }
}

pub(crate) fn check(spec: &SourceSpec) -> Result<(), ControlError> {
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
        let new =
            NewEnvironment { name: "Production".to_string(), slug: None, kind: EnvironmentKind::Production, previews: false };
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
