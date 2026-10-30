use async_trait::async_trait;
use spinikus_core::{
    id::{SessionId, UserId},
    secret::Secret,
    token::{NewSession, Session}
};

use crate::{
    SqliteStore, StoreError,
    api_tokens::check_user_active,
    db::format_timestamp,
    error::parse_column,
    secrets::{generate_bearer, hash_bearer}
};

const ENTITY: &str = "session";

#[async_trait]
pub trait SessionRepository: Send + Sync {
    /// Starts a session and returns it with the cookie value, which is never available again.
    /// Fails with `NotFound` for a missing user and `Rejected` for a disabled one.
    async fn create_session(&self, new: NewSession) -> Result<(Session, Secret), StoreError>;

    /// The session with this cookie value, if it has not expired and its user is not disabled.
    /// Records the activity, at most once a minute.
    async fn authenticate_session(&self, secret: &str) -> Result<Option<Session>, StoreError>;

    /// The user's unexpired sessions, oldest first.
    async fn list_user_sessions(&self, user_id: UserId) -> Result<Vec<Session>, StoreError>;

    /// Ends one session, such as on sign-out.
    async fn delete_session(&self, id: SessionId) -> Result<(), StoreError>;

    /// Ends all of a user's sessions, such as after a password change. Returns how many.
    async fn delete_user_sessions(&self, user_id: UserId) -> Result<u64, StoreError>;

    /// Deletes expired sessions. Returns how many.
    async fn delete_expired_sessions(&self) -> Result<u64, StoreError>;
}

macro_rules! columns {
    () => {
        "id, user_id, expires_at, last_seen_at, user_agent, ip_address, created_at"
    };
}

#[derive(sqlx::FromRow)]
struct SessionRow {
    id: String,
    user_id: String,
    expires_at: String,
    last_seen_at: String,
    user_agent: Option<String>,
    ip_address: Option<String>,
    created_at: String
}

impl TryFrom<SessionRow> for Session {
    type Error = StoreError;

    fn try_from(row: SessionRow) -> Result<Self, Self::Error> {
        Ok(Session {
            id: parse_column(ENTITY, "id", &row.id)?,
            user_id: parse_column(ENTITY, "user_id", &row.user_id)?,
            expires_at: parse_column(ENTITY, "expires_at", &row.expires_at)?,
            last_seen_at: parse_column(ENTITY, "last_seen_at", &row.last_seen_at)?,
            user_agent: row.user_agent,
            ip_address: row.ip_address,
            created_at: parse_column(ENTITY, "created_at", &row.created_at)?
        })
    }
}

#[async_trait]
impl SessionRepository for SqliteStore {
    async fn create_session(&self, new: NewSession) -> Result<(Session, Secret), StoreError> {
        check_user_active(&mut *self.pool().acquire().await?, new.user_id).await?;

        let secret = generate_bearer("");
        let row: SessionRow = sqlx::query_as(concat!(
            "INSERT INTO sessions (id, user_id, token_hash, expires_at, user_agent, ip_address)
             VALUES (?, ?, ?, ?, ?, ?)
             RETURNING ",
            columns!()
        ))
        .bind(SessionId::new().to_string())
        .bind(new.user_id.to_string())
        .bind(hash_bearer(secret.expose()))
        .bind(format_timestamp(new.expires_at))
        .bind(&new.user_agent)
        .bind(&new.ip_address)
        .fetch_one(self.pool())
        .await?;

        Ok((row.try_into()?, secret))
    }

    async fn authenticate_session(&self, secret: &str) -> Result<Option<Session>, StoreError> {
        let row: Option<SessionRow> = sqlx::query_as(
            "SELECT s.id, s.user_id, s.expires_at, s.last_seen_at, s.user_agent, s.ip_address, s.created_at
             FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = ? AND u.disabled_at IS NULL AND s.expires_at > strftime('%Y-%m-%dT%H:%M:%fZ', 'now')"
        )
        .bind(hash_bearer(secret))
        .fetch_optional(self.pool())
        .await?;
        let Some(row) = row else { return Ok(None) };

        // Throttled, so page loads do not each turn into a write.
        let touched: Option<SessionRow> = sqlx::query_as(concat!(
            "UPDATE sessions SET last_seen_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ? AND last_seen_at < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-60 seconds')
             RETURNING ",
            columns!()
        ))
        .bind(&row.id)
        .fetch_optional(self.pool())
        .await?;

        touched.unwrap_or(row).try_into().map(Some)
    }

