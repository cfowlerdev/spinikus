//! Crash loops: a live deployment whose container keeps restarting, or isn't running.
//!
//! Docker restarts a crashing container itself (the `unless-stopped` policy, with its own
//! backoff), so the control plane only notices: it watches each live deployment's container
//! as agents report it, with a [`Watch`] per deployment, and records a [`CrashLoop`] from
//! when the watch says the container is crashing until it says it has recovered.

use std::{collections::VecDeque, time::Duration};

use jiff::Timestamp;
use serde::Serialize;

use crate::{
    id::{CrashLoopId, DeploymentId, ServerId, ServiceId},
    server::InstanceStatus,
    text::impl_text,
};

/// A time a live deployment's container was crashing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CrashLoop {
    pub id: CrashLoopId,
    pub service_id: ServiceId,
    pub deployment_id: DeploymentId,
    pub server_id: ServerId,
    /// The latest reason; it can change while the loop lasts.
    pub reason: CrashReason,
    /// How often the container was restarted from just before the loop began to the last
    /// report.
    pub restarts: u32,
    pub started_at: Timestamp,
    pub updated_at: Timestamp,
    /// When it recovered, or stopped mattering (another deployment went live, or the service
    /// was stopped); `None` while it lasts.
    pub ended_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum CrashReason {
    /// The container was restarted again and again.
    Restarting,
    /// The container is not running, or not reported at all.
    NotRunning,
}

impl CrashReason {
    pub const ALL: [CrashReason; 2] = [Self::Restarting, Self::NotRunning];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Restarting => "restarting",
            Self::NotRunning => "not_running",
        }
    }
}

impl_text!(CrashReason, "crash reason");

/// When a container counts as crashing, and as recovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrashRules {
    /// Restarts within [`CrashRules::window`] that make a crash loop.
    pub restarts: u32,
    pub window: Duration,
    /// How long a container may be down before it counts as crashing, such as during a
    /// restart.
    pub not_running_after: Duration,
    /// How long a container must run without restarting to have recovered.
    pub recovered_after: Duration,
}

impl Default for CrashRules {
    fn default() -> Self {
        Self {
            restarts: 3,
            window: Duration::from_secs(10 * 60),
            not_running_after: Duration::from_secs(60),
            recovered_after: Duration::from_secs(10 * 60),
        }
    }
}

/// What a [`Watch`] makes of the container now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Assessment {
    Crashing {
        reason: CrashReason,
        /// The restart count before the first restart in the window.
        restarts_before: u32,
    },
    /// Running, with no restart for [`CrashRules::recovered_after`].
    Healthy,
    /// Neither yet: whatever was the case goes on.
    Unsure,
}

/// One live deployment's container, observed over time.
#[derive(Debug, Clone)]
pub struct Watch {
    began: Timestamp,
    /// Restart counts seen, oldest first: one at or before the window's start, then those
    /// within it.
    counts: VecDeque<(Timestamp, u32)>,
    last_restart: Option<Timestamp>,
    down_since: Option<Timestamp>,
}

impl Watch {
    pub fn new(now: Timestamp) -> Self {
        Self { began: now, counts: VecDeque::new(), last_restart: None, down_since: None }
    }

