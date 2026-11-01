//! User accounts, managed by instance admins: creating them with an initial password,
//! changing their name, email and admin flag, disabling them (never deleting, so the audit
//! log and history keep pointing at them), resetting passwords, and seeing and revoking
//! their API tokens. Everyone changes their own password.

use serde_json::json;
use spinikus_core::{
    id::UserId,
    membership::Membership,
    secret::Secret,
    token::ApiToken,
    user::{Email, EmailError, User, UserFields},
};
use spinikus_store::{ApiTokenRepository, MembershipRepository, SessionRepository, UserRepository};

use crate::{
    Control, ControlError, Credential, Principal,
    audit::changed,
    input,
    page::{self, Page},
    password,
};

#[derive(Debug)]
pub struct NewUser {
    pub email: String,
    pub name: String,
    pub admin: bool,
    /// The initial password, which the user changes after signing in. `None` for a user
    /// who can't sign in with a password yet.
    pub password: Option<Secret>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserChanges {
    pub email: Option<String>,
    pub name: Option<String>,
    pub admin: Option<bool>,
}

fn email(value: &str) -> Result<Email, ControlError> {
    value.parse().map_err(|error: EmailError| ControlError::Invalid { field: "email", problem: error.to_string() })
}

fn require_admin(principal: &Principal) -> Result<(), ControlError> {
    match principal.is_instance_admin() {
        true => Ok(()),
        false => Err(ControlError::Forbidden),
    }
}

impl Control {
    pub async fn list_users(&self, principal: &Principal, after: Option<UserId>, limit: u32) -> Result<Page<User>, ControlError> {
        require_admin(principal)?;
        let limit = page::limit(limit);
        Ok(Page::from_overfetch(self.store().list_users(after, limit + 1).await?, limit))
    }

    /// A user: anyone may read their own account, instance admins anyone's.
    pub async fn get_user(&self, principal: &Principal, id: UserId) -> Result<User, ControlError> {
        if id != principal.user.id {
            require_admin(principal)?;
        }
        Ok(self.store().get_user(id).await?)
    }

    pub async fn create_user(&self, principal: &Principal, new: NewUser) -> Result<User, ControlError> {
        require_admin(principal)?;
        let fields = UserFields { email: email(&new.email)?, name: input::name(&new.name)?, admin: new.admin };
        let hash = match new.password {
            Some(password) => {
                password::check_policy(&password)?;
                Some(password::hash(password).await)
            }
            None => None,
        };
        let user = self.store().create_user(fields, hash).await?;
        self.audit_by(principal, "user.create", user.id, None, json!({ "admin": user.admin })).await?;
        Ok(user)
    }

    /// Changes the user's email, name or admin flag. The last active instance admin stays one.
    pub async fn update_user(
        &self,
        principal: &Principal,
        id: UserId,
        revision: i64,
        changes: UserChanges,
    ) -> Result<User, ControlError> {
        require_admin(principal)?;
        let current = self.store().get_user(id).await?;
        let fields = UserFields {
            email: match &changes.email {
                Some(value) => email(value)?,
                None => current.email.clone(),
            },
            name: match &changes.name {
                Some(value) => input::name(value)?,
                None => current.name.clone(),
            },
            admin: changes.admin.unwrap_or(current.admin),
        };
        let user = self.store().update_user(id, revision, fields).await?;
        let details =
            changed(&[("email", changes.email.is_some()), ("name", changes.name.is_some()), ("admin", changes.admin.is_some())]);
        self.audit_by(principal, "user.update", id, None, details).await?;
        Ok(user)
    }

    /// Disables the user: they can't sign in, and their sessions and API tokens stop working.
    /// Admins can't disable themselves, and the last active instance admin stays.
    pub async fn disable_user(&self, principal: &Principal, id: UserId) -> Result<User, ControlError> {
        require_admin(principal)?;
        if id == principal.user.id {
            return Err(ControlError::Rejected {
                entity: "user",
                id: id.to_string(),
                problem: "you can't disable yourself".to_string(),
            });
        }
        let user = self.store().disable_user(id).await?;
        self.store().delete_user_sessions(id).await?;
        self.audit_by(principal, "user.disable", id, None, json!({})).await?;
        Ok(user)
    }

