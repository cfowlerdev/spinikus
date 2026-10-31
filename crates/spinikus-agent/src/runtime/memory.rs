//! An in-memory runtime for tests: containers are records that commands change, with
//! events like Docker's, and a switch to make the runtime unreachable.

use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use jiff::Timestamp;
use spinikus_core::server::{InstanceHealth, InstanceStatus, ObservedInstance};
use tokio::sync::broadcast;
use tokio_stream::{StreamExt, wrappers::BroadcastStream};

use super::{BuildSpec, ContainerEvents, ContainerRuntime, ContainerSpec, RuntimeError};

#[derive(Clone)]
enum Event {
    Changed(String),
    /// The runtime became unreachable: open event streams fail.
    Down,
}

pub struct MemoryRuntime {
    containers: Mutex<BTreeMap<String, ObservedInstance>>,
    specs: Mutex<HashMap<String, ContainerSpec>>,
    /// Images that can be pulled, and the pinned reference each one resolves to.
    images: Mutex<HashMap<String, String>>,
    endpoint: Mutex<Option<SocketAddr>>,
    /// Builds so far, and the Dockerfile each one read.
    builds: Mutex<Vec<(BuildSpec, String)>>,
    /// Why builds fail, if they do.
    build_failure: Mutex<Option<String>>,
    exit_on_start: AtomicBool,
    next_id: AtomicU64,
    events: broadcast::Sender<Event>,
    available: AtomicBool,
}

impl Default for MemoryRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryRuntime {
    pub fn new() -> Self {
        Self {
            containers: Mutex::default(),
            specs: Mutex::default(),
            images: Mutex::default(),
            endpoint: Mutex::default(),
            builds: Mutex::default(),
            build_failure: Mutex::default(),
            exit_on_start: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            events: broadcast::channel(256).0,
            available: AtomicBool::new(true),
        }
    }

    /// Makes `image` pullable, resolving to `pinned`.
    pub fn add_image(&self, image: &str, pinned: &str) {
        lock(&self.images).insert(image.to_string(), pinned.to_string());
    }

    /// The builds so far, each with the text of the Dockerfile it read.
    pub fn builds(&self) -> Vec<(BuildSpec, String)> {
        lock(&self.builds).clone()
    }

    /// Makes builds fail with `message`, or succeed again with `None`.
    pub fn fail_builds(&self, message: Option<&str>) {
        *lock(&self.build_failure) = message.map(str::to_string);
    }

    /// Where running containers with a port are published: a server the test runs.
    pub fn set_endpoint(&self, endpoint: SocketAddr) {
        *lock(&self.endpoint) = Some(endpoint);
    }

    /// Makes containers exit as soon as they start, like an app that crashes on boot.
    pub fn exit_on_start(&self, exit: bool) {
        self.exit_on_start.store(exit, Ordering::SeqCst);
    }

    /// What a container was created with.
    pub fn spec(&self, container_id: &str) -> Option<ContainerSpec> {
        lock(&self.specs).get(container_id).cloned()
    }

    /// Adds or replaces a container, as if the runtime had changed it.
    pub fn put(&self, instance: ObservedInstance) {
        let id = instance.container_id.clone();
        self.lock().insert(id.clone(), instance);
        let _ = self.events.send(Event::Changed(id));
    }

    pub fn get(&self, container_id: &str) -> Option<ObservedInstance> {
        self.lock().get(container_id).cloned()
    }

    pub fn containers(&self) -> Vec<ObservedInstance> {
        self.lock().values().cloned().collect()
    }

