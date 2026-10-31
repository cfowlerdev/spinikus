use async_trait::async_trait;
use spinikus_core::{
    domain::{Domain, DomainFields, Hostname},
    id::{CertificateId, DomainId, ServiceId},
};
use sqlx::SqliteConnection;

use crate::{SqliteStore, StoreError, certificates::fetch_certificate, error::parse_column};

const ENTITY: &str = "domain";

#[async_trait]
pub trait DomainRepository: Send + Sync {
    /// Fails with `NotFound` if the service does not exist or is deleted, `Conflict` if another
    /// domain has the hostname, and `Rejected` for a redirect that is not allowed (see `DomainFields`).
    async fn create_domain(&self, service_id: ServiceId, fields: DomainFields) -> Result<Domain, StoreError>;

    /// Fails with `NotFound` for deleted domains too.
    async fn get_domain(&self, id: DomainId) -> Result<Domain, StoreError>;

    /// The domain that currently has `hostname`, if any.
    async fn find_domain_by_hostname(&self, hostname: &Hostname) -> Result<Option<Domain>, StoreError>;

    /// The service's domains, by hostname.
    async fn list_domains(&self, service_id: ServiceId) -> Result<Vec<Domain>, StoreError>;

    /// Every domain that is not deleted, by hostname.
    async fn list_all_domains(&self) -> Result<Vec<Domain>, StoreError>;

    /// Replaces the domain's fields if it is still at `revision`. Changing the hostname
    /// removes the certificate, which no longer applies.
    async fn update_domain(&self, id: DomainId, revision: i64, fields: DomainFields) -> Result<Domain, StoreError>;

    /// Soft-deletes the domain, which frees its hostname. Refused while other domains redirect to it.
    async fn delete_domain(&self, id: DomainId) -> Result<(), StoreError>;

    /// Sets or clears the certificate serving the domain. Fails with `Rejected` if the
    /// certificate does not cover the hostname. Not a user edit, so the revision stays.
    async fn set_domain_certificate(&self, id: DomainId, certificate_id: Option<CertificateId>) -> Result<Domain, StoreError>;
}

macro_rules! columns {
    () => {
        "id, service_id, hostname, redirect_to, certificate_id, revision, created_at, updated_at"
    };
}

#[derive(sqlx::FromRow)]
struct DomainRow {
    id: String,
    service_id: String,
    hostname: String,
    redirect_to: Option<String>,
    certificate_id: Option<String>,
    revision: i64,
    created_at: String,
    updated_at: String,
}

impl TryFrom<DomainRow> for Domain {
    type Error = StoreError;

    fn try_from(row: DomainRow) -> Result<Self, Self::Error> {
        Ok(Domain {
            id: parse_column(ENTITY, "id", &row.id)?,
            service_id: parse_column(ENTITY, "service_id", &row.service_id)?,
            hostname: parse_column(ENTITY, "hostname", &row.hostname)?,
            redirect_to: row.redirect_to.map(|id| parse_column(ENTITY, "redirect_to", &id)).transpose()?,
            certificate_id: row.certificate_id.map(|id| parse_column(ENTITY, "certificate_id", &id)).transpose()?,
            revision: row.revision,
            created_at: parse_column(ENTITY, "created_at", &row.created_at)?,
            updated_at: parse_column(ENTITY, "updated_at", &row.updated_at)?,
        })
    }
}

fn not_found(id: DomainId) -> StoreError {
    StoreError::NotFound { entity: ENTITY, id: id.to_string() }
}

fn rejected(id: DomainId, problem: &'static str) -> StoreError {
    StoreError::Rejected { entity: ENTITY, id: id.to_string(), problem }
}

fn hostname_conflict(fields: &DomainFields) -> impl FnOnce(&str) -> StoreError {
    let value = fields.hostname.to_string();
    move |_| StoreError::Conflict { entity: ENTITY, field: "hostname", value }
}

