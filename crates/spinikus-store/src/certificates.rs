use async_trait::async_trait;
use jiff::Timestamp;
use spinikus_core::{
    certificate::{AcmeAccount, Certificate, NewAcmeAccount, NewCertificate},
    domain::Hostname,
    id::{AcmeAccountId, CertificateId},
    secret::Secret,
};
use sqlx::SqliteConnection;

use crate::{Keyring, Sealed, SqliteStore, StoreError, db::format_timestamp, error::parse_column};

const ENTITY: &str = "certificate";
const ACCOUNT: &str = "acme account";

/// Certificates and ACME accounts. Written by the certificate manager, not by users directly.
#[async_trait]
pub trait CertificateRepository: Send + Sync {
    /// Fails with `Conflict` if the directory already has an account for this email.
    async fn create_acme_account(&self, new: NewAcmeAccount) -> Result<AcmeAccount, StoreError>;

    async fn find_acme_account(&self, directory_url: &str, email: &str) -> Result<Option<AcmeAccount>, StoreError>;

    async fn acme_account_credentials(&self, id: AcmeAccountId) -> Result<Secret, StoreError>;

    /// Fails with `NotFound` if the ACME account does not exist.
    async fn create_certificate(&self, new: NewCertificate) -> Result<Certificate, StoreError>;

    async fn get_certificate(&self, id: CertificateId) -> Result<Certificate, StoreError>;

    /// All certificates, oldest first.
    async fn list_certificates(&self) -> Result<Vec<Certificate>, StoreError>;

    /// Certificates that expire before `at`, soonest first, for renewal.
    async fn list_certificates_expiring_before(&self, at: Timestamp) -> Result<Vec<Certificate>, StoreError>;

    async fn certificate_private_key(&self, id: CertificateId) -> Result<Secret, StoreError>;

    /// Deletes the certificate. Refused while a domain uses it.
    async fn delete_certificate(&self, id: CertificateId) -> Result<(), StoreError>;
}

macro_rules! columns {
    () => {
        "id, hostnames, chain_pem, not_before, not_after, acme_account_id, created_at"
    };
}

#[derive(sqlx::FromRow)]
struct CertificateRow {
    id: String,
    hostnames: String,
    chain_pem: String,
    not_before: String,
    not_after: String,
    acme_account_id: Option<String>,
    created_at: String,
}

impl TryFrom<CertificateRow> for Certificate {
    type Error = StoreError;

    fn try_from(row: CertificateRow) -> Result<Self, Self::Error> {
        Ok(Certificate {
            id: parse_column(ENTITY, "id", &row.id)?,
            hostnames: serde_json::from_str::<Vec<Hostname>>(&row.hostnames).map_err(|error| StoreError::InvalidRow {
                entity: ENTITY,
                column: "hostnames",
                problem: error.to_string(),
            })?,
            chain_pem: row.chain_pem,
            not_before: parse_column(ENTITY, "not_before", &row.not_before)?,
            not_after: parse_column(ENTITY, "not_after", &row.not_after)?,
            acme_account_id: row.acme_account_id.map(|id| parse_column(ENTITY, "acme_account_id", &id)).transpose()?,
            created_at: parse_column(ENTITY, "created_at", &row.created_at)?,
        })
    }
}

#[derive(sqlx::FromRow)]
struct AccountRow {
    id: String,
    directory_url: String,
    email: String,
    created_at: String,
}

impl TryFrom<AccountRow> for AcmeAccount {
    type Error = StoreError;

    fn try_from(row: AccountRow) -> Result<Self, Self::Error> {
        Ok(AcmeAccount {
            id: parse_column(ACCOUNT, "id", &row.id)?,
            directory_url: row.directory_url,
            email: row.email,
            created_at: parse_column(ACCOUNT, "created_at", &row.created_at)?,
        })
    }
}

/// Runs `query` (selecting a sealed column and its data key ID) for row `id`, and decrypts the value,
/// which was sealed with the row's ID as context.
async fn open_sealed(store: &SqliteStore, query: &'static str, entity: &'static str, id: String) -> Result<Secret, StoreError> {
    let row: Option<(Vec<u8>, String)> = sqlx::query_as(query).bind(&id).fetch_optional(store.pool()).await?;
    let (ciphertext, key_id) = row.ok_or_else(|| StoreError::NotFound { entity, id: id.clone() })?;
    let sealed = Sealed { key_id: parse_column(entity, "data_key_id", &key_id)?, ciphertext };
    secret_from(store.keyring(), &sealed, entity, &id)
}

fn secret_from(keyring: &Keyring, sealed: &Sealed, entity: &'static str, id: &str) -> Result<Secret, StoreError> {
    let plaintext = keyring.open(sealed, id.as_bytes())?;
    let text = std::str::from_utf8(&plaintext).map_err(|_| StoreError::InvalidRow {
        entity,
        column: "secret",
        problem: "not UTF-8".to_string(),
    })?;
    Ok(Secret::new(text))
}

