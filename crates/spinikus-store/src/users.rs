use async_trait::async_trait;
use spinikus_core::{
    id::UserId,
    user::{Email, PasswordHash, User, UserFields},
};
use sqlx::SqliteConnection;

use crate::{SqliteStore, StoreError, error::parse_column};

const ENTITY: &str = "user";

#[async_trait]
pub trait UserRepository: Send + Sync {
    /// Fails with `Conflict` if another user has the email.
    async fn create_user(&self, fields: UserFields, password_hash: Option<PasswordHash>) -> Result<User, StoreError>;

    /// Creates the first user, as an instance admin, but only while there are no users at all.
    /// Returns `None` if a user already exists. Atomic, so two concurrent setups cannot both succeed.
    async fn create_first_admin(
        &self,
        fields: UserFields,
        password_hash: Option<PasswordHash>,
    ) -> Result<Option<User>, StoreError>;

    /// Disabled users included.
    async fn get_user(&self, id: UserId) -> Result<User, StoreError>;

    async fn find_user_by_email(&self, email: &Email) -> Result<Option<User>, StoreError>;

    /// Users in creation order, disabled ones included, starting after the `after` cursor.
    async fn list_users(&self, after: Option<UserId>, limit: u32) -> Result<Vec<User>, StoreError>;

    /// How many users exist, so first-run setup knows whether to create the first admin.
    async fn count_users(&self) -> Result<u64, StoreError>;

    /// Replaces the user's fields if it is still at `revision`. Fails with `Rejected` if it
    /// would leave no active instance admin.
    async fn update_user(&self, id: UserId, revision: i64, fields: UserFields) -> Result<User, StoreError>;

    /// Sets or removes the password. Not a profile edit, so the revision stays.
    async fn set_user_password(&self, id: UserId, password_hash: Option<PasswordHash>) -> Result<User, StoreError>;

    /// The password hash to verify a sign-in against, if the user has a password.
    async fn user_password_hash(&self, id: UserId) -> Result<Option<PasswordHash>, StoreError>;

    /// Stops the user from signing in. Fails with `Rejected` if they are the last active instance admin.
    async fn disable_user(&self, id: UserId) -> Result<User, StoreError>;

    async fn enable_user(&self, id: UserId) -> Result<User, StoreError>;
}

macro_rules! columns {
    () => {
        "id, email, name, admin, password_hash IS NOT NULL AS has_password, revision, created_at, updated_at, disabled_at"
    };
}

#[derive(sqlx::FromRow)]
struct UserRow {
    id: String,
    email: String,
    name: String,
    admin: bool,
    has_password: bool,
    revision: i64,
    created_at: String,
    updated_at: String,
    disabled_at: Option<String>,
}

impl TryFrom<UserRow> for User {
    type Error = StoreError;

    fn try_from(row: UserRow) -> Result<Self, Self::Error> {
        Ok(User {
            id: parse_column(ENTITY, "id", &row.id)?,
            email: parse_column(ENTITY, "email", &row.email)?,
            name: row.name,
            admin: row.admin,
            has_password: row.has_password,
            revision: row.revision,
            created_at: parse_column(ENTITY, "created_at", &row.created_at)?,
            updated_at: parse_column(ENTITY, "updated_at", &row.updated_at)?,
            disabled_at: row.disabled_at.map(|at| parse_column(ENTITY, "disabled_at", &at)).transpose()?,
        })
    }
}

fn not_found(id: UserId) -> StoreError {
    StoreError::NotFound { entity: ENTITY, id: id.to_string() }
}

fn email_conflict(fields: &UserFields) -> impl FnOnce(&str) -> StoreError {
    let value = fields.email.to_string();
    move |_| StoreError::Conflict { entity: ENTITY, field: "email", value }
}

async fn fetch(conn: &mut SqliteConnection, id: UserId) -> Result<User, StoreError> {
    let row: Option<UserRow> = sqlx::query_as(concat!("SELECT ", columns!(), " FROM users WHERE id = ?"))
        .bind(id.to_string())
        .fetch_optional(conn)
        .await?;
    row.ok_or_else(|| not_found(id))?.try_into()
}

