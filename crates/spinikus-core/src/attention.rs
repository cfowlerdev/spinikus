//! What needs attention: the problems the overview lists, each something a person should
//! look at or fix, gathered from what Spinikus already records (crash loops, deployments,
//! backups, certificates, servers, notifications).

use jiff::Timestamp;
use serde::Serialize;

use crate::{
    crash::CrashLoop,
    deployment::EndReason,
    domain::Hostname,
    id::{BackupId, DeploymentId, EnvironmentId, InstanceBackupId, NotificationChannelId, ProjectId, ServerId, ServiceId},
    notification::NotificationEvent,
    service::ServiceKind,
};

/// The service an item is about, with the names to show it by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ServiceRef {
    pub id: ServiceId,
    pub name: String,
    pub kind: ServiceKind,
    pub environment_id: EnvironmentId,
    pub environment_name: String,
    pub project_id: ProjectId,
    pub project_name: String,
}

/// One thing that needs attention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttentionItem {
    /// A server's agent connected before and isn't connected now.
    ServerDisconnected { server_id: ServerId, server_name: String, last_seen_at: Option<Timestamp> },
    /// A server's disk is at least [`crate::metrics::DISK_LOW_PERCENT`] full.
    ServerLowOnDisk { server_id: ServerId, server_name: String },
    /// A service's live container keeps crashing.
    ServiceCrashing { service: ServiceRef, crash_loop: CrashLoop },
    /// A service's latest deployment, canceled ones left out, failed.
    DeploymentFailed {
        service: ServiceRef,
        deployment_id: DeploymentId,
        reason: Option<EndReason>,
        message: Option<String>,
        at: Timestamp,
    },
    /// A database's latest finished backup failed.
    BackupFailed { service: ServiceRef, backup_id: BackupId, error: Option<String>, at: Timestamp },
    /// Getting an HTTPS certificate for a service's hostname failed, the last time it was
    /// tried; `expires_at` is when the certificate it has now, if any, expires.
    CertificateFailed {
        service: ServiceRef,
        hostname: Hostname,
        error: Option<String>,
        failures: u32,
        retry_at: Option<Timestamp>,
        expires_at: Option<Timestamp>,
    },
    /// A service's HTTPS certificate expires soon, and hasn't been renewed.
    CertificateExpiring { service: ServiceRef, hostname: Hostname, expires_at: Timestamp },
    /// The latest finished backup of Spinikus failed.
    InstanceBackupFailed { backup_id: InstanceBackupId, error: Option<String>, at: Timestamp },
    /// A notification channel's latest notification couldn't be sent.
    NotificationsFailing {
        channel_id: NotificationChannelId,
        channel_name: String,
        event: NotificationEvent,
        error: Option<String>,
        at: Timestamp,
    },
}

impl AttentionItem {
    /// Where it goes in the list: servers first, since everything on them depends on them,
    /// then services, then Spinikus's own backups and notifications.
    fn rank(&self) -> u8 {
        match self {
            Self::ServerDisconnected { .. } => 0,
            Self::ServerLowOnDisk { .. } => 1,
            Self::ServiceCrashing { .. } => 2,
            Self::DeploymentFailed { .. } => 3,
            Self::BackupFailed { .. } => 4,
            Self::CertificateFailed { .. } => 5,
            Self::CertificateExpiring { .. } => 6,
            Self::InstanceBackupFailed { .. } => 7,
            Self::NotificationsFailing { .. } => 8,
        }
    }

    /// When it happened or began, for ordering items of a kind, newest first.
    fn at(&self) -> Option<Timestamp> {
        match self {
            Self::ServerDisconnected { last_seen_at, .. } => *last_seen_at,
            Self::ServerLowOnDisk { .. } => None,
            Self::ServiceCrashing { crash_loop, .. } => Some(crash_loop.started_at),
            Self::DeploymentFailed { at, .. }
            | Self::BackupFailed { at, .. }
            | Self::InstanceBackupFailed { at, .. }
            | Self::NotificationsFailing { at, .. } => Some(*at),
            Self::CertificateFailed { expires_at, .. } => *expires_at,
            Self::CertificateExpiring { expires_at, .. } => Some(*expires_at),
        }
    }
}

/// Puts items in the order to show them: by kind, as [`AttentionItem::rank`] says, then
/// newest first, except certificates, which come soonest to expire first.
pub fn sort(items: &mut [AttentionItem]) {
    items.sort_by(|a, b| {
        a.rank().cmp(&b.rank()).then_with(|| match a {
            AttentionItem::CertificateFailed { .. } | AttentionItem::CertificateExpiring { .. } => {
                // `None` (no certificate at all) first.
                a.at().cmp(&b.at())
            }
            _ => b.at().cmp(&a.at()),
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disconnected(name: &str, last_seen_at: &str) -> AttentionItem {
        AttentionItem::ServerDisconnected {
            server_id: ServerId::new(),
            server_name: name.to_string(),
            last_seen_at: Some(last_seen_at.parse().unwrap()),
        }
    }

    fn instance_backup(at: &str) -> AttentionItem {
        AttentionItem::InstanceBackupFailed { backup_id: InstanceBackupId::new(), error: None, at: at.parse().unwrap() }
    }

    fn certificate(expires_at: Option<&str>) -> AttentionItem {
        AttentionItem::CertificateFailed {
            service: ServiceRef {
                id: ServiceId::new(),
                name: "Web".to_string(),
                kind: ServiceKind::App,
                environment_id: EnvironmentId::new(),
                environment_name: "Production".to_string(),
                project_id: ProjectId::new(),
                project_name: "Shop".to_string(),
            },
            hostname: "shop.example.com".parse().unwrap(),
            error: None,
            failures: 1,
            retry_at: None,
            expires_at: expires_at.map(|at| at.parse().unwrap()),
        }
    }

    #[test]
    fn sorts_by_kind_then_newest_first_and_certificates_by_expiry() {
        let mut items = vec![
            instance_backup("2026-01-01T00:00:00Z"),
            certificate(Some("2026-03-01T00:00:00Z")),
            disconnected("old", "2026-01-01T00:00:00Z"),
            certificate(None),
            certificate(Some("2026-02-01T00:00:00Z")),
            disconnected("new", "2026-01-02T00:00:00Z"),
        ];
        sort(&mut items);
        let order: Vec<(u8, Option<Timestamp>)> = items.iter().map(|item| (item.rank(), item.at())).collect();
        let at = |value: &str| Some(value.parse::<Timestamp>().unwrap());
        assert_eq!(
            order,
            vec![
                (0, at("2026-01-02T00:00:00Z")),
                (0, at("2026-01-01T00:00:00Z")),
                (5, None),
                (5, at("2026-02-01T00:00:00Z")),
                (5, at("2026-03-01T00:00:00Z")),
                (7, at("2026-01-01T00:00:00Z")),
            ]
        );
    }

    #[test]
    fn serializes_with_its_kind() {
        let value = serde_json::to_value(disconnected("web-1", "2026-01-01T00:00:00Z")).unwrap();
        assert_eq!(value["kind"], "server_disconnected");
        assert_eq!(value["server_name"], "web-1");
    }
}
