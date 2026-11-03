//! Signing in and out, and turning a session cookie or API token into a [`Principal`].

use std::net::IpAddr;

use jiff::{SignedDuration, Timestamp};
use serde_json::json;
use spinikus_core::{
    actor::Actor,
    audit::NewAuditEntry,
    id::{ApiTokenId, EnvironmentId, ProjectId, SessionId},
    membership::{self, Membership, Role},
    secret::Secret,
    token::{ApiToken, NewApiToken, NewSession, Session},
    user::{Email, User},
};
use spinikus_store::{ApiTokenRepository, SessionRepository, UserRepository};

use crate::{Control, ControlError, input, password};

/// How long a web session lasts after signing in.
pub const SESSION_LIFETIME: SignedDuration = SignedDuration::from_hours(14 * 24);

/// How long an API token from [`Control::create_login_token`] lasts.
pub const LOGIN_TOKEN_LIFETIME: SignedDuration = SignedDuration::from_hours(90 * 24);

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

impl ClientInfo {
    /// What a request says about its client. The address is the peer's, unless the peer is
    /// on this machine, such as the proxy in front of the dashboard: then it is the last
    /// address in `X-Forwarded-For`, the one that proxy got the request from (earlier ones
    /// are whatever the client sent).
    pub fn from_request(user_agent: Option<&str>, peer: Option<IpAddr>, forwarded_for: Option<&str>) -> Self {
        let forwarded = || forwarded_for?.rsplit(',').next()?.trim().parse::<IpAddr>().ok();
        let ip_address = match peer {
            Some(peer) if peer.is_loopback() => Some(forwarded().unwrap_or(peer)),
            peer => peer,
        };
        Self { user_agent: user_agent.map(str::to_string), ip_address: ip_address.map(|ip| ip.to_string()) }
    }
}

impl Control {
    /// Checks the email and password and starts a web session. Returns the session and the
    /// cookie value. Every failure is `InvalidCredentials`, whatever the reason.
    pub async fn sign_in(&self, email: &str, password: Secret, client: ClientInfo) -> Result<(Session, Secret), ControlError> {
        let user = self.check_password(email, password, &client).await?;
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

    /// Checks the email and password and creates an API token for the user, as `spinikus login`
    /// does. The token has the user's own access (an unscoped admin token, capped by their
    /// role) and expires after [`LOGIN_TOKEN_LIFETIME`]. Every failure is `InvalidCredentials`.
    pub async fn create_login_token(
        &self,
        email: &str,
        password: Secret,
        token_name: &str,
        client: ClientInfo,
    ) -> Result<(ApiToken, Secret), ControlError> {
        let name = input::name(token_name)?;
        let user = self.check_password(email, password, &client).await?;
        let new = NewApiToken {
            user_id: user.id,
            name,
            role: Role::Admin,
            scope: None,
            expires_at: Some(Timestamp::now() + LOGIN_TOKEN_LIFETIME),
        };
        let (token, secret) = self.store().create_api_token(new).await?;
        self.audit(NewAuditEntry {
            actor: Actor::User(user.id),
            token_id: None,
            action: "api_token.create".parse().expect("valid audit action"),
            target_id: Some(token.id.to_string()),
            project_id: None,
            details: json!({ "via": "login", "ip_address": client.ip_address }).as_object().cloned().unwrap_or_default(),
        })
        .await?;
        Ok((token, secret))
    }

    /// The active user with this email and password, or `InvalidCredentials`; or
    /// `TooManyAttempts`, without checking, after too many wrong passwords from the client's
    /// address ([`crate::throttle`]).
    async fn check_password(&self, email: &str, password: Secret, client: &ClientInfo) -> Result<User, ControlError> {
        let address = client.ip_address.as_deref();
        if let Err(retry_after) = self.sign_ins.check(email, address) {
            tracing::info!(address, "sign-in refused after too many failures");
            return Err(ControlError::TooManyAttempts { retry_after });
        }
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
        match user.filter(|_| matches) {
            Some(user) => {
                self.sign_ins.succeeded(email, address);
                Ok(user)
            }
            None => {
                self.sign_ins.failed(email, address);
                tracing::info!(address, "sign-in failed");
                Err(ControlError::InvalidCredentials)
            }
        }
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

    #[test]
    fn believes_forwarded_addresses_only_from_this_machine() {
        let ip = |client: ClientInfo| client.ip_address;
        let remote = Some("203.0.113.7".parse().unwrap());
        let local = Some("127.0.0.1".parse().unwrap());
        let forwarded = Some("10.0.0.1, 198.51.100.2");

        assert_eq!(ip(ClientInfo::from_request(None, remote, forwarded)).as_deref(), Some("203.0.113.7"));
        assert_eq!(ip(ClientInfo::from_request(None, local, forwarded)).as_deref(), Some("198.51.100.2"));
        assert_eq!(ip(ClientInfo::from_request(None, local, Some("not an address"))).as_deref(), Some("127.0.0.1"));
        assert_eq!(ip(ClientInfo::from_request(None, local, None)).as_deref(), Some("127.0.0.1"));
        assert_eq!(ClientInfo::from_request(Some("Firefox"), None, forwarded).user_agent.as_deref(), Some("Firefox"));
        assert_eq!(ip(ClientInfo::from_request(None, None, forwarded)), None);
    }

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
    async fn refuses_sign_ins_after_too_many_wrong_passwords() {
        let (_dir, control) = control().await;
        user(&control, "ada@example.com", true).await;

        for _ in 0..crate::throttle::MAX_FAILURES {
            let wrong = control.sign_in("ada@example.com", Secret::new("wrong password here"), client()).await;
            assert!(matches!(wrong, Err(ControlError::InvalidCredentials)), "{wrong:?}");
        }
        // Even the right password, until the window ends; tokens for the CLI too.
        let refused = control.sign_in("ada@example.com", Secret::new(PASSWORD), client()).await;
        let token = control.create_login_token("ada@example.com", Secret::new(PASSWORD), "laptop", client()).await;
        let elsewhere = ClientInfo { ip_address: Some("198.51.100.2".to_string()), ..client() };

        assert!(matches!(refused, Err(ControlError::TooManyAttempts { .. })), "{refused:?}");
        assert!(matches!(token, Err(ControlError::TooManyAttempts { .. })), "{token:?}");
        assert!(control.sign_in("ada@example.com", Secret::new(PASSWORD), elsewhere).await.is_ok());
        let message = refused.unwrap_err().to_string();
        assert_eq!(message, "too many failed sign-ins; try again in 15 minutes");
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

    #[tokio::test]
    async fn logging_in_creates_an_expiring_api_token() {
        let (_dir, control) = control().await;
        let ada = user(&control, "ada@example.com", false).await;

        let (token, secret) =
            control.create_login_token("ada@example.com", Secret::new(PASSWORD), "CLI on laptop", client()).await.unwrap();
        let wrong = control.create_login_token("ada@example.com", Secret::new("nope nope nope"), "CLI", client()).await;
        let principal = control.authenticate_api_token(secret.expose()).await.unwrap();

        assert_eq!((token.user_id, token.role, token.scope), (ada.id, Role::Admin, None));
        assert!(token.expires_at.unwrap() > Timestamp::now() + SignedDuration::from_hours(89 * 24));
        assert!(matches!(wrong, Err(ControlError::InvalidCredentials)), "{wrong:?}");
        assert_eq!(principal.token_id(), Some(token.id));
        let audit = control.store().list_audit_entries(None, None, None, 10).await.unwrap();
        assert_eq!(audit[0].action.as_str(), "api_token.create");
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