/// Refuses a change that takes away `user`'s active admin rights if no other active admin
/// would be left, so the instance can never lock itself out.
async fn keep_an_admin(conn: &mut SqliteConnection, user: &User) -> Result<(), StoreError> {
    if !(user.admin && user.is_active()) {
        return Ok(());
    }
    let others: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE admin = 1 AND disabled_at IS NULL AND id <> ?")
        .bind(user.id.to_string())
        .fetch_one(conn)
        .await?;
    if others == 0 {
        return Err(StoreError::Rejected {
            entity: ENTITY,
            id: user.id.to_string(),
            problem: "the last active instance admin must stay one",
        });
    }
    Ok(())
}

#[async_trait]
impl UserRepository for SqliteStore {
    async fn create_user(&self, fields: UserFields, password_hash: Option<PasswordHash>) -> Result<User, StoreError> {
        let row: UserRow = sqlx::query_as(concat!(
            "INSERT INTO users (id, email, name, admin, password_hash) VALUES (?, ?, ?, ?, ?) RETURNING ",
            columns!()
        ))
        .bind(UserId::new().to_string())
        .bind(fields.email.as_str())
        .bind(&fields.name)
        .bind(fields.admin)
        .bind(password_hash.as_ref().map(PasswordHash::as_str))
        .fetch_one(self.pool())
        .await
        .map_err(|error| StoreError::on_unique_violation(error, email_conflict(&fields)))?;

        row.try_into()
    }

    async fn create_first_admin(
        &self,
        fields: UserFields,
        password_hash: Option<PasswordHash>,
    ) -> Result<Option<User>, StoreError> {
        let row: Option<UserRow> = sqlx::query_as(concat!(
            "INSERT INTO users (id, email, name, admin, password_hash)
             SELECT ?, ?, ?, 1, ? WHERE NOT EXISTS (SELECT 1 FROM users)
             RETURNING ",
            columns!()
        ))
        .bind(UserId::new().to_string())
        .bind(fields.email.as_str())
        .bind(&fields.name)
        .bind(password_hash.as_ref().map(PasswordHash::as_str))
        .fetch_optional(self.pool())
        .await?;

        row.map(User::try_from).transpose()
    }

    async fn get_user(&self, id: UserId) -> Result<User, StoreError> {
        fetch(&mut *self.pool().acquire().await?, id).await
    }

    async fn find_user_by_email(&self, email: &Email) -> Result<Option<User>, StoreError> {
        let row: Option<UserRow> = sqlx::query_as(concat!("SELECT ", columns!(), " FROM users WHERE email = ?"))
            .bind(email.as_str())
            .fetch_optional(self.pool())
            .await?;
        row.map(User::try_from).transpose()
    }

    async fn list_users(&self, after: Option<UserId>, limit: u32) -> Result<Vec<User>, StoreError> {
        let rows: Vec<UserRow> = sqlx::query_as(concat!("SELECT ", columns!(), " FROM users WHERE id > ? ORDER BY id LIMIT ?"))
            .bind(after.map(|id| id.to_string()).unwrap_or_default())
            .bind(limit)
            .fetch_all(self.pool())
            .await?;
        rows.into_iter().map(User::try_from).collect()
    }

    async fn count_users(&self) -> Result<u64, StoreError> {
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM users").fetch_one(self.pool()).await?;
        Ok(count as u64)
    }

    async fn update_user(&self, id: UserId, revision: i64, fields: UserFields) -> Result<User, StoreError> {
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
        if !fields.admin {
            keep_an_admin(&mut tx, &current).await?;
        }

        let row: UserRow = sqlx::query_as(concat!(
            "UPDATE users
             SET email = ?, name = ?, admin = ?, revision = revision + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?
             RETURNING ",
            columns!()
        ))
        .bind(fields.email.as_str())
        .bind(&fields.name)
        .bind(fields.admin)
        .bind(id.to_string())
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| StoreError::on_unique_violation(error, email_conflict(&fields)))?;

