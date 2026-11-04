//! Noticing crash loops. Each deployment runner pass ends with [`Control::watch_crashes`],
//! which feeds what agents last reported about each live deployment's container to that
//! deployment's [`Watch`], and records a [`CrashLoop`] while it says the container is
//! crashing: begun when it starts, updated as it goes on, and ended when the container has
//! recovered, its deployment is no longer live, or its service was stopped or deleted.
//! Docker restarts the container itself; nothing here acts on it. Servers whose agent is
//! away are left as they were, since what they last reported may be stale. Watches live in
//! memory, so after the control plane starts a lasting crash loop ends only once its
//! container has run for `recovered_after` again.

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use jiff::Timestamp;
use spinikus_core::{
    crash::{Assessment, CrashLoop, CrashReason, CrashRules, Watch},
    id::{CrashLoopId, DeploymentId, EnvironmentId, ServerId, ServiceId},
    membership::Role,
    notification::{Message, NotificationEvent},
    server::Instance,
};
use spinikus_store::{
    CrashLoopRepository, DeploymentRepository, InstanceRepository, NewCrashLoop, ServiceRepository, StoreError,
};
use tokio::sync::Mutex;

use crate::{
    Control, ControlError, Principal,
    page::{self, Page},
};

/// How long ended crash loops are kept.
pub const KEEP_ENDED_FOR: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How often ended crash loops past [`KEEP_ENDED_FOR`] are deleted.
const PRUNE_EVERY: Duration = Duration::from_secs(60 * 60);

pub(crate) struct Crashes {
    rules: CrashRules,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    watches: HashMap<DeploymentId, Watched>,
    pruned_at: Option<Timestamp>,
}

struct Watched {
    watch: Watch,
    /// The restart count just before the lasting crash loop began, which its `restarts`
    /// count from.
    baseline: Option<u32>,
}

impl Crashes {
    pub(crate) fn new(rules: CrashRules) -> Self {
        Self { rules, state: Mutex::default() }
    }
}

impl Default for Crashes {
    fn default() -> Self {
        Self::new(CrashRules::default())
    }
}

impl Control {
    /// Replaces the rules for crash loops, such as to shorten them in tests.
    pub fn with_crash_rules(mut self, rules: CrashRules) -> Self {
        self.crashes = std::sync::Arc::new(Crashes::new(rules));
        self
    }

    /// The service's crash loops, newest first. Viewers may list them.
    pub async fn list_crash_loops(
        &self,
        principal: &Principal,
        service_id: ServiceId,
        after: Option<CrashLoopId>,
        limit: u32,
    ) -> Result<Page<CrashLoop>, ControlError> {
        self.service_with_access(principal, service_id, Role::Viewer).await?;
        let limit = page::limit(limit);
        let items = self.store().list_crash_loops(service_id, after, limit + 1).await?;
        Ok(Page::from_overfetch(items, limit))
    }

    /// The service's crash loop that hasn't ended, if it has one.
    pub async fn lasting_crash_loop(
        &self,
        principal: &Principal,
        service_id: ServiceId,
    ) -> Result<Option<CrashLoop>, ControlError> {
        self.service_with_access(principal, service_id, Role::Viewer).await?;
        let newest = self.store().list_crash_loops(service_id, None, 1).await?;
        Ok(newest.into_iter().find(|crash| crash.ended_at.is_none()))
    }

