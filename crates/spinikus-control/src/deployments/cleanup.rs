//! Cleaning up finished deployments: removing their containers and images from their
//! server once nothing needs them, and recording it as `cleaned_at`.
//!
//! A service keeps, besides its active deployments:
//! - the newest `releases_kept` superseded deployments, which can be rolled back to;
//! - failed and canceled deployments newer than the last one that went live (at most
//!   [`FAILURES_KEPT`]), so a failure's container and logs stay until a fix goes live;
//! - the target of a rollback in progress (once it runs, the image check below keeps the
//!   image they share).
//!
//! A deleted service keeps nothing that has finished: the runner cancels its deployments in
//! progress and drains its live one, and each is cleaned up once it has finished.
//!
//! Everything else that has finished is cleaned up by a `RemoveDeployment` sent to its
//! server's agent, which removes the deployment's containers and its image references. An
//! image is removed only when no deployment kept on that server runs it, since rollbacks and
//! cached builds share images. On a revoked server nothing can be removed, so its
//! deployments are only marked cleaned.

use std::collections::HashSet;

use spinikus_core::{
    build::image_tag,
    deployment::{Deployment, DeploymentStatus, DeploymentTrigger},
    id::{DeploymentId, ServiceId},
    source::SourceSpec,
};
use spinikus_proto::agent::v1::{self as pb, command::Kind};
use spinikus_store::{DeploymentRepository, ServerRepository};

use super::commands::{Step, command_id};
use crate::{Control, ControlError};

/// The most failed and canceled deployments a service keeps.
pub const FAILURES_KEPT: usize = 10;

/// The deployments to clean up, out of every deployment not cleaned up yet. Those of
/// `deleted` services are all cleaned up once they have finished.
pub(crate) fn to_clean<'a>(
    uncleaned: &'a [Deployment],
    releases_kept: usize,
    deleted: &HashSet<ServiceId>,
) -> Vec<&'a Deployment> {
    let rollback_targets: HashSet<DeploymentId> = uncleaned
        .iter()
        .filter(|deployment| deployment.status.is_in_progress())
        .filter_map(|deployment| deployment.rollback_of)
        .collect();
    let services: HashSet<ServiceId> = uncleaned.iter().map(|deployment| deployment.service_id).collect();
    let mut clean = Vec::new();
    for service in services {
        let mut newest_first: Vec<&Deployment> = uncleaned.iter().filter(|deployment| deployment.service_id == service).collect();
        newest_first.sort_by_key(|deployment| std::cmp::Reverse(deployment.id));
        let last_live = newest_first
            .iter()
            .find(|deployment| {
                matches!(deployment.status, DeploymentStatus::Live | DeploymentStatus::Draining | DeploymentStatus::Superseded)
            })
            .map(|deployment| deployment.id);
        let (mut releases, mut failures) = (0, 0);
        for deployment in newest_first {
            let keep = match deployment.status {
                status if deleted.contains(&service) => !status.is_terminal(),
                _ if rollback_targets.contains(&deployment.id) => true,
                DeploymentStatus::Superseded => {
                    releases += 1;
                    releases <= releases_kept
                }
                DeploymentStatus::Failed | DeploymentStatus::Canceled => {
                    failures += 1;
                    failures <= FAILURES_KEPT && last_live.is_none_or(|live| deployment.id > live)
                }
                _ => true,
            };
            if !keep {
                clean.push(deployment);
            }
        }
    }
    clean.sort_by_key(|deployment| deployment.id);
    clean
}

/// The references a deployment's own image goes by: the tag of its build, or what it pulled
/// (pinned by digest). A rollback has none of its own; it runs its target's image.
fn own_references(deployment: &Deployment) -> Vec<String> {
    match (&deployment.snapshot.source, deployment.trigger, &deployment.image) {
        (_, DeploymentTrigger::Rollback, _) | (_, _, None) => Vec::new(),
        (SourceSpec::Git(_), _, Some(_)) => vec![image_tag(deployment.service_id, deployment.id)],
        (SourceSpec::Image(_), _, Some(image)) => vec![image.clone()],
    }
}

impl Control {
    /// Sends a `RemoveDeployment` for each deployment no longer kept, or marks it cleaned if
    /// its server is revoked. Deployments on servers whose agent is away wait for it.
    pub(super) async fn clean_up_deployments(
        &self,
        sent: &mut super::runner::Sent,
        uncleaned: &[Deployment],
        deleted: &HashSet<ServiceId>,
    ) -> Result<(), ControlError> {
        let kept = self.releases_kept().await?;
        let cleaning = to_clean(uncleaned, kept, deleted);
        let cleaning_ids: HashSet<DeploymentId> = cleaning.iter().map(|deployment| deployment.id).collect();
        for deployment in cleaning {
            let server = self.store().get_server(deployment.server_id).await?;
            if server.revoked_at.is_some() {
                self.store().mark_deployment_cleaned(deployment.id).await?;
                continue;
            }
            if !self.agents().is_connected(server.id) {
                continue;
            }
            let images = self.images_to_remove(deployment, &cleaning_ids).await?;
            let remove = pb::RemoveDeployment { deployment_id: deployment.id.to_string(), images };
            let command =
                pb::Command { command_id: command_id(deployment.id, Step::Remove), kind: Some(Kind::RemoveDeployment(remove)) };
            self.send_command(sent, server.id, command).await;
        }
        Ok(())
    }

