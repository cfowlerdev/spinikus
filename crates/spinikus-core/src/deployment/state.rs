//! The deployment state machine.
//!
//! A deployment moves `queued → preparing → releasing → starting → verifying → live`,
//! and later `live → draining → superseded` when a newer deployment replaces it.
//! Before going live it can end early as `failed` or `canceled`, always with a reason.
//! Only `live` deployments receive traffic, so a deployment that ends early never does.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::text::impl_text;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum DeploymentStatus {
    /// Created, waiting for its turn.
    Queued,
    /// The agent clones and builds, or pulls the image.
    Preparing,
    /// The release command (such as migrations) runs once in a one-off container.
    Releasing,
    /// New containers start from the snapshot config.
    Starting,
    /// The health check must pass before routes switch.
    Verifying,
    /// Routes have switched; this is the service's current deployment.
    Live,
    /// Replaced; finishing in-flight requests before its containers stop.
    Draining,
    /// Replaced by a newer live deployment.
    Superseded,
    /// Ended before going live, with a failure reason.
    Failed,
    /// Ended before going live, with a cancel reason.
    Canceled,
}

/// Why a deployment ended as `failed` or `canceled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    BuildFailed,
    ReleaseFailed,
    StartFailed,
    HealthCheckFailed,
    /// A phase ran past its timeout.
    Timeout,
    /// The agent running the deployment was disconnected past the agent-lost timeout.
    AgentLost,
    CanceledByUser,
    /// A newer deployment of the same service was queued before this one started.
    SupersededInQueue,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TransitionError {
    #[error("a deployment cannot move from `{from}` to `{to}`")]
    NotAllowed { from: DeploymentStatus, to: DeploymentStatus },
    #[error("moving to `{to}` requires a reason")]
    MissingReason { to: DeploymentStatus },
    #[error("moving to `{to}` does not take a reason")]
    UnexpectedReason { to: DeploymentStatus },
    #[error("reason `{reason}` cannot end a deployment that is `{from}` as `{to}`")]
    WrongReason { from: DeploymentStatus, to: DeploymentStatus, reason: EndReason },
}

/// A checked change of status, ready to be recorded as a deployment event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    pub from: DeploymentStatus,
    pub to: DeploymentStatus,
    pub reason: Option<EndReason>,
}

impl DeploymentStatus {
    pub const ALL: [DeploymentStatus; 10] = [
        Self::Queued,
        Self::Preparing,
        Self::Releasing,
        Self::Starting,
        Self::Verifying,
        Self::Live,
        Self::Draining,
        Self::Superseded,
        Self::Failed,
        Self::Canceled,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Preparing => "preparing",
            Self::Releasing => "releasing",
            Self::Starting => "starting",
            Self::Verifying => "verifying",
            Self::Live => "live",
            Self::Draining => "draining",
            Self::Superseded => "superseded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }

    /// No further transitions are possible.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Superseded | Self::Failed | Self::Canceled)
    }

    /// Still on its way to going live, so it can fail or be canceled.
    pub fn is_in_progress(self) -> bool {
        matches!(self, Self::Queued | Self::Preparing | Self::Releasing | Self::Starting | Self::Verifying)
    }

    /// Checks that a deployment in this status may move to `to`, for `reason`.
    ///
    /// `failed` and `canceled` require a reason that fits the current status; every
    /// other target takes none. Two forward skips are allowed: `preparing → starting`
    /// when there is no release command, and `queued → starting` for a rollback,
    /// which reuses an existing image and skips the release command by default.
    pub fn transition(self, to: DeploymentStatus, reason: Option<EndReason>) -> Result<Transition, TransitionError> {
        use DeploymentStatus::*;

        let allowed = match (self, to) {
            (Queued, Preparing | Starting) => true,
            (Preparing, Releasing | Starting) => true,
            (Releasing, Starting) => true,
            (Starting, Verifying) => true,
            (Verifying, Live) => true,
            (Live, Draining) => true,
            (Draining, Superseded) => true,
            (from, Failed | Canceled) => from.is_in_progress(),
            _ => false,
        };
        if !allowed {
            return Err(TransitionError::NotAllowed { from: self, to });
        }

        match (to, reason) {
            (Failed | Canceled, None) => Err(TransitionError::MissingReason { to }),
            (Failed | Canceled, Some(reason)) if !reason.can_end(self, to) => {
                Err(TransitionError::WrongReason { from: self, to, reason })
            }
            (Failed | Canceled, Some(_)) => Ok(Transition { from: self, to, reason }),
            (_, Some(_)) => Err(TransitionError::UnexpectedReason { to }),
            (_, None) => Ok(Transition { from: self, to, reason: None }),
        }
    }
}

