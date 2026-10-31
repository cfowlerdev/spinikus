//! Deployment logs: what the agent reports while preparing and starting a deployment (the
//! clone, the build, the pull, the health check), kept on disk, never in the database. Each
//! deployment has one file, `<logs>/deployments/<id>.log`, a line per entry: the time, then
//! the text, with `==> phase` lines where a new step starts. A file stops growing at
//! [`MAX_LOG_SIZE`].
//!
//! Without a log directory ([`Control::with_log_dir`]) logs are only traced.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use jiff::Timestamp;
use spinikus_core::id::{DeploymentId, ServerId};
use spinikus_proto::agent::v1 as pb;
use spinikus_store::DeploymentRepository;
use tokio::{io::AsyncWriteExt, sync::Mutex};

use crate::{Control, ControlError, Principal};

/// The most a deployment's log keeps.
pub const MAX_LOG_SIZE: u64 = 10 * 1024 * 1024;

pub(crate) struct DeploymentLogs {
    dir: Option<PathBuf>,
    /// Deployments whose server was checked, with their server. Held while writing, so
    /// lines from concurrent messages never interleave.
    servers: Mutex<HashMap<DeploymentId, ServerId>>,
}

impl DeploymentLogs {
    pub(crate) fn new(dir: Option<PathBuf>) -> Self {
        Self { dir: dir.map(|dir| dir.join("deployments")), servers: Mutex::default() }
    }

    fn path(&self, deployment: DeploymentId) -> Option<PathBuf> {
        Some(self.dir.as_ref()?.join(format!("{deployment}.log")))
    }
}

impl Control {
    /// Keeps deployment logs in `dir`. Call it before the agent listener starts.
    pub fn with_log_dir(mut self, dir: PathBuf) -> Self {
        self.logs = std::sync::Arc::new(DeploymentLogs::new(Some(dir)));
        self
    }

    /// The deployment's log so far, empty if there is none.
    pub async fn deployment_logs(&self, principal: &Principal, id: DeploymentId) -> Result<String, ControlError> {
        self.get_deployment(principal, id).await?;
        let Some(path) = self.logs.path(id) else {
            return Ok(String::new());
        };
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(ControlError::internal("reading a deployment log", error)),
        }
    }

    /// Appends a command's progress to the deployment's log, if the deployment runs on
    /// `server`.
    pub(crate) async fn log_progress(&self, server: ServerId, deployment: DeploymentId, progress: &pb::CommandProgress) {
        let mut servers = self.logs.servers.lock().await;
        let owner = match servers.get(&deployment) {
            Some(owner) => *owner,
            None => match self.store().get_deployment(deployment).await {
                Ok(found) => *servers.entry(deployment).or_insert(found.server_id),
                Err(_) => return,
            },
        };
        if owner != server {
            return;
        }
        let mut text = String::new();
        if progress.log_lines.is_empty() {
            text.push_str(&format!("{} ==> {}\n", Timestamp::now(), progress.phase));
        }
        for line in &progress.log_lines {
            let at = line.at.and_then(|at| Timestamp::new(at.seconds, at.nanos).ok()).unwrap_or_else(Timestamp::now);
            tracing::debug!(deployment = %deployment, line = line.text, "deployment log");
            text.push_str(&format!("{at} {}\n", line.text.trim_end()));
        }
        let Some(path) = self.logs.path(deployment) else {
            return;
        };
        if let Err(error) = append(&path, &text).await {
            tracing::warn!(deployment = %deployment, %error, "failed to write a deployment log");
        }
    }

    /// Appends a `==> text` line to the deployment's log, such as why it failed.
    pub(crate) async fn log_line(&self, deployment: DeploymentId, text: &str) {
        let _servers = self.logs.servers.lock().await;
        let Some(path) = self.logs.path(deployment) else {
            return;
        };
        if let Err(error) = append(&path, &format!("{} ==> {text}\n", Timestamp::now())).await {
            tracing::warn!(deployment = %deployment, %error, "failed to write a deployment log");
        }
    }

    /// Forgets which server finished deployments run on.
    pub(crate) async fn forget_log_servers(&self, active: &[DeploymentId]) {
        self.logs.servers.lock().await.retain(|deployment, _| active.contains(deployment));
    }
}

async fn append(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = tokio::fs::OpenOptions::new().create(true).append(true).open(path).await?;
    let size = file.metadata().await?.len();
    if size >= MAX_LOG_SIZE {
        return Ok(());
    }
    if size + text.len() as u64 > MAX_LOG_SIZE {
        return file.write_all(format!("{} ==> the log is too long; the rest is not kept\n", Timestamp::now()).as_bytes()).await;
    }
    file.write_all(text.as_bytes()).await
}
