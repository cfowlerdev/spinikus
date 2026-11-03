//! The proxy's state on the agent: the route table and certificates the control plane
//! sends are saved, handed to the proxy, and acknowledged; ACME challenges are only held
//! in memory. The proxy finds a deployment's container through the runtime.

use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use async_trait::async_trait;
use spinikus_core::{
    certificate::CertificateBundle,
    id::DeploymentId,
    route::{ProxyTarget, Route, RouteAction, RouteTable},
    secret::Secret,
    server::InstanceStatus,
};
use spinikus_proto::agent::v1 as pb;
use spinikus_proxy::{Proxy, Upstreams};

use crate::{AgentError, AgentFiles, runtime::ContainerRuntime};

/// The current routes and certificates, kept across connections.
pub(crate) struct ProxyState {
    files: AgentFiles,
    routes: RouteTable,
    certificates_revision: u64,
    proxy: Option<Arc<Proxy>>,
}

impl ProxyState {
    /// Starts from the saved routes and certificates, so the proxy serves before the
    /// control plane answers.
    pub(crate) fn load(files: &AgentFiles, proxy: Option<Arc<Proxy>>) -> Self {
        let routes = files.load_routes().unwrap_or_else(|error| {
            tracing::warn!(%error, "ignored the saved routes");
            RouteTable::default()
        });
        let (certificates_revision, certificates) = files.load_certificates().unwrap_or_else(|error| {
            tracing::warn!(%error, "ignored the saved certificates");
            (0, Vec::new())
        });
        if let Some(proxy) = &proxy {
            proxy.set_routes(&routes.routes);
            serve_certificates(proxy, &certificates);
        }
        Self { files: files.clone(), routes, certificates_revision, proxy }
    }

    pub(crate) fn routes_revision(&self) -> u64 {
        self.routes.revision
    }

    pub(crate) fn certificates_revision(&self) -> u64 {
        self.certificates_revision
    }

    /// Saves and serves a new table. Routes the agent cannot read are left out.
    pub(crate) fn apply_routes(&mut self, set: pb::SetRoutes) -> Result<(), AgentError> {
        let table = RouteTable { revision: set.revision, routes: set.routes.into_iter().filter_map(route).collect() };
        self.files.save_routes(&table)?;
        if let Some(proxy) = &self.proxy {
            proxy.set_routes(&table.routes);
        }
        tracing::info!(revision = table.revision, routes = table.routes.len(), "applied routes");
        self.routes = table;
        Ok(())
    }

    /// Saves and serves new certificates. Certificates the agent cannot read are left out.
    pub(crate) fn apply_certificates(&mut self, set: pb::SetCertificates) -> Result<(), AgentError> {
        let bundles: Vec<_> = set.certificates.into_iter().filter_map(bundle).collect();
        self.files.save_certificates(set.revision, &bundles)?;
        if let Some(proxy) = &self.proxy {
            serve_certificates(proxy, &bundles);
        }
        tracing::info!(revision = set.revision, certificates = bundles.len(), "applied certificates");
        self.certificates_revision = set.revision;
        Ok(())
    }

    /// Answers these HTTP-01 challenges from now on, instead of the previous ones.
    pub(crate) fn apply_challenges(&self, set: pb::AcmeChallenges) {
        if let Some(proxy) = &self.proxy {
            let challenges = set.challenges.into_iter().map(|challenge| (challenge.token, challenge.key_authorization));
            proxy.set_challenges(challenges.collect());
        }
    }
}

fn serve_certificates(proxy: &Proxy, bundles: &[CertificateBundle]) {
    for problem in proxy.set_certificates(bundles) {
        tracing::warn!(problem, "ignored a certificate");
    }
}

