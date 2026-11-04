//! What needs attention, for the overview: [`Control::attention`] gathers the problems
//! Spinikus already records, keeping only those the principal may see: a service's to
//! anyone with a role in its environment, the instance's (servers, Spinikus's own backups,
//! notification channels) to instance admins.

use std::collections::{BTreeSet, HashMap};

use jiff::Timestamp;
use spinikus_core::{
    attention::{self, AttentionItem, ServiceRef},
    backup::BackupStatus,
    domain::Hostname,
    id::ServiceId,
    notification::NotificationStatus,
    server::ServerStatus,
};
use spinikus_store::{
    BackupRepository, CertificateRepository, CrashLoopRepository, DeploymentRepository, DomainRepository, EnvironmentRepository,
    InstanceBackupRepository, NotificationRepository, ProjectRepository, ServerRepository, ServiceRepository, StoreError,
};

use crate::{Control, ControlError, Principal, access::Access, certificates};

/// How many servers are read per query.
const SERVER_PAGE: u32 = 200;

/// Instance backups looked through for the latest finished one; more running or queued
/// than this at once don't happen.
const INSTANCE_BACKUPS_READ: u32 = 10;

impl Control {
    /// Everything that needs attention that the principal may see, in the order to show it
    /// ([`attention::sort`]).
    pub async fn attention(&self, principal: &Principal) -> Result<Vec<AttentionItem>, ControlError> {
        let access = self.access(principal).await?;
        let mut services = Services { control: self, access: &access, known: HashMap::new() };
        let mut items = Vec::new();

        for crash_loop in self.store().list_lasting_crash_loops().await? {
            if let Some(service) = services.visible(crash_loop.service_id).await? {
                items.push(AttentionItem::ServiceCrashing { service, crash_loop });
            }
        }
        for deployment in self.store().list_failed_latest_deployments().await? {
            if let Some(service) = services.visible(deployment.service_id).await? {
                let events = self.store().list_deployment_events(deployment.id).await?;
                let message = events.into_iter().rev().find_map(|event| event.message);
                items.push(AttentionItem::DeploymentFailed {
                    service,
                    deployment_id: deployment.id,
                    reason: deployment.reason,
                    message,
                    at: deployment.updated_at,
                });
            }
        }
        // One item per service: an app's volumes each have their latest backup.
        let mut failed = self.store().list_failed_latest_backups().await?;
        failed.sort_by_key(|backup| std::cmp::Reverse(backup.finished_at.unwrap_or(backup.created_at)));
        let mut seen = std::collections::HashSet::new();
        for backup in failed.into_iter().filter(|backup| seen.insert(backup.service_id)) {
            if let Some(service) = services.visible(backup.service_id).await? {
                let at = backup.finished_at.unwrap_or(backup.created_at);
                items.push(AttentionItem::BackupFailed { service, backup_id: backup.id, error: backup.error, at });
            }
        }
        self.certificate_attention(&mut services, &mut items).await?;
        if principal.is_instance_admin() {
            self.instance_attention(&mut items).await?;
        }
        attention::sort(&mut items);
        Ok(items)
    }

    /// Hostnames routed now whose certificate couldn't be had or expires soon, while
    /// Spinikus gets certificates at all.
    async fn certificate_attention(
        &self,
        services: &mut Services<'_>,
        items: &mut Vec<AttentionItem>,
    ) -> Result<(), ControlError> {
        if self.acme_settings().await?.is_none() {
            return Ok(());
        }
        let routed: BTreeSet<Hostname> = self
            .build_routes()
            .await?
            .into_values()
            .flatten()
            .map(|route| route.hostname)
            .filter(certificates::certifiable)
            .collect();
        let mut owners: HashMap<Hostname, ServiceId> =
            self.generated_hostnames().await?.into_iter().map(|(service, hostname)| (hostname, service)).collect();
        owners.extend(self.store().list_all_domains().await?.into_iter().map(|domain| (domain.hostname, domain.service_id)));
        let certificates = self.certificates_by_id().await?;
        let soon = Timestamp::now() + certificates::EXPIRING_WITHIN;

        for record in self.store().list_hostname_certificates().await? {
            if !routed.contains(&record.hostname) {
                continue;
            }
            let Some(&service_id) = owners.get(&record.hostname) else {
                continue;
            };
            let expires_at = record.certificate_id.and_then(|id| certificates.get(&id)).map(|certificate| certificate.not_after);
            let item = |service| match (record.failures, expires_at) {
                (0, Some(expires_at)) if expires_at <= soon => {
                    Some(AttentionItem::CertificateExpiring { service, hostname: record.hostname.clone(), expires_at })
                }
                (0, _) => None,
                (failures, _) => Some(AttentionItem::CertificateFailed {
                    service,
                    hostname: record.hostname.clone(),
                    error: record.last_error.clone(),
                    failures,
                    retry_at: record.retry_at,
                    expires_at,
                }),
            };
            if let Some(service) = services.visible(service_id).await?
                && let Some(item) = item(service)
            {
                items.push(item);
            }
        }
        Ok(())
    }

