//! Signing in and out, and turning a session cookie or API token into a [`Principal`].

use jiff::{SignedDuration, Timestamp};
use serde_json::json;
use spinikus_core::{
    actor::Actor,
    audit::NewAuditEntry,
    id::{ApiTokenId, EnvironmentId, ProjectId, SessionId},
    membership::{self, Membership, Role},
    secret::Secret,
    token::{ApiToken, NewSession, Session},
    user::{Email, User},
};
use spinikus_store::{ApiTokenRepository, SessionRepository, UserRepository};

use crate::{Control, ControlError, password};

/// How long a web session lasts after signing in.
pub const SESSION_LIFETIME: SignedDuration = SignedDuration::from_hours(14 * 24);

/// Who is making a request: a signed-in user, through a web session or an API token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub user: User,
    pub credential: Credential,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    Session(SessionId),
    ApiToken(ApiToken),
}

impl Principal {
    pub fn actor(&self) -> Actor {
        Actor::User(self.user.id)
    }

    /// The API token in use, for the audit log.
    pub fn token_id(&self) -> Option<ApiTokenId> {
        match &self.credential {
            Credential::ApiToken(token) => Some(token.id),
            Credential::Session(_) => None,
        }
    }

    /// The role this principal has in `environment` of `project`, given the user's memberships:
    /// admin for instance admins, otherwise from memberships, and capped by the API token if one is in use.
    pub fn role_in(&self, memberships: &[Membership], project: ProjectId, environment: Option<EnvironmentId>) -> Option<Role> {
        let user_role =
            if self.user.admin { Some(Role::Admin) } else { membership::effective_role(memberships, project, environment) };
        match &self.credential {
            Credential::Session(_) => user_role,
            Credential::ApiToken(token) => token.effective_role(user_role, project, environment),
        }
    }

    /// Whether this principal may act as an instance admin: managing users and instance
    /// settings. A token only qualifies if it is an unscoped admin token.
    pub fn is_instance_admin(&self) -> bool {
        self.user.admin
            && match &self.credential {
                Credential::Session(_) => true,
                Credential::ApiToken(token) => token.scope.is_none() && token.role == Role::Admin,
            }
    }
}

/// About the client signing in, shown in the user's list of sessions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientInfo {
    pub user_agent: Option<String>,
    pub ip_address: Option<String>,
}

impl Control {
    /// Checks the email and password and starts a web session. Returns the session and the
    /// cookie value. Every failure is `InvalidCredentials`, whatever the reason.
    pub async fn sign_in(&self, email: &str, password: Secret, client: ClientInfo) -> Result<(Session, Secret), ControlError> {
        let user = match email.parse::<Email>() {
            Ok(email) => self.store().find_user_by_email(&email).await?.filter(User::is_active),
            Err(_) => None,
        };
        let hash = match &user {
            Some(user) => self.store().user_password_hash(user.id).await?,
            None => None,
        };
        // Verify even without a user, so timing does not reveal which emails have accounts.
        let matches = password::verify(password, hash).await;
        let Some(user) = user.filter(|_| matches) else {
            tracing::info!("sign-in failed");
            return Err(ControlError::InvalidCredentials);
        };

        let (session, secret) = self
            .store()
            .create_session(NewSession {
                user_id: user.id,
                expires_at: Timestamp::now() + SESSION_LIFETIME,
                user_agent: client.user_agent,
                ip_address: client.ip_address.clone(),
            })
            .await?;
        self.audit(NewAuditEntry {
            actor: Actor::User(user.id),
            token_id: None,
            action: "session.create".parse().expect("valid audit action"),
            target_id: Some(session.id.to_string()),
            project_id: None,
            details: json!({ "ip_address": client.ip_address }).as_object().cloned().unwrap_or_default(),
        })
        .await?;
        Ok((session, secret))
    }

    /// Ends the principal's web session. Does nothing for API tokens, which are revoked instead.
    pub async fn sign_out(&self, principal: &Principal) -> Result<(), ControlError> {
        if let Credential::Session(id) = principal.credential {
            self.store().delete_session(id).await?;
        }
        Ok(())
    }

    /// The principal for a session cookie, or `Unauthenticated` if it is not a live session.
    pub async fn authenticate_session(&self, cookie: &str) -> Result<Principal, ControlError> {
        let session = self.store().authenticate_session(cookie).await?.ok_or(ControlError::Unauthenticated)?;
        let user = self.store().get_user(session.user_id).await?;
        Ok(Principal { user, credential: Credential::Session(session.id) })
    }

    /// The principal for a bearer API token, or `Unauthenticated` if it is not a valid token.
    pub async fn authenticate_api_token(&self, bearer: &str) -> Result<Principal, ControlError> {
        let token = self.store().authenticate_api_token(bearer).await?.ok_or(ControlError::Unauthenticated)?;
        let user = self.store().get_user(token.user_id).await?;
        Ok(Principal { user, credential: Credential::ApiToken(token) })
    }
}

