use async_trait::async_trait;
use spinikus_core::{
    id::{ApiTokenId, UserId},
    membership::MembershipScope,
    secret::Secret,
    token::{API_TOKEN_PREFIX, ApiToken, NewApiToken}
};
use sqlx::SqliteConnection;

use crate::{
    SqliteStore, StoreError,
    db::format_timestamp,
    error::parse_column,
    secrets::{generate_bearer, hash_bearer}
};

const ENTITY: &str = "api token";

#[async_trait]
pub trait ApiTokenRepository: Send + Sync {
    /// Creates a token and returns it with its secret, which is never available again.
    /// Fails with `NotFound` for a missing user or an inactive scope, and `Rejected` for a disabled user.
    async fn create_api_token(&self, new: NewApiToken) -> Result<(ApiToken, Secret), StoreError>;

    /// The token with this secret, if it is valid: not revoked, not expired, and its user not
    /// disabled. Records the use, at most once a minute.
    async fn authenticate_api_token(&self, secret: &str) -> Result<Option<ApiToken>, StoreError>;

    async fn get_api_token(&self, id: ApiTokenId) -> Result<ApiToken, StoreError>;

    /// The user's tokens, revoked ones included, in creation order.
    async fn list_user_api_tokens(&self, user_id: UserId) -> Result<Vec<ApiToken>, StoreError>;

    /// Revokes the token; revoking it again changes nothing.
    async fn revoke_api_token(&self, id: ApiTokenId) -> Result<ApiToken, StoreError>;
}

macro_rules! columns {
    () => {
        "id, user_id, name, role, project_id, environment_id, expires_at, last_used_at, created_at, revoked_at"
    };
}

#[derive(sqlx::FromRow)]
struct ApiTokenRow {
    id: String,
    user_id: String,
    name: String,
    role: String,
    project_id: Option<String>,
    environment_id: Option<String>,
    expires_at: Option<String>,
    last_used_at: Option<String>,
    created_at: String,
    revoked_at: Option<String>
}

impl TryFrom<ApiTokenRow> for ApiToken {
    type Error = StoreError;

    fn try_from(row: ApiTokenRow) -> Result<Self, Self::Error> {
        let scope = match (&row.project_id, &row.environment_id) {
            (None, None) => None,
            (Some(id), None) => Some(MembershipScope::Project(parse_column(ENTITY, "project_id", id)?)),
            (None, Some(id)) => Some(MembershipScope::Environment(parse_column(ENTITY, "environment_id", id)?)),
            (Some(_), Some(_)) => {
                return Err(StoreError::InvalidRow { entity: ENTITY, column: "scope", problem: "two scopes".to_string() });
            }
        };
        let timestamp = |column, value: Option<String>| value.map(|at| parse_column(ENTITY, column, &at)).transpose();
        Ok(ApiToken {
            id: parse_column(ENTITY, "id", &row.id)?,
            user_id: parse_column(ENTITY, "user_id", &row.user_id)?,
            name: row.name,
            role: parse_column(ENTITY, "role", &row.role)?,
            scope,
            expires_at: timestamp("expires_at", row.expires_at)?,
            last_used_at: timestamp("last_used_at", row.last_used_at)?,
            created_at: parse_column(ENTITY, "created_at", &row.created_at)?,
            revoked_at: timestamp("revoked_at", row.revoked_at)?
        })
    }
}

fn not_found(id: ApiTokenId) -> StoreError {
    StoreError::NotFound { entity: ENTITY, id: id.to_string() }
}

/// Fails with `NotFound` for a missing user and `Rejected` for a disabled one, who must not
/// get new credentials. Shared with sessions.
pub(crate) async fn check_user_active(conn: &mut SqliteConnection, user_id: UserId) -> Result<(), StoreError> {
    let disabled: Option<(bool,)> = sqlx::query_as("SELECT disabled_at IS NOT NULL FROM users WHERE id = ?")
        .bind(user_id.to_string())
        .fetch_optional(conn)
        .await?;
    match disabled {
        None => Err(StoreError::NotFound { entity: "user", id: user_id.to_string() }),
        Some((true,)) => Err(StoreError::Rejected { entity: "user", id: user_id.to_string(), problem: "the user is disabled" }),
        Some((false,)) => Ok(())
    }
}