pub(crate) async fn fetch_certificate(conn: &mut SqliteConnection, id: CertificateId) -> Result<Certificate, StoreError> {
    let row: Option<CertificateRow> = sqlx::query_as(concat!("SELECT ", columns!(), " FROM certificates WHERE id = ?"))
        .bind(id.to_string())
        .fetch_optional(conn)
        .await?;
    row.ok_or_else(|| StoreError::NotFound { entity: ENTITY, id: id.to_string() })?.try_into()
}

#[async_trait]
impl CertificateRepository for SqliteStore {
    async fn create_acme_account(&self, new: NewAcmeAccount) -> Result<AcmeAccount, StoreError> {
        let id = AcmeAccountId::new();
        let sealed = self.keyring().seal(new.credentials.expose().as_bytes(), id.to_string().as_bytes());
        let row: AccountRow = sqlx::query_as(
            "INSERT INTO acme_accounts (id, directory_url, email, credentials, data_key_id) VALUES (?, ?, ?, ?, ?)
             RETURNING id, directory_url, email, created_at",
        )
        .bind(id.to_string())
        .bind(&new.directory_url)
        .bind(&new.email)
        .bind(sealed.ciphertext)
        .bind(sealed.key_id.to_string())
        .fetch_one(self.pool())
        .await
        .map_err(|error| {
            StoreError::on_unique_violation(error, |_| StoreError::Conflict {
                entity: ACCOUNT,
                field: "email",
                value: new.email.clone(),
            })
        })?;

        row.try_into()
    }

    async fn find_acme_account(&self, directory_url: &str, email: &str) -> Result<Option<AcmeAccount>, StoreError> {
        let row: Option<AccountRow> = sqlx::query_as(
            "SELECT id, directory_url, email, created_at FROM acme_accounts WHERE directory_url = ? AND email = ?",
        )
        .bind(directory_url)
        .bind(email)
        .fetch_optional(self.pool())
        .await?;

        row.map(AcmeAccount::try_from).transpose()
    }

    async fn acme_account_credentials(&self, id: AcmeAccountId) -> Result<Secret, StoreError> {
        open_sealed(self, "SELECT credentials, data_key_id FROM acme_accounts WHERE id = ?", ACCOUNT, id.to_string()).await
    }

    async fn create_certificate(&self, new: NewCertificate) -> Result<Certificate, StoreError> {
        let id = CertificateId::new();
        let sealed = self.keyring().seal(new.private_key_pem.expose().as_bytes(), id.to_string().as_bytes());
        let hostnames = serde_json::to_string(&new.hostnames).expect("hostnames serialize to JSON");
        let mut tx = self.begin_write().await?;

        if let Some(account_id) = new.acme_account_id {
            let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM acme_accounts WHERE id = ?")
                .bind(account_id.to_string())
                .fetch_optional(&mut *tx)
                .await?;
            if exists.is_none() {
                return Err(StoreError::NotFound { entity: ACCOUNT, id: account_id.to_string() });
            }
        }

        let row: CertificateRow = sqlx::query_as(concat!(
            "INSERT INTO certificates (id, hostnames, chain_pem, private_key, data_key_id, not_before, not_after, acme_account_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             RETURNING ",
            columns!()
        ))
        .bind(id.to_string())
        .bind(hostnames)
        .bind(&new.chain_pem)
        .bind(sealed.ciphertext)
        .bind(sealed.key_id.to_string())
        .bind(format_timestamp(new.not_before))
        .bind(format_timestamp(new.not_after))
        .bind(new.acme_account_id.map(|id| id.to_string()))
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;
        row.try_into()
    }

    async fn get_certificate(&self, id: CertificateId) -> Result<Certificate, StoreError> {
        fetch_certificate(&mut *self.pool().acquire().await?, id).await
    }

    async fn list_certificates(&self) -> Result<Vec<Certificate>, StoreError> {
        let rows: Vec<CertificateRow> =
            sqlx::query_as(concat!("SELECT ", columns!(), " FROM certificates ORDER BY id")).fetch_all(self.pool()).await?;
        rows.into_iter().map(Certificate::try_from).collect()
    }

    async fn list_certificates_expiring_before(&self, at: Timestamp) -> Result<Vec<Certificate>, StoreError> {
        let rows: Vec<CertificateRow> =
            sqlx::query_as(concat!("SELECT ", columns!(), " FROM certificates WHERE not_after < ? ORDER BY not_after, id"))
                .bind(format_timestamp(at))
                .fetch_all(self.pool())
                .await?;
        rows.into_iter().map(Certificate::try_from).collect()
    }

    async fn certificate_private_key(&self, id: CertificateId) -> Result<Secret, StoreError> {
        open_sealed(self, "SELECT private_key, data_key_id FROM certificates WHERE id = ?", ENTITY, id.to_string()).await
    }