#[cfg(test)]
mod tests {
    use spinikus_core::{membership::MembershipScope, token::NewApiToken, user::UserFields};
    use spinikus_store::{AuditLogRepository, UserRepository};

    use super::*;
    use crate::test_support::control;

    const PASSWORD: &str = "correct horse battery staple";

    async fn user(control: &Control, email: &str, admin: bool) -> User {
        let fields = UserFields { email: email.parse().unwrap(), name: "Ada".to_string(), admin };
        let hash = password::hash(Secret::new(PASSWORD)).await;
        control.store().create_user(fields, Some(hash)).await.unwrap()
    }

    fn client() -> ClientInfo {
        ClientInfo { user_agent: Some("Firefox".to_string()), ip_address: Some("203.0.113.7".to_string()) }
    }

    #[tokio::test]
    async fn signs_in_and_out() {
        let (_dir, control) = control().await;
        let ada = user(&control, "ada@example.com", false).await;

        let (session, cookie) = control.sign_in("ADA@example.com", Secret::new(PASSWORD), client()).await.unwrap();
        let principal = control.authenticate_session(cookie.expose()).await.unwrap();
        control.sign_out(&principal).await.unwrap();

        assert_eq!(principal.user.id, ada.id);
        assert_eq!(principal.credential, Credential::Session(session.id));
        assert!(matches!(control.authenticate_session(cookie.expose()).await, Err(ControlError::Unauthenticated)));
        let audit = control.store().list_audit_entries(None, None, None, 10).await.unwrap();
        assert_eq!(audit[0].action.as_str(), "session.create");
        assert_eq!(audit[0].details["ip_address"], "203.0.113.7");
    }

    #[tokio::test]
    async fn every_failed_sign_in_looks_the_same() {
        let (_dir, control) = control().await;
        let bob = user(&control, "bob@example.com", true).await;
        user(&control, "ada@example.com", true).await;
        control.store().disable_user(bob.id).await.unwrap();

        for (email, password) in [
            ("ada@example.com", "wrong password here"),
            ("nobody@example.com", PASSWORD),
            ("not an email", PASSWORD),
            ("bob@example.com", PASSWORD),
        ] {
            let result = control.sign_in(email, Secret::new(password), client()).await;
            assert!(matches!(result, Err(ControlError::InvalidCredentials)), "{email}: {result:?}");
        }
    }

    #[tokio::test]
    async fn authenticates_api_tokens() {
        let (_dir, control) = control().await;
        let ada = user(&control, "ada@example.com", false).await;
        let new = NewApiToken { user_id: ada.id, name: "CI".to_string(), role: Role::Viewer, scope: None, expires_at: None };
        let (token, secret) = control.store().create_api_token(new).await.unwrap();

        let principal = control.authenticate_api_token(secret.expose()).await.unwrap();

        assert_eq!(principal.token_id(), Some(token.id));
        assert!(matches!(control.authenticate_api_token("spk_nope").await, Err(ControlError::Unauthenticated)));
    }

    fn principal(admin: bool, credential: Credential) -> Principal {
        let user = User {
            id: spinikus_core::id::UserId::new(),
            email: "ada@example.com".parse().unwrap(),
            name: "Ada".to_string(),
            admin,
            has_password: true,
            revision: 1,
            created_at: Timestamp::UNIX_EPOCH,
            updated_at: Timestamp::UNIX_EPOCH,
            disabled_at: None,
        };
        Principal { user, credential }
    }

    fn token(role: Role, scope: Option<MembershipScope>) -> Credential {
        Credential::ApiToken(ApiToken {
            id: ApiTokenId::new(),
            user_id: spinikus_core::id::UserId::new(),
            name: "CI".to_string(),
            role,
            scope,
            expires_at: None,
            last_used_at: None,
            created_at: Timestamp::UNIX_EPOCH,
            revoked_at: None,
        })
    }

    #[test]
    fn instance_admins_have_every_role_but_tokens_still_cap_them() {
        let (project, other) = (ProjectId::new(), ProjectId::new());
        let session_admin = principal(true, Credential::Session(SessionId::new()));
        let viewer_token_admin = principal(true, token(Role::Viewer, None));
        let scoped_token_admin = principal(true, token(Role::Admin, Some(MembershipScope::Project(project))));
        let member = principal(false, Credential::Session(SessionId::new()));

        assert_eq!(session_admin.role_in(&[], project, None), Some(Role::Admin));
        assert_eq!(viewer_token_admin.role_in(&[], project, None), Some(Role::Viewer));
        assert_eq!(scoped_token_admin.role_in(&[], other, None), None);
        assert_eq!(member.role_in(&[], project, None), None);

        assert!(session_admin.is_instance_admin());
        assert!(principal(true, token(Role::Admin, None)).is_instance_admin());
        assert!(!viewer_token_admin.is_instance_admin());
        assert!(!scoped_token_admin.is_instance_admin());
        assert!(!member.is_instance_admin());
    }
}
