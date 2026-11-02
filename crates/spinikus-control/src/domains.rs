//! Domains: the hostnames an app service is reachable at, besides its generated one.
//! Developers add, change and remove them; viewers see them. Names under the base domain
//! are reserved for generated hostnames.

use serde::Serialize;
use serde_json::json;
use spinikus_core::{
    certificate::HostnameCertificate,
    domain::{Domain, DomainFields, Hostname},
    environment::Environment,
    id::{DomainId, ServiceId},
    membership::Role,
    service::{Service, ServiceKind},
};
use spinikus_store::{CertificateRepository, DomainRepository};

use crate::{Control, ControlError, Principal, access::not_found, audit};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDomain {
    pub hostname: String,
    /// Another domain of the service to redirect to, instead of serving the app.
    pub redirect_to: Option<DomainId>,
}

/// Changes to a domain; `None` keeps the current value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DomainChanges {
    pub hostname: Option<String>,
    /// `Some(None)` serves the app again instead of redirecting.
    pub redirect_to: Option<Option<DomainId>>,
}

/// Where a service is reachable: its generated hostname, if it has one, and its domains,
/// with where getting a certificate stands for those of its hostnames that have tried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceDomains {
    pub generated_hostname: Option<Hostname>,
    pub domains: Vec<Domain>,
    pub certificates: Vec<HostnameCertificate>,
}

impl Control {
    pub async fn create_domain(
        &self,
        principal: &Principal,
        service_id: ServiceId,
        new: NewDomain,
    ) -> Result<Domain, ControlError> {
        let (service, environment) = self.service_with_access(principal, service_id, Role::Developer).await?;
        check_kind(&service)?;
        let hostname = self.hostname(&new.hostname).await?;
        let domain = self.store().create_domain(service_id, DomainFields { hostname, redirect_to: new.redirect_to }).await?;
        let details = json!({ "hostname": domain.hostname, "service_id": service_id.to_string() });
        self.audit_by(principal, "domain.create", domain.id, Some(environment.project_id), details).await?;
        self.wake_deployments();
        Ok(domain)
    }

    pub async fn list_domains(&self, principal: &Principal, service_id: ServiceId) -> Result<ServiceDomains, ControlError> {
        self.service_with_access(principal, service_id, Role::Viewer).await?;
        let generated_hostname = self.generated_hostname(service_id).await?;
        let domains = self.store().list_domains(service_id).await?;
        let mut certificates = self.store().list_hostname_certificates().await?;
        certificates.retain(|record| {
            generated_hostname.as_ref() == Some(&record.hostname)
                || domains.iter().any(|domain| domain.hostname == record.hostname)
        });
        Ok(ServiceDomains { generated_hostname, domains, certificates })
    }

    pub async fn get_domain(&self, principal: &Principal, id: DomainId) -> Result<Domain, ControlError> {
        let (domain, _) = self.domain_with_access(principal, id, Role::Viewer).await?;
        Ok(domain)
    }

    pub async fn update_domain(
        &self,
        principal: &Principal,
        id: DomainId,
        revision: i64,
        changes: DomainChanges,
    ) -> Result<Domain, ControlError> {
        let (current, environment) = self.domain_with_access(principal, id, Role::Developer).await?;
        let hostname = match &changes.hostname {
            Some(hostname) => self.hostname(hostname).await?,
            None => current.hostname,
        };
        let redirect_to = changes.redirect_to.unwrap_or(current.redirect_to);
        let domain = self.store().update_domain(id, revision, DomainFields { hostname, redirect_to }).await?;
        let changed = audit::changed(&[("hostname", changes.hostname.is_some()), ("redirect_to", changes.redirect_to.is_some())]);
        self.audit_by(principal, "domain.update", id, Some(environment.project_id), changed).await?;
        self.wake_deployments();
        Ok(domain)
    }

    pub async fn delete_domain(&self, principal: &Principal, id: DomainId) -> Result<(), ControlError> {
        let (domain, environment) = self.domain_with_access(principal, id, Role::Developer).await?;
        self.store().delete_domain(id).await?;
        let details = json!({ "hostname": domain.hostname, "service_id": domain.service_id.to_string() });
        self.audit_by(principal, "domain.delete", id, Some(environment.project_id), details).await?;
        self.wake_deployments();
        Ok(())
    }

