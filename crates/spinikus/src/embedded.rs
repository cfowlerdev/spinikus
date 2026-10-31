//! The embedded agent: on a single-server install, the control plane's own machine runs an
//! agent inside the control plane's process. It is not a shortcut: it enrolls with a join
//! token and connects over loopback mutual TLS, through the same calls as any remote agent.

use std::{future::Future, net::SocketAddr};

use anyhow::Context;
use spinikus_agent::{AgentError, AgentFiles, Backoff, Credentials, RunOptions};
use spinikus_control::{Control, ca};

/// Enrolls the embedded agent if it has not been (or its files belong to another server or
/// control plane), then runs it until `shutdown`. Does nothing if the embedded server was
/// revoked: an admin turned it off.
pub async fn run(
    control: Control,
    files: AgentFiles,
    agent_address: SocketAddr,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    let facts = spinikus_agent::server_facts();
    let server = control.embedded_server(&facts.hostname).await.context("Failed to look up the embedded server")?;
    if server.revoked_at.is_some() {
        tracing::warn!(server = %server.id, "The embedded agent's server is revoked, so the embedded agent does not run");
        return Ok(());
    }
    let url = format!("https://{}", loopback(agent_address));
    let ca_fingerprint = control.certificate_authority().await?.fingerprint();

    let current = match files.load() {
        Ok(credentials) if credentials.server_id == server.id && fingerprint(&credentials) == Some(ca_fingerprint.clone()) => {
            Some(credentials)
        }
        Ok(_) => {
            tracing::warn!("The embedded agent's files are for another server or control plane; enrolling it again");
            None
        }
        Err(AgentError::NotEnrolled) => None,
        Err(error) => {
            tracing::warn!(%error, "The embedded agent's files are unreadable; enrolling it again");
            None
        }
    };
    let credentials = match current {
        Some(credentials) => credentials,
        None => {
            let join = control.create_embedded_join_token(server.id).await?;
            let credentials = spinikus_agent::join(&url, &join.secret, &join.ca_fingerprint, facts.clone())
                .await
                .context("Failed to enroll the embedded agent")?;
            tracing::info!(server = %server.id, "Enrolled the embedded agent");
            credentials
        }
    };
    // The agent listener's port may have changed since the agent enrolled.
    files.save(&Credentials { control_plane: url, ..credentials }).context("Failed to save the embedded agent's files")?;

    let options = RunOptions { facts, backoff: Backoff::default() };
    spinikus_agent::run(&files, options, shutdown).await.context("The embedded agent stopped")
}

/// The address to reach the agent listener from this machine.
fn loopback(address: SocketAddr) -> SocketAddr {
    let mut address = address;
    if address.ip().is_unspecified() {
        address.set_ip(if address.is_ipv4() { [127, 0, 0, 1].into() } else { std::net::Ipv6Addr::LOCALHOST.into() });
    }
    address
}

fn fingerprint(credentials: &Credentials) -> Option<String> {
    ca::certificate_der(&credentials.ca_pem).map(|der| ca::fingerprint(&der))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dials_loopback_when_listening_everywhere() {
        let any_v4: SocketAddr = "0.0.0.0:8443".parse().unwrap();
        let any_v6: SocketAddr = "[::]:8443".parse().unwrap();
        let specific: SocketAddr = "10.0.0.5:8443".parse().unwrap();

        assert_eq!(loopback(any_v4).to_string(), "127.0.0.1:8443");
        assert_eq!(loopback(any_v6).to_string(), "[::1]:8443");
        assert_eq!(loopback(specific), specific);
    }
}
