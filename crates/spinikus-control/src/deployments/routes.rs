//! Route tables: what each server's proxy serves, built from the live deployments on it.
//!
//! A live app deployment with a port gets a route for each of its service's domains (a
//! redirect for domains that redirect) and for the service's generated hostname. Each pass
//! of the runner rebuilds every server's table, stores it (a change gets a new revision),
//! and sends a table its agent has not applied, again after a while if unanswered. A
//! draining deployment is stopped only once its server's agent has applied a table that no
//! longer routes to it, so no request reaches a stopping container. The certificates of the
//! routed hostnames follow the same way ([`Control::sync_certificates`]).
//!
//! The `dashboard_domain` setting adds a route on the embedded server to the control
//! plane's own dashboard and API, which gets its certificate like any other.

use std::collections::{BTreeMap, HashMap, HashSet};

use spinikus_core::{
    domain::Hostname,
    id::{ServerId, ServiceId},
    route::{ProxyTarget, Route, RouteAction, generated_hostnames},
};
use spinikus_proto::agent::v1::{self as pb, control_message};
use spinikus_store::{
    DeploymentRepository, DomainRepository, RouteRepository, ServerRepository, ServiceRepository, StoredRoutes,
};
use tokio::time::Instant;

use super::commands::settings;
use crate::{Control, ControlError};

/// Servers are listed in pages of this size.
const SERVER_PAGE: u32 = 200;

impl Control {
    /// The service's generated hostname, if generated hostnames are on and it is an app.
    pub(crate) async fn generated_hostname(&self, service_id: ServiceId) -> Result<Option<Hostname>, ControlError> {
        Ok(self.generated_hostnames().await?.remove(&service_id))
    }

    async fn generated_hostnames(&self) -> Result<BTreeMap<ServiceId, Hostname>, ControlError> {
        let Some(base) = self.base_domain().await? else {
            return Ok(BTreeMap::new());
        };
        Ok(generated_hostnames(&self.store().list_app_service_names().await?, &base))
    }

    /// Every server's routes, sorted by hostname. Servers without live deployments get
    /// none, so routes they had are removed.
    pub(crate) async fn build_routes(&self) -> Result<HashMap<ServerId, Vec<Route>>, ControlError> {
        let mut tables: HashMap<ServerId, Vec<Route>> = HashMap::new();
        let mut after = None;
        loop {
            let servers = self.store().list_servers(after, SERVER_PAGE).await?;
            after = servers.last().map(|server| server.id);
            for server in &servers {
                if server.revoked_at.is_none() {
                    tables.insert(server.id, Vec::new());
                }
            }
            if servers.len() < SERVER_PAGE as usize {
                break;
            }
        }

        // The dashboard's domain goes first and wins over any app's: only the embedded
        // server's agent can reach the dashboard.
        let mut claimed = HashSet::new();
        if let Some(hostname) = self.dashboard_domain().await?
            && let Some(server) = self.store().find_embedded_server().await?
            && let Some(table) = tables.get_mut(&server.id)
        {
            claimed.insert(hostname.clone());
            table.push(Route { hostname, action: RouteAction::Dashboard });
        }

        let generated = self.generated_hostnames().await?;
        let mut domains: HashMap<ServiceId, Vec<_>> = HashMap::new();
        for domain in self.store().list_all_domains().await? {
            domains.entry(domain.service_id).or_default().push(domain);
        }
        // Domains are unique, but a domain added before the base domain changed can have a
        // generated hostname's name: the domain wins.
        let mut live = Vec::new();
        for deployment in self.store().list_live_deployments().await? {
            let Some(port) = settings(&deployment).ok().and_then(|settings| settings.port) else {
                continue;
            };
            let Some(table) = tables.get_mut(&deployment.server_id) else {
                continue;
            };
            let target = ProxyTarget { service_id: deployment.service_id, deployment_id: deployment.id, port };
            for domain in domains.get(&deployment.service_id).into_iter().flatten() {
                let action = match domain.redirect_to {
                    Some(to) => match domains[&deployment.service_id].iter().find(|other| other.id == to) {
                        Some(to) => RouteAction::Redirect { to: to.hostname.clone() },
                        None => continue,
                    },
                    None => RouteAction::Proxy(target.clone()),
                };
                if claimed.insert(domain.hostname.clone()) {
                    table.push(Route { hostname: domain.hostname.clone(), action });
                }
            }
            live.push((deployment.server_id, target));
        }
        for (server, target) in live {
            if let Some(hostname) = generated.get(&target.service_id)
                && claimed.insert(hostname.clone())
            {
                let route = Route { hostname: hostname.clone(), action: RouteAction::Proxy(target) };
                tables.get_mut(&server).expect("checked above").push(route);
            }
        }
        for routes in tables.values_mut() {
            routes.sort_by(|a, b| a.hostname.cmp(&b.hostname));
        }
        Ok(tables)
    }

    /// Stores every server's routes and sends each table its agent has not applied yet, then
    /// does the same for their certificates.
    pub(super) async fn sync_routes(&self) -> Result<(), ControlError> {
        let tables = self.build_routes().await?;
        let mut sent = self.runner().routes_sent.lock().await;
        for (&server, routes) in &tables {
            let stored = self.store().save_route_table(server, routes.clone()).await?;
            if stored.applied() {
                sent.remove(&server);
                continue;
            }
            if let Some((revision, at)) = sent.get(&server)
                && *revision == stored.table.revision
                && at.elapsed() < self.runner().timings.resend_after
            {
                continue;
            }
            if self.agents().send(server, set_routes(&stored)).await.is_ok() {
                tracing::debug!(server = %server, revision = stored.table.revision, "sent routes");
                sent.insert(server, (stored.table.revision, Instant::now()));
                // New hostnames may need certificates.
                self.wake_certificates();
            }
        }
        drop(sent);
        self.sync_certificates(&tables).await
    }

    /// The agent applied the routes with `revision`, now or before it connected.
    pub(crate) async fn routes_applied(&self, server: ServerId, revision: u64) {
        if let Err(error) = self.store().record_routes_applied(server, revision).await {
            tracing::error!(server = %server, %error, "failed to record applied routes");
        }
        self.wake_deployments();
    }
}

fn set_routes(stored: &StoredRoutes) -> pb::ControlMessage {
    let routes = stored
        .table
        .routes
        .iter()
        .map(|route| pb::Route {
            hostname: route.hostname.to_string(),
            action: Some(match &route.action {
                RouteAction::Proxy(target) => pb::route::Action::Proxy(pb::ProxyTarget {
                    service_id: target.service_id.to_string(),
                    deployment_id: target.deployment_id.to_string(),
                    port: target.port.into(),
                }),
                RouteAction::Redirect { to } => pb::route::Action::RedirectTo(to.to_string()),
                RouteAction::Dashboard => pb::route::Action::Dashboard(true),
            }),
        })
        .collect();
    let set = pb::SetRoutes { revision: stored.table.revision, routes };
    pb::ControlMessage { message: Some(control_message::Message::SetRoutes(set)) }
}
