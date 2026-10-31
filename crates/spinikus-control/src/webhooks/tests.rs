use serde_json::json;
use spinikus_core::{
    deployment::DeploymentStatus,
    source::{GitSource, ImageSource},
};
use spinikus_proto::agent::v1::{command::Kind, control_message};

use super::*;
use crate::deployments::{
    DeploymentTimings,
    tests::{Setup, connect, setup},
};

const BEFORE: &str = "1111111111111111111111111111111111111111";
const COMMIT: &str = "abcdef0123456789abcdef0123456789abcdef01";

/// The test service, deploying `main` of a Git repository from `root_directory`.
async fn git_setup(root_directory: Option<&str>) -> Setup {
    let setup = setup(DeploymentTimings::default()).await;
    let source = setup.control.get_source(&setup.admin, setup.service.id).await.unwrap();
    let git = SourceSpec::Git(GitSource {
        repo_url: "https://github.com/acme/shop.git".to_string(),
        branch: "main".to_string(),
        root_directory: root_directory.map(str::to_string),
    });
    setup.control.replace_source(&setup.admin, setup.service.id, source.revision, git).await.unwrap();
    setup
}

fn push(branch: &str, commit: &str, paths: &[&str]) -> Vec<u8> {
    json!({
        "ref": format!("refs/heads/{branch}"), "before": BEFORE, "after": commit,
        "commits": [{ "added": [], "modified": paths, "removed": [] }],
    })
    .to_string()
    .into_bytes()
}

fn hmac_hex(secret: &Secret, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.expose().as_bytes()).unwrap();
    mac.update(body);
    mac.finalize().into_bytes().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Delivers a GitHub event signed with `secret`.
async fn github(setup: &Setup, hook: &NewWebhook, event: &str, body: &[u8]) -> Result<PushOutcome, ControlError> {
    let signature = format!("sha256={}", hmac_hex(&hook.secret, body));
    let delivery = PushDelivery { event: Some(event), signature: Some(&signature), body };
    setup.control.receive_push(GitProvider::GitHub, hook.webhook.id, delivery).await
}

fn deployed(outcome: Result<PushOutcome, ControlError>) -> Deployment {
    match outcome {
        Ok(PushOutcome::Deployed(deployment)) => *deployment,
        other => panic!("expected a deployment, got {other:?}"),
    }
}

fn ignored(outcome: Result<PushOutcome, ControlError>) -> String {
    match outcome {
        Ok(PushOutcome::Ignored(why)) => why,
        other => panic!("expected the push to be ignored, got {other:?}"),
    }
}

#[tokio::test]
async fn needs_a_git_source_and_keeps_its_url_when_the_secret_changes() {
    let setup = setup(DeploymentTimings::default()).await;
    let control = &setup.control;
    let (admin, service) = (&setup.admin, setup.service.id);

    let on_an_image = control.create_webhook(admin, service).await;
    let source = control.get_source(admin, service).await.unwrap();
    let git =
        SourceSpec::Git(GitSource { repo_url: "https://h/r".to_string(), branch: "main".to_string(), root_directory: None });
    control.replace_source(admin, service, source.revision, git).await.unwrap();
    let first = control.create_webhook(admin, service).await.unwrap();
    let second = control.create_webhook(admin, service).await.unwrap();
    let shown = control.get_webhook(admin, service).await.unwrap();
    control.delete_webhook(admin, service).await.unwrap();
    let gone = control.get_webhook(admin, service).await;

    assert!(matches!(on_an_image, Err(ControlError::Invalid { field: "source", .. })), "{on_an_image:?}");
    assert_eq!(first.secret.expose().len(), 64);
    assert_eq!(second.webhook.id, first.webhook.id);
    assert_ne!(second.secret.expose(), first.secret.expose());
    assert_eq!(shown.paths.github, format!("/hooks/git/github/{}", first.webhook.id));
    assert!(matches!(gone, Err(ControlError::NotFound { entity: "webhook", .. })), "{gone:?}");
}