    pub async fn enable_user(&self, principal: &Principal, id: UserId) -> Result<User, ControlError> {
        require_admin(principal)?;
        let user = self.store().enable_user(id).await?;
        self.audit_by(principal, "user.enable", id, None, json!({})).await?;
        Ok(user)
    }

    /// Sets a user's password, as an admin does when they forgot theirs. Their sessions end.
    pub async fn reset_password(&self, principal: &Principal, id: UserId, password: Secret) -> Result<User, ControlError> {
        require_admin(principal)?;
        password::check_policy(&password)?;
        self.store().get_user(id).await?;
        let user = self.store().set_user_password(id, Some(password::hash(password).await)).await?;
        self.store().delete_user_sessions(id).await?;
        self.audit_by(principal, "user.password_reset", id, None, json!({})).await?;
        Ok(user)
    }

    /// Changes the principal's own password after checking the current one (`Invalid` on
    /// `current_password` if it is wrong). Their other sessions end; this one stays.
    pub async fn change_password(&self, principal: &Principal, current: Secret, new: Secret) -> Result<(), ControlError> {
        password::check_policy(&new)?;
        let user = &principal.user;
        let hash = self.store().user_password_hash(user.id).await?;
        if !password::verify(current, hash).await {
            return Err(ControlError::Invalid {
                field: "current_password",
                problem: "it is not your current password".to_string(),
            });
        }
        self.store().set_user_password(user.id, Some(password::hash(new).await)).await?;
        let keep = match principal.credential {
            Credential::Session(id) => Some(id),
            Credential::ApiToken(_) => None,
        };
        for session in self.store().list_user_sessions(user.id).await? {
            if Some(session.id) != keep {
                self.store().delete_session(session.id).await?;
            }
        }
        self.audit_by(principal, "user.password_change", user.id, None, json!({})).await
    }

    /// The user's memberships, for instance admins: where they have a role.
    pub async fn list_user_memberships(&self, principal: &Principal, id: UserId) -> Result<Vec<Membership>, ControlError> {
        require_admin(principal)?;
        self.store().get_user(id).await?;
        Ok(self.store().list_user_memberships(id).await?)
    }

    /// Another user's API tokens that are not revoked, for instance admins.
    pub async fn list_user_api_tokens(&self, principal: &Principal, id: UserId) -> Result<Vec<ApiToken>, ControlError> {
        require_admin(principal)?;
        self.store().get_user(id).await?;
        let tokens = self.store().list_user_api_tokens(id).await?;
        Ok(tokens.into_iter().filter(|token| token.revoked_at.is_none()).collect())
    }
}

#[cfg(test)]
mod tests {
    use spinikus_core::id::SessionId;
    use spinikus_store::AuditLogRepository;

    use super::*;
    use crate::{
        ClientInfo,
        test_support::{control, user},
    };

    const PASSWORD: &str = "correct horse battery staple";

    fn new(email: &str, admin: bool) -> NewUser {
        NewUser { email: email.to_string(), name: "Grace".to_string(), admin, password: Some(Secret::new(PASSWORD)) }
    }

