//! Users' own API tokens, for the CLI and automation. Each user manages only their own: a
//! token acts as its user, capped by its role and limited to its scope, and its secret is
//! shown only when it is created.

use jiff::Timestamp;
use serde_json::json;
use spinikus_core::{
    id::ApiTokenId,
    membership::{MembershipScope, Role},
    secret::Secret,
    token::{ApiToken, NewApiToken},
};
use spinikus_store::ApiTokenRepository;

use crate::{Control, ControlError, Credential, Principal, access::not_found, input};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewToken {
    pub name: String,
    /// The most the token may do; it never exceeds what its user may do.
    pub role: Role,
    /// One project or environment, or `None` for wherever the user has access.
    pub scope: Option<MembershipScope>,
    /// `None` for a token that does not expire.
    pub expires_at: Option<Timestamp>,
}

impl Control {
    /// Creates an API token for the principal's own user. A token created with a token can't
    /// do more than that one: its role is at most that token's, and a scoped token's are scoped too.
    pub async fn create_api_token(&self, principal: &Principal, new: NewToken) -> Result<(ApiToken, Secret), ControlError> {
        let name = input::name(&new.name)?;
        if let Credential::ApiToken(current) = &principal.credential
            && (new.role > current.role || (current.scope.is_some() && new.scope.is_none()))
        {
            return Err(ControlError::Forbidden);
        }
        // The scope must be something the principal can see (which also keeps a scoped
        // token's tokens inside its scope).
        match new.scope {
            None => {}
            Some(MembershipScope::Project(id)) => {
                self.get_project(principal, id).await?;
            }
            Some(MembershipScope::Environment(id)) => {
                self.get_environment(principal, id).await?;
            }
        }
        if new.expires_at.is_some_and(|expires_at| expires_at <= Timestamp::now()) {
            return Err(ControlError::Invalid { field: "expires_at", problem: "it is in the past".to_string() });
        }
        let fields =
            NewApiToken { user_id: principal.user.id, name, role: new.role, scope: new.scope, expires_at: new.expires_at };
        let (token, secret) = self.store().create_api_token(fields).await?;
        let details = json!({ "name": token.name, "role": token.role, "scope": token.scope, "expires_at": token.expires_at });
        self.audit_by(principal, "api_token.create", token.id, None, details).await?;
        Ok((token, secret))
    }

    /// The principal's own tokens that are not revoked, expired ones included, oldest first.
    pub async fn list_api_tokens(&self, principal: &Principal) -> Result<Vec<ApiToken>, ControlError> {
        let tokens = self.store().list_user_api_tokens(principal.user.id).await?;
        Ok(tokens.into_iter().filter(|token| token.revoked_at.is_none()).collect())
    }

    /// Revokes one of the principal's own tokens, which may be the one in use, or for instance
    /// admins anyone's. Someone else's otherwise, or one already revoked, is `NotFound`.
    pub async fn revoke_api_token(&self, principal: &Principal, id: ApiTokenId) -> Result<(), ControlError> {
        let token = match self.store().get_api_token(id).await {
            Ok(token) if (token.user_id == principal.user.id || principal.is_instance_admin()) && token.revoked_at.is_none() => {
                token
            }
            Ok(_) | Err(spinikus_store::StoreError::NotFound { .. }) => return Err(not_found("api_token", id)),
            Err(error) => return Err(error.into()),
        };
        self.store().revoke_api_token(token.id).await?;
        self.audit_by(principal, "api_token.revoke", token.id, None, json!({})).await
    }
}

#[cfg(test)]
mod tests {
    use jiff::SignedDuration;
    use spinikus_core::{
        environment::{EnvironmentFields, EnvironmentKind},
        project::ProjectFields,
    };
    use spinikus_store::{AuditLogRepository, EnvironmentRepository, ProjectRepository};

    use super::*;
    use crate::test_support::{control, grant, user, with_token};

    fn new(name: &str, role: Role, scope: Option<MembershipScope>) -> NewToken {
        NewToken { name: name.to_string(), role, scope, expires_at: None }
    }