    /// Takes in what the agent last reported, `None` if it reports no container, and says
    /// what that makes of it.
    pub fn observe(&mut self, now: Timestamp, seen: Option<(InstanceStatus, u32)>, rules: &CrashRules) -> Assessment {
        let running = matches!(seen, Some((InstanceStatus::Running | InstanceStatus::Paused, _)));
        if running {
            self.down_since = None;
        } else {
            self.down_since.get_or_insert(now);
        }
        if let Some((_, count)) = seen {
            match self.counts.back() {
                // A count going down is another container: start over.
                Some(&(_, last)) if count < last => self.counts.clear(),
                Some(&(_, last)) if count > last => self.last_restart = Some(now),
                _ => {}
            }
            self.counts.push_back((now, count));
            let start = now - rules.window;
            while self.counts.len() > 1 && self.counts[1].0 <= start {
                self.counts.pop_front();
            }
        }

        let since = |at: Timestamp| Duration::try_from(now.duration_since(at)).unwrap_or_default();
        let restarts_before = self.counts.front().map_or(0, |&(_, count)| count);
        let latest = self.counts.back().map_or(0, |&(_, count)| count);
        if latest - restarts_before >= rules.restarts {
            return Assessment::Crashing { reason: CrashReason::Restarting, restarts_before };
        }
        if self.down_since.is_some_and(|down| since(down) >= rules.not_running_after) {
            return Assessment::Crashing { reason: CrashReason::NotRunning, restarts_before };
        }
        if running && since(self.last_restart.unwrap_or(self.began)) >= rules.recovered_after {
            return Assessment::Healthy;
        }
        Assessment::Unsure
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minutes: f64) -> Timestamp {
        Timestamp::UNIX_EPOCH + Duration::from_secs_f64(minutes * 60.0)
    }

    fn running(count: u32) -> Option<(InstanceStatus, u32)> {
        Some((InstanceStatus::Running, count))
    }

    #[test]
    fn restarts_within_the_window_make_a_crash_loop() {
        let rules = CrashRules::default();
        let mut watch = Watch::new(at(0.0));

        let steady = watch.observe(at(0.0), running(7), &rules);
        let two = [1.0, 2.0].map(|minute| watch.observe(at(minute), running(7 + minute as u32), &rules));
        let three = watch.observe(at(3.0), running(10), &rules);

        assert_eq!(steady, Assessment::Unsure, "a count from before the watch is no restart");
        assert_eq!(two, [Assessment::Unsure; 2]);
        assert_eq!(three, Assessment::Crashing { reason: CrashReason::Restarting, restarts_before: 7 });
    }

    #[test]
    fn restarts_spread_beyond_the_window_are_not_a_loop() {
        let rules = CrashRules::default();
        let mut watch = Watch::new(at(0.0));
        watch.observe(at(0.0), running(0), &rules);

        let assessments: Vec<_> = (1..=4).map(|n| watch.observe(at(6.0 * f64::from(n)), running(n), &rules)).collect();

        assert!(assessments.iter().all(|assessment| *assessment == Assessment::Unsure), "{assessments:?}");
    }

    #[test]
    fn a_container_down_too_long_is_crashing() {
        let rules = CrashRules::default();
        let mut watch = Watch::new(at(0.0));

        let restarting = watch.observe(at(0.0), Some((InstanceStatus::Restarting, 0)), &rules);
        let still = watch.observe(at(1.0), Some((InstanceStatus::Exited, 0)), &rules);
        let mut other = Watch::new(at(0.0));
        other.observe(at(0.0), None, &rules);
        let missing = other.observe(at(1.5), None, &rules);

        assert_eq!(restarting, Assessment::Unsure);
        assert_eq!(still, Assessment::Crashing { reason: CrashReason::NotRunning, restarts_before: 0 });
        assert_eq!(missing, Assessment::Crashing { reason: CrashReason::NotRunning, restarts_before: 0 });
    }

    #[test]
    fn recovers_after_running_without_restarts() {
        let rules = CrashRules::default();
        let mut watch = Watch::new(at(0.0));
        for (minute, count) in [(0.0, 0), (1.0, 1), (2.0, 2), (3.0, 3)] {
            watch.observe(at(minute), running(count), &rules);
        }

        let soon = watch.observe(at(12.0), running(3), &rules);
        let later = watch.observe(at(13.0), running(3), &rules);
        let new_container = watch.observe(at(14.0), running(0), &rules);

        assert_eq!(soon, Assessment::Unsure, "the window still holds the restarts, but they are old");
        assert_eq!(later, Assessment::Healthy);
        assert_eq!(new_container, Assessment::Healthy);
    }
}
