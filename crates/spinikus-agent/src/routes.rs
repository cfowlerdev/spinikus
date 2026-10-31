//! Routes on the agent: the table the control plane sends is saved, handed to the proxy,
//! and acknowledged. The proxy finds a deployment's container through the runtime.

use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use async_trait::async_trait;
use spinikus_core::{
    id::DeploymentId,
    route::{ProxyTarget, Route, RouteAction, RouteTable},
    server::InstanceStatus,
};
use spinikus_proto::agent::v1 as pb;
use spinikus_proxy::{Proxy, Upstreams};

use crate::{AgentError, AgentFiles, runtime::ContainerRuntime};

/// The current routes, kept across connections.
pub(crate) struct RouteState {
    files: AgentFiles,
    table: RouteTable,
    proxy: Option<Arc<Proxy>>,
}

impl RouteState {
    /// Starts from the saved table, so the proxy serves before the control plane answers.
    pub(crate) fn load(files: &AgentFiles, proxy: Option<Arc<Proxy>>) -> Self {
        let table = files.load_routes().unwrap_or_else(|error| {
            tracing::warn!(%error, "ignored the saved routes");
            RouteTable::default()
        });
        if let Some(proxy) = &proxy {
            proxy.set_routes(&table.routes);
        }
        Self { files: files.clone(), table, proxy }
    }

    pub(crate) fn revision(&self) -> u64 {
        self.table.revision
    }

    /// Saves and serves a new table. Routes the agent cannot read are left out.
    pub(crate) fn apply(&mut self, set: pb::SetRoutes) -> Result<(), AgentError> {
        let table = RouteTable { revision: set.revision, routes: set.routes.into_iter().filter_map(route).collect() };
        self.files.save_routes(&table)?;
        if let Some(proxy) = &self.proxy {
            proxy.set_routes(&table.routes);
        }
        tracing::info!(revision = table.revision, routes = table.routes.len(), "applied routes");
        self.table = table;
        Ok(())
    }
}

fn route(route: pb::Route) -> Option<Route> {
    let skip = |problem: &str| tracing::warn!(hostname = route.hostname, problem, "ignored a route");
    let Ok(hostname) = route.hostname.parse() else {
        skip("invalid hostname");
        return None;
    };
    let action = match route.action {
        Some(pb::route::Action::Proxy(target)) => {
            let (Ok(service_id), Ok(deployment_id), Ok(port)) =
                (target.service_id.parse(), target.deployment_id.parse(), u16::try_from(target.port))
            else {
                skip("invalid target");
                return None;
            };
            RouteAction::Proxy(ProxyTarget { service_id, deployment_id, port })
        }
        Some(pb::route::Action::RedirectTo(to)) => match to.parse() {
            Ok(to) => RouteAction::Redirect { to },
            Err(_) => {
                skip("invalid redirect");
                return None;
            }
        },
        None => {
            skip("no action");
            return None;
        }
    };
    Some(Route { hostname, action })
}

/// Finds a deployment's running container and the loopback port its app port is
/// published on, remembering it until connecting there fails.
pub(crate) struct RuntimeUpstreams {
    runtime: Arc<dyn ContainerRuntime>,
    known: std::sync::Mutex<HashMap<DeploymentId, SocketAddr>>,
}

impl RuntimeUpstreams {
    pub(crate) fn new(runtime: Arc<dyn ContainerRuntime>) -> Self {
        Self { runtime, known: Default::default() }
    }

    fn known(&self) -> std::sync::MutexGuard<'_, HashMap<DeploymentId, SocketAddr>> {
        self.known.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl Upstreams for RuntimeUpstreams {
    async fn resolve(&self, target: &ProxyTarget) -> Option<SocketAddr> {
        if let Some(address) = self.known().get(&target.deployment_id) {
            return Some(*address);
        }
        let instances = match self.runtime.list().await {
            Ok(instances) => instances,
            Err(error) => {
                tracing::warn!(%error, "the proxy could not list containers");
                return None;
            }
        };
        let container = instances
            .into_iter()
            .find(|instance| instance.deployment_id == target.deployment_id && instance.status == InstanceStatus::Running)?;
        let address = self.runtime.endpoint(&container.container_id).await.ok().flatten()?;
        self.known().insert(target.deployment_id, address);
        Some(address)
    }

    fn failed(&self, target: &ProxyTarget, address: SocketAddr) {
        let mut known = self.known();
        if known.get(&target.deployment_id) == Some(&address) {
            known.remove(&target.deployment_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use spinikus_core::id::ServiceId;

    use super::*;
    use crate::runtime::{ContainerSpec, MemoryRuntime};

    fn set(revision: u64, routes: Vec<pb::Route>) -> pb::SetRoutes {
        pb::SetRoutes { revision, routes }
    }

    #[test]
    fn saves_valid_routes_and_skips_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let files = AgentFiles::new(dir.path().join("agent"));
        let mut state = RouteState::load(&files, None);
        let target = pb::ProxyTarget {
            service_id: ServiceId::new().to_string(),
            deployment_id: DeploymentId::new().to_string(),
            port: 3000,
        };
        let routes = vec![
            pb::Route { hostname: "app.example.com".to_string(), action: Some(pb::route::Action::Proxy(target.clone())) },
            pb::Route {
                hostname: "www.example.com".to_string(),
                action: Some(pb::route::Action::RedirectTo("app.example.com".to_string())),
            },
            pb::Route { hostname: "not a host".to_string(), action: Some(pb::route::Action::Proxy(target.clone())) },
            pb::Route {
                hostname: "big.example.com".to_string(),
                action: Some(pb::route::Action::Proxy(pb::ProxyTarget { port: 70000, ..target })),
            },
            pb::Route { hostname: "none.example.com".to_string(), action: None },
        ];

        assert_eq!(state.revision(), 0);
        state.apply(set(5, routes)).unwrap();

        assert_eq!(state.revision(), 5);
        let saved = files.load_routes().unwrap();
        assert_eq!(saved.revision, 5);
        let names: Vec<_> = saved.routes.iter().map(|route| route.hostname.as_str()).collect();
        assert_eq!(names, ["app.example.com", "www.example.com"]);
        assert_eq!(RouteState::load(&files, None).revision(), 5);
    }

    #[tokio::test]
    async fn finds_the_running_container_of_a_deployment() {
        let runtime = Arc::new(MemoryRuntime::new());
        let (service, deployment) = (ServiceId::new(), DeploymentId::new());
        let spec = ContainerSpec {
            name: format!("spinikus-{deployment}"),
            image: "web@sha256:01".to_string(),
            service_id: service,
            deployment_id: deployment,
            env: Vec::new(),
            port: Some(3000),
            volumes: Vec::new(),
        };
        let container = runtime.create(&spec).await.unwrap();
        let address: SocketAddr = "127.0.0.1:49152".parse().unwrap();
        runtime.set_endpoint(address);
        let upstreams = RuntimeUpstreams::new(runtime.clone());
        let target = ProxyTarget { service_id: service, deployment_id: deployment, port: 3000 };

        let before_start = upstreams.resolve(&target).await;
        runtime.start(&container).await.unwrap();
        let running = upstreams.resolve(&target).await;
        let other = ProxyTarget { deployment_id: DeploymentId::new(), ..target.clone() };
        let unknown = upstreams.resolve(&other).await;
        upstreams.failed(&target, address);

        assert_eq!(before_start, None);
        assert_eq!(running, Some(address));
        assert_eq!(unknown, None);
        assert!(upstreams.known().is_empty());
    }
}
