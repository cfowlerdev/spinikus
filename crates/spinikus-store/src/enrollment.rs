use async_trait::async_trait;
use spinikus_core::{
    enrollment::{ENROLLMENT_TOKEN_PREFIX, EnrollmentToken},
    id::{EnrollmentTokenId, ServerId},
    secret::Secret,
};

use crate::{
    SqliteStore, StoreError,
    db::format_timestamp,
    error::parse_column,
    secrets::{Sealed, generate_bearer, hash_bearer},
};

const TOKEN: &str = "enrollment token";
const AUTHORITY: &str = "certificate authority";
/// The context the CA's private key is sealed with.
const AUTHORITY_CONTEXT: &[u8] = b"certificate_authority";

/// The internal CA's certificate and private key, both PEM-encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAuthority {
    pub certificate_pem: String,
    pub private_key_pem: Secret,
}

#[async_trait]
pub trait EnrollmentRepository: Send + Sync {
    /// The internal CA, if it has been created.
    async fn get_certificate_authority(&self) -> Result<Option<StoredAuthority>, StoreError>;

    /// Stores the internal CA unless one exists, and returns the stored one: the given
    /// authority, or the one another process stored first.
    async fn create_certificate_authority(&self, authority: StoredAuthority) -> Result<StoredAuthority, StoreError>;

    /// Creates a join token for the server and returns it with the token, which is never
    /// available again. Fails with `NotFound` for unknown or revoked servers.
    async fn create_enrollment_token(
        &self,
        server_id: ServerId,
        expires_at: jiff::Timestamp,
    ) -> Result<(EnrollmentToken, Secret), StoreError>;

    /// Uses up a join token. Returns its server if the token is unused, unexpired, and its
    /// server not revoked; otherwise `None`, whatever the reason.
    async fn redeem_enrollment_token(&self, secret: &str) -> Result<Option<ServerId>, StoreError>;
}

macro_rules! token_columns {
    () => {
        "id, server_id, expires_at, used_at, created_at"
    };
}

#[derive(sqlx::FromRow)]
struct TokenRow {
    id: String,
    server_id: String,
    expires_at: String,
    used_at: Option<String>,
    created_at: String,
}

impl TryFrom<TokenRow> for EnrollmentToken {
    type Error = StoreError;

    fn try_from(row: TokenRow) -> Result<Self, Self::Error> {
        Ok(EnrollmentToken {
            id: parse_column(TOKEN, "id", &row.id)?,
            server_id: parse_column(TOKEN, "server_id", &row.server_id)?,
            expires_at: parse_column(TOKEN, "expires_at", &row.expires_at)?,
            used_at: row.used_at.as_deref().map(|value| parse_column(TOKEN, "used_at", value)).transpose()?,
            created_at: parse_column(TOKEN, "created_at", &row.created_at)?,
        })
    }
}

impl SqliteStore {
    fn open_authority(&self, certificate_pem: String, private_key: Vec<u8>, key_id: &str) -> Result<StoredAuthority, StoreError> {
        let sealed = Sealed { key_id: parse_column(AUTHORITY, "data_key_id", key_id)?, ciphertext: private_key };
        let plaintext = self.keyring().open(&sealed, AUTHORITY_CONTEXT)?;
        let private_key_pem = std::str::from_utf8(&plaintext).map_err(|_| StoreError::InvalidRow {
            entity: AUTHORITY,
            column: "private_key",
            problem: "not UTF-8".to_string(),
        })?;
        Ok(StoredAuthority { certificate_pem, private_key_pem: Secret::new(private_key_pem) })
    }
}

#[async_trait]
impl EnrollmentRepository for SqliteStore {
    async fn get_certificate_authority(&self) -> Result<Option<StoredAuthority>, StoreError> {
        let row: Option<(String, Vec<u8>, String)> =
            sqlx::query_as("SELECT certificate_pem, private_key, data_key_id FROM certificate_authority WHERE id = 1")
                .fetch_optional(self.pool())
                .await?;
        row.map(|(certificate_pem, private_key, key_id)| self.open_authority(certificate_pem, private_key, &key_id)).transpose()
    }

