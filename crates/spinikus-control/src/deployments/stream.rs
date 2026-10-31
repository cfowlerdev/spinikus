//! Following a deployment as it happens: its log so far and then each new line, and its
//! status whenever it changes, until it is live or has finished. The log file is the source
//! of truth, so a follower that reconnects resumes from a byte offset and misses nothing.
//! New lines wake followers at once; status changes are noticed within [`STATUS_INTERVAL`].

use std::{io::SeekFrom, path::Path, time::Duration};

use spinikus_core::{
    deployment::{DeploymentStatus, EndReason},
    id::DeploymentId,
};
use spinikus_store::DeploymentRepository;
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    sync::{broadcast, mpsc},
};
use tokio_stream::wrappers::ReceiverStream;

use crate::{Control, ControlError, Principal};

/// How often a follower checks the deployment's status.
pub const STATUS_INTERVAL: Duration = Duration::from_millis(500);

/// What a follower is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentUpdate {
    /// A line of the log, and the offset just after it, to resume from.
    Log { line: String, offset: u64 },
    /// The deployment's status, sent first and then whenever it changes. A failure comes
    /// with its reason and message.
    Status { status: DeploymentStatus, reason: Option<EndReason>, message: Option<String> },
    /// The deployment is live or has finished, with this status; nothing more will come.
    End(DeploymentStatus),
}

/// Whether a deployment in `status` is done being deployed.
fn finished(status: DeploymentStatus) -> bool {
    !status.is_in_progress()
}

impl Control {
    /// Follows the deployment from byte `offset` of its log (0 for all of it).
    pub async fn follow_deployment(
        &self,
        principal: &Principal,
        id: DeploymentId,
        offset: u64,
    ) -> Result<ReceiverStream<DeploymentUpdate>, ControlError> {
        self.get_deployment(principal, id).await?;
        let (sender, receiver) = mpsc::channel(256);
        let control = self.clone();
        let appended = self.logs.appended.subscribe();
        tokio::spawn(async move {
            if let Err(error) = control.feed(id, offset, appended, &sender).await {
                tracing::warn!(deployment = %id, %error, "stopped following a deployment");
            }
        });
        Ok(ReceiverStream::new(receiver))
    }

    async fn feed(
        &self,
        id: DeploymentId,
        mut offset: u64,
        mut appended: broadcast::Receiver<DeploymentId>,
        sender: &mpsc::Sender<DeploymentUpdate>,
    ) -> Result<(), ControlError> {
        let path = self.logs.path(id);
        let mut last = None;
        loop {
            // The status is read before the log, so a finished deployment's last lines are
            // read below. A new status goes out before the lines, the final one after them.
            let deployment = self.store().get_deployment(id).await?;
            let done = finished(deployment.status);
            let status = match last != Some(deployment.status) {
                true => {
                    last = Some(deployment.status);
                    let message = match deployment.reason {
                        Some(_) => self.store().list_deployment_events(id).await?.pop().and_then(|event| event.message),
                        None => None,
                    };
                    Some(DeploymentUpdate::Status { status: deployment.status, reason: deployment.reason, message })
                }
                false => None,
            };
            let (before, after) = if done { (None, status) } else { (status, None) };
            let lines = read_lines(path.as_deref(), offset).await;
            if let Some((_, end)) = lines.last() {
                offset = *end;
            }
            let lines = lines.into_iter().map(|(line, offset)| DeploymentUpdate::Log { line, offset });
            for update in before.into_iter().chain(lines).chain(after) {
                if sender.send(update).await.is_err() {
                    return Ok(());
                }
            }
            if done {
                let _ = sender.send(DeploymentUpdate::End(deployment.status)).await;
                return Ok(());
            }
            tokio::select! {
                _ = sender.closed() => return Ok(()),
                _ = tokio::time::sleep(STATUS_INTERVAL) => {}
                _ = wait_for(&mut appended, id) => {}
            }
        }
    }
}

/// Waits until `id`'s log grows. A follower that fell behind just reads again.
async fn wait_for(appended: &mut broadcast::Receiver<DeploymentId>, id: DeploymentId) {
    loop {
        match appended.recv().await {
            Ok(grown) if grown != id => continue,
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => return,
            Err(broadcast::error::RecvError::Closed) => return std::future::pending().await,
        }
    }
}

/// The complete lines of the log after byte `from`, each with the offset just after it. An
/// offset inside a line starts at the next one.
async fn read_lines(path: Option<&Path>, from: u64) -> Vec<(String, u64)> {
    let Some(path) = path else {
        return Vec::new();
    };
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return Vec::new();
    };
    // From the byte before, to see whether `from` starts a line.
    let base = from.saturating_sub(1);
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(base)).await.is_err() || file.read_to_end(&mut bytes).await.is_err() {
        return Vec::new();
    }
    let mut start = match from {
        0 => 0,
        _ => match bytes.iter().position(|byte| *byte == b'\n') {
            Some(newline) => newline + 1,
            None => return Vec::new(),
        },
    };
    let mut lines = Vec::new();
    for (at, byte) in bytes.iter().enumerate().skip(start) {
        if *byte == b'\n' {
            let line = String::from_utf8_lossy(&bytes[start..at]).into_owned();
            lines.push((line, base + at as u64 + 1));
            start = at + 1;
        }
    }
    lines
}
