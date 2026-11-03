//! An in-memory runtime for tests: containers are records that commands change, with
//! events like Docker's, and a switch to make the runtime unreachable.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
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

use super::{
    BuildSpec, ContainerEvents, ContainerRuntime, ContainerSpec, ExecInput, ExecOutput, ExecStream, LogLine, LogLines,
    LogOptions, OneOffExit, OneOffSpec, RuntimeError, TerminalInput, TerminalInputs, TerminalSize,
};

/// A terminal opened in a container, as [`MemoryRuntime::terminals`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terminal {
    pub container_id: String,
    pub command: Vec<String>,
    pub sizes: Vec<TerminalSize>,
}

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
    removed_images: Mutex<Vec<String>>,
    removed_volumes: Mutex<Vec<String>>,
    /// Every line containers wrote, and a channel for followers.
    logs: Mutex<Vec<(String, LogLine)>>,
    written: broadcast::Sender<(String, LogLine)>,
    /// Why builds fail, if they do.
    build_failure: Mutex<Option<String>>,
    /// What every command run in a container writes, and the commands run so far.
    exec_output: Mutex<Vec<ExecOutput>>,
    /// What the next commands write, one entry each, before `exec_output` again.
    queued_exec_outputs: Mutex<VecDeque<Vec<ExecOutput>>>,
    execs: Mutex<Vec<(String, Vec<String>)>>,
    /// What each command given input read on stdin, in order.
    exec_inputs: std::sync::Arc<Mutex<Vec<Vec<u8>>>>,
    /// Terminals opened so far: the container, the command, and every size it was given.
    terminals: std::sync::Arc<Mutex<Vec<Terminal>>>,
    exit_on_start: AtomicBool,
    /// One-off containers run so far, how the next ones end (with what they write), and the
    /// names removed with `remove_one_off`.
    one_offs: Mutex<Vec<OneOffSpec>>,
    one_off_outcomes: Mutex<VecDeque<(Vec<String>, OneOffExit)>>,
    removed_one_offs: Mutex<Vec<String>>,
    /// The networks containers were created on, and the build cache prunes so far.
    networks: Mutex<std::collections::BTreeSet<String>>,
    build_cache_prunes: Mutex<Vec<Duration>>,
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
            removed_images: Mutex::default(),
            removed_volumes: Mutex::default(),
            logs: Mutex::default(),
            written: broadcast::channel(256).0,
            build_failure: Mutex::default(),
            exec_output: Mutex::new(vec![ExecOutput::Exited(0)]),
            queued_exec_outputs: Mutex::default(),
            execs: Mutex::default(),
            exec_inputs: std::sync::Arc::default(),
            terminals: std::sync::Arc::default(),
            exit_on_start: AtomicBool::new(false),
            one_offs: Mutex::default(),
            one_off_outcomes: Mutex::default(),
            removed_one_offs: Mutex::default(),
            networks: Mutex::default(),
            build_cache_prunes: Mutex::default(),
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

    /// Writes a line to a container's log, as if the app had.
    pub fn write_log(&self, container_id: &str, stderr: bool, text: &str) {
        let line = LogLine { at: Some(Timestamp::now()), stderr, text: text.to_string() };
        lock(&self.logs).push((container_id.to_string(), line.clone()));
        let _ = self.written.send((container_id.to_string(), line));
    }

    /// How many followers a container's log has.
    pub fn log_followers(&self) -> usize {
        self.written.receiver_count()
    }

    /// The image references removed so far, in order.
    pub fn removed_images(&self) -> Vec<String> {
        lock(&self.removed_images).clone()
    }

    /// The volumes removed so far, in order. Every volume exists until removed once.
    pub fn removed_volumes(&self) -> Vec<String> {
        lock(&self.removed_volumes).clone()
    }

    /// Whether `image` (a reference or what it resolves to) is still there.
    pub fn has_image(&self, image: &str) -> bool {
        lock(&self.images).iter().any(|(reference, resolved)| reference == image || resolved == image)
    }

    /// What commands run in containers write from now on; `output` should end with
    /// [`ExecOutput::Exited`]. By default they write nothing and exit with 0.
    pub fn set_exec_output(&self, output: Vec<ExecOutput>) {
        *lock(&self.exec_output) = output;
    }

    /// What the next command run in a container writes, in place of what
    /// [`MemoryRuntime::set_exec_output`] set; each call queues one more.
    pub fn queue_exec_output(&self, output: Vec<ExecOutput>) {
        lock(&self.queued_exec_outputs).push_back(output);
    }

    /// The commands run in containers so far, with the container each ran in.
    pub fn execs(&self) -> Vec<(String, Vec<String>)> {
        lock(&self.execs).clone()
    }

    /// Terminals opened so far, with the sizes each was given.
    pub fn terminals(&self) -> Vec<Terminal> {
        lock(&self.terminals).clone()
    }

    /// What the commands given input read on stdin, in order, once they have read it all.
    pub fn exec_inputs(&self) -> Vec<Vec<u8>> {
        lock(&self.exec_inputs).clone()
    }

    /// How the next one-off container ends, and the lines it writes first; each call queues
    /// one more. Without one queued, they write nothing and exit with 0.
    pub fn queue_one_off(&self, lines: &[&str], exit: OneOffExit) {
        let lines = lines.iter().map(|line| line.to_string()).collect();
        lock(&self.one_off_outcomes).push_back((lines, exit));
    }

    /// The one-off containers run so far.
    pub fn one_offs(&self) -> Vec<OneOffSpec> {
        lock(&self.one_offs).clone()
    }

    /// The one-off containers removed by name so far.
    pub fn removed_one_offs(&self) -> Vec<String> {
        lock(&self.removed_one_offs).clone()
    }

    /// The networks that exist: those containers were created on, until removed as unused.
    pub fn networks(&self) -> Vec<String> {
        lock(&self.networks).iter().cloned().collect()
    }

    /// The `unused_for` of each build cache prune so far.
    pub fn build_cache_prunes(&self) -> Vec<Duration> {
        lock(&self.build_cache_prunes).clone()
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

    fn logs(&self, container_id: &str, options: LogOptions) -> LogLines {
        if let Err(error) = self.check() {
            return Box::pin(tokio_stream::once(Err(error)));
        }
        // Subscribe first, so no line falls between the two.
        let new = BroadcastStream::new(self.written.subscribe());
        let mut written: Vec<LogLine> = lock(&self.logs)
            .iter()
            .filter(|(container, line)| container == container_id && options.since.is_none_or(|since| line.at >= Some(since)))
            .map(|(_, line)| line.clone())
            .collect();
        if let Some(tail) = options.tail {
            written.drain(..written.len().saturating_sub(tail as usize));
        }
        let written = tokio_stream::iter(written.into_iter().map(Ok));
        if !options.follow {
            return Box::pin(written);
        }
        let container_id = container_id.to_string();
        let new = new.filter_map(move |line| match line {
            Ok((container, line)) if container == container_id => Some(Ok(line)),
            Ok(_) => None,
            Err(lagged) => Some(Err(RuntimeError::Failed(lagged.to_string()))),
        });
        Box::pin(written.chain(new))
    }

    fn exec(&self, container_id: &str, command: Vec<String>, input: Option<ExecInput>) -> ExecStream {
        if let Err(error) = self.check() {
            return Box::pin(tokio_stream::once(Err(error)));
        }
        if self.get(container_id).is_none() {
            return Box::pin(tokio_stream::once(Err(RuntimeError::NotFound)));
        }
        lock(&self.execs).push((container_id.to_string(), command));
        let queued = lock(&self.queued_exec_outputs).pop_front();
        let output = queued.unwrap_or_else(|| lock(&self.exec_output).clone());
        let Some(mut input) = input else {
            return Box::pin(tokio_stream::iter(output.into_iter().map(Ok)));
        };
        // Reads all of stdin, like a command that loads it, before writing its output.
        let inputs = self.exec_inputs.clone();
        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move {
            let mut read = Vec::new();
            while let Some(chunk) = input.next().await {
                read.extend_from_slice(&chunk);
            }
            lock(&inputs).push(read);
            for item in output {
                let _ = sender.send(Ok(item)).await;
            }
        });
        Box::pin(tokio_stream::wrappers::ReceiverStream::new(receiver))
    }

    /// Echoes what it is sent, like a terminal with a shell that does nothing; a line
    /// `exit <code>` ends it with that code, and Ctrl+D at the start of a line or the input's
    /// end with 0.
    fn terminal(&self, container_id: &str, command: Vec<String>, size: TerminalSize, mut input: TerminalInputs) -> ExecStream {
        if let Err(error) = self.check() {
            return Box::pin(tokio_stream::once(Err(error)));
        }
        if self.get(container_id).is_none() {
            return Box::pin(tokio_stream::once(Err(RuntimeError::NotFound)));
        }
        let terminals = self.terminals.clone();
        let index = {
            let mut terminals = lock(&terminals);
            terminals.push(Terminal { container_id: container_id.to_string(), command, sizes: vec![size] });
            terminals.len() - 1
        };
        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move {
            let mut line = Vec::new();
            while let Some(item) = input.next().await {
                let data = match item {
                    TerminalInput::Data(data) => data,
                    TerminalInput::Resize(size) => {
                        lock(&terminals)[index].sizes.push(size);
                        continue;
                    }
                };
                // Like a terminal, Ctrl+D at the start of a line is the end of input, unechoed.
                let echoed: Vec<u8> = data.iter().copied().filter(|byte| *byte != 0x04).collect();
                if !echoed.is_empty() && sender.send(Ok(ExecOutput::Stdout(echoed.into()))).await.is_err() {
                    return;
                }
                for byte in data {
                    if byte == 0x04 && line.is_empty() {
                        let _ = sender.send(Ok(ExecOutput::Exited(0))).await;
                        return;
                    }
                    if byte != b'\r' && byte != b'\n' {
                        line.push(byte);
                        continue;
                    }
                    let typed = String::from_utf8_lossy(&line).trim().to_string();
                    line.clear();
                    if let Some(code) = typed.strip_prefix("exit ").and_then(|code| code.trim().parse().ok()) {
                        let _ = sender.send(Ok(ExecOutput::Exited(code))).await;
                        return;
                    }
                }
            }
            let _ = sender.send(Ok(ExecOutput::Exited(0))).await;
        });
        Box::pin(tokio_stream::wrappers::ReceiverStream::new(receiver))
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

    /// Removes the references named `image` and those resolving to it, unless a container
    /// was created from it.
    async fn remove_volume(&self, name: &str) -> Result<(), RuntimeError> {
        self.check()?;
        if lock(&self.specs).values().any(|spec| spec.volumes.iter().any(|mount| mount.volume == name)) {
            return Err(RuntimeError::InUse);
        }
        let mut removed = lock(&self.removed_volumes);
        if removed.iter().any(|volume| volume == name) {
            return Err(RuntimeError::NotFound);
        }
        removed.push(name.to_string());
        Ok(())
    }

    async fn remove_image(&self, image: &str) -> Result<(), RuntimeError> {
        self.check()?;
        let mut images = lock(&self.images);
        let resolved: Vec<String> = images
            .iter()
            .filter(|(reference, resolved)| *reference == image || *resolved == image)
            .map(|(_, resolved)| resolved.clone())
            .collect();
        if resolved.is_empty() {
            return Err(RuntimeError::NotFound);
        }
        if lock(&self.specs).values().any(|spec| spec.image == image || resolved.contains(&spec.image)) {
            return Err(RuntimeError::InUse);
        }
        images.retain(|reference, resolved| reference != image && resolved != image);
        lock(&self.removed_images).push(image.to_string());
        Ok(())
    }

    async fn create(&self, spec: &ContainerSpec) -> Result<String, RuntimeError> {
        self.check()?;
        let mut specs = lock(&self.specs);
        if specs.values().any(|existing| existing.name == spec.name) {
            return Err(RuntimeError::Conflict);
        }
        let id = format!("mem-{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        if let Some(network) = &spec.network {
            lock(&self.networks).insert(network.name.clone());
        }
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

    async fn run_once(&self, spec: &OneOffSpec, output: &(dyn Fn(LogLine) + Send + Sync)) -> Result<OneOffExit, RuntimeError> {
        self.check()?;
        lock(&self.one_offs).push(spec.clone());
        let (lines, exit) = lock(&self.one_off_outcomes).pop_front().unwrap_or((Vec::new(), OneOffExit::Exited(0)));
        for text in lines {
            output(LogLine { at: Some(Timestamp::now()), stderr: false, text });
        }
        Ok(exit)
    }

    async fn remove_one_off(&self, name: &str) -> Result<(), RuntimeError> {
        self.check()?;
        lock(&self.removed_one_offs).push(name.to_string());
        Ok(())
    }

    async fn prune_build_cache(&self, unused_for: Duration) -> Result<u64, RuntimeError> {
        self.check()?;
        lock(&self.build_cache_prunes).push(unused_for);
        Ok(0)
    }

    async fn remove_unused_networks(&self) -> Result<Vec<String>, RuntimeError> {
        self.check()?;
        let specs = lock(&self.specs);
        let used: std::collections::HashSet<&str> =
            specs.values().filter_map(|spec| spec.network.as_ref()).map(|network| network.name.as_str()).collect();
        let mut networks = lock(&self.networks);
        let unused: Vec<String> = networks.iter().filter(|name| !used.contains(name.as_str())).cloned().collect();
        networks.retain(|name| used.contains(name.as_str()));
        Ok(unused)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
