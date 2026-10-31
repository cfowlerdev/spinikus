use spinikus_proto::agent::v1 as pb;

/// What the agent tells the control plane about this machine in `Hello`.
pub fn server_facts() -> pb::ServerFacts {
    pb::ServerFacts {
        hostname: gethostname::gethostname().to_string_lossy().into_owned(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        cpu_count: std::thread::available_parallelism().map_or(0, |count| count.get() as u32),
        memory_bytes: memory_bytes().unwrap_or(0),
    }
}

/// Total memory, from `/proc/meminfo` on Linux; unknown elsewhere for now.
fn memory_bytes() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = meminfo.lines().find(|line| line.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}
