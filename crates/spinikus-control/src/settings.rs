//! Instance-wide settings. Anyone signed in reads them; instance admins change them. Only
//! known keys exist, each with a default and its own validation.

use serde_json::{Value, json};
use spinikus_core::{route::BaseDomain, setting::Setting};
use spinikus_store::SettingRepository;

use crate::{Control, ControlError, Principal, access::not_found};

/// The domain generated hostnames go under. `null` turns generated hostnames off.
pub const BASE_DOMAIN: &str = "base_domain";

/// `localhost` names resolve to this machine without any DNS, which suits a first start.
pub const DEFAULT_BASE_DOMAIN: &str = "localhost";

/// The known settings and their defaults.
fn default(key: &str) -> Option<Value> {
    match key {
        BASE_DOMAIN => Some(json!(DEFAULT_BASE_DOMAIN)),
        _ => None,
    }
}

/// Checks a new value for a known key, returning it normalized.
fn check(key: &str, value: Value) -> Result<Value, ControlError> {
    let invalid = |problem: String| ControlError::Invalid { field: "value", problem };
    match key {
        BASE_DOMAIN => match value {
            Value::Null => Ok(Value::Null),
            Value::String(domain) => {
                let domain: BaseDomain =
                    domain.parse().map_err(|error: spinikus_core::route::BaseDomainError| invalid(error.to_string()))?;
                Ok(json!(domain))
            }
            _ => Err(invalid("give a domain such as `apps.example.com`, or null".to_string())),
        },
        _ => Err(not_found("setting", key)),
    }
}

impl Control {
    pub async fn get_setting(&self, _principal: &Principal, key: &str) -> Result<Setting, ControlError> {
        self.setting(key).await
    }

    /// Sets the value if the setting is still at `revision` (0 for one never set).
    pub async fn set_setting(
        &self,
        principal: &Principal,
        key: &str,
        revision: i64,
        value: Value,
    ) -> Result<Setting, ControlError> {
        if !principal.is_instance_admin() {
            return Err(ControlError::Forbidden);
        }
        default(key).ok_or_else(|| not_found("setting", key))?;
        let value = check(key, value)?;
        let setting = self.store().put_setting(key, &value, revision).await?;
        self.audit_by(principal, "setting.update", key, None, json!({})).await?;
        // The base domain changes generated hostnames, so the routes.
        self.wake_deployments();
        Ok(setting)
    }

    async fn setting(&self, key: &str) -> Result<Setting, ControlError> {
        let default = default(key).ok_or_else(|| not_found("setting", key))?;
        Ok(self.store().get_setting(key).await?.unwrap_or(Setting {
            key: key.to_string(),
            value: default,
            revision: 0,
            updated_at: None,
        }))
    }

    /// The base domain, or `None` if generated hostnames are off.
    pub(crate) async fn base_domain(&self) -> Result<Option<BaseDomain>, ControlError> {
        let setting = self.setting(BASE_DOMAIN).await?;
        match setting.value {
            Value::Null => Ok(None),
            value => {
                serde_json::from_value(value).map(Some).map_err(|error| ControlError::internal("reading the base domain", error))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{control, user};

    #[tokio::test]
    async fn admins_set_the_base_domain() {
        let (_dir, control) = control().await;
        let admin = user(&control, "admin@example.com", true).await;
        let member = user(&control, "ada@example.com", false).await;

        let default = control.get_setting(&member, BASE_DOMAIN).await.unwrap();
        let by_member = control.set_setting(&member, BASE_DOMAIN, 0, json!("apps.example.com")).await;
        let invalid = control.set_setting(&admin, BASE_DOMAIN, 0, json!("*.example.com")).await;
        let wrong_type = control.set_setting(&admin, BASE_DOMAIN, 0, json!(42)).await;
        let unknown = control.set_setting(&admin, "theme", 0, json!("dark")).await;
        let set = control.set_setting(&admin, BASE_DOMAIN, 0, json!("Apps.Example.com")).await.unwrap();
        let base = control.base_domain().await.unwrap();
        let off = control.set_setting(&admin, BASE_DOMAIN, set.revision, Value::Null).await.unwrap();

        assert_eq!((default.value, default.revision), (json!("localhost"), 0));
        assert!(matches!(by_member, Err(ControlError::Forbidden)), "{by_member:?}");
        assert!(matches!(invalid, Err(ControlError::Invalid { field: "value", .. })), "{invalid:?}");
        assert!(matches!(wrong_type, Err(ControlError::Invalid { field: "value", .. })), "{wrong_type:?}");
        assert!(matches!(unknown, Err(ControlError::NotFound { entity: "setting", .. })), "{unknown:?}");
        assert_eq!((set.value, set.revision), (json!("apps.example.com"), 1));
        assert_eq!(base.unwrap().as_str(), "apps.example.com");
        assert_eq!(off.value, Value::Null);
        assert_eq!(control.base_domain().await.unwrap(), None);
    }
}
