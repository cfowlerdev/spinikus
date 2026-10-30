//! Recording actions in the audit log.

use spinikus_core::{audit::NewAuditEntry, id::ProjectId};
use spinikus_store::AuditLogRepository;

use crate::{Control, ControlError, Principal};

impl Control {
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
