//! Routes: what each server's proxy does with a hostname. The control plane builds a route
//! table per server from the live deployments on it and their services' domains, and the
//! agent hands it to its proxy.
//!
//! Every app service also gets a generated hostname under the instance's base domain,
//! `<service>-<environment>-<project>.<base domain>`, such as `web-production-shop.localhost`.
//! It is one label under the base domain, so one wildcard DNS record and one wildcard
//! certificate cover every generated hostname.

use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    str::FromStr,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    domain::Hostname,
    id::{DeploymentId, ServiceId},
    slug::Slug,
};

/// The longest DNS label.
const MAX_LABEL_LEN: usize = 63;

/// How many characters of the service ID tell apart generated labels that would clash.
const SUFFIX_LEN: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Route {
    pub hostname: Hostname,
    #[serde(flatten)]
    pub action: RouteAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RouteAction {
    /// Proxy requests to the deployment's container on this server.
    Proxy(ProxyTarget),
    /// Redirect to another hostname, keeping the path and query.
    Redirect { to: Hostname },
    /// Proxy requests to the control plane's dashboard and API. Only the embedded server's
    /// agent knows where they listen; other agents answer that nothing is served here.
    Dashboard,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ProxyTarget {
    pub service_id: ServiceId,
    pub deployment_id: DeploymentId,
    /// The port the app listens on inside its container.
    pub port: u16,
}

/// A server's full route table. Revisions only identify a table: a new one gets a higher
/// revision than the one before, but the agent applies whatever it is sent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct RouteTable {
    pub revision: u64,
    pub routes: Vec<Route>,
}

impl RouteTable {
    /// Whether any route proxies to `deployment`.
    pub fn targets(&self, deployment: DeploymentId) -> bool {
        self.routes.iter().any(|route| matches!(&route.action, RouteAction::Proxy(target) if target.deployment_id == deployment))
    }
}

/// The domain generated hostnames go under, such as `apps.example.com` or `localhost`.
/// Unlike a [`Hostname`] it can be a single label, and it is never a wildcard.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BaseDomain(String);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("`{value}` is not a valid base domain: {problem}")]
pub struct BaseDomainError {
    value: String,
    problem: String,
}

impl BaseDomain {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `hostname` is the base domain or any name under it, such as a generated one.
    pub fn contains(&self, hostname: &Hostname) -> bool {
        let name = hostname.as_str();
        name == self.0 || name.strip_suffix(self.0.as_str()).is_some_and(|prefix| prefix.ends_with('.'))
    }

    /// The hostname for a label directly under the base domain.
    fn hostname(&self, label: &str) -> Option<Hostname> {
        format!("{label}.{}", self.0).parse().ok()
    }
}

impl FromStr for BaseDomain {
    type Err = BaseDomainError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = |problem: &str| BaseDomainError { value: value.to_string(), problem: problem.to_string() };
        if value.starts_with("*.") {
            return Err(invalid("give the domain itself, without `*.`"));
        }
        // Valid exactly when a name one level below it is a valid hostname.
        let below: Hostname = format!("a.{value}")
            .parse()
            .map_err(|_| invalid("use letters, digits, hyphens and dots, such as apps.example.com or localhost"))?;
        let base = &below.as_str()["a.".len()..];
        // Room for the longest generated label.
        if MAX_LABEL_LEN + 1 + base.len() > crate::domain::MAX_HOSTNAME_LEN {
            return Err(invalid("it is too long to have names under it"));
        }
        Ok(Self(base.to_string()))
    }
}

