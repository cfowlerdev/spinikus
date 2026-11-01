//! Recording actions in the audit log.

use spinikus_core::{
    actor::Actor,
    audit::{AuditEntry, NewAuditEntry},
    id::{AuditEntryId, ProjectId, UserId},
    membership::Role,
    user::User,
};
use spinikus_store::{AuditLogRepository, ProjectRepository, StoreError, UserRepository};

use crate::{
    Control, ControlError, Principal,
    page::{self, Page},
};

/// A page of the audit log and the users who acted in it.
#[derive(Debug)]
pub struct AuditPage {
    pub entries: Page<AuditEntry>,
    pub users: Vec<User>,
}

impl Control {
    /// The audit log, newest first, from before the `before` cursor: all of it for instance
    /// admins, or one project's part for that project's admins. With it come the users who
    /// acted in these entries, so a client can name them.
    pub async fn list_audit_entries(
        &self,
        principal: &Principal,
        project_id: Option<ProjectId>,
        before: Option<AuditEntryId>,
        limit: u32,
    ) -> Result<AuditPage, ControlError> {
        match project_id {
            None if !principal.is_instance_admin() => return Err(ControlError::Forbidden),
            None => {}
            Some(id) => {
                let project = self.store().get_project(id).await?;
                self.access(principal).await?.check_project(&project, Role::Admin)?;
            }
        }
        let limit = page::limit(limit);
        let entries = Page::from_overfetch(self.store().list_audit_entries(project_id, None, before, limit + 1).await?, limit);
        let mut ids: Vec<UserId> = entries
            .items
            .iter()
            .filter_map(|entry| match entry.actor {
                Actor::User(id) => Some(id),
                _ => None,
            })
            .collect();
        ids.sort();
        ids.dedup();
        let mut users = Vec::new();
        for id in ids {
            match self.store().get_user(id).await {
                Ok(user) => users.push(user),
                Err(StoreError::NotFound { .. }) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(AuditPage { entries, users })
    }

    pub(crate) async fn audit(&self, entry: NewAuditEntry) -> Result<(), ControlError> {
        self.store().record_audit_entry(entry).await?;
        Ok(())
    }

    /// Records `action` (a static `entity.verb`) by `principal` on `target`. `details` is a JSON object.
    pub(crate) async fn audit_by(
        &self,
        principal: &Principal,
        action: &'static str,
        target: impl ToString,
        project_id: Option<ProjectId>,
        details: serde_json::Value,
    ) -> Result<(), ControlError> {
        self.audit(NewAuditEntry {
            actor: principal.actor(),
            token_id: principal.token_id(),
            action: action.parse().expect("valid audit action"),
            target_id: Some(target.to_string()),
            project_id,
            details: details.as_object().cloned().unwrap_or_default(),
        })
        .await
    }
}

/// Audit details for an update: which fields were given, as `{"changed": ["name", …]}`.
/// Values are left out, since some fields are secret.
pub(crate) fn changed(fields: &[(&'static str, bool)]) -> serde_json::Value {
    let changed: Vec<_> = fields.iter().filter(|(_, given)| *given).map(|(field, _)| *field).collect();
    serde_json::json!({ "changed": changed })
}
