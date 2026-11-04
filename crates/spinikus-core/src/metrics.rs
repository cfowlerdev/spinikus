//! Resource metrics: CPU, memory, network and disk, per service (the sum of its containers)
//! and per server. Agents send one point per container and per server each minute (the mean
//! of their samples in it); the control plane keeps those for a day and hourly means for
//! [`HOURS_KEPT_FOR`], and graphs them in buckets that keep a few hundred points per range.

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use crate::text::impl_text;

/// How long minute points are kept.
pub const MINUTES_KEPT_FOR: SignedDuration = SignedDuration::from_hours(24);
/// How long hourly points are kept.
pub const HOURS_KEPT_FOR: SignedDuration = SignedDuration::from_hours(30 * 24);

/// A server whose disk is at least this full is low on disk…
pub const DISK_LOW_PERCENT: f64 = 90.0;
/// …until it is below this again, so a disk hovering at the line isn't reported each minute.
pub const DISK_RECOVERED_PERCENT: f64 = 85.0;

/// One period's resource use. `at` is the period's end. Fields are `None` where nothing was
/// measured: network and disk for containers that share the host's, or a server whose
/// system the agent can't read.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MetricPoint {
    pub at: Timestamp,
    /// CPU cores in use: 0.5 is half of one core.
    pub cpu_cores: Option<f64>,
    pub memory_bytes: Option<u64>,
    /// A container's memory limit, or the server's memory.
    pub memory_limit_bytes: Option<u64>,
    /// Bytes received per second.
    pub network_rx_rate: Option<f64>,
    /// Bytes sent per second.
    pub network_tx_rate: Option<f64>,
    /// Servers only: the disk Docker keeps images, containers and volumes on.
    pub disk_used_bytes: Option<u64>,
    pub disk_total_bytes: Option<u64>,
}

impl MetricPoint {
    /// How full the disk is, in percent, when known.
    pub fn disk_percent(&self) -> Option<f64> {
        match (self.disk_used_bytes, self.disk_total_bytes) {
            (Some(used), Some(total)) if total > 0 => Some(used as f64 * 100.0 / total as f64),
            _ => None,
        }
    }
}

/// Which points a range is drawn from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    Minute,
    Hour,
}

impl Resolution {
    pub const ALL: [Resolution; 2] = [Self::Minute, Self::Hour];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Minute => "minute",
            Self::Hour => "hour",
        }
    }
}

impl_text!(Resolution, "resolution");

/// How far back a graph goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum MetricRange {
    #[default]
    #[serde(rename = "1h")]
    Hour,
    #[serde(rename = "6h")]
    SixHours,
    #[serde(rename = "24h")]
    Day,
    #[serde(rename = "7d")]
    Week,
    #[serde(rename = "30d")]
    Month,
}

impl MetricRange {
    pub const ALL: [MetricRange; 5] = [Self::Hour, Self::SixHours, Self::Day, Self::Week, Self::Month];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hour => "1h",
            Self::SixHours => "6h",
            Self::Day => "24h",
            Self::Week => "7d",
            Self::Month => "30d",
        }
    }

    pub fn duration(self) -> SignedDuration {
        SignedDuration::from_hours(match self {
            Self::Hour => 1,
            Self::SixHours => 6,
            Self::Day => 24,
            Self::Week => 7 * 24,
            Self::Month => 30 * 24,
        })
    }

    /// The points it reads.
    pub fn resolution(self) -> Resolution {
        match self {
            Self::Hour | Self::SixHours | Self::Day => Resolution::Minute,
            Self::Week | Self::Month => Resolution::Hour,
        }
    }

    /// Each point it shows covers this long: a few hundred points at most.
    pub fn step(self) -> SignedDuration {
        match self {
            Self::Hour => SignedDuration::from_mins(1),
            Self::SixHours => SignedDuration::from_mins(2),
            Self::Day => SignedDuration::from_mins(5),
            Self::Week => SignedDuration::from_hours(1),
            Self::Month => SignedDuration::from_hours(3),
        }
    }
}

impl_text!(MetricRange, "range");

/// A graph's points over a range.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Metrics {
    pub range: MetricRange,
    /// How long each point covers, in seconds.
    pub step_seconds: i64,
    /// From `since` until `until`, oldest first; periods with nothing measured are left out.
    pub points: Vec<MetricPoint>,
    pub since: Timestamp,
    pub until: Timestamp,
}

/// The end of the step-long period that `at` falls in (an end already on the boundary is
/// its own), counted from the Unix epoch.
pub fn period_end(at: Timestamp, step: SignedDuration) -> Timestamp {
    let step = step.as_millis().max(1) as i64;
    let millis = at.as_millisecond();
    let end = millis.div_euclid(step) * step + if millis.rem_euclid(step) == 0 { 0 } else { step };
    Timestamp::from_millisecond(end).unwrap_or(at)
}

/// Means of the points in each `step`-long period, labeled with the period's end. Points
/// must be oldest first. A field is the mean of the points that have it.
pub fn bucket(points: &[MetricPoint], step: SignedDuration) -> Vec<MetricPoint> {
    let mut buckets: Vec<(MetricPoint, Sums)> = Vec::new();
    for point in points {
        let end = period_end(point.at, step);
        match buckets.last_mut() {
            Some((bucket, sums)) if bucket.at == end => sums.add(point),
            _ => {
                let mut sums = Sums::default();
                sums.add(point);
                buckets.push((MetricPoint { at: end, ..MetricPoint::default() }, sums));
            }
        }
    }
    buckets.into_iter().map(|(bucket, sums)| sums.mean(bucket.at)).collect()
}

