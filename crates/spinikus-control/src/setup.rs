//! First-run setup: creating the first instance admin, from the setup wizard or
//! `spinikus admin create-user`.

use serde_json::json;
use spinikus_core::{
    actor::Actor,
    audit::NewAuditEntry,
    secret::Secret,
    user::{Email, User, UserFields},
};
use spinikus_store::UserRepository;

use crate::{Control, ControlError, password};

impl Control {
    /// Whether Spinikus still needs its first admin.
    pub async fn needs_setup(&self) -> Result<bool, ControlError> {
        Ok(self.store().count_users().await? == 0)
    }

    /// Creates the first user as an instance admin with a password. Fails with
    /// `AlreadySetUp` once any user exists, so the unauthenticated setup page cannot be
    /// used again later.
    pub async fn create_first_admin(&self, email: &str, name: &str, password: Secret) -> Result<User, ControlError> {
        let email: Email = email.parse().map_err(|error: spinikus_core::user::EmailError| ControlError::Invalid {
            field: "email",
            problem: error.to_string(),
        })?;
        let name = name.trim();
        if name.is_empty() {
            return Err(ControlError::Invalid { field: "name", problem: "it is empty".to_string() });
        }
        password::check_policy(&password)?;
        // Cheap early exit before spending time on the hash; the insert below is what enforces it.
        if !self.needs_setup().await? {
            return Err(ControlError::AlreadySetUp);
        }

        let hash = password::hash(password).await;
        let fields = UserFields { email, name: name.to_string(), admin: true };
        let user = self.store().create_first_admin(fields, Some(hash)).await?.ok_or(ControlError::AlreadySetUp)?;

        self.audit(NewAuditEntry {
            actor: Actor::System,
            token_id: None,
            action: "user.create".parse().expect("valid audit action"),
            target_id: Some(user.id.to_string()),
            project_id: None,
            details: json!({ "first_admin": true }).as_object().cloned().unwrap_or_default(),
        })
        .await?;
        tracing::info!(user = %user.id, "created the first admin");
        Ok(user)
    }
}

#[cfg(test)]
mod tests {
    use crate::{ClientInfo, ControlError, test_support::control};
    use spinikus_core::secret::Secret;

    const PASSWORD: &str = "correct horse battery staple";

    #[tokio::test]
    async fn creates_the_first_admin_once() {
        let (_dir, control) = control().await;
        assert!(control.needs_setup().await.unwrap());

        let admin = control.create_first_admin(" Ada@Example.com ", " Ada ", Secret::new(PASSWORD)).await.unwrap();
        let again = control.create_first_admin("bob@example.com", "Bob", Secret::new(PASSWORD)).await;

        assert!(admin.admin && admin.has_password);
        assert_eq!((admin.email.as_str(), admin.name.as_str()), ("ada@example.com", "Ada"));
        assert!(!control.needs_setup().await.unwrap());
        assert!(matches!(again, Err(ControlError::AlreadySetUp)), "{again:?}");
        control.sign_in("ada@example.com", Secret::new(PASSWORD), ClientInfo::default()).await.unwrap();
    }

    #[tokio::test]
    async fn validates_input() {
        let (_dir, control) = control().await;

        let bad_email = control.create_first_admin("ada", "Ada", Secret::new(PASSWORD)).await;
        let no_name = control.create_first_admin("ada@example.com", "  ", Secret::new(PASSWORD)).await;
        let short_password = control.create_first_admin("ada@example.com", "Ada", Secret::new("short")).await;

        assert!(matches!(bad_email, Err(ControlError::Invalid { field: "email", .. })), "{bad_email:?}");
        assert!(matches!(no_name, Err(ControlError::Invalid { field: "name", .. })), "{no_name:?}");
        assert!(matches!(short_password, Err(ControlError::Invalid { field: "password", .. })), "{short_password:?}");
        assert!(control.needs_setup().await.unwrap());
    }
}