    /// Makes every call fail with [`RuntimeError::Unavailable`], and open event streams
    /// fail, until it is made available again.
    pub fn set_available(&self, available: bool) {
        self.available.store(available, Ordering::SeqCst);
        if !available {
            let _ = self.events.send(Event::Down);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, ObservedInstance>> {
        lock(&self.containers)
    }

    fn check(&self) -> Result<(), RuntimeError> {
        match self.available.load(Ordering::SeqCst) {
            true => Ok(()),
            false => Err(RuntimeError::Unavailable("the memory runtime is switched off".to_string())),
        }
    }

    fn change(&self, container_id: &str, change: impl FnOnce(&mut ObservedInstance)) -> Result<(), RuntimeError> {
        self.check()?;
        change(self.lock().get_mut(container_id).ok_or(RuntimeError::NotFound)?);
        let _ = self.events.send(Event::Changed(container_id.to_string()));
        Ok(())
    }
}

#[async_trait]
impl ContainerRuntime for MemoryRuntime {
    async fn list(&self) -> Result<Vec<ObservedInstance>, RuntimeError> {
        self.check()?;
        Ok(self.containers())
    }

    async fn inspect(&self, container_id: &str) -> Result<Option<ObservedInstance>, RuntimeError> {
        self.check()?;
        Ok(self.get(container_id))
    }

    async fn stop(&self, container_id: &str, _grace: Duration) -> Result<(), RuntimeError> {
        self.change(container_id, |instance| instance.status = InstanceStatus::Exited)
    }

    async fn remove(&self, container_id: &str) -> Result<(), RuntimeError> {
        self.check()?;
        self.lock().remove(container_id).ok_or(RuntimeError::NotFound)?;
        lock(&self.specs).remove(container_id);
        let _ = self.events.send(Event::Changed(container_id.to_string()));
        Ok(())
    }

    async fn restart(&self, container_id: &str, _grace: Duration) -> Result<(), RuntimeError> {
        self.change(container_id, |instance| {
            instance.status = InstanceStatus::Running;
            instance.started_at = Some(Timestamp::now());
        })
    }

    fn events(&self) -> ContainerEvents {
        let events = BroadcastStream::new(self.events.subscribe()).map(|event| match event {
            Ok(Event::Changed(id)) => Ok(id),
            Ok(Event::Down) => Err(RuntimeError::Unavailable("the memory runtime was switched off".to_string())),
            Err(lagged) => Err(RuntimeError::Unavailable(lagged.to_string())),
        });
        Box::pin(events)
    }

    async fn pull(&self, image: &str, progress: &(dyn Fn(String) + Send + Sync)) -> Result<String, RuntimeError> {
        self.check()?;
        progress(format!("Pulling {image}"));
        lock(&self.images).get(image).cloned().ok_or_else(|| RuntimeError::Failed(format!("cannot pull {image}: not found")))
    }

    /// Reads the Dockerfile and makes an image ID from the tag, which can then be started.
    async fn build(&self, spec: &BuildSpec, progress: &(dyn Fn(String) + Send + Sync)) -> Result<String, RuntimeError> {
        self.check()?;
        let dockerfile = std::fs::read_to_string(spec.context.join(&spec.dockerfile))
            .map_err(|error| RuntimeError::Failed(format!("cannot read the Dockerfile: {error}")))?;
        progress(format!("Building {}", spec.tag));
        lock(&self.builds).push((spec.clone(), dockerfile));
        if let Some(message) = lock(&self.build_failure).clone() {
            return Err(RuntimeError::Failed(message));
        }
        let id = format!("sha256:{:064x}", self.next_id.fetch_add(1, Ordering::SeqCst));
        lock(&self.images).insert(spec.tag.clone(), id.clone());
        Ok(id)
    }

    async fn create(&self, spec: &ContainerSpec) -> Result<String, RuntimeError> {
        self.check()?;
        let mut specs = lock(&self.specs);
        if specs.values().any(|existing| existing.name == spec.name) {
            return Err(RuntimeError::Conflict);
        }
        let id = format!("mem-{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        specs.insert(id.clone(), spec.clone());
        drop(specs);
        self.put(ObservedInstance {
            service_id: spec.service_id,
            deployment_id: spec.deployment_id,
            container_id: id.clone(),
            status: InstanceStatus::Created,
            health: InstanceHealth::None,
            restart_count: 0,
            started_at: None,
        });
        Ok(id)
    }

    async fn start(&self, container_id: &str) -> Result<(), RuntimeError> {
        let status = match self.exit_on_start.load(Ordering::SeqCst) {
            true => InstanceStatus::Exited,
            false => InstanceStatus::Running,
        };
        self.change(container_id, |instance| {
            instance.status = status;
            instance.started_at = Some(Timestamp::now());
        })
    }

    async fn endpoint(&self, container_id: &str) -> Result<Option<SocketAddr>, RuntimeError> {
        self.check()?;
        let running = self.get(container_id).is_some_and(|instance| instance.status == InstanceStatus::Running);
        let has_port = self.spec(container_id).is_some_and(|spec| spec.port.is_some());
        Ok((running && has_port).then(|| *lock(&self.endpoint)).flatten())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
