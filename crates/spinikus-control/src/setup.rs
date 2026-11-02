//! First-run setup: creating the first instance admin, from the setup wizard or
//! `spinikus admin create-user`.
//!
//! An installed instance is reachable before anyone has set it up, so `server run` gives
//! the service layer a setup code ([`Control::with_setup_code`]), kept in a file only root
//! and the instance's user can read; the install script prints the setup URL with it. The
//! setup page and `POST /api/v1/setup` then need it ([`Control::set_up`]).

use serde_json::json;
use sha2::{Digest, Sha256};
use spinikus_core::{
    actor::Actor,
    audit::NewAuditEntry,
    secret::Secret,
    user::{Email, User, UserFields},
};
use spinikus_store::UserRepository;
use std::sync::Arc;

use crate::{Control, ControlError, password};

impl Control {
    /// Makes the setup page and API need `code` to create the first admin.
    pub fn with_setup_code(mut self, code: Secret) -> Self {
        self.setup_code = Some(Arc::new(code));
        self
    }

    /// A new random setup code: 32 hex digits.
    pub fn new_setup_code() -> Secret {
        Secret::new(rand::random::<[u8; 16]>().iter().map(|byte| format!("{byte:02x}")).collect::<String>())
    }

    /// Whether creating the first admin needs a setup code.
    pub fn setup_code_required(&self) -> bool {
        self.setup_code.is_some()
    }

    /// Creates the first admin from the setup page or API, checking the setup code first if
    /// there is one. A wrong code is `Invalid` on `setup_code`.
    pub async fn set_up(&self, code: &str, email: &str, name: &str, password: Secret) -> Result<User, ControlError> {
        if let Some(expected) = &self.setup_code {
            // Compared by digest, so the time taken says nothing about the code.
            if Sha256::digest(code.trim().as_bytes()) != Sha256::digest(expected.expose().as_bytes()) {
                if !self.needs_setup().await? {
                    return Err(ControlError::AlreadySetUp);
                }
                return Err(ControlError::Invalid {
                    field: "setup_code",
                    problem: "it is not this instance's setup code, which the install script printed".to_string(),
                });
            }
        }
        self.create_first_admin(email, name, password).await
    }

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
    use crate::{ClientInfo, Control, ControlError, test_support::control};
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
    }

    #[tokio::test]
    async fn a_setup_code_guards_the_first_admin() {
        let (_dir, control) = control().await;
        assert!(!control.setup_code_required());
        let code = Control::new_setup_code();
        assert_eq!(code.expose().len(), 32);
        let control = control.with_setup_code(Secret::new(code.expose()));
        assert!(control.setup_code_required());

        let wrong = control.set_up("nope", "ada@example.com", "Ada", Secret::new(PASSWORD)).await;
        assert!(matches!(wrong, Err(ControlError::Invalid { field: "setup_code", .. })), "{wrong:?}");
        assert!(control.needs_setup().await.unwrap());
        let padded = format!(" {} ", code.expose());
        control.set_up(&padded, "ada@example.com", "Ada", Secret::new(PASSWORD)).await.unwrap();
        let again = control.set_up("nope", "bob@example.com", "Bob", Secret::new(PASSWORD)).await;
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