    /// The image references to remove with `deployment`: none while a deployment that is kept
    /// runs the same image on that server, and otherwise every reference to it that a
    /// deployment there made, then the image itself.
    async fn images_to_remove(
        &self,
        deployment: &Deployment,
        cleaning: &HashSet<DeploymentId>,
    ) -> Result<Vec<String>, ControlError> {
        let Some(image) = &deployment.image else {
            return Ok(Vec::new());
        };
        let sharing = self.store().list_deployments_with_image(deployment.server_id, image).await?;
        let still_needed = |other: &Deployment| other.cleaned_at.is_none() && !cleaning.contains(&other.id);
        if sharing.iter().any(still_needed) {
            return Ok(Vec::new());
        }
        let mut references: Vec<String> = sharing.iter().chain([deployment]).flat_map(own_references).collect();
        references.push(image.clone());
        let mut seen = HashSet::new();
        references.retain(|reference| seen.insert(reference.clone()));
        Ok(references)
    }
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use serde_json::Map;
    use spinikus_core::{
        deployment::{DeploymentSnapshot, DeploymentStrategy},
        id::ServerId,
        source::{GitSource, ImageSource},
    };

    use super::*;

    fn deployment(service: ServiceId, status: DeploymentStatus) -> Deployment {
        Deployment {
            id: DeploymentId::new(),
            service_id: service,
            server_id: ServerId::new(),
            trigger: DeploymentTrigger::Manual,
            strategy: DeploymentStrategy::BlueGreen,
            status,
            reason: None,
            rollback_of: None,
            snapshot: DeploymentSnapshot {
                source: SourceSpec::Image(ImageSource { image: "web:1".to_string() }),
                config: Map::new(),
                from_file: Vec::new(),
            },
            commit_sha: None,
            image: Some("web@sha256:01".to_string()),
            cleaned_at: None,
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
        }
    }

    fn ids(deployments: Vec<&Deployment>) -> Vec<DeploymentId> {
        deployments.into_iter().map(|deployment| deployment.id).collect()
    }

    #[test]
    fn keeps_the_newest_releases_and_the_failures_since_the_last_live_one() {
        use DeploymentStatus::*;
        let service = ServiceId::new();
        // Oldest first, as IDs are made.
        let all: Vec<Deployment> = [Superseded, Failed, Superseded, Superseded, Canceled, Live, Failed, Queued]
            .into_iter()
            .map(|status| deployment(service, status))
            .collect();

        let two = ids(to_clean(&all, 2, &HashSet::new()));
        let none = ids(to_clean(&all, 0, &HashSet::new()));

        // The oldest release, and the failures before the live one.
        assert_eq!(two, [all[0].id, all[1].id, all[4].id]);
        assert_eq!(none, [all[0].id, all[1].id, all[2].id, all[3].id, all[4].id]);
    }

    #[test]
    fn keeps_a_rollbacks_target_and_looks_at_each_service_alone() {
        use DeploymentStatus::*;
        let (web, api) = (ServiceId::new(), ServiceId::new());
        let target = deployment(web, Superseded);
        let newer = deployment(web, Superseded);
        let mut rollback = deployment(web, Starting);
        rollback.rollback_of = Some(target.id);
        let other = deployment(api, Superseded);
        let all = [target, newer, rollback, other.clone()];

        assert_eq!(ids(to_clean(&all, 1, &HashSet::new())), Vec::<DeploymentId>::new());
        assert_eq!(ids(to_clean(&all, 0, &HashSet::new())), [all[1].id, other.id]);
        // A live rollback no longer holds on to its target.
        let mut live = all.clone();
        live[2].status = Live;
        assert_eq!(ids(to_clean(&live, 0, &HashSet::new())), [all[0].id, all[1].id, other.id]);
    }

    #[test]
    fn keeps_failures_until_one_goes_live_but_not_too_many() {
        let service = ServiceId::new();
        let failures: Vec<Deployment> = (0..FAILURES_KEPT + 2).map(|_| deployment(service, DeploymentStatus::Failed)).collect();

        assert_eq!(ids(to_clean(&failures, 5, &HashSet::new())), [failures[0].id, failures[1].id]);
    }

    #[test]
    fn a_deleted_service_keeps_only_what_has_not_finished() {
        use DeploymentStatus::*;
        let (deleted, other) = (ServiceId::new(), ServiceId::new());
        let mut all: Vec<Deployment> =
            [Superseded, Failed, Canceled, Draining, Queued].into_iter().map(|status| deployment(deleted, status)).collect();
        all.push(deployment(other, Superseded));

        let cleaning = ids(to_clean(&all, 5, &HashSet::from([deleted])));

        assert_eq!(
            cleaning,
            [all[0].id, all[1].id, all[2].id],
            "the draining and queued ones wait, the other service keeps its release"
        );
    }

    #[test]
    fn builds_are_removed_by_tag_and_pulls_by_digest() {
        let service = ServiceId::new();
        let pulled = deployment(service, DeploymentStatus::Superseded);
        let mut built = deployment(service, DeploymentStatus::Superseded);
        built.snapshot.source =
            SourceSpec::Git(GitSource { repo_url: "https://h/r".to_string(), branch: "main".to_string(), root_directory: None });
        built.image = Some("sha256:01".to_string());
        let mut rollback = built.clone();
        rollback.trigger = DeploymentTrigger::Rollback;
        let mut failed_build = built.clone();
        failed_build.image = None;

        assert_eq!(own_references(&pulled), ["web@sha256:01"]);
        assert_eq!(own_references(&built), [image_tag(service, built.id)]);
        assert!(own_references(&rollback).is_empty() && own_references(&failed_build).is_empty());
    }
}