    #[tokio::test]
    async fn admins_create_edit_and_disable_users() {
        let (_dir, control) = control().await;
        let admin = user(&control, "admin@example.com", true).await;
        let member = user(&control, "ada@example.com", false).await;

        let grace = control.create_user(&admin, new(" Grace@Example.com ", false)).await.unwrap();
        let duplicate = control.create_user(&admin, new("grace@example.com", false)).await;
        let weak =
            control.create_user(&admin, NewUser { password: Some(Secret::new("short")), ..new("x@example.com", false) }).await;
        let by_member = control.create_user(&member, new("y@example.com", false)).await;

        assert_eq!((grace.email.as_str(), grace.has_password), ("grace@example.com", true));
        assert!(matches!(duplicate, Err(ControlError::Conflict { .. })), "{duplicate:?}");
        assert!(matches!(weak, Err(ControlError::Invalid { field: "password", .. })), "{weak:?}");
        assert!(matches!(by_member, Err(ControlError::Forbidden)));
        assert!(control.sign_in("grace@example.com", Secret::new(PASSWORD), ClientInfo::default()).await.is_ok());

        let changes = UserChanges { name: Some("Grace H.".to_string()), admin: Some(true), ..Default::default() };
        let grace = control.update_user(&admin, grace.id, grace.revision, changes).await.unwrap();
        assert_eq!((grace.name.as_str(), grace.admin), ("Grace H.", true));
        assert!(matches!(control.disable_user(&admin, admin.user.id).await, Err(ControlError::Rejected { .. })));
        let (_, session) = control.sign_in("grace@example.com", Secret::new(PASSWORD), ClientInfo::default()).await.unwrap();

        let disabled = control.disable_user(&admin, grace.id).await.unwrap();

        assert!(!disabled.is_active());
        assert!(matches!(control.authenticate_session(session.expose()).await, Err(ControlError::Unauthenticated)));
        assert!(control.enable_user(&admin, grace.id).await.unwrap().is_active());
        let users = control.list_users(&admin, None, 10).await.unwrap().items;
        assert_eq!(users.len(), 3);
        assert!(matches!(control.list_users(&member, None, 10).await, Err(ControlError::Forbidden)));
        assert_eq!(control.get_user(&member, member.user.id).await.unwrap().id, member.user.id, "their own");
        assert!(matches!(control.get_user(&member, grace.id).await, Err(ControlError::Forbidden)));
        let audit = control.store().list_audit_entries(None, None, None, 10).await.unwrap();
        let actions: Vec<_> =
            audit.iter().map(|entry| entry.action.as_str()).filter(|action| action.starts_with("user.")).collect();
        assert_eq!(actions, ["user.enable", "user.disable", "user.update", "user.create"]);
    }

    #[tokio::test]
    async fn the_last_admin_stays_an_admin() {
        let (_dir, control) = control().await;
        let admin = user(&control, "admin@example.com", true).await;
        let current = control.get_user(&admin, admin.user.id).await.unwrap();
        let demoted = control
            .update_user(&admin, current.id, current.revision, UserChanges { admin: Some(false), ..Default::default() })
            .await;
        assert!(matches!(demoted, Err(ControlError::Rejected { .. })), "{demoted:?}");
    }

    #[tokio::test]
    async fn passwords_are_reset_by_admins_and_changed_by_their_owners() {
        let (_dir, control) = control().await;
        let admin = user(&control, "admin@example.com", true).await;
        let grace = control.create_user(&admin, new("grace@example.com", false)).await.unwrap();
        let (first, first_secret) =
            control.sign_in("grace@example.com", Secret::new(PASSWORD), ClientInfo::default()).await.unwrap();
        let (_, second_secret) =
            control.sign_in("grace@example.com", Secret::new(PASSWORD), ClientInfo::default()).await.unwrap();
        let signed = Principal { user: grace.clone(), credential: Credential::Session(first.id) };

        let wrong = control.change_password(&signed, Secret::new("not my password"), Secret::new("a new long password")).await;
        assert!(matches!(wrong, Err(ControlError::Invalid { field: "current_password", .. })), "{wrong:?}");
        control.change_password(&signed, Secret::new(PASSWORD), Secret::new("a new long password")).await.unwrap();

        assert!(control.authenticate_session(first_secret.expose()).await.is_ok(), "this session stays");
        assert!(control.authenticate_session(second_secret.expose()).await.is_err(), "the others end");
        assert!(control.sign_in("grace@example.com", Secret::new("a new long password"), ClientInfo::default()).await.is_ok());

        control.reset_password(&admin, grace.id, Secret::new("reset by the admin")).await.unwrap();
        assert!(control.authenticate_session(first_secret.expose()).await.is_err(), "a reset ends every session");
        assert!(control.sign_in("grace@example.com", Secret::new("reset by the admin"), ClientInfo::default()).await.is_ok());
        let other = Principal { user: grace.clone(), credential: Credential::Session(SessionId::new()) };
        assert!(matches!(
            control.reset_password(&other, admin.user.id, Secret::new("reset by the admin")).await,
            Err(ControlError::Forbidden)
        ));
    }
}
