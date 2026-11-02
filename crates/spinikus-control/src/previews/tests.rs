use hmac::{Hmac, KeyInit, Mac};
use serde_json::json;
use sha2::Sha256;
use spinikus_core::{
    deployment::DeploymentStatus,
    secret::Secret,
    service::ServiceKind,
    source::ImageSource,
    variable::{VariableAvailability, VariableName},
};
use spinikus_proto::agent::v1::{self as pb, command::Kind, control_message};
use spinikus_store::{DeploymentRepository, WebhookRepository};

use super::*;
use crate::{
    EnvironmentChanges, NewDomain, NewService, NewVariable, NewVolume,
    deployments::{
        DeploymentTimings,
        tests::{Setup, connect, setup},
    },
    webhooks::{NewWebhook, PushDelivery},
};

const HEAD: &str = "abcdef0123456789abcdef0123456789abcdef01";
const NEXT: &str = "1234567890abcdef1234567890abcdef12345678";
const REPO: &str = "https://github.com/acme/shop.git";

/// The test environment with previews on: `web` from `main` of the repository, with a volume,
/// a secret, a domain and a webhook, and `cache`, an image.
async fn previews_setup() -> (Setup, NewWebhook, ServiceId) {
    let setup = setup(DeploymentTimings::default()).await;
    let (control, admin, web) = (&setup.control, &setup.admin, setup.service.id);
    let source = control.get_source(admin, web).await.unwrap();
    let git = SourceSpec::Git(GitSource { repo_url: REPO.to_string(), branch: "main".to_string(), root_directory: None });
    control.replace_source(admin, web, source.revision, git).await.unwrap();
    let volume = NewVolume { name: "uploads".to_string(), mount_path: "/uploads".to_string() };
    control.create_volume(admin, web, volume).await.unwrap();
    let secret = NewVariable {
        name: "API_KEY".to_string(),
        value: Secret::new("s3cret"),
        availability: VariableAvailability::Runtime,
        secret: true,
    };
    control.create_variable(admin, VariableScope::Service(web), secret).await.unwrap();
    let shared = NewVariable {
        name: "LEVEL".to_string(),
        value: Secret::new("info"),
        availability: VariableAvailability::Both,
        secret: false,
    };
    control.create_variable(admin, VariableScope::Environment(setup.environment.id), shared).await.unwrap();
    control.create_domain(admin, web, NewDomain { hostname: "shop.example.com".to_string(), redirect_to: None }).await.unwrap();
    let cache = NewService {
        name: "Cache".to_string(),
        slug: None,
        kind: ServiceKind::App,
        config: serde_json::Map::new(),
        server_id: Some(setup.server.id),
    };
    let cache = control.create_service(admin, setup.environment.id, cache).await.unwrap();
    let image = SourceSpec::Image(ImageSource { image: "redis:8".to_string() });
    control.create_source(admin, cache.id, image).await.unwrap();
    let on = EnvironmentChanges { previews: Some(true), ..Default::default() };
    control.update_environment(admin, setup.environment.id, setup.environment.revision, on).await.unwrap();
    let hook = control.create_webhook(admin, web).await.unwrap();
    (setup, hook, cache.id)
}

fn pull_request(action: &str, number: u32, base: &str, head: &str, from_fork: bool) -> Vec<u8> {
    let head_repo = if from_fork { 2 } else { 1 };
    json!({
        "action": action,
        "number": number,
        "pull_request": {
            "head": { "ref": "feature", "sha": head, "repo": { "id": head_repo } },
            "base": { "ref": base, "sha": NEXT, "repo": { "id": 1 } },
        },
    })
    .to_string()
    .into_bytes()
}

async fn deliver(setup: &Setup, hook: &NewWebhook, body: &[u8]) -> Result<PushOutcome, ControlError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(hook.secret.expose().as_bytes()).unwrap();
    mac.update(body);
    let signature =
        format!("sha256={}", mac.finalize().into_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<String>());
    let delivery = PushDelivery { event: Some("pull_request"), signature: Some(&signature), body };
    setup.control.receive_push(GitProvider::GitHub, hook.webhook.id, delivery).await
}

fn previewed(outcome: Result<PushOutcome, ControlError>) -> (Environment, Vec<Deployment>) {
    match outcome {
        Ok(PushOutcome::Previewed { environment, deployments }) => (*environment, deployments),
        other => panic!("expected a preview, got {other:?}"),
    }
}