    async fn create_certificate_authority(&self, authority: StoredAuthority) -> Result<StoredAuthority, StoreError> {
        let sealed = self.keyring().seal(authority.private_key_pem.expose().as_bytes(), AUTHORITY_CONTEXT);
        sqlx::query(
            "INSERT INTO certificate_authority (id, certificate_pem, private_key, data_key_id) VALUES (1, ?, ?, ?)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(&authority.certificate_pem)
        .bind(&sealed.ciphertext)
        .bind(sealed.key_id.to_string())
        .execute(self.pool())
        .await?;
        self.get_certificate_authority().await?.ok_or_else(|| StoreError::NotFound { entity: AUTHORITY, id: "1".to_string() })
    }

    async fn create_enrollment_token(
        &self,
        server_id: ServerId,
        expires_at: jiff::Timestamp,
    ) -> Result<(EnrollmentToken, Secret), StoreError> {
        let secret = generate_bearer(ENROLLMENT_TOKEN_PREFIX);
        let row: Option<TokenRow> = sqlx::query_as(concat!(
            "INSERT INTO enrollment_tokens (id, server_id, token_hash, expires_at)
             SELECT ?, id, ?, ? FROM servers WHERE id = ? AND revoked_at IS NULL
             RETURNING ",
            token_columns!()
        ))
        .bind(EnrollmentTokenId::new().to_string())
        .bind(hash_bearer(secret.expose()))
        .bind(format_timestamp(expires_at))
        .bind(server_id.to_string())
        .fetch_optional(self.pool())
        .await?;
        let row = row.ok_or_else(|| StoreError::NotFound { entity: "server", id: server_id.to_string() })?;
        Ok((row.try_into()?, secret))
    }

    async fn redeem_enrollment_token(&self, secret: &str) -> Result<Option<ServerId>, StoreError> {
        let row: Option<TokenRow> = sqlx::query_as(concat!(
            "UPDATE enrollment_tokens SET used_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE token_hash = ? AND used_at IS NULL AND expires_at > strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               AND EXISTS (SELECT 1 FROM servers WHERE servers.id = server_id AND revoked_at IS NULL)
             RETURNING ",
            token_columns!()
        ))
        .bind(hash_bearer(secret))
        .fetch_optional(self.pool())
        .await?;
        row.map(|row| EnrollmentToken::try_from(row).map(|token| token.server_id)).transpose()
    }
}

#[cfg(test)]
mod tests {
    use jiff::{SignedDuration, Timestamp};
    use spinikus_core::server::ServerFields;

    use super::*;
    use crate::{ServerRepository, test_support::store};

    async fn server(store: &SqliteStore) -> ServerId {
        store.create_server(ServerFields { name: "web-1".to_string() }).await.unwrap().id
    }

    fn in_an_hour() -> Timestamp {
        Timestamp::now() + SignedDuration::from_hours(1)
    }

    #[tokio::test]
    async fn stores_one_authority_with_a_sealed_key() {
        let (_dir, store) = store().await;
        let first = StoredAuthority { certificate_pem: "first".to_string(), private_key_pem: Secret::new("key one") };
        let second = StoredAuthority { certificate_pem: "second".to_string(), private_key_pem: Secret::new("key two") };

        assert_eq!(store.get_certificate_authority().await.unwrap(), None);
        assert_eq!(store.create_certificate_authority(first.clone()).await.unwrap(), first);
        assert_eq!(store.create_certificate_authority(second).await.unwrap(), first, "the first one stays");
        assert_eq!(store.get_certificate_authority().await.unwrap(), Some(first));
        let (stored,): (Vec<u8>,) =
            sqlx::query_as("SELECT private_key FROM certificate_authority").fetch_one(store.pool()).await.unwrap();
        assert!(!stored.windows(7).any(|window| window == b"key one"), "the key is encrypted");
    }

    #[tokio::test]
    async fn join_tokens_work_once() {
        let (_dir, store) = store().await;
        let id = server(&store).await;

        let (token, secret) = store.create_enrollment_token(id, in_an_hour()).await.unwrap();

        assert!(secret.expose().starts_with(ENROLLMENT_TOKEN_PREFIX));
        assert_eq!((token.server_id, token.used_at), (id, None));
        assert_eq!(store.redeem_enrollment_token(secret.expose()).await.unwrap(), Some(id));
        assert_eq!(store.redeem_enrollment_token(secret.expose()).await.unwrap(), None);
        assert_eq!(store.redeem_enrollment_token("spj_unknown").await.unwrap(), None);
    }

    #[tokio::test]
    async fn expired_tokens_and_revoked_servers_do_not_enroll() {
        let (_dir, store) = store().await;
        let id = server(&store).await;
        let (_, expired) = store.create_enrollment_token(id, Timestamp::now() - SignedDuration::from_secs(1)).await.unwrap();
        let (_, unused) = store.create_enrollment_token(id, in_an_hour()).await.unwrap();
        store.revoke_server(id).await.unwrap();

        assert_eq!(store.redeem_enrollment_token(expired.expose()).await.unwrap(), None);
        assert_eq!(store.redeem_enrollment_token(unused.expose()).await.unwrap(), None);
        assert!(matches!(store.create_enrollment_token(id, in_an_hour()).await, Err(StoreError::NotFound { .. })));
        let unknown = store.create_enrollment_token(ServerId::new(), in_an_hour()).await;
        assert!(matches!(unknown, Err(StoreError::NotFound { entity: "server", .. })));
    }
}