        tx.commit().await?;
        row.try_into()
    }

    async fn set_user_password(&self, id: UserId, password_hash: Option<PasswordHash>) -> Result<User, StoreError> {
        let row: Option<UserRow> = sqlx::query_as(concat!(
            "UPDATE users SET password_hash = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ? RETURNING ",
            columns!()
        ))
        .bind(password_hash.as_ref().map(PasswordHash::as_str))
        .bind(id.to_string())
        .fetch_optional(self.pool())
        .await?;
        row.ok_or_else(|| not_found(id))?.try_into()
    }

    async fn user_password_hash(&self, id: UserId) -> Result<Option<PasswordHash>, StoreError> {
        let row: Option<(Option<String>,)> = sqlx::query_as("SELECT password_hash FROM users WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(self.pool())
            .await?;
        let (hash,) = row.ok_or_else(|| not_found(id))?;
        hash.map(|hash| parse_column(ENTITY, "password_hash", &hash)).transpose()
    }

    async fn disable_user(&self, id: UserId) -> Result<User, StoreError> {
        let mut tx = self.begin_write().await?;
        let current = fetch(&mut tx, id).await?;
        if !current.is_active() {
            return Ok(current);
        }
        keep_an_admin(&mut tx, &current).await?;

        let row: UserRow = sqlx::query_as(concat!(
            "UPDATE users
             SET disabled_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                 revision = revision + 1
             WHERE id = ?
             RETURNING ",
            columns!()
        ))
        .bind(id.to_string())
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;
        row.try_into()
    }

    async fn enable_user(&self, id: UserId) -> Result<User, StoreError> {
        let row: Option<UserRow> = sqlx::query_as(concat!(
            "UPDATE users
             SET disabled_at = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), revision = revision + 1
             WHERE id = ? AND disabled_at IS NOT NULL
             RETURNING ",
            columns!()
        ))
        .bind(id.to_string())
        .fetch_optional(self.pool())
        .await?;

        match row {
            Some(row) => row.try_into(),
            // Already enabled, or missing.
            None => self.get_user(id).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::store;

    const HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA";

    fn fields(email: &str, admin: bool) -> UserFields {
        UserFields { email: email.parse().unwrap(), name: "Ada Lovelace".to_string(), admin }
    }

    #[tokio::test]
    async fn creates_and_finds_users() {
        let (_dir, store) = store().await;
        assert_eq!(store.count_users().await.unwrap(), 0);

        let user = store.create_user(fields("Ada@Example.com", true), Some(HASH.parse().unwrap())).await.unwrap();

        assert_eq!(store.get_user(user.id).await.unwrap(), user);
        assert_eq!(user.email.as_str(), "ada@example.com");
        assert!(user.admin && user.has_password && user.is_active());
        assert_eq!(store.find_user_by_email(&"ADA@example.com".parse().unwrap()).await.unwrap(), Some(user.clone()));
        assert_eq!(store.find_user_by_email(&"bob@example.com".parse().unwrap()).await.unwrap(), None);
        assert_eq!(store.count_users().await.unwrap(), 1);
        assert_eq!(store.user_password_hash(user.id).await.unwrap().unwrap().as_str(), HASH);
    }

    #[tokio::test]
    async fn only_the_first_user_is_created_by_setup() {
        let (_dir, store) = store().await;

        let first = store.create_first_admin(fields("ada@example.com", false), None).await.unwrap().unwrap();
        let second = store.create_first_admin(fields("bob@example.com", false), None).await.unwrap();

        assert!(first.admin, "the first user is always an admin");
        assert_eq!(second, None);
        assert_eq!(store.count_users().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn emails_are_unique() {
        let (_dir, store) = store().await;
        store.create_user(fields("ada@example.com", false), None).await.unwrap();
        let bob = store.create_user(fields("bob@example.com", false), None).await.unwrap();

        let created = store.create_user(fields("ada@example.com", false), None).await;
        let updated = store.update_user(bob.id, 1, fields("ada@example.com", false)).await;

        assert!(matches!(created, Err(StoreError::Conflict { field: "email", .. })), "{created:?}");
        assert!(matches!(updated, Err(StoreError::Conflict { field: "email", .. })), "{updated:?}");
    }

    #[tokio::test]
    async fn passwords_can_be_set_and_removed_without_a_revision() {
        let (_dir, store) = store().await;
        let user = store.create_user(fields("ada@example.com", false), None).await.unwrap();
        assert!(!user.has_password);
        assert_eq!(store.user_password_hash(user.id).await.unwrap(), None);

        let with_password = store.set_user_password(user.id, Some(HASH.parse().unwrap())).await.unwrap();
        let without = store.set_user_password(user.id, None).await.unwrap();

        assert!(with_password.has_password);
        assert!(!without.has_password);
        assert_eq!(without.revision, 1);
        assert!(matches!(store.set_user_password(UserId::new(), None).await, Err(StoreError::NotFound { .. })));
    }

    #[tokio::test]
    async fn updates_at_the_current_revision() {
        let (_dir, store) = store().await;
        let user = store.create_user(fields("ada@example.com", false), None).await.unwrap();

        let updated = store
            .update_user(user.id, 1, UserFields { name: "Ada".to_string(), ..fields("ada@example.org", false) })
            .await
            .unwrap();
        let stale = store.update_user(user.id, 1, fields("x@example.com", false)).await;

        assert_eq!((updated.name.as_str(), updated.email.as_str(), updated.revision), ("Ada", "ada@example.org", 2));
        assert!(matches!(stale, Err(StoreError::RevisionMismatch { expected: 1, actual: 2, .. })), "{stale:?}");
    }

    #[tokio::test]
    async fn disables_and_enables_users() {
        let (_dir, store) = store().await;
        let user = store.create_user(fields("ada@example.com", false), None).await.unwrap();

        let disabled = store.disable_user(user.id).await.unwrap();
        let again = store.disable_user(user.id).await.unwrap();
        let enabled = store.enable_user(user.id).await.unwrap();

        assert!(!disabled.is_active());
        assert_eq!(again, disabled);
        assert!(enabled.is_active());
        assert_eq!(enabled.revision, 3);
        assert_eq!(store.list_users(None, 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_last_active_admin_cannot_be_removed() {
        let (_dir, store) = store().await;
        let ada = store.create_user(fields("ada@example.com", true), None).await.unwrap();
        let bob = store.create_user(fields("bob@example.com", true), None).await.unwrap();

        store.disable_user(bob.id).await.unwrap();
        let demote_last = store.update_user(ada.id, 1, fields("ada@example.com", false)).await;
        let disable_last = store.disable_user(ada.id).await;
        store.enable_user(bob.id).await.unwrap();
        let demoted = store.update_user(ada.id, 1, fields("ada@example.com", false)).await.unwrap();

        assert!(matches!(demote_last, Err(StoreError::Rejected { .. })), "{demote_last:?}");
        assert!(matches!(disable_last, Err(StoreError::Rejected { .. })), "{disable_last:?}");
        assert!(!demoted.admin);
    }

    #[tokio::test]
    async fn lists_in_creation_order_with_a_cursor() {
        let (_dir, store) = store().await;
        let mut ids = Vec::new();
        for email in ["a@example.com", "b@example.com", "c@example.com"] {
            ids.push(store.create_user(fields(email, false), None).await.unwrap().id);
        }

        let first = store.list_users(None, 2).await.unwrap();
        let second = store.list_users(Some(first[1].id), 2).await.unwrap();

        assert_eq!(first.iter().map(|u| u.id).collect::<Vec<_>>(), ids[..2]);
        assert_eq!(second.iter().map(|u| u.id).collect::<Vec<_>>(), ids[2..]);
    }
}
