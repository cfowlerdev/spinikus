//! Instance-wide settings. Anyone signed in reads them; instance admins change them. Only
//! known keys exist, each with a default and its own validation.

use serde_json::{Value, json};
use spinikus_core::{domain::Hostname, route::BaseDomain, setting::Setting};
use spinikus_store::{DomainRepository, SettingRepository};

use crate::{Control, ControlError, Principal, access::not_found};

/// The domain generated hostnames go under. `null` turns generated hostnames off.
pub const BASE_DOMAIN: &str = "base_domain";

/// `localhost` names resolve to this machine without any DNS, which suits a first start.
pub const DEFAULT_BASE_DOMAIN: &str = "localhost";

/// The hostname the control plane's own server serves the dashboard and API on, through
/// its proxy, with a certificate like any routed hostname. `null`, the default, serves
/// them only on `[http] listen`.
pub const DASHBOARD_DOMAIN: &str = "dashboard_domain";

/// The contact email for the ACME account that gets HTTPS certificates. Setting it agrees
/// to the CA's terms of service; `null`, the default, turns getting certificates off.
pub const ACME_EMAIL: &str = "acme_email";

/// The ACME directory of the CA that issues certificates.
pub const ACME_DIRECTORY_URL: &str = "acme_directory_url";

/// Let's Encrypt's production directory.
pub const DEFAULT_ACME_DIRECTORY_URL: &str = "https://acme-v02.api.letsencrypt.org/directory";

/// How many earlier deployments per service keep their image, so they can be rolled back to.
pub const RELEASES_KEPT: &str = "releases_kept";

pub const DEFAULT_RELEASES_KEPT: u64 = 5;

/// The most `releases_kept` can be.
pub const MAX_RELEASES_KEPT: u64 = 100;

/// The known settings and their defaults.
fn default(key: &str) -> Option<Value> {
    match key {
        BASE_DOMAIN => Some(json!(DEFAULT_BASE_DOMAIN)),
        DASHBOARD_DOMAIN => Some(Value::Null),
        ACME_EMAIL => Some(Value::Null),
        ACME_DIRECTORY_URL => Some(json!(DEFAULT_ACME_DIRECTORY_URL)),
        RELEASES_KEPT => Some(json!(DEFAULT_RELEASES_KEPT)),
        _ => None,
    }
}

