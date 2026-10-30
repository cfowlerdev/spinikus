use spinikus_core::{
    id::SessionId,
    membership::{MembershipScope, Role},
    token::NewApiToken,
    user::UserFields,
};
use spinikus_store::{ApiTokenRepository, MasterKey, MembershipRepository, SqliteStore, UserRepository, connect, migrate};

use crate::{Control, Credential, Principal};

/// A service layer over a migrated database in a temporary directory, deleted when the `TempDir` is dropped.
pub async fn control() -> (tempfile::TempDir, Control) {
    let dir = tempfile::tempdir().unwrap();
    let pool = connect(&dir.path().join("spinikus.db")).await.unwrap();
    migrate(&pool).await.unwrap();
    let store = SqliteStore::open(pool, &MasterKey::generate()).await.unwrap();
    (dir, Control::new(store))
}

/// A signed-in user, an instance admin or not, without a password.
pub async fn user(control: &Control, email: &str, admin: bool) -> Principal {
    let fields = UserFields { email: email.parse().unwrap(), name: email.to_string(), admin };
    let user = control.store().create_user(fields, None).await.unwrap();
    Principal { user, credential: Credential::Session(SessionId::new()) }
}

pub async fn grant(control: &Control, principal: &Principal, scope: MembershipScope, role: Role) {
    control.store().create_membership(principal.user.id, scope, role).await.unwrap();
}

/// The same user acting through a new API token with `role` and `scope`.
pub async fn with_token(control: &Control, principal: &Principal, role: Role, scope: Option<MembershipScope>) -> Principal {
    let new = NewApiToken { user_id: principal.user.id, name: "test".to_string(), role, scope, expires_at: None };
    let (token, _) = control.store().create_api_token(new).await.unwrap();
    Principal { user: principal.user.clone(), credential: Credential::ApiToken(token) }
}
