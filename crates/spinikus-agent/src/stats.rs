//! Resource metrics: every [`SAMPLE_INTERVAL`] the agent reads each running container's
//! counters from the runtime and the server's from `/proc` and the filesystem Docker keeps
//! its data on, turns successive counters into rates, and at the end of each minute queues
//! one point per container and for the server: the mean of that minute's samples. The
//! connection sends what is queued every [`PUSH_INTERVAL`] over `PushStats`, and puts it
//! back if that fails, keeping at most [`MAX_QUEUED`] points.

use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use jiff::{SignedDuration, Timestamp};
use spinikus_core::{metrics::period_end, server::InstanceStatus};
use spinikus_proto::agent::v1 as pb;

use crate::runtime::{ContainerCounters, ContainerRuntime};

pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(15);
pub const PUSH_INTERVAL: Duration = Duration::from_secs(60);
/// About an hour of points for 50 containers, kept while the control plane is away.
pub const MAX_QUEUED: usize = 3_000;

const MINUTE: SignedDuration = SignedDuration::from_mins(1);

/// Points waiting to be sent, shared by the sampler and the connection.
#[derive(Clone, Default)]
pub struct StatsQueue(Arc<Mutex<VecDeque<pb::StatSample>>>);

impl StatsQueue {
    /// Everything queued, oldest first.
    pub fn take(&self) -> Vec<pb::StatSample> {
        self.lock().drain(..).collect()
    }

    /// Puts points that could not be sent back in front, keeping the newest.
    pub fn put_back(&self, samples: Vec<pb::StatSample>) {
        let mut queue = self.lock();
        for sample in samples.into_iter().rev() {
            queue.push_front(sample);
        }
        trim(&mut queue);
    }

    fn push(&self, samples: impl IntoIterator<Item = pb::StatSample>) {
        let mut queue = self.lock();
        queue.extend(samples);
        trim(&mut queue);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<pb::StatSample>> {
        self.0.lock().expect("not poisoned")
    }
}

fn trim(queue: &mut VecDeque<pb::StatSample>) {
    while queue.len() > MAX_QUEUED {
        queue.pop_front();
    }
}

/// Where the server's own numbers come from: `/proc` (Linux only; elsewhere CPU, memory
/// and network are left out) and the disk holding `disk`.
#[derive(Debug, Clone)]
pub struct HostSource {
    pub proc: PathBuf,
    /// The directory whose filesystem is watched, if known.
    pub disk: Option<PathBuf>,
}

impl HostSource {
    /// This machine: Docker's data directory if it is on this machine, else `fallback`.
    pub async fn of(runtime: &dyn ContainerRuntime, fallback: &Path) -> Self {
        let disk = runtime.data_root().await.filter(|root| root.is_dir()).unwrap_or_else(|| fallback.to_path_buf());
        Self { proc: PathBuf::from("/proc"), disk: Some(disk) }
    }
}

/// Samples until `stopped` resolves.
pub async fn run(runtime: Arc<dyn ContainerRuntime>, host: HostSource, queue: StatsQueue, stopped: impl Future<Output = ()>) {
    let mut stopped = std::pin::pin!(stopped);
    let mut sampler = Sampler::new(runtime, host, queue);
    let mut ticks = tokio::time::interval(SAMPLE_INTERVAL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = stopped.as_mut() => return,
            _ = ticks.tick() => sampler.sample(Timestamp::now()).await,
        }
    }
}

/// Turns counters into rates and samples into minutes.
pub struct Sampler {
    runtime: Arc<dyn ContainerRuntime>,
    host: HostSource,
    queue: StatsQueue,
    /// Each container's counters at its last sample.
    containers: HashMap<String, (Timestamp, ContainerCounters)>,
    server: Option<(Timestamp, HostCounters)>,
    /// The minute being summed, by its end, and each subject's sums ("" for the server).
    minute: Option<Timestamp>,
    sums: HashMap<String, Sums>,
}

impl Sampler {
    pub fn new(runtime: Arc<dyn ContainerRuntime>, host: HostSource, queue: StatsQueue) -> Self {
        Self { runtime, host, queue, containers: HashMap::new(), server: None, minute: None, sums: HashMap::new() }
    }