    async fn delete_certificate(&self, id: CertificateId) -> Result<(), StoreError> {
        let result = sqlx::query(
            "DELETE FROM certificates WHERE id = ? AND NOT EXISTS (SELECT 1 FROM domains WHERE certificate_id = certificates.id)",
        )
        .bind(id.to_string())
        .execute(self.pool())
        .await?;

        if result.rows_affected() == 0 {
            // Either the certificate is gone, or domains still use it.
            self.get_certificate(id).await?;
            return Err(StoreError::HasChildren { entity: ENTITY, id: id.to_string(), children: "domains" });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::store;

    const LETS_ENCRYPT: &str = "https://acme-v02.api.letsencrypt.org/directory";

    fn new_certificate(hostnames: &[&str], not_after: &str) -> NewCertificate {
        NewCertificate {
            hostnames: hostnames.iter().map(|name| name.parse().unwrap()).collect(),
            chain_pem: "-----BEGIN CERTIFICATE-----\n...".to_string(),
            private_key_pem: Secret::new("-----BEGIN PRIVATE KEY-----\nsecret"),
            not_before: "2026-10-01T00:00:00Z".parse().unwrap(),
            not_after: not_after.parse().unwrap(),
            acme_account_id: None,
        }
    }

    fn new_account(email: &str) -> NewAcmeAccount {
        NewAcmeAccount {
            directory_url: LETS_ENCRYPT.to_string(),
            email: email.to_string(),
            credentials: Secret::new(r#"{"id":"https://acme/acct/1","key_pkcs8":"..."}"#),
        }
    }

    #[tokio::test]
    async fn stores_acme_accounts_with_encrypted_credentials() {
        let (_dir, store) = store().await;

        let account = store.create_acme_account(new_account("ops@example.com")).await.unwrap();
        let duplicate = store.create_acme_account(new_account("ops@example.com")).await;

        let found = store.find_acme_account(LETS_ENCRYPT, "ops@example.com").await.unwrap();
        assert_eq!(found, Some(account.clone()));
        assert_eq!(store.find_acme_account(LETS_ENCRYPT, "other@example.com").await.unwrap(), None);
        assert!(store.acme_account_credentials(account.id).await.unwrap().expose().contains("key_pkcs8"));
        assert!(matches!(duplicate, Err(StoreError::Conflict { field: "email", .. })), "{duplicate:?}");
    }

    #[tokio::test]
    async fn stores_certificates_with_encrypted_keys() {
        let (_dir, store) = store().await;
        let account = store.create_acme_account(new_account("ops@example.com")).await.unwrap();

        let certificate = store
            .create_certificate(NewCertificate {
                acme_account_id: Some(account.id),
                ..new_certificate(&["example.com", "*.example.com"], "2027-01-01T00:00:00Z")
            })
            .await
            .unwrap();

        assert_eq!(store.get_certificate(certificate.id).await.unwrap(), certificate);
        assert_eq!(certificate.hostnames.len(), 2);
        assert_eq!(certificate.acme_account_id, Some(account.id));
        assert_eq!(store.certificate_private_key(certificate.id).await.unwrap().expose(), "-----BEGIN PRIVATE KEY-----\nsecret");
        let stored: Vec<u8> = sqlx::query_scalar("SELECT private_key FROM certificates").fetch_one(store.pool()).await.unwrap();
        assert!(!stored.windows(6).any(|window| window == b"secret"));
    }

    #[tokio::test]
    async fn requires_an_existing_acme_account() {
        let (_dir, store) = store().await;

        let result = store
            .create_certificate(NewCertificate {
                acme_account_id: Some(AcmeAccountId::new()),
                ..new_certificate(&["example.com"], "2027-01-01T00:00:00Z")
            })
            .await;

        assert!(matches!(result, Err(StoreError::NotFound { entity: "acme account", .. })), "{result:?}");
    }

    #[tokio::test]
    async fn lists_certificates_expiring_soonest_first() {
        let (_dir, store) = store().await;
        let late = store.create_certificate(new_certificate(&["a.example.com"], "2027-03-01T00:00:00Z")).await.unwrap();
        let soon = store.create_certificate(new_certificate(&["b.example.com"], "2026-11-15T00:00:00Z")).await.unwrap();
        let sooner = store.create_certificate(new_certificate(&["c.example.com"], "2026-11-10T00:00:00Z")).await.unwrap();

        let expiring = store.list_certificates_expiring_before("2026-12-01T00:00:00Z".parse().unwrap()).await.unwrap();
        let all = store.list_certificates().await.unwrap();

        assert_eq!(expiring.iter().map(|c| c.id).collect::<Vec<_>>(), vec![sooner.id, soon.id]);
        assert_eq!(all.iter().map(|c| c.id).collect::<Vec<_>>(), vec![late.id, soon.id, sooner.id]);
    }

    #[tokio::test]
    async fn deletes_unused_certificates() {
        let (_dir, store) = store().await;
        let certificate = store.create_certificate(new_certificate(&["example.com"], "2027-01-01T00:00:00Z")).await.unwrap();

        store.delete_certificate(certificate.id).await.unwrap();

        assert!(matches!(store.get_certificate(certificate.id).await, Err(StoreError::NotFound { .. })));
        assert!(matches!(store.delete_certificate(certificate.id).await, Err(StoreError::NotFound { .. })));
    }
}