/// Where and as whom to get certificates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AcmeSettings {
    pub(crate) email: String,
    pub(crate) directory_url: String,
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
        DASHBOARD_DOMAIN => match value {
            Value::Null => Ok(Value::Null),
            Value::String(name) => {
                let hostname: Hostname =
                    name.trim().parse().map_err(|error: spinikus_core::domain::HostnameError| invalid(error.to_string()))?;
                if hostname.is_wildcard() {
                    return Err(invalid("give one hostname, such as `spinikus.example.com`, not a wildcard".to_string()));
                }
                Ok(json!(hostname))
            }
            _ => Err(invalid("give a hostname such as `spinikus.example.com`, or null".to_string())),
        },
        ACME_EMAIL => match value {
            Value::Null => Ok(Value::Null),
            Value::String(email) => {
                let email = email.trim();
                let valid = email.split_once('@').is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'))
                    && !email.chars().any(|c| c.is_whitespace() || c.is_control() || c == ',');
                match valid {
                    true => Ok(json!(email)),
                    false => Err(invalid("give an email address, or null".to_string())),
                }
            }
            _ => Err(invalid("give an email address, or null".to_string())),
        },
        ACME_DIRECTORY_URL => match value {
            Value::String(url) if url.starts_with("https://") && url.len() > "https://".len() => Ok(json!(url.trim())),
            _ => Err(invalid("give the https URL of an ACME directory".to_string())),
        },
        RELEASES_KEPT => match value.as_u64() {
            Some(count) if count <= MAX_RELEASES_KEPT => Ok(json!(count)),
            _ => Err(invalid(format!("give a whole number from 0 to {MAX_RELEASES_KEPT}"))),
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
        if key == DASHBOARD_DOMAIN
            && let Value::String(name) = &value
        {
            let hostname: Hostname = name.parse().map_err(|error| ControlError::internal("reading a checked hostname", error))?;
            if self.store().find_domain_by_hostname(&hostname).await?.is_some() {
                return Err(ControlError::Conflict { entity: "domain", field: "hostname", value: hostname.to_string() });
            }
        }
        let setting = self.store().put_setting(key, &value, revision).await?;
        self.audit_by(principal, "setting.update", key, None, json!({})).await?;
        // The base and dashboard domains change the routes, and the ACME settings which
        // certificates to get.
        self.wake_deployments();
        self.wake_certificates();
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

    /// The ACME email and directory, or `None` if getting certificates is off.
    pub(crate) async fn acme_settings(&self) -> Result<Option<AcmeSettings>, ControlError> {
        let read = |value: Value| {
            serde_json::from_value::<Option<String>>(value)
                .map_err(|error| ControlError::internal("reading the ACME settings", error))
        };
        let Some(email) = read(self.setting(ACME_EMAIL).await?.value)? else {
            return Ok(None);
        };
        let directory_url = read(self.setting(ACME_DIRECTORY_URL).await?.value)?.unwrap_or_default();
        Ok(Some(AcmeSettings { email, directory_url }))
    }

    /// How many earlier deployments per service to keep for rollback.
    pub(crate) async fn releases_kept(&self) -> Result<usize, ControlError> {
        let value = self.setting(RELEASES_KEPT).await?.value;
        let count = value.as_u64().ok_or_else(|| {
            ControlError::internal("reading releases_kept", std::io::Error::other(format!("not a count: {value}")))
        })?;
        Ok(count as usize)
    }

    /// The dashboard's own hostname, or `None` if it has none.
    pub(crate) async fn dashboard_domain(&self) -> Result<Option<Hostname>, ControlError> {
        let value = self.setting(DASHBOARD_DOMAIN).await?.value;
        serde_json::from_value(value).map_err(|error| ControlError::internal("reading the dashboard domain", error))
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

    #[tokio::test]
    async fn admins_turn_on_certificates_with_an_email() {
        let (_dir, control) = control().await;
        let admin = user(&control, "admin@example.com", true).await;

        let off = control.acme_settings().await.unwrap();
        let invalid = control.set_setting(&admin, ACME_EMAIL, 0, json!("not an email")).await;
        let plain_http = control.set_setting(&admin, ACME_DIRECTORY_URL, 0, json!("http://ca.test/dir")).await;
        control.set_setting(&admin, ACME_EMAIL, 0, json!(" ops@example.com ")).await.unwrap();
        let on = control.acme_settings().await.unwrap();

        assert_eq!(off, None);
        assert!(matches!(invalid, Err(ControlError::Invalid { field: "value", .. })), "{invalid:?}");
        assert!(matches!(plain_http, Err(ControlError::Invalid { field: "value", .. })), "{plain_http:?}");
        let expected =
            AcmeSettings { email: "ops@example.com".to_string(), directory_url: DEFAULT_ACME_DIRECTORY_URL.to_string() };
        assert_eq!(on, Some(expected));
    }

    #[tokio::test]
    async fn keeps_a_count_of_releases() {
        let (_dir, control) = control().await;
        let admin = user(&control, "admin@example.com", true).await;

        let default = control.releases_kept().await.unwrap();
        let too_many = control.set_setting(&admin, RELEASES_KEPT, 0, json!(MAX_RELEASES_KEPT + 1)).await;
        let negative = control.set_setting(&admin, RELEASES_KEPT, 0, json!(-1)).await;
        let fraction = control.set_setting(&admin, RELEASES_KEPT, 0, json!(2.5)).await;
        control.set_setting(&admin, RELEASES_KEPT, 0, json!(0)).await.unwrap();

        assert_eq!(default, 5);
        for refused in [too_many, negative, fraction] {
            assert!(matches!(refused, Err(ControlError::Invalid { field: "value", .. })), "{refused:?}");
        }
        assert_eq!(control.releases_kept().await.unwrap(), 0);
    }
}