    /// Takes one sample at `now`, first queueing the minute before if `now` is past it.
    pub async fn sample(&mut self, now: Timestamp) {
        let minute = period_end(now, MINUTE);
        if self.minute.is_some_and(|current| current != minute) {
            self.finish_minute();
        }
        self.minute = Some(minute);

        let running = match self.runtime.list().await {
            Ok(instances) => instances,
            Err(error) => {
                tracing::debug!(%error, "no container stats: the runtime is unavailable");
                Vec::new()
            }
        };
        let mut seen = Vec::new();
        for instance in running.into_iter().filter(|instance| instance.status == InstanceStatus::Running) {
            let id = instance.container_id;
            let counters = match self.runtime.stats(&id).await {
                Ok(Some(counters)) => counters,
                Ok(None) => continue,
                Err(error) => {
                    tracing::debug!(%error, container = id, "no stats for a container");
                    continue;
                }
            };
            let sums = self.sums.entry(id.clone()).or_default();
            sums.memory.add(counters.memory_bytes as f64);
            if let Some((then, before)) = self.containers.get(&id) {
                let seconds = now.duration_since(*then).as_secs_f64();
                // Counters that went down belong to a restarted container: start over.
                if seconds > 0.0 && counters.cpu_nanos >= before.cpu_nanos {
                    sums.cpu.add((counters.cpu_nanos - before.cpu_nanos) as f64 / 1e9 / seconds);
                }
                if seconds > 0.0 && counters.network_rx_bytes >= before.network_rx_bytes {
                    sums.rx.add((counters.network_rx_bytes - before.network_rx_bytes) as f64 / seconds);
                }
                if seconds > 0.0 && counters.network_tx_bytes >= before.network_tx_bytes {
                    sums.tx.add((counters.network_tx_bytes - before.network_tx_bytes) as f64 / seconds);
                }
            }
            self.containers.insert(id.clone(), (now, counters));
            seen.push(id);
        }
        self.containers.retain(|id, _| seen.contains(id));

        self.sample_server(now);
    }

    fn sample_server(&mut self, now: Timestamp) {
        let counters = HostCounters::read(&self.host.proc);
        let sums = self.sums.entry(String::new()).or_default();
        if let Some((used, total)) = counters.memory {
            sums.memory.add(used as f64);
            sums.memory_limit = Some(total);
        }
        if let Some(path) = &self.host.disk
            && let Some(disk) = disk_usage(path)
        {
            sums.disk = Some(disk);
        }
        if let Some((then, before)) = &self.server {
            let seconds = now.duration_since(*then).as_secs_f64();
            if let (Some((busy, total)), Some((busy_before, total_before))) = (counters.cpu, before.cpu)
                && total > total_before
                && busy >= busy_before
            {
                let cpus = std::thread::available_parallelism().map_or(1, |count| count.get()) as f64;
                sums.cpu.add((busy - busy_before) as f64 / (total - total_before) as f64 * cpus);
            }
            if let (Some((rx, tx)), Some((rx_before, tx_before))) = (counters.network, before.network)
                && seconds > 0.0
                && rx >= rx_before
                && tx >= tx_before
            {
                sums.rx.add((rx - rx_before) as f64 / seconds);
                sums.tx.add((tx - tx_before) as f64 / seconds);
            }
        }
        self.server = Some((now, counters));
    }

    /// Queues the current minute's points.
    fn finish_minute(&mut self) {
        let Some(minute) = self.minute else { return };
        let at = Some(std::time::SystemTime::from(minute).into());
        let samples = self.sums.drain().filter(|(_, sums)| !sums.is_empty()).map(|(container_id, sums)| pb::StatSample {
            at,
            container_id,
            cpu_cores: sums.cpu.mean(),
            memory_bytes: sums.memory.mean().map(|mean| mean.round() as u64),
            memory_limit_bytes: sums.memory_limit,
            network_rx_rate: sums.rx.mean(),
            network_tx_rate: sums.tx.mean(),
            disk_used_bytes: sums.disk.map(|(used, _)| used),
            disk_total_bytes: sums.disk.map(|(_, total)| total),
        });
        self.queue.push(samples.collect::<Vec<_>>());
    }
}