fn bundle(certificate: pb::Certificate) -> Option<CertificateBundle> {
    let Ok(id) = certificate.certificate_id.parse() else {
        tracing::warn!(certificate = certificate.certificate_id, "ignored a certificate with an invalid ID");
        return None;
    };
    let hostnames = certificate
        .hostnames
        .iter()
        .filter_map(|hostname| match hostname.parse() {
            Ok(hostname) => Some(hostname),
            Err(_) => {
                tracing::warn!(certificate = %id, hostname, "ignored an invalid hostname of a certificate");
                None
            }
        })
        .collect();
    Some(CertificateBundle {
        id,
        hostnames,
        chain_pem: certificate.chain_pem,
        private_key_pem: Secret::new(certificate.private_key_pem),
    })
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
        Some(pb::route::Action::Dashboard(_)) => RouteAction::Dashboard,
        None => {
            skip("no action");
            return None;
        }
    };
    Some(Route { hostname, action })
}

/// Finds a deployment's running container and the loopback port its app port is
/// published on, remembering it until connecting there fails.
/// The dashboard is at a fixed address, if this agent runs beside the control plane.
pub(crate) struct RuntimeUpstreams {
    runtime: Arc<dyn ContainerRuntime>,
    known: std::sync::Mutex<HashMap<DeploymentId, SocketAddr>>,
    dashboard: Option<SocketAddr>,
}

impl RuntimeUpstreams {
    pub(crate) fn new(runtime: Arc<dyn ContainerRuntime>, dashboard: Option<SocketAddr>) -> Self {
        Self { runtime, known: Default::default(), dashboard }
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

    fn dashboard(&self) -> Option<SocketAddr> {
        self.dashboard
    }
}

#[cfg(test)]
mod tests {
    use spinikus_core::id::{CertificateId, ServiceId};

    use super::*;
    use crate::runtime::{ContainerSpec, MemoryRuntime};

    fn set(revision: u64, routes: Vec<pb::Route>) -> pb::SetRoutes {
        pb::SetRoutes { revision, routes }
    }

    #[test]
    fn saves_valid_routes_and_skips_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let files = AgentFiles::new(dir.path().join("agent"));
        let mut state = ProxyState::load(&files, None);
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

        assert_eq!(state.routes_revision(), 0);
        state.apply_routes(set(5, routes)).unwrap();

        assert_eq!(state.routes_revision(), 5);
        let saved = files.load_routes().unwrap();
        assert_eq!(saved.revision, 5);
        let names: Vec<_> = saved.routes.iter().map(|route| route.hostname.as_str()).collect();
        assert_eq!(names, ["app.example.com", "www.example.com"]);
        assert_eq!(ProxyState::load(&files, None).routes_revision(), 5);
    }

    #[test]
    fn saves_certificates_and_skips_unreadable_ones() {
        let dir = tempfile::tempdir().unwrap();
        let files = AgentFiles::new(dir.path().join("agent"));
        let mut state = ProxyState::load(&files, None);
        let id = CertificateId::new();
        let certificates = vec![
            pb::Certificate {
                certificate_id: id.to_string(),
                hostnames: vec!["app.example.com".to_string(), "not a host".to_string()],
                chain_pem: "chain".to_string(),
                private_key_pem: "key".to_string(),
            },
            pb::Certificate { certificate_id: "nope".to_string(), ..Default::default() },
        ];

        state.apply_certificates(pb::SetCertificates { revision: 7, certificates }).unwrap();

        assert_eq!(state.certificates_revision(), 7);
        let (revision, saved) = files.load_certificates().unwrap();
        assert_eq!(revision, 7);
        assert_eq!(saved.len(), 1);
        assert_eq!((saved[0].id, saved[0].hostnames.len()), (id, 1));
        assert_eq!(ProxyState::load(&files, None).certificates_revision(), 7);
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
            network: None,
            command: Vec::new(),
        };
        let container = runtime.create(&spec).await.unwrap();
        let address: SocketAddr = "127.0.0.1:49152".parse().unwrap();
        runtime.set_endpoint(address);
        let upstreams = RuntimeUpstreams::new(runtime.clone(), None);
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