async fn fetch(conn: &mut SqliteConnection, id: DomainId) -> Result<Domain, StoreError> {
    let row: Option<DomainRow> =
        sqlx::query_as(concat!("SELECT ", columns!(), " FROM domains WHERE id = ? AND deleted_at IS NULL"))
            .bind(id.to_string())
            .fetch_optional(conn)
            .await?;
    row.ok_or_else(|| not_found(id))?.try_into()
}

/// Checks that domain `id` of `service_id` may redirect to `target`: another domain of the
/// same service that does not redirect itself, while no domain redirects to `id`. Redirects
/// are therefore always a single hop.
async fn check_redirect(
    conn: &mut SqliteConnection,
    id: DomainId,
    service_id: ServiceId,
    target: Option<DomainId>,
) -> Result<(), StoreError> {
    let Some(target) = target else { return Ok(()) };
    if target == id {
        return Err(rejected(id, "a domain cannot redirect to itself"));
    }

    let target = fetch(conn, target).await?;
    if target.service_id != service_id {
        return Err(rejected(id, "a domain can only redirect to a domain of the same service"));
    }
    if target.redirect_to.is_some() {
        return Err(rejected(id, "a domain cannot redirect to a domain that redirects"));
    }

    let redirected_to: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM domains WHERE redirect_to = ? AND deleted_at IS NULL LIMIT 1")
            .bind(id.to_string())
            .fetch_optional(conn)
            .await?;
    if redirected_to.is_some() {
        return Err(rejected(id, "a domain that other domains redirect to cannot redirect"));
    }
    Ok(())
}

#[async_trait]
impl DomainRepository for SqliteStore {
    async fn create_domain(&self, service_id: ServiceId, fields: DomainFields) -> Result<Domain, StoreError> {
        let id = DomainId::new();
        let mut tx = self.begin_write().await?;

        let service_active: Option<i64> = sqlx::query_scalar("SELECT 1 FROM services WHERE id = ? AND deleted_at IS NULL")
            .bind(service_id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        if service_active.is_none() {
            return Err(StoreError::NotFound { entity: "service", id: service_id.to_string() });
        }
        check_redirect(&mut tx, id, service_id, fields.redirect_to).await?;

        let row: DomainRow = sqlx::query_as(concat!(
            "INSERT INTO domains (id, service_id, hostname, redirect_to) VALUES (?, ?, ?, ?) RETURNING ",
            columns!()
        ))
        .bind(id.to_string())
        .bind(service_id.to_string())
        .bind(fields.hostname.as_str())
        .bind(fields.redirect_to.map(|id| id.to_string()))
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| StoreError::on_unique_violation(error, hostname_conflict(&fields)))?;

        tx.commit().await?;
        row.try_into()
    }

    async fn get_domain(&self, id: DomainId) -> Result<Domain, StoreError> {
        fetch(&mut *self.pool().acquire().await?, id).await
    }

    async fn find_domain_by_hostname(&self, hostname: &Hostname) -> Result<Option<Domain>, StoreError> {
        let row: Option<DomainRow> =
            sqlx::query_as(concat!("SELECT ", columns!(), " FROM domains WHERE hostname = ? AND deleted_at IS NULL"))
                .bind(hostname.as_str())
                .fetch_optional(self.pool())
                .await?;
        row.map(Domain::try_from).transpose()
    }

    async fn list_domains(&self, service_id: ServiceId) -> Result<Vec<Domain>, StoreError> {
        let rows: Vec<DomainRow> = sqlx::query_as(concat!(
            "SELECT ",
            columns!(),
            " FROM domains WHERE service_id = ? AND deleted_at IS NULL ORDER BY hostname"
        ))
        .bind(service_id.to_string())
        .fetch_all(self.pool())
        .await?;

        if rows.is_empty() {
            // Tell an empty list apart from a missing or deleted service.
            let active: Option<i64> = sqlx::query_scalar("SELECT 1 FROM services WHERE id = ? AND deleted_at IS NULL")
                .bind(service_id.to_string())
                .fetch_optional(self.pool())
                .await?;
            if active.is_none() {
                return Err(StoreError::NotFound { entity: "service", id: service_id.to_string() });
            }
        }
        rows.into_iter().map(Domain::try_from).collect()
    }

