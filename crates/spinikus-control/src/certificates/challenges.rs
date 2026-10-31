//! HTTP-01 challenges, served by every connected agent's proxy for any host, since the CA
//! may reach whichever server the hostname resolves to. The set is held in memory and sent
//! whole on each change, and again to agents that connect.

use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard},
};

use async_trait::async_trait;
use jiff::Timestamp;
use spinikus_core::id::ServerId;
use spinikus_proto::agent::v1::{self as pb, control_message};
use tokio::sync::watch;

use super::acme::ChallengeResponder;
use crate::Control;

#[derive(Default)]
pub(crate) struct Challenges {
    current: Mutex<Current>,
    /// The revision each agent last applied.
    applied: watch::Sender<HashMap<ServerId, u64>>,
}

#[derive(Default)]
struct Current {
    revision: u64,
    /// Key authorizations by token.
    challenges: HashMap<String, String>,
}

impl Challenges {
    fn current(&self) -> MutexGuard<'_, Current> {
        self.current.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Changes the set, returning the message that carries it.
    fn change(&self, change: impl FnOnce(&mut HashMap<String, String>)) -> (u64, pb::ControlMessage) {
        let mut current = self.current();
        change(&mut current.challenges);
        // Like snapshot revisions: increasing, also across restarts.
        current.revision = (current.revision + 1).max(Timestamp::now().as_millisecond() as u64);
        (current.revision, message(&current))
    }
}

fn message(current: &Current) -> pb::ControlMessage {
    let challenges = current
        .challenges
        .iter()
        .map(|(token, key_authorization)| pb::AcmeChallenge {
            token: token.clone(),
            key_authorization: key_authorization.clone(),
        })
        .collect();
    let set = pb::AcmeChallenges { revision: current.revision, challenges };
    pb::ControlMessage { message: Some(control_message::Message::AcmeChallenges(set)) }
}

impl Control {
    fn challenges(&self) -> &Challenges {
        &self.certificates().challenges
    }

    async fn send_challenges(&self, message: pb::ControlMessage) -> Vec<ServerId> {
        let mut sent = Vec::new();
        for server in self.agents().connected_servers() {
            if self.agents().send(server, message.clone()).await.is_ok() {
                sent.push(server);
            }
        }
        sent
    }

    /// Sends a connecting agent the current challenges, if there are any.
    pub(crate) async fn resend_challenges(&self, server: ServerId) {
        let message = {
            let current = self.challenges().current();
            if current.challenges.is_empty() {
                return;
            }
            message(&current)
        };
        let _ = self.agents().send(server, message).await;
    }

    /// The agent's proxy serves the challenges with `revision`.
    pub(crate) fn challenges_applied(&self, server: ServerId, revision: u64) {
        self.challenges().applied.send_modify(|applied| {
            applied.insert(server, revision);
        });
    }
}

/// Publishes challenges through the connected agents.
pub(super) struct AgentResponder<'a>(pub(super) &'a Control);

#[async_trait]
impl ChallengeResponder for AgentResponder<'_> {
    async fn publish(&self, token: &str, key_authorization: &str) {
        let control = self.0;
        let challenges = control.challenges();
        let (revision, message) = challenges.change(|challenges| {
            challenges.insert(token.to_string(), key_authorization.to_string());
        });
        let mut applied = challenges.applied.subscribe();
        let servers = control.send_challenges(message).await;
        // Agents that disconnect meanwhile are not waited for.
        let served = applied.wait_for(|applied| {
            servers.iter().all(|server| {
                applied.get(server).is_some_and(|applied| *applied >= revision) || !control.agents().is_connected(*server)
            })
        });
        if tokio::time::timeout(control.certificates().timings.challenge_wait, served).await.is_err() {
            tracing::warn!("not every agent confirmed it serves the ACME challenge in time; asking the CA anyway");
        }
    }

    async fn withdraw(&self, token: &str) {
        let (_, message) = self.0.challenges().change(|challenges| {
            challenges.remove(token);
        });
        self.0.send_challenges(message).await;
    }
}