    /// Servers that are away or low on disk, a failed backup of Spinikus, and channels that
    /// couldn't send their latest notification.
    async fn instance_attention(&self, items: &mut Vec<AttentionItem>) -> Result<(), ControlError> {
        let low_on_disk = self.servers_low_on_disk();
        let mut after = None;
        loop {
            let servers = self.store().list_servers(after, SERVER_PAGE).await?;
            after = servers.last().map(|server| server.id);
            for server in &servers {
                if server.revoked_at.is_some() {
                    continue;
                }
                if server.status == ServerStatus::Disconnected {
                    items.push(AttentionItem::ServerDisconnected {
                        server_id: server.id,
                        server_name: server.name.clone(),
                        last_seen_at: server.last_seen_at,
                    });
                }
                if low_on_disk.contains(&server.id) {
                    items.push(AttentionItem::ServerLowOnDisk { server_id: server.id, server_name: server.name.clone() });
                }
            }
            if servers.len() < SERVER_PAGE as usize {
                break;
            }
        }

        let backups = self.store().list_instance_backups(None, INSTANCE_BACKUPS_READ).await?;
        let latest = backups.into_iter().find(|backup| matches!(backup.status, BackupStatus::Succeeded | BackupStatus::Failed));
        if let Some(backup) = latest
            && backup.status == BackupStatus::Failed
        {
            let at = backup.finished_at.unwrap_or(backup.created_at);
            items.push(AttentionItem::InstanceBackupFailed { backup_id: backup.id, error: backup.error, at });
        }

        for channel in self.store().list_notification_channels().await? {
            if !channel.fields.enabled {
                continue;
            }
            let latest = self.store().list_notifications(channel.id, None, 1).await?;
            if let Some(notification) = latest.into_iter().next()
                && notification.status == NotificationStatus::Failed
            {
                items.push(AttentionItem::NotificationsFailing {
                    channel_id: channel.id,
                    channel_name: channel.fields.name,
                    event: notification.event,
                    error: notification.error,
                    at: notification.created_at,
                });
            }
        }
        Ok(())
    }
}

/// Services looked up once each, with whether the principal may see them.
struct Services<'a> {
    control: &'a Control,
    access: &'a Access<'a>,
    known: HashMap<ServiceId, Option<ServiceRef>>,
}

impl Services<'_> {
    /// The service, if it exists and the principal has a role in its environment.
    async fn visible(&mut self, id: ServiceId) -> Result<Option<ServiceRef>, ControlError> {
        if let Some(known) = self.known.get(&id) {
            return Ok(known.clone());
        }
        let found = self.look_up(id).await?;
        self.known.insert(id, found.clone());
        Ok(found)
    }

    async fn look_up(&self, id: ServiceId) -> Result<Option<ServiceRef>, ControlError> {
        let store = self.control.store();
        let found = async {
            let service = store.get_service(id).await?;
            let environment = store.get_environment(service.environment_id).await?;
            let project = store.get_project(environment.project_id).await?;
            Ok::<_, StoreError>((service, environment, project))
        };
        let (service, environment, project) = match found.await {
            Ok(found) => found,
            Err(StoreError::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if self.access.role_in(project.id, Some(environment.id)).is_none() {
            return Ok(None);
        }
        Ok(Some(ServiceRef {
            id: service.id,
            name: service.name,
            kind: service.kind,
            environment_id: environment.id,
            environment_name: environment.name,
            project_id: project.id,
            project_name: project.name,
        }))
    }
}

#[cfg(test)]
mod tests;
