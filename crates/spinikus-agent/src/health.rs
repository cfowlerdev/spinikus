//! The health check that gates cutover, run by the agent next to the container. With a
//! path, the agent sends `GET path` to the app's published loopback port until it answers
//! with a 2xx or 3xx status; failures during the start period do not count, and the check
//! fails after `retries` more. Without a path, the container only has to keep running
//! through the start period. Either way, a container that stops running fails at once.

use std::{net::SocketAddr, time::Duration};

use spinikus_core::server::InstanceStatus;
use spinikus_proto::agent::v1::{self as pb, FailureReason};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{Instant, timeout},
};

use crate::{
    commands::{Progress, failure},
    runtime::ContainerRuntime,
};

// The control plane sends every field; these defaults are for ones it leaves out.
use spinikus_core::runtime::{
    DEFAULT_HEALTHCHECK_INTERVAL as DEFAULT_INTERVAL, DEFAULT_HEALTHCHECK_RETRIES as DEFAULT_RETRIES,
    DEFAULT_HEALTHCHECK_START_PERIOD as DEFAULT_START_PERIOD, DEFAULT_HEALTHCHECK_TIMEOUT as DEFAULT_TIMEOUT,
    DEFAULT_SETTLE_PERIOD as DEFAULT_SETTLE,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HealthCheck {
    pub path: Option<String>,
    pub interval: Duration,
    pub timeout: Duration,
    pub retries: u32,
    pub start_period: Duration,
}

impl HealthCheck {
    /// The check to run, with defaults for unset fields. Fails for a path that is not an
    /// absolute path of visible ASCII characters, since it goes into the request line.
    pub(crate) fn new(check: Option<pb::HealthCheck>) -> Result<Self, String> {
        let check = check.unwrap_or_default();
        let duration = |duration: Option<prost_types::Duration>| duration.and_then(|duration| Duration::try_from(duration).ok());
        let path = Some(check.path).filter(|path| !path.is_empty());
        if let Some(path) = &path
            && (!path.starts_with('/') || !path.bytes().all(|byte| byte.is_ascii_graphic()))
        {
            return Err(format!("invalid health check path `{}`", path.escape_debug()));
        }
        let default_start = if path.is_some() { DEFAULT_START_PERIOD } else { DEFAULT_SETTLE };
        Ok(Self {
            interval: duration(check.interval).filter(|interval| !interval.is_zero()).unwrap_or(DEFAULT_INTERVAL),
            timeout: duration(check.timeout).filter(|timeout| !timeout.is_zero()).unwrap_or(DEFAULT_TIMEOUT),
            retries: if check.retries == 0 { DEFAULT_RETRIES } else { check.retries },
            start_period: duration(check.start_period).unwrap_or(default_start),
            path,
        })
    }
}

/// Runs the check against a started container.
pub(crate) async fn verify(
    runtime: &dyn ContainerRuntime,
    container_id: &str,
    check: &HealthCheck,
    progress: &Progress,
) -> Result<(), pb::CommandFailed> {
    let started = Instant::now();
    let mut failures = 0;
    loop {
        let status = runtime.inspect(container_id).await?.map(|instance| instance.status);
        match status {
            Some(InstanceStatus::Running) => {}
            Some(status) => {
                let message = format!("the container stopped running during the health check (it is {})", status.as_str());
                return Err(failure(FailureReason::StartFailed, message));
            }
            None => return Err(failure(FailureReason::StartFailed, "the container is gone")),
        }
        let in_start_period = started.elapsed() < check.start_period;
        let Some(path) = &check.path else {
            if in_start_period {
                tokio::time::sleep(check.interval.min(check.start_period - started.elapsed())).await;
                continue;
            }
            return Ok(());
        };
        let endpoint = runtime.endpoint(container_id).await?.ok_or_else(|| {
            failure(FailureReason::StartFailed, "the container's port is not published, so it cannot be checked")
        })?;
        match probe(endpoint, path, check.timeout).await {
            Ok(status) => {
                progress.line(format!("GET {path}: {status}"));
                return Ok(());
            }
            Err(problem) => {
                progress.line(format!("GET {path}: {problem}"));
                if !in_start_period {
                    failures += 1;
                    if failures > check.retries {
                        let message = format!("GET {path} failed {failures} times after the start period, last with: {problem}");
                        return Err(failure(FailureReason::HealthCheckFailed, message));
                    }
                }
            }
        }
        tokio::time::sleep(check.interval).await;
    }
}

/// Sends `GET path` and returns the status code if it is 2xx or 3xx, otherwise what went
/// wrong.
async fn probe(endpoint: SocketAddr, path: &str, limit: Duration) -> Result<u16, String> {
    let exchange = async {
        let mut stream = TcpStream::connect(endpoint).await.map_err(|error| format!("cannot connect: {error}"))?;
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nUser-Agent: spinikus-health-check\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.map_err(|error| format!("cannot send the request: {error}"))?;
        // Only the status line matters, so read until it is complete.
        let mut head = Vec::with_capacity(256);
        let mut buffer = [0; 256];
        while !head.windows(2).any(|pair| pair == b"\r\n") && head.len() < 8192 {
            let read = stream.read(&mut buffer).await.map_err(|error| format!("cannot read the response: {error}"))?;
            if read == 0 {
                break;
            }
            head.extend_from_slice(&buffer[..read]);
        }
        status(&head).ok_or_else(|| "not an HTTP response".to_string())
    };
    match timeout(limit, exchange).await {
        Ok(Ok(code)) if (200..400).contains(&code) => Ok(code),
        Ok(Ok(code)) => Err(format!("status {code}")),
        Ok(Err(problem)) => Err(problem),
        Err(_) => Err(format!("no response within {limit:?}")),
    }
}

/// The status code from an HTTP/1.x status line.
fn status(head: &[u8]) -> Option<u16> {
    let line = head.split(|&byte| byte == b'\r').next()?;
    let line = std::str::from_utf8(line).ok()?;
    let mut parts = line.split(' ');
    parts.next().filter(|version| version.starts_with("HTTP/1."))?;
    parts.next()?.parse().ok().filter(|code| (100..600).contains(code))
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use spinikus_core::id::{DeploymentId, ServiceId};
    use tokio::net::TcpListener;

    use super::*;
    use crate::runtime::{ContainerSpec, MemoryRuntime};

    /// An app that answers 503 to its first `failures` requests, then 200.
    async fn app(failures: usize) -> (SocketAddr, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = requests.clone();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 1024];
                let _ = stream.read(&mut request).await;
                assert!(request.starts_with(b"GET /healthz HTTP/1.1\r\n"));
                let status = match counted.fetch_add(1, Ordering::SeqCst) < failures {
                    true => "503 Service Unavailable",
                    false => "200 OK",
                };
                let _ = stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").as_bytes()).await;
            }
        });
        (address, requests)
    }

    async fn started(runtime: &MemoryRuntime, port: Option<u16>) -> String {
        let spec = ContainerSpec {
            name: "web".to_string(),
            image: "app@sha256:0".to_string(),
            service_id: ServiceId::new(),
            deployment_id: DeploymentId::new(),
            env: Vec::new(),
            port,
            volumes: Vec::new(),
            network: None,
        };
        let id = runtime.create(&spec).await.unwrap();
        runtime.start(&id).await.unwrap();
        id
    }

    fn check(path: &str, start_period: Duration) -> HealthCheck {
        HealthCheck {
            path: Some(path.to_string()).filter(|path| !path.is_empty()),
            interval: Duration::from_millis(10),
            timeout: Duration::from_secs(1),
            retries: 2,
            start_period,
        }
    }

    fn progress() -> Progress {
        Progress::new("cmd".to_string(), tokio::sync::mpsc::channel(64).0)
    }

    #[tokio::test]
    async fn passes_once_the_app_answers() {
        let runtime = MemoryRuntime::new();
        let (address, requests) = app(3).await;
        runtime.set_endpoint(address);
        let id = started(&runtime, Some(3000)).await;

        verify(&runtime, &id, &check("/healthz", Duration::from_secs(5)), &progress()).await.unwrap();

        assert_eq!(requests.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn fails_after_the_retries_once_the_start_period_is_over() {
        let runtime = MemoryRuntime::new();
        let (address, requests) = app(usize::MAX).await;
        runtime.set_endpoint(address);
        let id = started(&runtime, Some(3000)).await;

        let failed = verify(&runtime, &id, &check("/healthz", Duration::ZERO), &progress()).await.unwrap_err();

        assert_eq!(failed.reason, FailureReason::HealthCheckFailed as i32);
        assert_eq!(failed.message, "GET /healthz failed 3 times after the start period, last with: status 503");
        assert_eq!(requests.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_container_that_exits_fails_to_start() {
        let runtime = MemoryRuntime::new();
        runtime.exit_on_start(true);
        let id = started(&runtime, Some(3000)).await;

        let failed = verify(&runtime, &id, &check("/healthz", Duration::from_secs(5)), &progress()).await.unwrap_err();

        assert_eq!(failed.reason, FailureReason::StartFailed as i32);
        assert!(failed.message.contains("it is exited"), "{failed:?}");
    }

    #[tokio::test]
    async fn without_a_path_the_container_only_has_to_keep_running() {
        let runtime = MemoryRuntime::new();
        let id = started(&runtime, None).await;

        verify(&runtime, &id, &check("", Duration::from_millis(30)), &progress()).await.unwrap();
    }

    #[tokio::test]
    async fn an_unreachable_app_fails() {
        let runtime = MemoryRuntime::new();
        // Nothing listens on a port that was just freed.
        let address = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
        runtime.set_endpoint(address);
        let id = started(&runtime, Some(3000)).await;

        let failed = verify(&runtime, &id, &check("/healthz", Duration::ZERO), &progress()).await.unwrap_err();

        assert!(failed.message.contains("cannot connect"), "{failed:?}");
    }

    #[test]
    fn fills_in_defaults_and_rejects_unsafe_paths() {
        let defaults = HealthCheck::new(None).unwrap();
        let with_path = HealthCheck::new(Some(pb::HealthCheck { path: "/up".to_string(), ..Default::default() })).unwrap();
        let injected = pb::HealthCheck { path: "/ HTTP/1.1\r\nX: y".to_string(), ..Default::default() };

        assert_eq!(defaults.path, None);
        assert_eq!(defaults.start_period, DEFAULT_SETTLE);
        assert_eq!((with_path.interval, with_path.retries), (DEFAULT_INTERVAL, DEFAULT_RETRIES));
        assert_eq!(with_path.start_period, DEFAULT_START_PERIOD);
        assert!(HealthCheck::new(Some(injected)).is_err());
        assert!(HealthCheck::new(Some(pb::HealthCheck { path: "up".to_string(), ..Default::default() })).is_err());
    }

    #[test]
    fn reads_status_lines() {
        assert_eq!(status(b"HTTP/1.1 204 No Content\r\n"), Some(204));
        assert_eq!(status(b"HTTP/1.0 302 Found\r\nLocation: /"), Some(302));
        assert_eq!(status(b"SSH-2.0-OpenSSH\r\n"), None);
        assert_eq!(status(b"HTTP/1.1 abc\r\n"), None);
    }
}
