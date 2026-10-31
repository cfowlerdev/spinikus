//! Looking up a request's host in the route table.

use std::collections::HashMap;

use spinikus_core::route::{Route, RouteAction};

/// A route table ready for lookups: exact hostnames first, then wildcards one level up.
#[derive(Debug, Default)]
pub struct Routes {
    exact: HashMap<String, RouteAction>,
    /// By the part after `*.`.
    wildcard: HashMap<String, RouteAction>,
}

impl Routes {
    pub fn new(routes: &[Route]) -> Self {
        let mut table = Self::default();
        for route in routes {
            let name = route.hostname.as_str();
            match name.strip_prefix("*.") {
                Some(parent) => table.wildcard.insert(parent.to_string(), route.action.clone()),
                None => table.exact.insert(name.to_string(), route.action.clone()),
            };
        }
        table
    }

    pub fn len(&self) -> usize {
        self.exact.len() + self.wildcard.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The action for a `Host` header value: case and a port or trailing dot don't matter.
    pub fn lookup(&self, host: &str) -> Option<&RouteAction> {
        let name = host_name(host).to_ascii_lowercase();
        let name = name.strip_suffix('.').unwrap_or(&name);
        if let Some(action) = self.exact.get(name) {
            return Some(action);
        }
        let (_, parent) = name.split_once('.')?;
        self.wildcard.get(parent)
    }
}

/// The host without its port. IPv6 literals keep their brackets; they never match a route.
pub(crate) fn host_name(host: &str) -> &str {
    if host.starts_with('[') {
        return host.split_once(']').map_or(host, |(name, _)| &host[..=name.len()]);
    }
    host.rsplit_once(':').filter(|(_, port)| port.bytes().all(|byte| byte.is_ascii_digit())).map_or(host, |(name, _)| name)
}

#[cfg(test)]
mod tests {
    use spinikus_core::{
        id::{DeploymentId, ServiceId},
        route::ProxyTarget,
    };

    use super::*;

    fn proxy(port: u16) -> RouteAction {
        RouteAction::Proxy(ProxyTarget { service_id: ServiceId::new(), deployment_id: DeploymentId::new(), port })
    }

    #[test]
    fn finds_exact_names_then_wildcards() {
        let routes = Routes::new(&[
            Route { hostname: "app.example.com".parse().unwrap(), action: proxy(1) },
            Route { hostname: "*.example.com".parse().unwrap(), action: proxy(2) },
            Route {
                hostname: "www.example.org".parse().unwrap(),
                action: RouteAction::Redirect { to: "example.org".parse().unwrap() },
            },
        ]);
        let port = |host| match routes.lookup(host) {
            Some(RouteAction::Proxy(target)) => Some(target.port),
            _ => None,
        };

        assert_eq!(routes.len(), 3);
        assert_eq!(port("app.example.com"), Some(1));
        assert_eq!(port("APP.example.com:8000"), Some(1));
        assert_eq!(port("app.example.com."), Some(1));
        assert_eq!(port("other.example.com"), Some(2));
        assert_eq!(port("a.b.example.com"), None);
        assert_eq!(port("example.com"), None);
        assert!(matches!(routes.lookup("www.example.org"), Some(RouteAction::Redirect { .. })));
        assert!(routes.lookup("[::1]:8000").is_none());
        assert!(routes.lookup("").is_none());
    }

    #[test]
    fn strips_ports() {
        assert_eq!(host_name("example.com:8080"), "example.com");
        assert_eq!(host_name("example.com"), "example.com");
        assert_eq!(host_name("[::1]:80"), "[::1]");
        assert_eq!(host_name("[::1]"), "[::1]");
    }
}