#[tokio::test]
async fn a_signed_push_deploys_its_commit_once() {
    let setup = git_setup(None).await;
    let control = &setup.control;
    let mut agent = connect(control, setup.server.id);
    let hook = control.create_webhook(&setup.admin, setup.service.id).await.unwrap();
    let body = push("main", COMMIT, &["src/main.rs"]);

    let pong = github(&setup, &hook, "ping", br#"{"zen": "Design for failure."}"#).await;
    let deployment = deployed(github(&setup, &hook, "push", &body).await);
    let again = ignored(github(&setup, &hook, "push", &body).await);
    control.advance_deployments().await;

    assert!(matches!(pong, Ok(PushOutcome::Pong)), "{pong:?}");
    assert_eq!((deployment.trigger, deployment.commit_sha.as_deref()), (DeploymentTrigger::GitPush, Some(COMMIT)));
    assert!(again.contains("already"), "{again}");
    let events = control.list_deployment_events(&setup.admin, deployment.id).await.unwrap();
    assert_eq!(events[0].actor, Actor::Webhook(hook.webhook.id));
    let command = std::iter::from_fn(|| agent.try_recv().ok())
        .find_map(|message| match message.message {
            Some(control_message::Message::Command(command)) => Some(command),
            _ => None,
        })
        .unwrap();
    let Some(Kind::PrepareDeployment(prepare)) = command.kind else { panic!("{command:?}") };
    let Some(spinikus_proto::agent::v1::prepare_deployment::Source::Git(git)) = prepare.source else { panic!() };
    assert_eq!(git.commit_sha, COMMIT);
    let webhook = control.get_webhook(&setup.admin, setup.service.id).await.unwrap();
    assert!(webhook.last_delivery.unwrap().starts_with("ignored: "));
}

#[tokio::test]
async fn refuses_deliveries_without_the_secret() {
    let setup = git_setup(None).await;
    let control = &setup.control;
    let hook = control.create_webhook(&setup.admin, setup.service.id).await.unwrap();
    let body = push("main", COMMIT, &["a"]);
    let other = Secret::new("not the secret");
    let forged = format!("sha256={}", hmac_hex(&other, &body));

    let mut outcomes = Vec::new();
    for (provider, signature) in [
        (GitProvider::GitHub, Some(forged.as_str())),
        (GitProvider::GitHub, None),
        (GitProvider::GitHub, Some("sha256=zz")),
        (GitProvider::Gitea, Some(forged.trim_start_matches("sha256="))),
        (GitProvider::GitLab, Some("not the secret")),
    ] {
        let delivery = PushDelivery { event: Some("push"), signature, body: &body };
        outcomes.push(control.receive_push(provider, hook.webhook.id, delivery).await);
    }
    let unknown = control.receive_push(
        GitProvider::GitHub,
        WebhookId::new(),
        PushDelivery { event: Some("push"), signature: None, body: &body },
    );

    for outcome in outcomes {
        assert!(matches!(outcome, Err(ControlError::Unauthenticated)), "{outcome:?}");
    }
    assert!(matches!(unknown.await, Err(ControlError::NotFound { entity: "webhook", .. })));
    let webhook = control.get_webhook(&setup.admin, setup.service.id).await.unwrap();
    assert_eq!(webhook.last_delivery.as_deref(), Some("refused: the gitlab signature does not match the secret"));
    assert!(control.store().list_deployments(setup.service.id, None, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn gitlab_and_gitea_deliveries_deploy_too() {
    let setup = git_setup(None).await;
    let control = &setup.control;
    let hook = control.create_webhook(&setup.admin, setup.service.id).await.unwrap();
    let first = push("main", COMMIT, &["a"]);
    let second = push("main", "abcdef0123456789abcdef0123456789abcdef02", &["a"]);

    let gitlab = PushDelivery { event: Some("Push Hook"), signature: Some(hook.secret.expose()), body: &first };
    let gitlab = deployed(control.receive_push(GitProvider::GitLab, hook.webhook.id, gitlab).await);
    let signature = hmac_hex(&hook.secret, &second);
    let gitea = PushDelivery { event: Some("push"), signature: Some(&signature), body: &second };
    let gitea = deployed(control.receive_push(GitProvider::Gitea, hook.webhook.id, gitea).await);

    assert_eq!(gitlab.commit_sha.as_deref(), Some(COMMIT));
    assert_eq!(gitea.commit_sha.as_deref(), Some("abcdef0123456789abcdef0123456789abcdef02"));
    // The newer push supersedes the queued one.
    assert_eq!(control.store().get_deployment(gitlab.id).await.unwrap().status, DeploymentStatus::Canceled);
}

#[tokio::test]
async fn ignores_what_is_not_a_push_to_the_service() {
    let setup = git_setup(Some("apps/web")).await;
    let control = &setup.control;
    let hook = control.create_webhook(&setup.admin, setup.service.id).await.unwrap();
    let zero = "0000000000000000000000000000000000000000";

    let issue = ignored(github(&setup, &hook, "issues", b"{}").await);
    let branch = ignored(github(&setup, &hook, "push", &push("dev", COMMIT, &["apps/web/a"])).await);
    let elsewhere = ignored(github(&setup, &hook, "push", &push("main", COMMIT, &["apps/api/main.go", "README.md"])).await);
    let deleted = ignored(github(&setup, &hook, "push", &push("main", zero, &[])).await);
    let tag = json!({ "ref": "refs/tags/v1", "before": zero, "after": COMMIT }).to_string();
    let tag = ignored(github(&setup, &hook, "push", tag.as_bytes()).await);
    let garbage = github(&setup, &hook, "push", b"[1, 2]").await;
    let web = deployed(github(&setup, &hook, "push", &push("main", COMMIT, &["apps/web/src/app.ts"])).await);
    let source = control.get_source(&setup.admin, setup.service.id).await.unwrap();
    let image = SourceSpec::Image(ImageSource { image: "web:1".to_string() });
    control.replace_source(&setup.admin, setup.service.id, source.revision, image).await.unwrap();
    let now_an_image = ignored(github(&setup, &hook, "push", &push("main", COMMIT, &["apps/web/b"])).await);

    assert!(issue.contains("`issues` events do not deploy"), "{issue}");
    assert_eq!(branch, "a push to `dev`; the service deploys `main`");
    assert_eq!(elsewhere, "nothing under `apps/web` changed");
    assert_eq!(deleted, "the push deleted `main`");
    assert_eq!(tag, "tags do not deploy");
    assert!(matches!(garbage, Err(ControlError::Invalid { field: "body", .. })), "{garbage:?}");
    assert_eq!(web.commit_sha.as_deref(), Some(COMMIT));
    assert_eq!(now_an_image, "the service deploys an image now");
}