    /// A hostname users may claim: valid, and not under the base domain.
    async fn hostname(&self, value: &str) -> Result<Hostname, ControlError> {
        let hostname: Hostname = value.parse().map_err(|error: spinikus_core::domain::HostnameError| ControlError::Invalid {
            field: "hostname",
            problem: error.to_string(),
        })?;
        if let Some(base) = self.base_domain().await?
            && base.contains(&hostname)
        {
            return Err(ControlError::Invalid {
                field: "hostname",
                problem: format!("names under the base domain `{base}` are generated by Spinikus"),
            });
        }
        Ok(hostname)
    }

    async fn domain_with_access(
        &self,
        principal: &Principal,
        id: DomainId,
        needed: Role,
    ) -> Result<(Domain, Environment), ControlError> {
        let domain = self.store().get_domain(id).await?;
        match self.service_with_access(principal, domain.service_id, needed).await {
            Err(ControlError::NotFound { .. }) => Err(not_found("domain", id)),
            Err(error) => Err(error),
            Ok((_, environment)) => Ok((domain, environment)),
        }
    }
}

/// Only apps serve HTTP.
fn check_kind(service: &Service) -> Result<(), ControlError> {
    match service.kind {
        ServiceKind::App => Ok(()),
        _ => Err(ControlError::Invalid { field: "service", problem: "only app services have domains".to_string() }),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use spinikus_core::{environment::EnvironmentKind, membership::MembershipScope};

    use super::*;
    use crate::{
        NewEnvironment, NewProject, NewService,
        settings::BASE_DOMAIN,
        test_support::{control, grant, user},
    };

    #[tokio::test]
    async fn developers_manage_the_domains_of_app_services() {
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
        let service = |name: &str, kind| NewService {
            name: name.to_string(),
            slug: None,
            kind,
            config: Default::default(),
            server_id: None,
        };
        let web = control.create_service(&admin, environment.id, service("Web", ServiceKind::App)).await.unwrap();
        let db = control.create_service(&admin, environment.id, service("DB", ServiceKind::Database)).await.unwrap();
        let domain = |hostname: &str| NewDomain { hostname: hostname.to_string(), redirect_to: None };

        let apex = control.create_domain(&developer, web.id, domain("Example.com")).await.unwrap();
        let www = NewDomain { hostname: "www.example.com".to_string(), redirect_to: Some(apex.id) };
        let www = control.create_domain(&developer, web.id, www).await.unwrap();
        let by_viewer = control.create_domain(&viewer, web.id, domain("shop.example.com")).await;
        let on_db = control.create_domain(&admin, db.id, domain("db.example.com")).await;
        let invalid = control.create_domain(&admin, web.id, domain("not a host")).await;
        let reserved = control.create_domain(&admin, web.id, domain("mine.localhost")).await;
        let taken = control.create_domain(&admin, web.id, domain("example.com")).await;
        let listed = control.list_domains(&viewer, web.id).await.unwrap();
        let changes = DomainChanges { redirect_to: Some(None), ..Default::default() };
        let unredirected = control.update_domain(&developer, www.id, www.revision, changes).await.unwrap();
        control.delete_domain(&developer, www.id).await.unwrap();
        control.set_setting(&admin, BASE_DOMAIN, 0, Value::Null).await.unwrap();
        let without_base = control.list_domains(&viewer, web.id).await.unwrap();

        assert_eq!(apex.hostname.as_str(), "example.com");
        assert!(matches!(by_viewer, Err(ControlError::Forbidden)), "{by_viewer:?}");
        assert!(matches!(on_db, Err(ControlError::Invalid { field: "service", .. })), "{on_db:?}");
        assert!(matches!(invalid, Err(ControlError::Invalid { field: "hostname", .. })), "{invalid:?}");
        assert!(matches!(reserved, Err(ControlError::Invalid { field: "hostname", .. })), "{reserved:?}");
        assert!(matches!(taken, Err(ControlError::Conflict { .. })), "{taken:?}");
        assert_eq!(listed.generated_hostname.unwrap().as_str(), "web-production-shop.localhost");
        assert_eq!(listed.domains.iter().map(|domain| domain.id).collect::<Vec<_>>(), [apex.id, www.id]);
        assert_eq!(unredirected.redirect_to, None);
        assert_eq!(without_base.generated_hostname, None);
        assert_eq!(without_base.domains.iter().map(|domain| domain.id).collect::<Vec<_>>(), [apex.id]);
    }
}