#[async_trait]
impl ApiTokenRepository for SqliteStore {
    async fn create_api_token(&self, new: NewApiToken) -> Result<(ApiToken, Secret), StoreError> {
        check_user_active(&mut *self.pool().acquire().await?, new.user_id).await?;
        if let Some(scope) = new.scope {
            self.check_membership_scope(scope).await?;
        }

        let secret = generate_bearer(API_TOKEN_PREFIX);
        let (project_id, environment_id) = match new.scope {
            None => (None, None),
            Some(MembershipScope::Project(id)) => (Some(id.to_string()), None),
            Some(MembershipScope::Environment(id)) => (None, Some(id.to_string()))
        };
        let row: ApiTokenRow = sqlx::query_as(concat!(
            "INSERT INTO api_tokens (id, user_id, name, token_hash, role, project_id, environment_id, expires_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             RETURNING ",
            columns!()
        ))
        .bind(ApiTokenId::new().to_string())
        .bind(new.user_id.to_string())
        .bind(&new.name)
        .bind(hash_bearer(secret.expose()))
        .bind(new.role.as_str())
        .bind(project_id)
        .bind(environment_id)
        .bind(new.expires_at.map(format_timestamp))
        .fetch_one(self.pool())
        .await?;

        Ok((row.try_into()?, secret))
    }

    async fn authenticate_api_token(&self, secret: &str) -> Result<Option<ApiToken>, StoreError> {
        if !secret.starts_with(API_TOKEN_PREFIX) {
            return Ok(None);
        }
        let row: Option<ApiTokenRow> = sqlx::query_as(
            "SELECT t.id, t.user_id, t.name, t.role, t.project_id, t.environment_id, t.expires_at, t.last_used_at,
                    t.created_at, t.revoked_at
             FROM api_tokens t JOIN users u ON u.id = t.user_id
             WHERE t.token_hash = ? AND t.revoked_at IS NULL AND u.disabled_at IS NULL
               AND (t.expires_at IS NULL OR t.expires_at > strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))"
        )
        .bind(hash_bearer(secret))
        .fetch_optional(self.pool())
        .await?;
        let Some(row) = row else { return Ok(None) };

        // Throttled, so a busy CI token does not turn every request into a write.
        let touched: Option<ApiTokenRow> = sqlx::query_as(concat!(
            "UPDATE api_tokens SET last_used_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ? AND (last_used_at IS NULL OR last_used_at < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-60 seconds'))
             RETURNING ",
            columns!()
        ))
        .bind(&row.id)
        .fetch_optional(self.pool())
        .await?;

        touched.unwrap_or(row).try_into().map(Some)
    }

    async fn get_api_token(&self, id: ApiTokenId) -> Result<ApiToken, StoreError> {
        let row: Option<ApiTokenRow> = sqlx::query_as(concat!("SELECT ", columns!(), " FROM api_tokens WHERE id = ?"))
            .bind(id.to_string())
            .fetch_optional(self.pool())
            .await?;
        row.ok_or_else(|| not_found(id))?.try_into()
    }

    async fn list_user_api_tokens(&self, user_id: UserId) -> Result<Vec<ApiToken>, StoreError> {
        let rows: Vec<ApiTokenRow> =
            sqlx::query_as(concat!("SELECT ", columns!(), " FROM api_tokens WHERE user_id = ? ORDER BY id"))
                .bind(user_id.to_string())
                .fetch_all(self.pool())
                .await?;
        rows.into_iter().map(ApiToken::try_from).collect()
    }

    async fn revoke_api_token(&self, id: ApiTokenId) -> Result<ApiToken, StoreError> {
        sqlx::query("UPDATE api_tokens SET revoked_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ? AND revoked_at IS NULL")
            .bind(id.to_string())
            .execute(self.pool())
            .await?;
        self.get_api_token(id).await
    }
}

#[cfg(test)]
mod tests {
    use jiff::{SignedDuration, Timestamp};
    use spinikus_core::{
        id::ProjectId,
        membership::Role,
        project::ProjectFields,
        user::UserFields
    };

    use super::*;
    use crate::{ProjectRepository, UserRepository, test_support::store};

    async fn user(store: &SqliteStore, email: &str) -> UserId {
        let fields = UserFields { email: email.parse().unwrap(), name: "Ada".to_string(), admin: false };
        store.create_user(fields, None).await.unwrap().id
    }