    /// The services of the environment that are crashing now.
    pub async fn crashing_services(
        &self,
        principal: &Principal,
        environment_id: EnvironmentId,
    ) -> Result<HashSet<ServiceId>, ControlError> {
        self.get_environment(principal, environment_id).await?;
        let mut crashing = HashSet::new();
        for crash in self.store().list_lasting_crash_loops().await? {
            match self.store().get_service(crash.service_id).await {
                Ok(service) if service.environment_id == environment_id => {
                    crashing.insert(service.id);
                }
                Ok(_) | Err(StoreError::NotFound { .. }) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(crashing)
    }

    /// Looks at every live deployment's container now.
    pub(crate) async fn watch_crashes(&self) {
        if let Err(error) = self.watch_crashes_at(Timestamp::now()).await {
            tracing::error!(%error, "failed to watch for crash loops");
        }
    }

    pub(crate) async fn watch_crashes_at(&self, now: Timestamp) -> Result<(), ControlError> {
        let rules = self.crashes.rules;
        let mut state = self.crashes.state.lock().await;
        let mut lasting: HashMap<DeploymentId, CrashLoop> =
            self.store().list_lasting_crash_loops().await?.into_iter().map(|crash| (crash.deployment_id, crash)).collect();
        let mut instances: HashMap<ServerId, Vec<Instance>> = HashMap::new();
        let mut watched = HashSet::new();

        for deployment in self.store().list_live_deployments().await? {
            match self.store().get_service(deployment.service_id).await {
                Ok(service) if service.stopped_at.is_none() => {}
                Ok(_) | Err(StoreError::NotFound { .. }) => continue,
                Err(error) => return Err(error.into()),
            }
            watched.insert(deployment.id);
            if !self.agents().is_connected(deployment.server_id) {
                continue;
            }
            let reported = match instances.get(&deployment.server_id) {
                Some(reported) => reported,
                None => {
                    let reported = self.store().list_server_instances(deployment.server_id).await?;
                    instances.entry(deployment.server_id).or_insert(reported)
                }
            };
            let seen = reported
                .iter()
                .find(|instance| instance.observed.deployment_id == deployment.id)
                .map(|instance| (instance.observed.status, instance.observed.restart_count));
            let watched =
                state.watches.entry(deployment.id).or_insert_with(|| Watched { watch: Watch::new(now), baseline: None });
            let count = seen.map_or(0, |(_, count)| count);

            match (watched.watch.observe(now, seen, &rules), lasting.remove(&deployment.id)) {
                (Assessment::Crashing { reason, restarts_before }, None) => {
                    let restarts = count.saturating_sub(restarts_before);
                    watched.baseline = Some(restarts_before);
                    let new = NewCrashLoop {
                        service_id: deployment.service_id,
                        deployment_id: deployment.id,
                        server_id: deployment.server_id,
                        reason,
                        restarts,
                    };
                    let crash = self.store().create_crash_loop(new).await?;
                    tracing::warn!(service = %crash.service_id, deployment = %deployment.id, reason = %reason, restarts, "a service is crashing");
                    let what = match reason {
                        CrashReason::Restarting => format!("Its container was restarted {restarts} times in a short while."),
                        CrashReason::NotRunning => "Its container isn't running.".to_string(),
                    };
                    let text = format!("{what} Its logs usually say why; fix it and deploy, or roll back.");
                    let path = format!("/services/{}/logs?deployment_id={}", crash.service_id, deployment.id);
                    self.notify_crash(&crash, NotificationEvent::ServiceCrashing, "is crashing", text, path).await;
                }
                (Assessment::Crashing { reason, .. }, Some(crash)) => {
                    // After the control plane starts, carry on from what the loop counted.
                    let baseline = *watched.baseline.get_or_insert(count.saturating_sub(crash.restarts));
                    let restarts = count.saturating_sub(baseline);
                    if (crash.reason, crash.restarts) != (reason, restarts) {
                        self.store().update_crash_loop(crash.id, reason, restarts).await?;
                    }
                }
                (Assessment::Healthy, Some(crash)) => {
                    watched.baseline = None;
                    self.store().end_crash_loop(crash.id).await?;
                    tracing::info!(service = %crash.service_id, deployment = %deployment.id, "a service has recovered");
                    let minutes = rules.recovered_after.as_secs() / 60;
                    let text = format!("Its container has run for {minutes} minutes without a restart.");
                    let path = format!("/services/{}", crash.service_id);
                    self.notify_crash(&crash, NotificationEvent::ServiceRecovered, "has recovered", text, path).await;
                }
                (Assessment::Healthy | Assessment::Unsure, None) | (Assessment::Unsure, Some(_)) => {}
            }
        }

        // What is left was not looked at: a server away keeps its loops, the rest ended.
        for crash in lasting.into_values().filter(|crash| !watched.contains(&crash.deployment_id)) {
            self.store().end_crash_loop(crash.id).await?;
        }
        state.watches.retain(|id, _| watched.contains(id));
        if state
            .pruned_at
            .is_none_or(|at| now.duration_since(at) >= jiff::SignedDuration::try_from(PRUNE_EVERY).unwrap_or_default())
        {
            state.pruned_at = Some(now);
            self.store().prune_crash_loops(now - KEEP_ENDED_FOR).await?;
        }
        Ok(())
    }
}

impl Control {
    async fn notify_crash(&self, crash: &CrashLoop, event: NotificationEvent, what: &str, text: String, path: String) {
        let (subject, mut details) = self.service_subject(crash.service_id).await;
        details.insert("deployment_id".to_string(), crash.deployment_id.to_string());
        details.insert("crash_loop_id".to_string(), crash.id.to_string());
        let message = Message { event, title: format!("{subject} {what}"), text, path: Some(path), details };
        self.notify(message).await;
    }
}

#[cfg(test)]
mod tests;
