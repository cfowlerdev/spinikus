//! The Docker driver against a real Docker daemon. Ignored by default, since it needs one:
//! run with `cargo test -p spinikus-agent --test docker -- --ignored`. It pulls `busybox`.

use std::{collections::HashMap, time::Duration};

use bollard::{
    models::ContainerCreateBody,
    query_parameters::{CreateImageOptionsBuilder, RemoveContainerOptionsBuilder},
};
use spinikus_agent::runtime::{ContainerRuntime, DEPLOYMENT_LABEL, DockerRuntime, SERVICE_LABEL};
use spinikus_core::{
    id::{DeploymentId, ServiceId},
    server::InstanceStatus,
};
use tokio::time::timeout;
use tokio_stream::StreamExt;

const IMAGE: &str = "busybox:1.37";

/// Removes the container even when an assertion fails.
struct Cleanup(DockerRuntime, String);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let (runtime, id) = (self.0.clone(), self.1.clone());
        let options = RemoveContainerOptionsBuilder::new().force(true).build();
        let _ = std::thread::spawn(move || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(async move { runtime.client().unwrap().remove_container(&id, Some(options)).await })
        })
        .join();
    }
}

/// Waits for an event about `id`, skipping others.
async fn event_for(events: &mut spinikus_agent::runtime::ContainerEvents, id: &str) {
    timeout(Duration::from_secs(20), async {
        while let Some(event) = events.next().await {
            if event.unwrap() == id {
                return;
            }
        }
        panic!("the event stream ended");
    })
    .await
    .expect("an event for the container");
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn follows_a_managed_container_through_its_life() {
    let runtime = DockerRuntime::new();
    let docker = runtime.client().unwrap();
    let pull = CreateImageOptionsBuilder::new().from_image(IMAGE).build();
    let mut pulling = docker.create_image(Some(pull), None, None);
    while let Some(progress) = pulling.next().await {
        progress.unwrap();
    }
    let (service, deployment) = (ServiceId::new(), DeploymentId::new());
    let labels =
        HashMap::from([(SERVICE_LABEL.to_string(), service.to_string()), (DEPLOYMENT_LABEL.to_string(), deployment.to_string())]);
    let body = ContainerCreateBody {
        image: Some(IMAGE.to_string()),
        cmd: Some(vec!["sleep".to_string(), "300".to_string()]),
        labels: Some(labels),
        ..Default::default()
    };
    let mut events = runtime.events();

    let id = docker.create_container(None, body).await.unwrap().id;
    let _cleanup = Cleanup(runtime.clone(), id.clone());
    docker.start_container(&id, None).await.unwrap();
    event_for(&mut events, &id).await;
    let listed = runtime.list().await.unwrap();
    let started = listed.iter().find(|instance| instance.container_id == id).expect("listed");

    assert_eq!((started.service_id, started.deployment_id), (service, deployment));
    assert_eq!(started.status, InstanceStatus::Running);
    assert!(started.started_at.is_some());

    runtime.stop(&id, Duration::from_secs(1)).await.unwrap();
    runtime.stop(&id, Duration::from_secs(1)).await.unwrap();
    event_for(&mut events, &id).await;
    assert_eq!(runtime.inspect(&id).await.unwrap().unwrap().status, InstanceStatus::Exited);

    runtime.restart(&id, Duration::from_secs(1)).await.unwrap();
    assert_eq!(runtime.inspect(&id).await.unwrap().unwrap().status, InstanceStatus::Running);

    runtime.stop(&id, Duration::from_secs(1)).await.unwrap();
    runtime.remove(&id).await.unwrap();
    assert_eq!(runtime.inspect(&id).await.unwrap(), None);
    assert_eq!(runtime.remove(&id).await, Err(spinikus_agent::runtime::RuntimeError::NotFound));
}