/// Sums of each field over the points that have it, for means.
#[derive(Default)]
struct Sums {
    cpu: Mean,
    memory: Mean,
    memory_limit: Mean,
    rx: Mean,
    tx: Mean,
    disk_used: Mean,
    disk_total: Mean,
}

#[derive(Default)]
struct Mean {
    sum: f64,
    count: u32,
}

impl Mean {
    fn add(&mut self, value: Option<f64>) {
        if let Some(value) = value {
            self.sum += value;
            self.count += 1;
        }
    }

    fn get(&self) -> Option<f64> {
        (self.count > 0).then(|| self.sum / f64::from(self.count))
    }

    fn bytes(&self) -> Option<u64> {
        self.get().map(|mean| mean.round() as u64)
    }
}

impl Sums {
    fn add(&mut self, point: &MetricPoint) {
        let bytes = |value: Option<u64>| value.map(|value| value as f64);
        self.cpu.add(point.cpu_cores);
        self.memory.add(bytes(point.memory_bytes));
        self.memory_limit.add(bytes(point.memory_limit_bytes));
        self.rx.add(point.network_rx_rate);
        self.tx.add(point.network_tx_rate);
        self.disk_used.add(bytes(point.disk_used_bytes));
        self.disk_total.add(bytes(point.disk_total_bytes));
    }

    fn mean(&self, at: Timestamp) -> MetricPoint {
        MetricPoint {
            at,
            cpu_cores: self.cpu.get(),
            memory_bytes: self.memory.bytes(),
            memory_limit_bytes: self.memory_limit.bytes(),
            network_rx_rate: self.rx.get(),
            network_tx_rate: self.tx.get(),
            disk_used_bytes: self.disk_used.bytes(),
            disk_total_bytes: self.disk_total.bytes(),
        }
    }
}

/// Whether a server's disk is low, given whether it was: at [`DISK_LOW_PERCENT`] it
/// becomes low, and stays low until under [`DISK_RECOVERED_PERCENT`].
pub fn disk_low(percent: f64, was_low: bool) -> bool {
    match was_low {
        true => percent >= DISK_RECOVERED_PERCENT,
        false => percent >= DISK_LOW_PERCENT,
    }
}

/// A byte count for people: `512 B`, `1.5 KiB`, `20 GiB`.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{bytes} B"),
        _ if value < 10.0 => format!("{value:.1} {}", UNITS[unit]),
        _ => format!("{value:.0} {}", UNITS[unit]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn cpu(time: &str, cores: f64) -> MetricPoint {
        MetricPoint { at: at(time), cpu_cores: Some(cores), ..MetricPoint::default() }
    }

    #[test]
    fn periods_end_on_the_step() {
        let five = SignedDuration::from_mins(5);
        assert_eq!(period_end(at("2026-11-04T10:00:00Z"), five), at("2026-11-04T10:00:00Z"));
        assert_eq!(period_end(at("2026-11-04T10:00:00.001Z"), five), at("2026-11-04T10:05:00Z"));
        assert_eq!(period_end(at("2026-11-04T10:04:59Z"), five), at("2026-11-04T10:05:00Z"));
    }

    #[test]
    fn buckets_are_means_of_what_was_measured() {
        let memory = MetricPoint { at: at("2026-11-04T10:02:00Z"), memory_bytes: Some(100), ..MetricPoint::default() };
        let points = [
            cpu("2026-11-04T10:01:00Z", 1.0),
            memory,
            cpu("2026-11-04T10:03:00Z", 2.0),
            cpu("2026-11-04T10:10:00Z", 4.0),
            cpu("2026-11-04T10:21:00Z", 0.5),
        ];
        let buckets = bucket(&points, SignedDuration::from_mins(5));

        let summary: Vec<_> = buckets.iter().map(|point| (point.at, point.cpu_cores, point.memory_bytes)).collect();
        assert_eq!(
            summary,
            [
                (at("2026-11-04T10:05:00Z"), Some(1.5), Some(100)),
                (at("2026-11-04T10:10:00Z"), Some(4.0), None),
                (at("2026-11-04T10:25:00Z"), Some(0.5), None),
            ]
        );
    }

    #[test]
    fn bytes_read_well() {
        let sizes: Vec<_> = [0, 1023, 1536, 20 * 1024 * 1024 * 1024].into_iter().map(format_bytes).collect();
        assert_eq!(sizes, ["0 B", "1023 B", "1.5 KiB", "20 GiB"]);
    }

    #[test]
    fn a_low_disk_stays_low_until_it_has_room_again() {
        assert!(!disk_low(89.9, false));
        assert!(disk_low(90.0, false));
        assert!(disk_low(86.0, true));
        assert!(!disk_low(84.9, true));
    }

    #[test]
    fn ranges_read_from_the_points_kept_long_enough() {
        for range in MetricRange::ALL {
            let kept = match range.resolution() {
                Resolution::Minute => MINUTES_KEPT_FOR,
                Resolution::Hour => HOURS_KEPT_FOR,
            };
            assert!(range.duration() <= kept, "{range}");
            assert!(range.duration().as_secs() / range.step().as_secs() <= 300, "{range}");
        }
        assert_eq!("24h".parse::<MetricRange>().unwrap(), MetricRange::Day);
    }
}
