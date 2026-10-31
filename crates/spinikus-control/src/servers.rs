//! Servers and enrolling their agents. Managing servers is for instance admins; the
//! enrollment and renewal calls come from agents over the agent listener.

use jiff::Timestamp;
use serde_json::json;
use spinikus_core::{
    actor::Actor,
    audit::NewAuditEntry,
    enrollment::{AGENT_CERTIFICATE_LIFETIME, ENROLLMENT_TOKEN_LIFETIME, EnrollmentToken},
    id::ServerId,
    secret::Secret,
    server::{Server, ServerFields},
};
use spinikus_store::{EnrollmentRepository, ServerRepository};

use crate::{
    Control, ControlError, Principal,
    ca::AgentCsr,
    input,
    page::{self, Page},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewServer {
    pub name: String,
}

/// What an admin needs to enroll a server's agent: the join token (shown once) and the CA
/// fingerprint the agent checks before trusting the control plane.
#[derive(Debug)]
pub struct JoinToken {
    pub token: EnrollmentToken,
    pub secret: Secret,
    pub ca_fingerprint: String,
}

/// An agent's new identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrolled {
    pub server_id: ServerId,
    pub certificate_chain_pem: String,
    pub ca_certificate_pem: String,
}

impl Control {
    pub async fn create_server(&self, principal: &Principal, new: NewServer) -> Result<Server, ControlError> {
        require_instance_admin(principal)?;
        let name = input::name(&new.name)?;
        let server = self.store().create_server(ServerFields { name }).await?;
        self.audit_by(principal, "server.create", server.id, None, json!({ "name": server.name })).await?;
        Ok(server)
    }

    /// Servers that are not revoked.
    pub async fn list_servers(
        &self,
        principal: &Principal,
        after: Option<ServerId>,
        limit: u32,
    ) -> Result<Page<Server>, ControlError> {
        require_instance_admin(principal)?;
        let limit = page::limit(limit);
        let items = self.store().list_servers(after, limit + 1).await?;
        Ok(Page::from_overfetch(items, limit))
    }

    pub async fn get_server(&self, principal: &Principal, id: ServerId) -> Result<Server, ControlError> {
        require_instance_admin(principal)?;
        Ok(self.store().get_server(id).await?)
    }

    /// Revokes the server: its agent is disconnected and can never connect or renew again,
    /// even with an unexpired certificate, and its unused join tokens stop working.
    pub async fn revoke_server(&self, principal: &Principal, id: ServerId) -> Result<(), ControlError> {
        require_instance_admin(principal)?;
        self.store().revoke_server(id).await?;
        self.agents().disconnect(id);
        self.audit_by(principal, "server.revoke", id, None, json!({})).await
    }

    /// A single-use join token for the server's agent, valid for an hour.
    pub async fn create_join_token(&self, principal: &Principal, server_id: ServerId) -> Result<JoinToken, ControlError> {
        require_instance_admin(principal)?;
        self.issue_join_token(server_id, principal.actor(), principal.token_id()).await
    }

    /// The server this control plane runs on, created (named `name`) on first use. It may be
    /// revoked, in which case its embedded agent must not run.
    pub async fn embedded_server(&self, name: &str) -> Result<Server, ControlError> {
        let name = input::name(name).unwrap_or_else(|_| "local".to_string());
        Ok(self.store().get_or_create_embedded_server(ServerFields { name }).await?)
    }

    /// A join token for the embedded server's agent, issued by the system rather than a user,
    /// so the embedded agent enrolls through the same `Enroll` call as any other.
    pub async fn create_embedded_join_token(&self, server_id: ServerId) -> Result<JoinToken, ControlError> {
        let server = self.store().get_server(server_id).await?;
        if !server.embedded {
            return Err(ControlError::Forbidden);
        }
        self.issue_join_token(server_id, Actor::System, None).await
    }

    async fn issue_join_token(
        &self,
        server_id: ServerId,
        actor: Actor,
        token_id: Option<spinikus_core::id::ApiTokenId>,
    ) -> Result<JoinToken, ControlError> {
        let ca_fingerprint = self.certificate_authority().await?.fingerprint();
        let expires_at = Timestamp::now() + ENROLLMENT_TOKEN_LIFETIME;
        let (token, secret) = self.store().create_enrollment_token(server_id, expires_at).await?;
        self.audit(NewAuditEntry {
            actor,
            token_id,
            action: "enrollment_token.create".parse().expect("valid audit action"),
            target_id: Some(token.id.to_string()),
            project_id: None,
            details: json!({ "server_id": server_id.to_string() }).as_object().cloned().unwrap_or_default(),
        })
        .await?;
        Ok(JoinToken { token, secret, ca_fingerprint })
    }