impl fmt::Display for BaseDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for BaseDomain {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for BaseDomain {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// The slugs a service's generated hostname is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceNames {
    pub service_id: ServiceId,
    pub service: Slug,
    pub environment: Slug,
    pub project: Slug,
}

/// The generated hostname of each service. Labels are `<service>-<environment>-<project>`;
/// one that is too long, or that an older service already has (slugs contain hyphens, so
/// `a-b`/`c` and `a`/`b-c` meet), is shortened if needed and ends with the last characters
/// of the service ID instead. A service whose hostname would still clash gets none.
pub fn generated_hostnames(services: &[ServiceNames], base: &BaseDomain) -> BTreeMap<ServiceId, Hostname> {
    let mut services: Vec<_> = services.iter().collect();
    // Older services keep the plain label when two would clash.
    services.sort_by_key(|names| names.service_id);
    let mut taken = HashSet::new();
    let mut hostnames = BTreeMap::new();
    for names in services {
        let plain = format!("{}-{}-{}", names.service, names.environment, names.project);
        let id = names.service_id.to_string().to_ascii_lowercase();
        let suffix = &id[id.len() - SUFFIX_LEN..];
        let label = if plain.len() <= MAX_LABEL_LEN && !taken.contains(&plain) {
            plain
        } else {
            let mut prefix = plain;
            prefix.truncate(MAX_LABEL_LEN - SUFFIX_LEN - 1);
            format!("{}-{suffix}", prefix.trim_end_matches('-'))
        };
        if !taken.insert(label.clone()) {
            continue;
        }
        if let Some(hostname) = base.hostname(&label) {
            hostnames.insert(names.service_id, hostname);
        }
    }
    hostnames
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(service: &str, environment: &str, project: &str) -> ServiceNames {
        ServiceNames {
            service_id: ServiceId::new(),
            service: service.parse().unwrap(),
            environment: environment.parse().unwrap(),
            project: project.parse().unwrap(),
        }
    }

    #[test]
    fn base_domains_can_be_one_label() {
        let base: BaseDomain = "Apps.Example.com.".parse().unwrap();

        assert_eq!(base.as_str(), "apps.example.com");
        assert_eq!("localhost".parse::<BaseDomain>().unwrap().as_str(), "localhost");
        assert!(base.contains(&"web.apps.example.com".parse().unwrap()));
        assert!(base.contains(&"a.b.apps.example.com".parse().unwrap()));
        assert!(base.contains(&"apps.example.com".parse().unwrap()));
        assert!(!base.contains(&"xapps.example.com".parse().unwrap()));
        assert!(!base.contains(&"example.com".parse().unwrap()));
        for invalid in ["", "*.example.com", "a b", "-a.com", "1.2.3.4", &format!("{}.com", "a".repeat(64))] {
            assert!(invalid.parse::<BaseDomain>().is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn generates_one_label_per_service() {
        let base: BaseDomain = "localhost".parse().unwrap();
        let web = names("web", "production", "shop");
        let api = names("api", "staging", "shop");

        let hostnames = generated_hostnames(&[web.clone(), api.clone()], &base);

        assert_eq!(hostnames[&web.service_id].as_str(), "web-production-shop.localhost");
        assert_eq!(hostnames[&api.service_id].as_str(), "api-staging-shop.localhost");
    }

    #[test]
    fn clashing_and_long_labels_get_a_suffix() {
        let base: BaseDomain = "apps.example.com".parse().unwrap();
        let older = names("a-b", "c", "shop");
        let newer = names("a", "b-c", "shop");
        let long = names(&"x".repeat(50), "production", "shop");

        let hostnames = generated_hostnames(&[newer.clone(), long.clone(), older.clone()], &base);

        assert_eq!(hostnames[&older.service_id].as_str(), "a-b-c-shop.apps.example.com");
        let suffix = |names: &ServiceNames| {
            names.service_id.to_string().to_ascii_lowercase()[names.service_id.to_string().len() - 6..].to_string()
        };
        assert_eq!(hostnames[&newer.service_id].as_str(), format!("a-b-c-shop-{}.apps.example.com", suffix(&newer)));
        let label = hostnames[&long.service_id].as_str().split('.').next().unwrap().to_string();
        assert_eq!(label.len(), 63);
        assert!(label.starts_with(&"x".repeat(50)) && label.ends_with(&suffix(&long)));
    }

    #[test]
    fn route_tables_serialize_flat() {
        let target = ProxyTarget { service_id: ServiceId::new(), deployment_id: DeploymentId::new(), port: 3000 };
        let table = RouteTable {
            revision: 3,
            routes: vec![
                Route { hostname: "app.example.com".parse().unwrap(), action: RouteAction::Proxy(target.clone()) },
                Route {
                    hostname: "www.example.com".parse().unwrap(),
                    action: RouteAction::Redirect { to: "app.example.com".parse().unwrap() },
                },
            ],
        };

        let json = serde_json::to_value(&table).unwrap();

        assert_eq!(json["routes"][0]["action"], "proxy");
        assert_eq!(json["routes"][0]["port"], 3000);
        assert_eq!(json["routes"][1]["to"], "app.example.com");
        assert_eq!(serde_json::from_value::<RouteTable>(json).unwrap(), table);
        assert!(table.targets(target.deployment_id));
        assert!(!table.targets(DeploymentId::new()));
    }
}
