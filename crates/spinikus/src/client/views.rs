//! The parts of the API's resources the CLI reads to print tables and find things. They
//! ignore fields they don't name, so additions to the API don't break older clients; JSON
//! output prints what the server sent instead.

use serde::Deserialize;
use serde_json::Value;

/// Reads a view from a resource the server sent.
pub fn read<T: serde::de::DeserializeOwned>(value: &Value) -> anyhow::Result<T> {
    serde_json::from_value(value.clone())
        .map_err(|error| anyhow::anyhow!("The server's answer wasn't what was expected: {error}"))
}

#[derive(Debug, Clone, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub slug: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Environment {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub slug: String,
    pub kind: String,
    #[serde(default)]
    pub pr_number: Option<u32>,
    #[serde(default)]
    pub previews: bool,
    #[serde(default)]
    pub revision: i64,
}

impl Environment {
    /// The kind as tables show it: `preview #7`, or `staging (previews)` when it has them.
    pub fn described_kind(&self) -> String {
        match (self.pr_number, self.previews) {
            (Some(number), _) => format!("{} #{number}", self.kind),
            (None, true) => format!("{} (previews)", self.kind),
            (None, false) => self.kind.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Service {
    pub id: String,
    pub environment_id: String,
    pub name: String,
    pub slug: String,
    pub kind: String,
    #[serde(default)]
    pub server_id: Option<String>,
    #[serde(default)]
    pub stopped_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Deployment {
    pub id: String,
    pub status: String,
    #[serde(default)]
    pub reason: Option<String>,
    pub trigger: String,
    #[serde(default)]
    pub commit_sha: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub rollback_of: Option<String>,
    pub created_at: String,
}

impl Deployment {
    /// What it runs: the commit for builds, else the image.
    pub fn what(&self) -> String {
        match (&self.commit_sha, &self.image) {
            (Some(sha), _) => sha.chars().take(12).collect(),
            (None, Some(image)) => image.clone(),
            (None, None) => String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeploymentEvent {
    pub to: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Variable {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub value: Option<String>,
    pub availability: String,
    pub secret: bool,
    pub revision: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceDomains {
    #[serde(default)]
    pub generated_hostname: Option<String>,
    pub domains: Vec<Domain>,
    #[serde(default)]
    pub certificates: Vec<HostnameCertificate>,
}

impl ServiceDomains {
    /// Whether `hostname` has a certificate, so is served over HTTPS.
    pub fn https(&self, hostname: &str) -> bool {
        self.certificates.iter().any(|certificate| certificate.hostname == hostname && certificate.certificate_id.is_some())
    }

    /// Where the app answers: its first domain that serves it, else the generated hostname.
    pub fn url(&self) -> Option<String> {
        let hostname = self
            .domains
            .iter()
            .find(|domain| domain.redirect_to.is_none())
            .map(|domain| domain.hostname.clone())
            .or_else(|| self.generated_hostname.clone())?;
        let scheme = if self.https(&hostname) { "https" } else { "http" };
        Some(format!("{scheme}://{hostname}"))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Domain {
    pub id: String,
    pub hostname: String,
    #[serde(default)]
    pub redirect_to: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Volume {
    pub id: String,
    pub name: String,
    pub mount_path: String,
    pub revision: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HostnameCertificate {
    pub hostname: String,
    #[serde(default)]
    pub certificate_id: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Destination {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Backup {
    pub id: String,
    pub status: String,
    #[serde(default)]
    pub schedule_id: Option<String>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
    /// An app's volume; `None` for a database's dump.
    #[serde(default)]
    pub volume_id: Option<String>,
    pub service_id: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Restore {
    pub id: String,
    pub status: String,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Token {
    pub id: String,
    pub name: String,
    pub role: String,
    #[serde(default)]
    pub scope: Option<Scope>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub last_used_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Scope {
    pub scope: String,
    pub scope_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub name: String,
    pub email: String,
    pub admin: bool,
}