    #[tokio::test]
    async fn users_create_list_and_revoke_their_own_tokens() {
        let (_dir, control) = control().await;
        let ada = user(&control, "ada@example.com", false).await;
        let bob = user(&control, "bob@example.com", false).await;

        let (token, secret) = control.create_api_token(&ada, new(" CI ", Role::Developer, None)).await.unwrap();
        let authenticated = control.authenticate_api_token(secret.expose()).await.unwrap();

        assert_eq!((token.name.as_str(), token.role, token.user_id), ("CI", Role::Developer, ada.user.id));
        assert_eq!(authenticated.user.id, ada.user.id);
        let listed: Vec<_> = control.list_api_tokens(&ada).await.unwrap().into_iter().map(|token| token.id).collect();
        assert_eq!(listed, [token.id]);
        assert!(control.list_api_tokens(&bob).await.unwrap().is_empty());
        let by_bob = control.revoke_api_token(&bob, token.id).await;
        assert!(matches!(by_bob, Err(ControlError::NotFound { .. })), "{by_bob:?}");

        control.revoke_api_token(&ada, token.id).await.unwrap();

        assert!(control.list_api_tokens(&ada).await.unwrap().is_empty());
        assert!(matches!(control.authenticate_api_token(secret.expose()).await, Err(ControlError::Unauthenticated)));
        assert!(matches!(control.revoke_api_token(&ada, token.id).await, Err(ControlError::NotFound { .. })));
        let audit = control.store().list_audit_entries(None, None, None, 10).await.unwrap();
        let actions: Vec<_> = audit.iter().map(|entry| entry.action.as_str()).collect();
        assert_eq!(actions, ["api_token.revoke", "api_token.create"]);
    }

    #[tokio::test]
    async fn instance_admins_revoke_anyones_tokens() {
        let (_dir, control) = control().await;
        let admin = user(&control, "admin@example.com", true).await;
        let ada = user(&control, "ada@example.com", false).await;
        let (token, _) = control.create_api_token(&ada, new("CI", Role::Viewer, None)).await.unwrap();

        let listed = control.list_user_api_tokens(&admin, ada.user.id).await.unwrap();
        control.revoke_api_token(&admin, token.id).await.unwrap();

        assert_eq!(listed.iter().map(|token| token.id).collect::<Vec<_>>(), [token.id]);
        assert!(control.list_api_tokens(&ada).await.unwrap().is_empty());
        assert!(matches!(control.list_user_api_tokens(&ada, admin.user.id).await, Err(ControlError::Forbidden)));
    }

    #[tokio::test]
    async fn scopes_must_be_visible_and_tokens_never_widen_the_token_in_use() {
        let (_dir, control) = control().await;
        let ada = user(&control, "ada@example.com", false).await;
        let shop =
            control.store().create_project(ProjectFields { name: "Shop".into(), slug: "shop".parse().unwrap() }).await.unwrap();
        let blog =
            control.store().create_project(ProjectFields { name: "Blog".into(), slug: "blog".parse().unwrap() }).await.unwrap();
        let staging = control
            .store()
            .create_environment(
                shop.id,
                EnvironmentKind::Staging,
                EnvironmentFields { name: "Staging".into(), slug: "staging".parse().unwrap(), previews: false },
            )
            .await
            .unwrap();
        grant(&control, &ada, MembershipScope::Project(shop.id), Role::Admin).await;

        let in_shop = control.create_api_token(&ada, new("Shop", Role::Admin, Some(MembershipScope::Project(shop.id)))).await;
        let in_blog = control.create_api_token(&ada, new("Blog", Role::Admin, Some(MembershipScope::Project(blog.id)))).await;
        let expired =
            NewToken { expires_at: Some(Timestamp::now() - SignedDuration::from_secs(1)), ..new("Old", Role::Viewer, None) };
        let expired = control.create_api_token(&ada, expired).await;

        assert!(in_shop.is_ok(), "{in_shop:?}");
        assert!(matches!(in_blog, Err(ControlError::NotFound { .. })), "{in_blog:?}");
        assert!(matches!(expired, Err(ControlError::Invalid { field: "expires_at", .. })), "{expired:?}");

        let developer = with_token(&control, &ada, Role::Developer, Some(MembershipScope::Project(shop.id))).await;
        let higher =
            control.create_api_token(&developer, new("Higher", Role::Admin, Some(MembershipScope::Project(shop.id)))).await;
        let unscoped = control.create_api_token(&developer, new("Everywhere", Role::Viewer, None)).await;
        let narrower = control
            .create_api_token(&developer, new("Staging", Role::Viewer, Some(MembershipScope::Environment(staging.id))))
            .await;

        assert!(matches!(higher, Err(ControlError::Forbidden)), "{higher:?}");
        assert!(matches!(unscoped, Err(ControlError::Forbidden)), "{unscoped:?}");
        assert!(narrower.is_ok(), "{narrower:?}");
    }
}
