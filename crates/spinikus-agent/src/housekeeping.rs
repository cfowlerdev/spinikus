//! Housekeeping the control plane doesn't direct: every [`INTERVAL`], the agent prunes the
//! build cache no build has used for [`BUILD_CACHE_MAX_AGE`] (BuildKit's layers and the cache
//! mounts Railpack's builds use) and removes the environments' networks that no container
//! is on any more. Failures are logged and tried again next time.

use std::{sync::Arc, time::Duration};

use crate::runtime::{ContainerRuntime, RuntimeError};

/// How often to clean up, and how soon after the agent starts.
pub(crate) const INTERVAL: Duration = Duration::from_secs(60 * 60);
const FIRST_DELAY: Duration = Duration::from_secs(5 * 60);

/// Build cache unused for this long goes.
pub(crate) const BUILD_CACHE_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Cleans up every [`INTERVAL`] until `stopped` resolves.
pub(crate) async fn run(runtime: Arc<dyn ContainerRuntime>, stopped: impl Future<Output = ()>) {
    let mut stopped = std::pin::pin!(stopped);
    let mut delay = FIRST_DELAY;
    loop {
        tokio::select! {
            () = stopped.as_mut() => return,
            () = tokio::time::sleep(delay) => {}
        }
        housekeep(&*runtime).await;
        delay = INTERVAL;
    }
}

pub(crate) async fn housekeep(runtime: &dyn ContainerRuntime) {
    match runtime.prune_build_cache(BUILD_CACHE_MAX_AGE).await {
        Ok(0) => {}
        Ok(bytes) => tracing::info!(bytes, "pruned the build cache"),
        Err(error) => log(&error, "failed to prune the build cache"),
    }
    match runtime.remove_unused_networks().await {
        Ok(removed) if removed.is_empty() => {}
        Ok(removed) => tracing::info!(?removed, "removed networks no container is on"),
        Err(error) => log(&error, "failed to remove unused networks"),
    }
}

/// A runtime that is down is reported elsewhere.
fn log(error: &RuntimeError, message: &str) {
    match error {
        RuntimeError::Unavailable(_) => tracing::debug!(%error, message),
        _ => tracing::warn!(%error, message),
    }
}

#[cfg(test)]
mod tests {
    use spinikus_core::id::{DeploymentId, ServiceId};

    use super::*;
    use crate::runtime::{ContainerSpec, MemoryRuntime, Network};

    fn spec(network: &str) -> ContainerSpec {
        let deployment = DeploymentId::new();
        ContainerSpec {
            name: format!("spinikus-{deployment}"),
            image: "app@sha256:01".to_string(),
            service_id: ServiceId::new(),
            deployment_id: deployment,
            env: Vec::new(),
            port: None,
            volumes: Vec::new(),
            network: Some(Network { name: network.to_string(), aliases: Vec::new() }),
            command: Vec::new(),
        }
    }

    #[tokio::test]
    async fn prunes_old_build_cache_and_networks_no_container_is_on() {
        let runtime = MemoryRuntime::new();
        let kept = runtime.create(&spec("spinikus-env_a")).await.unwrap();
        let gone = runtime.create(&spec("spinikus-env_b")).await.unwrap();
        runtime.stop(&kept, Duration::ZERO).await.unwrap();
        runtime.remove(&gone).await.unwrap();

        housekeep(&runtime).await;

        assert_eq!(runtime.build_cache_prunes(), [BUILD_CACHE_MAX_AGE]);
        assert_eq!(runtime.networks(), ["spinikus-env_a"], "a stopped container keeps its network");
        // A runtime that is down only gets logged.
        runtime.set_available(false);
        housekeep(&runtime).await;
        assert_eq!(runtime.build_cache_prunes().len(), 1);
    }
}