    async fn list_user_sessions(&self, user_id: UserId) -> Result<Vec<Session>, StoreError> {
        let rows: Vec<SessionRow> = sqlx::query_as(concat!(
            "SELECT ", columns!(), " FROM sessions
             WHERE user_id = ? AND expires_at > strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             ORDER BY id"
        ))
        .bind(user_id.to_string())
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(Session::try_from).collect()
    }

    async fn delete_session(&self, id: SessionId) -> Result<(), StoreError> {
        let result = sqlx::query("DELETE FROM sessions WHERE id = ?").bind(id.to_string()).execute(self.pool()).await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound { entity: ENTITY, id: id.to_string() });
        }
        Ok(())
    }

    async fn delete_user_sessions(&self, user_id: UserId) -> Result<u64, StoreError> {
        let result = sqlx::query("DELETE FROM sessions WHERE user_id = ?").bind(user_id.to_string()).execute(self.pool()).await?;
        Ok(result.rows_affected())
    }

    async fn delete_expired_sessions(&self) -> Result<u64, StoreError> {
        let result = sqlx::query("DELETE FROM sessions WHERE expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ', 'now')")
            .execute(self.pool())
            .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use jiff::{SignedDuration, Timestamp};
    use spinikus_core::user::UserFields;

    use super::*;
    use crate::{UserRepository, test_support::store};

    async fn user(store: &SqliteStore, email: &str) -> UserId {
        let fields = UserFields { email: email.parse().unwrap(), name: "Ada".to_string(), admin: false };
        store.create_user(fields, None).await.unwrap().id
    }

    fn new_session(user_id: UserId, expires_in: SignedDuration) -> NewSession {
        NewSession {
            user_id,
            expires_at: Timestamp::now() + expires_in,
            user_agent: Some("Firefox".to_string()),
            ip_address: Some("203.0.113.7".to_string())
        }
    }

    #[tokio::test]
    async fn authenticates_live_sessions() {
        let (_dir, store) = store().await;
        let ada = user(&store, "ada@example.com").await;

        let (session, secret) = store.create_session(new_session(ada, SignedDuration::from_hours(12))).await.unwrap();
        let authenticated = store.authenticate_session(secret.expose()).await.unwrap();

        assert_eq!(authenticated, Some(session.clone()));
        assert_eq!(session.user_agent.as_deref(), Some("Firefox"));
        assert_eq!(store.authenticate_session("not-a-session").await.unwrap(), None);
        let stored: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM sessions").fetch_one(store.pool()).await.unwrap();
        assert_eq!(stored, hash_bearer(secret.expose()));
    }

    #[tokio::test]
    async fn rejects_expired_sessions_and_disabled_users() {
        let (_dir, store) = store().await;
        let ada = user(&store, "ada@example.com").await;
        let bob = user(&store, "bob@example.com").await;
        let (_, expired) = store.create_session(new_session(ada, SignedDuration::from_secs(-1))).await.unwrap();
        let (_, bobs) = store.create_session(new_session(bob, SignedDuration::from_hours(1))).await.unwrap();
        store.disable_user(bob).await.unwrap();

        assert_eq!(store.authenticate_session(expired.expose()).await.unwrap(), None);
        assert_eq!(store.authenticate_session(bobs.expose()).await.unwrap(), None);
        assert!(store.list_user_sessions(ada).await.unwrap().is_empty());
        assert!(matches!(store.create_session(new_session(bob, SignedDuration::from_hours(1))).await, Err(StoreError::Rejected { .. })));
        assert_eq!(store.delete_expired_sessions().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn ends_one_or_all_of_a_users_sessions() {
        let (_dir, store) = store().await;
        let ada = user(&store, "ada@example.com").await;
        let (first, first_secret) = store.create_session(new_session(ada, SignedDuration::from_hours(1))).await.unwrap();
        for _ in 0..2 {
            store.create_session(new_session(ada, SignedDuration::from_hours(1))).await.unwrap();
        }

        store.delete_session(first.id).await.unwrap();
        let remaining = store.list_user_sessions(ada).await.unwrap().len();
        let ended = store.delete_user_sessions(ada).await.unwrap();

        assert_eq!(store.authenticate_session(first_secret.expose()).await.unwrap(), None);
        assert_eq!((remaining, ended), (2, 2));
        assert!(matches!(store.delete_session(first.id).await, Err(StoreError::NotFound { .. })));
    }
}
