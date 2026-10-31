use std::path::PathBuf;

use jiff::Timestamp;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("this server's agent is not enrolled: run `spinikus agent join` first")]
    NotEnrolled,
    #[error("failed to {action} {path}: {source}")]
    File {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is invalid: {problem}")]
    InvalidFile { path: PathBuf, problem: String },
    #[error("invalid control plane URL `{0}`: use https://host:port")]
    InvalidUrl(String),
    #[error("invalid CA fingerprint: expected `sha256:` followed by 64 hex digits")]
    InvalidFingerprint,
    #[error("the control plane did not present a CA with fingerprint {expected}; check the URL and the join command")]
    FingerprintMismatch { expected: String },
    #[error("could not reach the control plane: {0}")]
    Connect(String),
    #[error("the control plane refused: {}", .0.message())]
    Refused(tonic::Status),
    #[error("this server has been revoked in Spinikus; enroll it again with a new join token")]
    Revoked,
    #[error("the agent's certificate expired at {0}; enroll it again with a new join token")]
    CertificateExpired(Timestamp),
    #[error("the control plane does not support this agent: {0}")]
    Unsupported(String),
}