impl EndReason {
    pub const ALL: [EndReason; 8] = [
        Self::BuildFailed,
        Self::ReleaseFailed,
        Self::StartFailed,
        Self::HealthCheckFailed,
        Self::Timeout,
        Self::AgentLost,
        Self::CanceledByUser,
        Self::SupersededInQueue,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::BuildFailed => "build_failed",
            Self::ReleaseFailed => "release_failed",
            Self::StartFailed => "start_failed",
            Self::HealthCheckFailed => "health_check_failed",
            Self::Timeout => "timeout",
            Self::AgentLost => "agent_lost",
            Self::CanceledByUser => "canceled_by_user",
            Self::SupersededInQueue => "superseded_in_queue",
        }
    }

    /// Whether this reason can end a deployment that is `from` with status `to`.
    fn can_end(self, from: DeploymentStatus, to: DeploymentStatus) -> bool {
        use DeploymentStatus::*;

        match self {
            Self::BuildFailed => from == Preparing && to == Failed,
            Self::ReleaseFailed => from == Releasing && to == Failed,
            Self::StartFailed => from == Starting && to == Failed,
            Self::HealthCheckFailed => from == Verifying && to == Failed,
            Self::Timeout | Self::AgentLost => from != Queued && to == Failed,
            Self::CanceledByUser => to == Canceled,
            Self::SupersededInQueue => from == Queued && to == Canceled,
        }
    }
}

impl_text!(DeploymentStatus, "deployment status");
impl_text!(EndReason, "end reason");

#[cfg(test)]
mod tests {
    use super::{DeploymentStatus::*, EndReason::*, *};

    /// Every allowed (from, to, reason) combination, written out independently of the implementation.
    const ALLOWED: &[(DeploymentStatus, DeploymentStatus, Option<EndReason>)] = &[
        (Queued, Preparing, None),
        (Queued, Starting, None),
        (Queued, Canceled, Some(CanceledByUser)),
        (Queued, Canceled, Some(SupersededInQueue)),
        (Preparing, Releasing, None),
        (Preparing, Starting, None),
        (Preparing, Failed, Some(BuildFailed)),
        (Preparing, Failed, Some(Timeout)),
        (Preparing, Failed, Some(AgentLost)),
        (Preparing, Canceled, Some(CanceledByUser)),
        (Releasing, Starting, None),
        (Releasing, Failed, Some(ReleaseFailed)),
        (Releasing, Failed, Some(Timeout)),
        (Releasing, Failed, Some(AgentLost)),
        (Releasing, Canceled, Some(CanceledByUser)),
        (Starting, Verifying, None),
        (Starting, Failed, Some(StartFailed)),
        (Starting, Failed, Some(Timeout)),
        (Starting, Failed, Some(AgentLost)),
        (Starting, Canceled, Some(CanceledByUser)),
        (Verifying, Live, None),
        (Verifying, Failed, Some(HealthCheckFailed)),
        (Verifying, Failed, Some(Timeout)),
        (Verifying, Failed, Some(AgentLost)),
        (Verifying, Canceled, Some(CanceledByUser)),
        (Live, Draining, None),
        (Draining, Superseded, None),
    ];

    #[test]
    fn allows_exactly_the_listed_transitions() {
        let reasons = std::iter::once(None).chain(EndReason::ALL.into_iter().map(Some));

        for from in DeploymentStatus::ALL {
            for to in DeploymentStatus::ALL {
                for reason in reasons.clone() {
                    let expected = ALLOWED.contains(&(from, to, reason));
                    let result = from.transition(to, reason);

                    assert_eq!(result.is_ok(), expected, "{from} -> {to} with {reason:?}: {result:?}");
                }
            }
        }
    }

    #[test]
    fn terminal_statuses_have_no_way_out() {
        for status in DeploymentStatus::ALL.into_iter().filter(|status| status.is_terminal()) {
            assert!(ALLOWED.iter().all(|(from, ..)| *from != status), "{status} has a transition");
        }
    }

    #[test]
    fn live_cannot_be_failed_or_canceled() {
        assert_eq!(
            Live.transition(Canceled, Some(CanceledByUser)),
            Err(TransitionError::NotAllowed { from: Live, to: Canceled })
        );
        assert_eq!(Live.transition(Failed, Some(AgentLost)), Err(TransitionError::NotAllowed { from: Live, to: Failed }));
    }

    #[test]
    fn reports_reason_errors() {
        assert_eq!(Preparing.transition(Failed, None), Err(TransitionError::MissingReason { to: Failed }));
        assert_eq!(Preparing.transition(Releasing, Some(Timeout)), Err(TransitionError::UnexpectedReason { to: Releasing }));
        assert_eq!(
            Verifying.transition(Failed, Some(BuildFailed)),
            Err(TransitionError::WrongReason { from: Verifying, to: Failed, reason: BuildFailed })
        );
    }

    #[test]
    fn text_round_trips() {
        for status in DeploymentStatus::ALL {
            assert_eq!(status.to_string().parse(), Ok(status));
            assert_eq!(serde_json::to_string(&status).unwrap(), format!("\"{status}\""));
        }
        for reason in EndReason::ALL {
            assert_eq!(reason.to_string().parse(), Ok(reason));
            assert_eq!(serde_json::to_string(&reason).unwrap(), format!("\"{reason}\""));
        }
        assert!("building".parse::<DeploymentStatus>().is_err());
    }
}