    fn new_token(user_id: UserId) -> NewApiToken {
        NewApiToken { user_id, name: "CI".to_string(), role: Role::Developer, scope: None, expires_at: None }
    }

    #[tokio::test]
    async fn creates_a_token_shown_once_and_stored_hashed() {
        let (_dir, store) = store().await;
        let user_id = user(&store, "ada@example.com").await;

        let (token, secret) = store.create_api_token(new_token(user_id)).await.unwrap();

        assert!(secret.expose().starts_with("spk_"));
        assert_eq!(secret.expose().len(), "spk_".len() + 43);
        assert_eq!(store.get_api_token(token.id).await.unwrap(), token);
        let stored: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM api_tokens").fetch_one(store.pool()).await.unwrap();
        assert_eq!(stored, hash_bearer(secret.expose()));
    }

    #[tokio::test]
    async fn authenticates_valid_tokens_and_records_use() {
        let (_dir, store) = store().await;
        let user_id = user(&store, "ada@example.com").await;
        let (token, secret) = store.create_api_token(new_token(user_id)).await.unwrap();

        let authenticated = store.authenticate_api_token(secret.expose()).await.unwrap().unwrap();
        let again = store.authenticate_api_token(secret.expose()).await.unwrap().unwrap();

        assert_eq!(authenticated.id, token.id);
        assert!(authenticated.last_used_at.is_some());
        assert_eq!(again.last_used_at, authenticated.last_used_at, "touched at most once a minute");
        assert_eq!(store.authenticate_api_token("spk_wrong").await.unwrap(), None);
        assert_eq!(store.authenticate_api_token("no-prefix").await.unwrap(), None);
    }

    #[tokio::test]
    async fn rejects_revoked_expired_and_disabled_users_tokens() {
        let (_dir, store) = store().await;
        let ada = user(&store, "ada@example.com").await;
        let bob = user(&store, "bob@example.com").await;
        let (revoked, revoked_secret) = store.create_api_token(new_token(ada)).await.unwrap();
        let expired = NewApiToken { expires_at: Some(Timestamp::now() - SignedDuration::from_secs(1)), ..new_token(ada) };
        let (_, expired_secret) = store.create_api_token(expired).await.unwrap();
        let (_, bobs_secret) = store.create_api_token(new_token(bob)).await.unwrap();

        let revoked = store.revoke_api_token(revoked.id).await.unwrap();
        let revoked_again = store.revoke_api_token(revoked.id).await.unwrap();
        store.disable_user(bob).await.unwrap();

        assert!(revoked.revoked_at.is_some());
        assert_eq!(revoked_again, revoked);
        for secret in [revoked_secret, expired_secret, bobs_secret] {
            assert_eq!(store.authenticate_api_token(secret.expose()).await.unwrap(), None);
        }
        assert_eq!(store.list_user_api_tokens(ada).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn requires_an_active_user_and_scope() {
        let (_dir, store) = store().await;
        let ada = user(&store, "ada@example.com").await;
        let project = store
            .create_project(ProjectFields { name: "Web".to_string(), slug: "web".parse().unwrap() })
            .await
            .unwrap()
            .id;
        let scoped = NewApiToken { scope: Some(MembershipScope::Project(project)), ..new_token(ada) };
        let (token, _) = store.create_api_token(scoped).await.unwrap();
        store.disable_user(ada).await.unwrap();

        let missing_user = store.create_api_token(new_token(UserId::new())).await;
        let disabled_user = store.create_api_token(new_token(ada)).await;
        store.enable_user(ada).await.unwrap();
        let missing_scope =
            store.create_api_token(NewApiToken { scope: Some(MembershipScope::Project(ProjectId::new())), ..new_token(ada) }).await;

        assert_eq!(token.scope, Some(MembershipScope::Project(project)));
        assert!(matches!(missing_user, Err(StoreError::NotFound { entity: "user", .. })), "{missing_user:?}");
        assert!(matches!(disabled_user, Err(StoreError::Rejected { .. })), "{disabled_user:?}");
        assert!(matches!(missing_scope, Err(StoreError::NotFound { entity: "project", .. })), "{missing_scope:?}");
    }
}