/// One subject's samples in a minute.
#[derive(Default)]
struct Sums {
    cpu: Mean,
    memory: Mean,
    rx: Mean,
    tx: Mean,
    memory_limit: Option<u64>,
    disk: Option<(u64, u64)>,
}

impl Sums {
    fn is_empty(&self) -> bool {
        [&self.cpu, &self.memory, &self.rx, &self.tx].iter().all(|mean| mean.count == 0) && self.disk.is_none()
    }
}

#[derive(Default)]
struct Mean {
    sum: f64,
    count: u32,
}

impl Mean {
    fn add(&mut self, value: f64) {
        if value.is_finite() {
            self.sum += value;
            self.count += 1;
        }
    }

    fn mean(&self) -> Option<f64> {
        (self.count > 0).then(|| self.sum / f64::from(self.count))
    }
}

/// The server's counters at one moment; parts it couldn't read are `None`.
#[derive(Debug, Clone, Copy, Default)]
struct HostCounters {
    /// Busy and total CPU time, in clock ticks, over all CPUs.
    cpu: Option<(u64, u64)>,
    /// Memory in use and in all.
    memory: Option<(u64, u64)>,
    /// Bytes received and sent over the physical interfaces.
    network: Option<(u64, u64)>,
}

impl HostCounters {
    fn read(proc: &Path) -> Self {
        let read = |name: &str| std::fs::read_to_string(proc.join(name)).ok();
        Self {
            cpu: read("stat").as_deref().and_then(cpu_times),
            memory: read("meminfo").as_deref().and_then(memory),
            network: read("net/dev").as_deref().and_then(network),
        }
    }
}

/// Busy and total ticks from `/proc/stat`'s `cpu` line: user, nice, system, idle, iowait,
/// irq, softirq and steal (guest time is already in user).
fn cpu_times(stat: &str) -> Option<(u64, u64)> {
    let line = stat.lines().find(|line| line.starts_with("cpu "))?;
    let fields: Vec<u64> = line.split_whitespace().skip(1).take(8).map(|field| field.parse().unwrap_or(0)).collect();
    if fields.len() < 4 {
        return None;
    }
    let total: u64 = fields.iter().sum();
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
    Some((total - idle, total))
}

/// Memory in use (total less available, as `free` counts it) and in all, from `/proc/meminfo`.
fn memory(meminfo: &str) -> Option<(u64, u64)> {
    let field = |name: &str| {
        let line = meminfo.lines().find(|line| line.starts_with(name))?;
        line.split_whitespace().nth(1)?.parse::<u64>().ok().map(|kib| kib * 1024)
    };
    let (total, available) = (field("MemTotal:")?, field("MemAvailable:")?);
    Some((total.saturating_sub(available), total))
}

/// Bytes received and sent from `/proc/net/dev`, over interfaces other than loopback and
/// the virtual ones containers use, which would count container traffic twice.
fn network(dev: &str) -> Option<(u64, u64)> {
    const VIRTUAL: [&str; 6] = ["lo", "docker", "veth", "br-", "virbr", "cni"];
    let mut totals = None;
    for line in dev.lines().skip(2) {
        let Some((name, counters)) = line.split_once(':') else { continue };
        if VIRTUAL.iter().any(|prefix| name.trim().starts_with(prefix)) {
            continue;
        }
        let fields: Vec<u64> = counters.split_whitespace().map(|field| field.parse().unwrap_or(0)).collect();
        if fields.len() < 9 {
            continue;
        }
        let (rx, tx) = totals.get_or_insert((0, 0));
        *rx += fields[0];
        *tx += fields[8];
    }
    totals
}

/// The used and total bytes of the filesystem holding `path`, as `df` counts them: the
/// total leaves out blocks reserved for root, so a full disk is 100%.
fn disk_usage(path: &Path) -> Option<(u64, u64)> {
    let stat = rustix::fs::statvfs(path).ok()?;
    let block = stat.f_frsize;
    let used = stat.f_blocks.saturating_sub(stat.f_bfree) * block;
    let total = used + stat.f_bavail * block;
    (total > 0).then_some((used, total))
}

#[cfg(test)]
mod tests;