fn ignored(outcome: Result<PushOutcome, ControlError>) -> String {
    match outcome {
        Ok(PushOutcome::Ignored(why)) => why,
        other => panic!("expected the event to be ignored, got {other:?}"),
    }
}

#[tokio::test]
async fn a_pull_request_gets_a_copy_of_the_environment_that_builds_its_branch() {
    let (setup, hook, cache) = previews_setup().await;
    let (control, admin) = (&setup.control, &setup.admin);

    let (preview, deployments) = previewed(deliver(&setup, &hook, &pull_request("opened", 7, "main", HEAD, false)).await);

    assert_eq!(preview.kind, EnvironmentKind::Preview { pr_number: 7 });
    assert_eq!((preview.slug.as_str(), preview.name.as_str()), ("pr-7", "PR #7"));
    assert_eq!(preview.base_environment_id, Some(setup.environment.id));
    let services = control.all_services(preview.id).await.unwrap();
    let slugs: Vec<&str> = services.iter().map(|service| service.slug.as_str()).collect();
    assert_eq!(slugs, ["web", "cache"]);
    let (web_copy, cache_copy) = (&services[0], &services[1]);
    assert_eq!((web_copy.config.clone(), web_copy.server_id), (setup.service.config.clone(), Some(setup.server.id)));
    let source = control.get_source(admin, web_copy.id).await.unwrap().spec;
    let SourceSpec::Git(git) = source else { panic!("{source:?}") };
    assert_eq!((git.repo_url.as_str(), git.branch.as_str()), (REPO, "feature"));
    let cache_source = control.get_source(admin, cache_copy.id).await.unwrap().spec;
    assert_eq!(cache_source, SourceSpec::Image(ImageSource { image: "redis:8".to_string() }));
    // Both deploy: the web copy at the head commit, the cache as it is.
    let deployed: Vec<(ServiceId, Option<&str>)> =
        deployments.iter().map(|deployment| (deployment.service_id, deployment.commit_sha.as_deref())).collect();
    assert_eq!(deployed, [(web_copy.id, Some(HEAD)), (cache_copy.id, None)]);
    assert_eq!(deployments[0].trigger, DeploymentTrigger::GitPush);
    // Volumes are new, variables are copied, domains and webhooks are not.
    let volumes = control.store().list_volumes(web_copy.id).await.unwrap();
    let original = control.store().list_volumes(setup.service.id).await.unwrap();
    assert_eq!((volumes[0].name.as_str(), volumes.len()), ("uploads", 1));
    assert_ne!(volumes[0].id, original[0].id);
    let variables = control.store().list_variables(VariableScope::Service(web_copy.id)).await.unwrap();
    let name: VariableName = "API_KEY".parse().unwrap();
    assert_eq!((&variables[0].name, variables[0].secret), (&name, true));
    assert_eq!(control.store().reveal_variable(variables[0].id).await.unwrap().expose(), "s3cret");
    let shared = control.store().list_variables(VariableScope::Environment(preview.id)).await.unwrap();
    assert_eq!(shared[0].value.as_deref(), Some("info"));
    assert!(control.store().list_domains(web_copy.id).await.unwrap().is_empty());
    assert!(control.store().find_service_webhook(web_copy.id).await.unwrap().is_none());
    assert!(control.store().list_deployments(cache, None, 10).await.unwrap().is_empty(), "the base is left alone");
    let events = control.list_deployment_events(admin, deployments[0].id).await.unwrap();
    assert_eq!(events[0].actor, Actor::Webhook(hook.webhook.id));
    let webhook = control.get_webhook(admin, setup.service.id).await.unwrap();
    let delivery = webhook.last_delivery.unwrap();
    assert!(delivery.starts_with("preview pr-7: deployed dep_"), "{delivery}");
}

