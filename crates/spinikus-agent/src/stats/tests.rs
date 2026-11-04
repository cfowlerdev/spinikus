//! The sampler against the in-memory runtime and a made-up `/proc`.

use spinikus_core::{
    id::{DeploymentId, ServiceId},
    server::{InstanceHealth, ObservedInstance},
};

use super::*;
use crate::runtime::MemoryRuntime;

const STAT: &str = "cpu  100 0 100 700 100 0 0 0 0 0\ncpu0 100 0 100 700 100 0 0 0 0 0\n";
const MEMINFO: &str = "MemTotal:        4000 kB\nMemFree:          500 kB\nMemAvailable:    1000 kB\n";
const NET_DEV: &str = "Inter-|   Receive\n face |bytes    packets errs drop fifo frame compressed multicast|bytes\n    \
    lo: 999 1 0 0 0 0 0 0 999 1 0 0 0 0 0 0\n  eth0: 1000 1 0 0 0 0 0 0 2000 1 0 0 0 0 0 0\n\
    veth12: 5000 1 0 0 0 0 0 0 5000 1 0 0 0 0 0 0\n";

fn at(seconds: i64) -> Timestamp {
    "2026-11-04T10:00:00Z".parse::<Timestamp>().unwrap() + SignedDuration::from_secs(seconds)
}

fn write_proc(dir: &Path, stat: &str, net_dev: &str) {
    std::fs::create_dir_all(dir.join("net")).unwrap();
    std::fs::write(dir.join("stat"), stat).unwrap();
    std::fs::write(dir.join("meminfo"), MEMINFO).unwrap();
    std::fs::write(dir.join("net/dev"), net_dev).unwrap();
}

#[test]
fn reads_proc() {
    assert_eq!(cpu_times(STAT), Some((200, 1000)));
    assert_eq!(memory(MEMINFO), Some((3000 * 1024, 4000 * 1024)));
    assert_eq!(network(NET_DEV), Some((1000, 2000)), "loopback and container interfaces left out");
    assert_eq!(cpu_times("intr 1 2 3"), None);
}

#[test]
fn the_queue_keeps_the_newest() {
    let queue = StatsQueue::default();
    let sample = |name: &str| pb::StatSample { container_id: name.to_string(), ..Default::default() };
    queue.push([sample("new")]);
    queue.put_back(vec![sample("old-1"), sample("old-2")]);
    let order: Vec<_> = queue.take().into_iter().map(|sample| sample.container_id).collect();
    assert_eq!(order, ["old-1", "old-2", "new"], "put back in front, in order");

    queue.push((0..MAX_QUEUED).map(|n| sample(&n.to_string())));
    queue.put_back(vec![sample("old")]);
    queue.push([sample("newest")]);
    let taken = queue.take();
    assert_eq!(taken.len(), MAX_QUEUED);
    assert_eq!((taken[0].container_id.as_str(), taken[MAX_QUEUED - 1].container_id.as_str()), ("1", "newest"));
    assert!(queue.take().is_empty());
}

#[tokio::test]
async fn queues_a_mean_per_container_and_for_the_server_each_minute() {
    let dir = tempfile::tempdir().unwrap();
    let proc = dir.path().join("proc");
    write_proc(&proc, STAT, NET_DEV);
    let runtime = Arc::new(MemoryRuntime::new());
    let running = |id: &str, status| ObservedInstance {
        service_id: ServiceId::new(),
        deployment_id: DeploymentId::new(),
        container_id: id.to_string(),
        status,
        health: InstanceHealth::None,
        restart_count: 0,
        started_at: None,
    };
    runtime.put(running("web", InstanceStatus::Running));
    runtime.put(running("stopped", InstanceStatus::Exited));
    let counters = |cpu_millis: u64, memory_bytes, rx| ContainerCounters {
        cpu_nanos: cpu_millis * 1_000_000,
        memory_bytes,
        network_rx_bytes: rx,
        network_tx_bytes: 0,
    };
    let queue = StatsQueue::default();
    let host = HostSource { proc: proc.clone(), disk: Some(dir.path().to_path_buf()) };
    let mut sampler = Sampler::new(runtime.clone(), host, queue.clone());

    runtime.set_stats("web", counters(10_000, 100, 0));
    sampler.sample(at(15)).await;
    // 15 seconds later: 7.5 CPU seconds is half a core; 1500 bytes is 100 a second.
    runtime.set_stats("web", counters(17_500, 300, 1500));
    write_proc(&proc, "cpu  200 0 200 1400 200 0 0 0 0 0\n", NET_DEV.replace("1000 1", "4000 1").as_str());
    sampler.sample(at(30)).await;
    let before_the_minute_ends = queue.take();
    sampler.sample(at(75)).await;
    let mut queued = queue.take();
    queued.sort_by(|a, b| a.container_id.cmp(&b.container_id));

    assert!(before_the_minute_ends.is_empty());
    assert_eq!(queued.len(), 2, "the server and the running container: {queued:?}");
    let (server, web) = (&queued[0], &queued[1]);
    let minute_end = Some(std::time::SystemTime::from(at(60)).into());
    assert_eq!((web.at, web.container_id.as_str()), (minute_end, "web"));
    assert_eq!((web.cpu_cores, web.memory_bytes, web.network_rx_rate), (Some(0.5), Some(200), Some(100.0)));
    assert_eq!((web.memory_limit_bytes, web.disk_used_bytes), (None, None));
    assert_eq!(server.container_id, "");
    // 200 of 1000 more ticks busy, on however many CPUs this machine has.
    let cpus = std::thread::available_parallelism().map_or(1, |count| count.get()) as f64;
    assert_eq!(server.cpu_cores, Some(0.2 * cpus));
    assert_eq!((server.memory_bytes, server.memory_limit_bytes), (Some(3000 * 1024), Some(4000 * 1024)));
    assert_eq!(server.network_rx_rate, Some(200.0));
    assert!(server.disk_total_bytes.unwrap() >= server.disk_used_bytes.unwrap());
}