    /// Enrolls an agent: uses up its join token and signs a client certificate for the
    /// token's server. Any problem with the token is `InvalidCredentials`; a malformed CSR
    /// is `Invalid` and leaves the token unused.
    pub async fn enroll_agent(&self, join_token: &str, csr_der: &[u8]) -> Result<Enrolled, ControlError> {
        let csr = AgentCsr::from_der(csr_der)?;
        let authority = self.certificate_authority().await?;
        let Some(server_id) = self.store().redeem_enrollment_token(join_token).await? else {
            tracing::info!("refused an enrollment with an invalid, used or expired join token");
            return Err(ControlError::InvalidCredentials);
        };
        let certificate_chain_pem =
            authority.issue_agent_certificate(server_id, &csr, Timestamp::now(), AGENT_CERTIFICATE_LIFETIME)?;
        self.audit(NewAuditEntry {
            actor: Actor::Server(server_id),
            token_id: None,
            action: "server.enroll".parse().expect("valid audit action"),
            target_id: Some(server_id.to_string()),
            project_id: None,
            details: Default::default(),
        })
        .await?;
        tracing::info!(server = %server_id, "enrolled an agent");
        Ok(Enrolled { server_id, certificate_chain_pem, ca_certificate_pem: authority.certificate_pem().to_string() })
    }

    /// Signs a new client certificate for a connected agent whose current one is still
    /// valid. Refused for revoked servers.
    pub async fn renew_agent_certificate(&self, server_id: ServerId, csr_der: &[u8]) -> Result<String, ControlError> {
        let csr = AgentCsr::from_der(csr_der)?;
        let server = self.store().get_server(server_id).await?;
        if server.revoked_at.is_some() {
            return Err(ControlError::Forbidden);
        }
        let authority = self.certificate_authority().await?;
        authority.issue_agent_certificate(server_id, &csr, Timestamp::now(), AGENT_CERTIFICATE_LIFETIME)
    }
}

fn require_instance_admin(principal: &Principal) -> Result<(), ControlError> {
    if principal.is_instance_admin() { Ok(()) } else { Err(ControlError::Forbidden) }
}

#[cfg(test)]
mod tests {
    use spinikus_core::{enrollment::ENROLLMENT_TOKEN_PREFIX, membership::Role, server::ServerStatus};
    use spinikus_store::AuditLogRepository;

    use super::*;
    use crate::test_support::{control, user, with_token};

    fn new(name: &str) -> NewServer {
        NewServer { name: name.to_string() }
    }

    #[tokio::test]
    async fn only_instance_admins_manage_servers() {
        let (_dir, control) = control().await;
        let admin = user(&control, "admin@example.com", true).await;
        let member = user(&control, "ada@example.com", false).await;
        let read_only_admin = with_token(&control, &admin, Role::Viewer, None).await;
        let server = control.create_server(&admin, new(" web-1 ")).await.unwrap();

        assert_eq!((server.name.as_str(), server.status), ("web-1", ServerStatus::Pending));
        assert!(matches!(control.create_server(&member, new("web-2")).await, Err(ControlError::Forbidden)));
        assert!(matches!(control.list_servers(&member, None, 10).await, Err(ControlError::Forbidden)));
        assert!(matches!(control.create_join_token(&read_only_admin, server.id).await, Err(ControlError::Forbidden)));
        assert!(matches!(control.revoke_server(&member, server.id).await, Err(ControlError::Forbidden)));
        assert_eq!(control.list_servers(&admin, None, 10).await.unwrap().items, [server]);
    }

    #[tokio::test]
    async fn only_the_embedded_server_gets_system_join_tokens() {
        let (_dir, control) = control().await;
        let admin = user(&control, "admin@example.com", true).await;
        let remote = control.create_server(&admin, new("web-1")).await.unwrap();

        let embedded = control.embedded_server("  ").await.unwrap();
        let join = control.create_embedded_join_token(embedded.id).await.unwrap();
        let for_remote = control.create_embedded_join_token(remote.id).await;

        assert_eq!((embedded.name.as_str(), embedded.embedded), ("local", true));
        assert_eq!(join.token.server_id, embedded.id);
        assert!(matches!(for_remote, Err(ControlError::Forbidden)), "{for_remote:?}");
        let audit = control.store().list_audit_entries(None, None, None, 10).await.unwrap();
        assert_eq!((audit[0].action.as_str(), audit[0].actor), ("enrollment_token.create", Actor::System));
    }

    #[tokio::test]
    async fn join_tokens_come_with_the_ca_fingerprint_and_are_audited() {
        let (_dir, control) = control().await;
        let admin = user(&control, "admin@example.com", true).await;
        let server = control.create_server(&admin, new("web-1")).await.unwrap();

        let join = control.create_join_token(&admin, server.id).await.unwrap();
        control.revoke_server(&admin, server.id).await.unwrap();
        let after_revoking = control.create_join_token(&admin, server.id).await;
        let revoke_again = control.revoke_server(&admin, server.id).await;

        assert!(join.secret.expose().starts_with(ENROLLMENT_TOKEN_PREFIX));
        assert_eq!(join.ca_fingerprint, control.certificate_authority().await.unwrap().fingerprint());
        assert!(join.token.expires_at > Timestamp::now());
        assert!(matches!(after_revoking, Err(ControlError::NotFound { .. })), "{after_revoking:?}");
        assert!(matches!(revoke_again, Err(ControlError::NotFound { .. })), "{revoke_again:?}");
        let audit = control.store().list_audit_entries(None, None, None, 10).await.unwrap();
        let actions: Vec<_> = audit.iter().map(|entry| entry.action.as_str()).collect();
        assert_eq!(actions, ["server.revoke", "enrollment_token.create", "server.create"]);
        assert!(!serde_json::to_string(&audit).unwrap().contains(join.secret.expose()), "the token is never logged");
    }
}