    async fn list_all_domains(&self) -> Result<Vec<Domain>, StoreError> {
        let rows: Vec<DomainRow> =
            sqlx::query_as(concat!("SELECT ", columns!(), " FROM domains WHERE deleted_at IS NULL ORDER BY hostname"))
                .fetch_all(self.pool())
                .await?;
        rows.into_iter().map(Domain::try_from).collect()
    }

    async fn update_domain(&self, id: DomainId, revision: i64, fields: DomainFields) -> Result<Domain, StoreError> {
        let mut tx = self.begin_write().await?;
        let current = fetch(&mut tx, id).await?;
        if current.revision != revision {
            return Err(StoreError::RevisionMismatch {
                entity: ENTITY,
                id: id.to_string(),
                expected: revision,
                actual: current.revision,
            });
        }
        check_redirect(&mut tx, id, current.service_id, fields.redirect_to).await?;

        let row: DomainRow = sqlx::query_as(concat!(
            "UPDATE domains
             SET certificate_id = CASE WHEN hostname = ?1 THEN certificate_id END, hostname = ?1, redirect_to = ?2,
                 revision = revision + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?3
             RETURNING ",
            columns!()
        ))
        .bind(fields.hostname.as_str())
        .bind(fields.redirect_to.map(|id| id.to_string()))
        .bind(id.to_string())
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| StoreError::on_unique_violation(error, hostname_conflict(&fields)))?;

