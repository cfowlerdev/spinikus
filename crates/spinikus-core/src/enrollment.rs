//! Enrolling a server's agent: single-use join tokens, exchanged with a certificate signing
//! request for the agent's client certificate.

use jiff::{SignedDuration, Timestamp};
use serde::Serialize;

use crate::id::{EnrollmentTokenId, ServerId};

/// Join tokens start with this, so they are recognizable (and scannable) in logs and scripts.
pub const ENROLLMENT_TOKEN_PREFIX: &str = "spj_";

/// How long a join token can be used.
pub const ENROLLMENT_TOKEN_LIFETIME: SignedDuration = SignedDuration::from_hours(1);

/// How long an agent's client certificate is valid. Agents renew at about two thirds of it.
pub const AGENT_CERTIFICATE_LIFETIME: SignedDuration = SignedDuration::from_hours(30 * 24);

/// The name in the agent listener's certificate. Agents connect with this as the TLS server
/// name, whatever address they dial, and trust only the control plane's CA, so the name
/// needs no DNS and the listener's address can change.
pub const CONTROL_PLANE_SERVER_NAME: &str = "control-plane.spinikus.internal";

/// A join token for one server. The token itself is shown once; only its hash is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct EnrollmentToken {
    pub id: EnrollmentTokenId,
    pub server_id: ServerId,
    pub expires_at: Timestamp,
    /// Set when an agent enrolled with it; a token works once.
    pub used_at: Option<Timestamp>,
    pub created_at: Timestamp,
}