#[tokio::test]
async fn new_commits_redeploy_the_repositorys_services_and_closing_removes_the_preview() {
    let (setup, hook, _) = previews_setup().await;
    let control = &setup.control;
    let mut agent = connect(control, setup.server.id);
    let (preview, _) = previewed(deliver(&setup, &hook, &pull_request("opened", 7, "main", HEAD, false)).await);
    let web_copy = control.all_services(preview.id).await.unwrap()[0].clone();
    let volume = control.store().list_volumes(web_copy.id).await.unwrap()[0].id;

    let again = ignored(deliver(&setup, &hook, &pull_request("reopened", 7, "main", HEAD, false)).await);
    let (_, pushed) = previewed(deliver(&setup, &hook, &pull_request("synchronize", 7, "main", NEXT, false)).await);
    let labeled = ignored(deliver(&setup, &hook, &pull_request("labeled", 7, "main", NEXT, false)).await);
    let closed = deliver(&setup, &hook, &pull_request("closed", 7, "main", NEXT, false)).await;
    let closed_again = ignored(deliver(&setup, &hook, &pull_request("closed", 7, "main", NEXT, false)).await);
    control.advance_deployments().await;

    assert!(again.contains("already"), "{again}");
    assert_eq!(
        pushed.iter().map(|deployment| (deployment.service_id, deployment.commit_sha.as_deref())).collect::<Vec<_>>(),
        [(web_copy.id, Some(NEXT))]
    );
    assert!(labeled.starts_with("only opening"), "{labeled}");
    assert!(matches!(closed, Ok(PushOutcome::PreviewRemoved(ref removed)) if removed.id == preview.id), "{closed:?}");
    assert_eq!(closed_again, "pull request #7 has no preview");
    assert!(control.store().get_environment(preview.id).await.is_err());
    assert!(control.store().get_service(web_copy.id).await.is_err());
    // The runner cancels the copies' deployments and removes them, the volume with them.
    let mut removed = Vec::new();
    while let Ok(message) = agent.try_recv() {
        if let Some(control_message::Message::Command(pb::Command { kind: Some(Kind::RemoveDeployment(remove)), .. })) =
            message.message
        {
            removed.push((remove.deployment_id, remove.volumes));
        }
    }
    let web_deployments = control.store().list_deployments(web_copy.id, None, 10).await.unwrap();
    assert!(web_deployments.iter().all(|deployment| deployment.status == DeploymentStatus::Canceled));
    let newest = web_deployments.iter().map(|deployment| deployment.id).max().unwrap();
    let web_removals: Vec<_> =
        removed.iter().filter(|(id, _)| web_deployments.iter().any(|deployment| deployment.id.to_string() == *id)).collect();
    assert_eq!(web_removals.len(), 1, "the newest waits for the older one: {removed:?}");
    assert!(web_removals[0].1.is_empty());
    control.store().mark_deployment_cleaned(web_deployments.iter().map(|deployment| deployment.id).min().unwrap()).await.unwrap();
    control.advance_deployments().await;
    let last = std::iter::from_fn(|| agent.try_recv().ok())
        .find_map(|message| match message.message {
            Some(control_message::Message::Command(pb::Command { kind: Some(Kind::RemoveDeployment(remove)), .. }))
                if remove.deployment_id == newest.to_string() =>
            {
                Some(remove)
            }
            _ => None,
        })
        .expect("the newest deployment is removed once the other is");
    assert_eq!(last.volumes, [volume.to_string()]);
}

#[tokio::test]
async fn only_pull_requests_to_the_branch_from_the_repository_get_previews_when_they_are_on() {
    let (setup, hook, _) = previews_setup().await;
    let (control, admin) = (&setup.control, &setup.admin);

    let other_branch = ignored(deliver(&setup, &hook, &pull_request("opened", 8, "develop", HEAD, false)).await);
    let fork = ignored(deliver(&setup, &hook, &pull_request("opened", 9, "main", HEAD, true)).await);
    let environment = control.get_environment(admin, setup.environment.id).await.unwrap();
    let off = EnvironmentChanges { previews: Some(false), ..Default::default() };
    control.update_environment(admin, environment.id, environment.revision, off).await.unwrap();
    let turned_off = ignored(deliver(&setup, &hook, &pull_request("opened", 10, "main", HEAD, false)).await);

    assert_eq!(other_branch, "pull request #8 is to `develop`; the service deploys `main`");
    assert!(fork.starts_with("pull requests from forks get no preview"), "{fork}");
    assert_eq!(turned_off, "previews of `production` are off");
    let environments = control.list_environments(admin, environment.project_id, None, 10).await.unwrap();
    assert_eq!(environments.items.len(), 1);
}

#[tokio::test]
async fn project_admins_delete_a_preview_with_its_services() {
    let (setup, hook, _) = previews_setup().await;
    let (control, admin) = (&setup.control, &setup.admin);
    let (preview, _) = previewed(deliver(&setup, &hook, &pull_request("opened", 7, "main", HEAD, false)).await);

    control.delete_environment(admin, preview.id).await.unwrap();
    let remade = previewed(deliver(&setup, &hook, &pull_request("synchronize", 7, "main", NEXT, false)).await);

    assert!(control.store().get_environment(preview.id).await.is_err());
    assert_ne!(remade.0.id, preview.id, "the next commit makes it again");
    assert_eq!(remade.1.len(), 2);
}