        tx.commit().await?;
        row.try_into()
    }

    async fn delete_domain(&self, id: DomainId) -> Result<(), StoreError> {
        let result = sqlx::query(
            "UPDATE domains
             SET deleted_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                 certificate_id = NULL, revision = revision + 1
             WHERE id = ? AND deleted_at IS NULL
               AND NOT EXISTS (SELECT 1 FROM domains AS other WHERE other.redirect_to = domains.id AND other.deleted_at IS NULL)",
        )
        .bind(id.to_string())
        .execute(self.pool())
        .await?;

        if result.rows_affected() == 0 {
            // Either the domain is gone, or other domains still redirect to it.
            self.get_domain(id).await?;
            return Err(StoreError::HasChildren { entity: ENTITY, id: id.to_string(), children: "domains redirecting to it" });
        }
        Ok(())
    }

    async fn set_domain_certificate(&self, id: DomainId, certificate_id: Option<CertificateId>) -> Result<Domain, StoreError> {
        let mut tx = self.begin_write().await?;
        let domain = fetch(&mut tx, id).await?;
        if let Some(certificate_id) = certificate_id
            && !fetch_certificate(&mut tx, certificate_id).await?.covers(&domain.hostname)
        {
            return Err(rejected(id, "the certificate does not cover the domain's hostname"));
        }

        let row: DomainRow = sqlx::query_as(concat!(
            "UPDATE domains SET certificate_id = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ? RETURNING ",
            columns!()
        ))
        .bind(certificate_id.map(|id| id.to_string()))
        .bind(id.to_string())
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;
        row.try_into()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Map;
    use spinikus_core::{
        certificate::NewCertificate,
        environment::{EnvironmentFields, EnvironmentKind},
        project::ProjectFields,
        secret::Secret,
        service::{ServiceFields, ServiceKind},
    };

    use super::*;
    use crate::{CertificateRepository, EnvironmentRepository, ProjectRepository, ServiceRepository, test_support::store};

    async fn services(store: &SqliteStore) -> (ServiceId, ServiceId) {
        let project = ProjectFields { name: "Web".to_string(), slug: "web".parse().unwrap() };
        let project_id = store.create_project(project).await.unwrap().id;
        let env = EnvironmentFields { name: "Production".to_string(), slug: "production".parse().unwrap() };
        let env_id = store.create_environment(project_id, EnvironmentKind::Production, env).await.unwrap().id;
        let mut ids = Vec::new();
        for slug in ["app", "api"] {
            let fields =
                ServiceFields { name: slug.to_string(), slug: slug.parse().unwrap(), config: Map::new(), server_id: None };
            ids.push(store.create_service(env_id, ServiceKind::App, fields).await.unwrap().id);
        }
        (ids[0], ids[1])
    }

    fn fields(hostname: &str) -> DomainFields {
        DomainFields { hostname: hostname.parse().unwrap(), redirect_to: None }
    }

    fn redirect(hostname: &str, to: DomainId) -> DomainFields {
        DomainFields { redirect_to: Some(to), ..fields(hostname) }
    }

    async fn certificate(store: &SqliteStore, hostnames: &[&str]) -> CertificateId {
        let new = NewCertificate {
            hostnames: hostnames.iter().map(|name| name.parse().unwrap()).collect(),
            chain_pem: "-----BEGIN CERTIFICATE-----".to_string(),
            private_key_pem: Secret::new("key"),
            not_before: "2026-10-01T00:00:00Z".parse().unwrap(),
            not_after: "2027-01-01T00:00:00Z".parse().unwrap(),
            acme_account_id: None,
        };
        store.create_certificate(new).await.unwrap().id
    }

    #[tokio::test]
    async fn creates_finds_and_lists_domains() {
        let (_dir, store) = store().await;
        let (app, _) = services(&store).await;

        let www = store.create_domain(app, fields("www.example.com")).await.unwrap();
        let apex = store.create_domain(app, fields("Example.com.")).await.unwrap();

        assert_eq!(store.get_domain(www.id).await.unwrap(), www);
        assert_eq!(apex.hostname.as_str(), "example.com");
        assert_eq!(store.find_domain_by_hostname(&"example.com".parse().unwrap()).await.unwrap(), Some(apex.clone()));
        let listed: Vec<_> = store.list_domains(app).await.unwrap().into_iter().map(|d| d.id).collect();
        assert_eq!(listed, vec![apex.id, www.id]);
        let all: Vec<_> = store.list_all_domains().await.unwrap().into_iter().map(|d| d.id).collect();
        assert_eq!(all, vec![apex.id, www.id]);
    }

    #[tokio::test]
    async fn hostnames_are_unique_across_services_until_deleted() {
        let (_dir, store) = store().await;
        let (app, api) = services(&store).await;
        let domain = store.create_domain(app, fields("example.com")).await.unwrap();

        let taken = store.create_domain(api, fields("example.com")).await;
        store.delete_domain(domain.id).await.unwrap();
        let freed = store.create_domain(api, fields("example.com")).await;

        assert!(matches!(taken, Err(StoreError::Conflict { field: "hostname", .. })), "{taken:?}");
        freed.unwrap();
        assert!(matches!(store.get_domain(domain.id).await, Err(StoreError::NotFound { .. })));
    }

    #[tokio::test]
    async fn requires_an_active_service() {
        let (_dir, store) = store().await;

        let missing = store.create_domain(ServiceId::new(), fields("example.com")).await;
        let listed = store.list_domains(ServiceId::new()).await;

        assert!(matches!(missing, Err(StoreError::NotFound { entity: "service", .. })), "{missing:?}");
        assert!(matches!(listed, Err(StoreError::NotFound { entity: "service", .. })), "{listed:?}");
    }

    #[tokio::test]
    async fn redirects_are_one_hop_within_a_service() {
        let (_dir, store) = store().await;
        let (app, api) = services(&store).await;
        let apex = store.create_domain(app, fields("example.com")).await.unwrap();
        let other_service = store.create_domain(api, fields("api.example.com")).await.unwrap();

        let www = store.create_domain(app, redirect("www.example.com", apex.id)).await.unwrap();
        let chained = store.create_domain(app, redirect("old.example.com", www.id)).await;
        let across = store.create_domain(app, redirect("x.example.com", other_service.id)).await;
        let apex_redirecting = store.update_domain(apex.id, 1, redirect("example.com", www.id)).await;
        let to_itself = store.update_domain(www.id, 1, redirect("www.example.com", www.id)).await;
        let missing = store.create_domain(app, redirect("y.example.com", DomainId::new())).await;

        assert_eq!(www.redirect_to, Some(apex.id));
        for result in [chained, across, apex_redirecting, to_itself] {
            assert!(matches!(result, Err(StoreError::Rejected { .. })), "{result:?}");
        }
        assert!(matches!(missing, Err(StoreError::NotFound { entity: "domain", .. })), "{missing:?}");
    }

    #[tokio::test]
    async fn a_redirect_target_cannot_be_deleted() {
        let (_dir, store) = store().await;
        let (app, _) = services(&store).await;
        let apex = store.create_domain(app, fields("example.com")).await.unwrap();
        let www = store.create_domain(app, redirect("www.example.com", apex.id)).await.unwrap();

        let refused = store.delete_domain(apex.id).await;
        store.delete_domain(www.id).await.unwrap();

        assert!(matches!(refused, Err(StoreError::HasChildren { .. })), "{refused:?}");
        store.delete_domain(apex.id).await.unwrap();
    }

    #[tokio::test]
    async fn updates_at_the_current_revision() {
        let (_dir, store) = store().await;
        let (app, _) = services(&store).await;
        let domain = store.create_domain(app, fields("example.com")).await.unwrap();

        let updated = store.update_domain(domain.id, 1, fields("example.org")).await.unwrap();
        let stale = store.update_domain(domain.id, 1, fields("example.net")).await;

        assert_eq!((updated.hostname.as_str(), updated.revision), ("example.org", 2));
        assert!(matches!(stale, Err(StoreError::RevisionMismatch { expected: 1, actual: 2, .. })), "{stale:?}");
    }

    #[tokio::test]
    async fn certificates_must_cover_the_hostname_and_go_when_it_changes() {
        let (_dir, store) = store().await;
        let (app, _) = services(&store).await;
        let domain = store.create_domain(app, fields("app.example.com")).await.unwrap();
        let wildcard = certificate(&store, &["*.example.com"]).await;
        let other = certificate(&store, &["example.org"]).await;

        let assigned = store.set_domain_certificate(domain.id, Some(wildcard)).await.unwrap();
        let mismatch = store.set_domain_certificate(domain.id, Some(other)).await;
        let in_use = store.delete_certificate(wildcard).await;
        let same_hostname = store.update_domain(domain.id, 1, fields("app.example.com")).await.unwrap();
        let new_hostname = store.update_domain(domain.id, 2, fields("web.example.com")).await.unwrap();

        assert_eq!((assigned.certificate_id, assigned.revision), (Some(wildcard), 1));
        assert!(matches!(mismatch, Err(StoreError::Rejected { .. })), "{mismatch:?}");
        assert!(matches!(in_use, Err(StoreError::HasChildren { .. })), "{in_use:?}");
        assert_eq!(same_hostname.certificate_id, Some(wildcard));
        assert_eq!(new_hostname.certificate_id, None);
        store.delete_certificate(wildcard).await.unwrap();
    }

    #[tokio::test]
    async fn deleting_a_domain_releases_its_certificate() {
        let (_dir, store) = store().await;
        let (app, _) = services(&store).await;
        let domain = store.create_domain(app, fields("example.com")).await.unwrap();
        let certificate = certificate(&store, &["example.com"]).await;
        store.set_domain_certificate(domain.id, Some(certificate)).await.unwrap();

        store.delete_domain(domain.id).await.unwrap();

        store.delete_certificate(certificate).await.unwrap();
    }

    #[tokio::test]
    async fn a_service_with_domains_cannot_be_deleted() {
        let (_dir, store) = store().await;
        let (app, _) = services(&store).await;
        let domain = store.create_domain(app, fields("example.com")).await.unwrap();

        let refused = store.delete_service(app).await;
        store.delete_domain(domain.id).await.unwrap();

        assert!(matches!(refused, Err(StoreError::HasChildren { children: "domains", .. })), "{refused:?}");
        store.delete_service(app).await.unwrap();
    }
}
