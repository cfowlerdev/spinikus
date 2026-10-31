//! An in-memory runtime for tests: containers are records that commands change, with
//! events like Docker's, and a switch to make the runtime unreachable.

use std::{
    collections::BTreeMap,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use jiff::Timestamp;
use spinikus_core::server::{InstanceStatus, ObservedInstance};
use tokio::sync::broadcast;
use tokio_stream::{StreamExt, wrappers::BroadcastStream};

use super::{ContainerEvents, ContainerRuntime, RuntimeError};

#[derive(Clone)]
enum Event {
    Changed(String),
    /// The runtime became unreachable: open event streams fail.
    Down,
}

pub struct MemoryRuntime {
    containers: Mutex<BTreeMap<String, ObservedInstance>>,
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
        Self { containers: Mutex::default(), events: broadcast::channel(256).0, available: AtomicBool::new(true) }
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
        self.containers.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
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
}
