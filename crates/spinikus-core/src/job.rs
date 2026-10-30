//! Durable background jobs (builds, backups, cleanups), run by control plane workers.
//!
//! A worker claims a job with a time-limited lease and extends it while working. If the
//! worker dies, the lease runs out and another worker picks the job up again, so a job
//! can run more than once: handlers must be idempotent. A failed attempt is retried with
//! exponential backoff until the job runs out of attempts.

use std::{fmt, str::FromStr, time::Duration};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{id::JobId, text::impl_text};

/// Job input, stored as a JSON object. Each kind's handler defines its shape.
pub type JobPayload = serde_json::Map<String, serde_json::Value>;

/// Longest allowed job kind.
pub const MAX_KIND_LEN: usize = 64;

/// The delay before the second attempt; it doubles for each attempt after that.
pub const FIRST_RETRY_DELAY: Duration = Duration::from_secs(30);

/// The longest delay between attempts.
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Job {
    pub id: JobId,
    pub kind: JobKind,
    pub payload: JobPayload,
    pub status: JobStatus,
    /// Attempts started so far, including one that is running.
    pub attempts: u32,
    pub max_attempts: u32,
    /// Earliest time the next attempt may start.
    pub run_at: Timestamp,
    /// While another job with the same key is queued or running, enqueueing returns that job instead.
    pub dedupe_key: Option<String>,
    /// The worker holding the lease, while running.
    pub locked_by: Option<String>,
    pub locked_until: Option<Timestamp>,
    /// The error of the last failed attempt.
    pub last_error: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub finished_at: Option<Timestamp>
}

/// What a new job starts from. The ID, status and timestamps are set by the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewJob {
    pub kind: JobKind,
    pub payload: JobPayload,
    /// Run no earlier than this; `None` means now.
    pub run_at: Option<Timestamp>,
    /// At least 1.
    pub max_attempts: u32,
    pub dedupe_key: Option<String>
}

impl NewJob {
    /// A job to run now, with 3 attempts and no dedupe key.
    pub fn new(kind: JobKind, payload: JobPayload) -> Self {
        Self { kind, payload, run_at: None, max_attempts: 3, dedupe_key: None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Waiting for `run_at`, for its first attempt or a retry.
    Queued,
    /// Leased by a worker.
    Running,
    Succeeded,
    /// Out of attempts, or failed with an error that retrying cannot fix.
    Failed,
    Canceled
}

impl JobStatus {
    pub const ALL: [JobStatus; 5] = [Self::Queued, Self::Running, Self::Succeeded, Self::Failed, Self::Canceled];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled"
        }
    }

    pub fn is_finished(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Canceled)
    }
}

impl_text!(JobStatus, "job status");

/// How long to wait before the attempt after `attempt` (counting from 1): 30 seconds,
/// doubling each time, up to an hour.
pub fn retry_delay(attempt: u32) -> Duration {
    let doublings = attempt.saturating_sub(1).min(16);
    FIRST_RETRY_DELAY.saturating_mul(1 << doublings).min(MAX_RETRY_DELAY)
}

/// The handler a job is for: lowercase letters, digits and underscores, in dot-separated
/// parts, for example `deployment.prepare` or `backup.run`. A string rather than an enum,
/// so adding a job kind needs no migration.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobKind(String);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("`{value}` is not a valid job kind: {problem}")]
pub struct JobKindError {
    value: String,
    problem: &'static str
}

impl JobKind {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for JobKind {
    type Err = JobKindError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let problem = if value.is_empty() {
            Some("it is empty")
        } else if value.len() > MAX_KIND_LEN {
            Some("it is longer than 64 characters")
        } else if !value.bytes().all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.')) {
            Some("only lowercase letters, digits, underscores and dots are allowed")
        } else if value.split('.').any(str::is_empty) {
            Some("it cannot start or end with a dot, or contain consecutive dots")
        } else {
            None
        };

        match problem {
            Some(problem) => Err(JobKindError { value: value.to_string(), problem }),
            None => Ok(Self(value.to_string()))
        }
    }
}

impl fmt::Display for JobKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for JobKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for JobKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_kinds() {
        for kind in ["backup", "deployment.prepare", "cleanup.old_images", "v2.task"] {
            assert_eq!(kind.parse::<JobKind>().map(|kind| kind.to_string()), Ok(kind.to_string()));
        }
    }

    #[test]
    fn rejects_invalid_kinds() {
        for kind in ["", "Backup", "backup-run", ".backup", "backup.", "a..b", &"a".repeat(MAX_KIND_LEN + 1)] {
            assert!(kind.parse::<JobKind>().is_err(), "{kind:?} should be rejected");
        }
    }

    #[test]
    fn retry_delay_doubles_up_to_an_hour() {
        let delays: Vec<u64> = (1..=9).map(|attempt| retry_delay(attempt).as_secs()).collect();

        assert_eq!(delays, vec![30, 60, 120, 240, 480, 960, 1920, 3600, 3600]);
        assert_eq!(retry_delay(u32::MAX), MAX_RETRY_DELAY);
        assert_eq!(retry_delay(0), FIRST_RETRY_DELAY);
    }

    #[test]
    fn finished_statuses() {
        let finished: Vec<_> = JobStatus::ALL.into_iter().filter(|status| status.is_finished()).collect();

        assert_eq!(finished, vec![JobStatus::Succeeded, JobStatus::Failed, JobStatus::Canceled]);
    }

    #[test]
    fn text_round_trips() {
        for status in JobStatus::ALL {
            assert_eq!(status.to_string().parse(), Ok(status));
            assert_eq!(serde_json::to_string(&status).unwrap(), format!("\"{status}\""));
        }
    }
}
