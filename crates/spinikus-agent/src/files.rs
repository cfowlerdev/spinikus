//! The agent's state on disk, in its own directory:
//!
//! - `agent.json`: the control plane's URL, this server's ID and the CA certificate;
//! - `identity.json`: the client certificate and its private key, readable by the owner only;
//! - `routes.json`: the last route table from the control plane, so the proxy serves it
//!   from the start, even while the control plane is unreachable;
//! - `certificates.json`: the last certificates, with their private keys, readable by the
//!   owner only, for the same reason.
//!
//! Each file is replaced atomically, so a crash never leaves a certificate next to the
//! wrong key.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use spinikus_core::{
    certificate::CertificateBundle,
    id::{CertificateId, ServerId},
    route::RouteTable,
    secret::Secret,
};

use crate::AgentError;

const ENROLLMENT_FILE: &str = "agent.json";
const IDENTITY_FILE: &str = "identity.json";
const ROUTES_FILE: &str = "routes.json";
const CERTIFICATES_FILE: &str = "certificates.json";

/// What the agent needs to connect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    /// Such as `https://spinikus.example.com:8443`.
    pub control_plane: String,
    pub server_id: ServerId,
    pub ca_pem: String,
    pub certificate_pem: String,
    pub key_pem: Secret,
}

#[derive(Serialize, Deserialize)]
struct Enrollment {
    control_plane: String,
    server_id: ServerId,
    ca_pem: String,
}

#[derive(Serialize, Deserialize)]
struct Identity {
    certificate_pem: String,
    key_pem: String,
}

/// The directory holding the agent's state.
#[derive(Debug, Clone)]
pub struct AgentFiles {
    dir: PathBuf,
}

impl AgentFiles {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The saved credentials, or `NotEnrolled` if the agent has not joined yet.
    pub fn load(&self) -> Result<Credentials, AgentError> {
        let enrollment_path = self.dir.join(ENROLLMENT_FILE);
        if !enrollment_path.exists() {
            return Err(AgentError::NotEnrolled);
        }
        let enrollment: Enrollment = read_json(&enrollment_path)?;
        let identity: Identity = read_json(&self.dir.join(IDENTITY_FILE))?;
        Ok(Credentials {
            control_plane: enrollment.control_plane,
            server_id: enrollment.server_id,
            ca_pem: enrollment.ca_pem,
            certificate_pem: identity.certificate_pem,
            key_pem: Secret::new(identity.key_pem),
        })
    }

    /// Saves everything after joining. The enrollment file is written last, so the agent
    /// counts as enrolled only once its identity is in place.
    pub fn save(&self, credentials: &Credentials) -> Result<(), AgentError> {
        self.save_identity(&credentials.certificate_pem, &credentials.key_pem)?;
        let enrollment = Enrollment {
            control_plane: credentials.control_plane.clone(),
            server_id: credentials.server_id,
            ca_pem: credentials.ca_pem.clone(),
        };
        write_json(&self.dir.join(ENROLLMENT_FILE), &enrollment, false)
    }

    /// Replaces the certificate and key, after a renewal.
    pub fn save_identity(&self, certificate_pem: &str, key_pem: &Secret) -> Result<(), AgentError> {
        let identity = Identity { certificate_pem: certificate_pem.to_string(), key_pem: key_pem.expose().to_string() };
        write_json(&self.dir.join(IDENTITY_FILE), &identity, true)
    }

    /// The saved route table, or an empty one at revision 0 if there is none yet.
    pub fn load_routes(&self) -> Result<RouteTable, AgentError> {
        let path = self.dir.join(ROUTES_FILE);
        if !path.exists() {
            return Ok(RouteTable::default());
        }
        read_json(&path)
    }

    pub fn save_routes(&self, table: &RouteTable) -> Result<(), AgentError> {
        write_json(&self.dir.join(ROUTES_FILE), table, false)
    }

    /// The saved certificates and their revision, or none at revision 0.
    pub fn load_certificates(&self) -> Result<(u64, Vec<CertificateBundle>), AgentError> {
        let path = self.dir.join(CERTIFICATES_FILE);
        if !path.exists() {
            return Ok((0, Vec::new()));
        }
        let saved: SavedCertificates = read_json(&path)?;
        let bundles = saved
            .certificates
            .into_iter()
            .map(|certificate| CertificateBundle {
                id: certificate.id,
                hostnames: certificate.hostnames,
                chain_pem: certificate.chain_pem,
                private_key_pem: Secret::new(certificate.private_key_pem),
            })
            .collect();
        Ok((saved.revision, bundles))
    }

    pub fn save_certificates(&self, revision: u64, bundles: &[CertificateBundle]) -> Result<(), AgentError> {
        let certificates = bundles
            .iter()
            .map(|bundle| SavedCertificate {
                id: bundle.id,
                hostnames: bundle.hostnames.clone(),
                chain_pem: bundle.chain_pem.clone(),
                private_key_pem: bundle.private_key_pem.expose().to_string(),
            })
            .collect();
        write_json(&self.dir.join(CERTIFICATES_FILE), &SavedCertificates { revision, certificates }, true)
    }
}

#[derive(Serialize, Deserialize)]
struct SavedCertificates {
    revision: u64,
    certificates: Vec<SavedCertificate>,
}

#[derive(Serialize, Deserialize)]
struct SavedCertificate {
    id: CertificateId,
    hostnames: Vec<spinikus_core::domain::Hostname>,
    chain_pem: String,
    private_key_pem: String,
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, AgentError> {
    let text =
        fs::read_to_string(path).map_err(|source| AgentError::File { action: "read", path: path.to_path_buf(), source })?;
    serde_json::from_str(&text).map_err(|error| AgentError::InvalidFile { path: path.to_path_buf(), problem: error.to_string() })
}

/// Writes `value` to a temporary file next to `path` and renames it into place. `private`
/// files are readable by the owner only.
fn write_json<T: Serialize>(path: &Path, value: &T, private: bool) -> Result<(), AgentError> {
    let error = |action| move |source| AgentError::File { action, path: path.to_path_buf(), source };
    let dir = path.parent().expect("agent files live in a directory");
    create_private_dir(dir).map_err(error("create the directory for"))?;
    let temporary = path.with_extension("json.tmp");
    let mut file = open_for_writing(&temporary, private).map_err(error("write"))?;
    let json = serde_json::to_vec_pretty(value).expect("agent state serializes");
    file.write_all(&json).and_then(|()| file.sync_all()).map_err(error("write"))?;
    fs::rename(&temporary, path).map_err(error("replace"))
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)
}

#[cfg(unix)]
fn open_for_writing(path: &Path, private: bool) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let _ = fs::remove_file(path);
    fs::OpenOptions::new().write(true).create_new(true).mode(if private { 0o600 } else { 0o644 }).open(path)
}

#[cfg(not(unix))]
fn open_for_writing(path: &Path, _private: bool) -> std::io::Result<fs::File> {
    fs::File::create(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> Credentials {
        Credentials {
            control_plane: "https://cp.example.com:8443".to_string(),
            server_id: ServerId::new(),
            ca_pem: "ca".to_string(),
            certificate_pem: "cert".to_string(),
            key_pem: Secret::new("key"),
        }
    }

    #[test]
    fn saves_and_loads_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let files = AgentFiles::new(dir.path().join("agent"));

        assert!(matches!(files.load(), Err(AgentError::NotEnrolled)));
        files.save(&credentials()).unwrap();
        let mut renewed = files.load().unwrap();
        files.save_identity("new cert", &Secret::new("new key")).unwrap();
        renewed.certificate_pem = "new cert".to_string();
        renewed.key_pem = Secret::new("new key");

        assert_eq!(files.load().unwrap(), renewed);
    }

    #[test]
    fn saves_and_loads_routes() {
        let dir = tempfile::tempdir().unwrap();
        let files = AgentFiles::new(dir.path().join("agent"));
        let table = RouteTable {
            revision: 7,
            routes: vec![spinikus_core::route::Route {
                hostname: "app.example.com".parse().unwrap(),
                action: spinikus_core::route::RouteAction::Redirect { to: "example.com".parse().unwrap() },
            }],
        };

        assert_eq!(files.load_routes().unwrap(), RouteTable::default());
        files.save_routes(&table).unwrap();

        assert_eq!(files.load_routes().unwrap(), table);
    }

    #[test]
    fn saves_certificates_privately() {
        let dir = tempfile::tempdir().unwrap();
        let files = AgentFiles::new(dir.path().join("agent"));
        let bundle = CertificateBundle {
            id: CertificateId::new(),
            hostnames: vec!["app.example.com".parse().unwrap()],
            chain_pem: "chain".to_string(),
            private_key_pem: Secret::new("key"),
        };

        assert_eq!(files.load_certificates().unwrap(), (0, Vec::new()));
        files.save_certificates(9, std::slice::from_ref(&bundle)).unwrap();

        assert_eq!(files.load_certificates().unwrap(), (9, vec![bundle]));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(files.dir().join(CERTIFICATES_FILE)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_key_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let files = AgentFiles::new(dir.path().join("agent"));
        files.save(&credentials()).unwrap();

        let mode = |name: &str| fs::metadata(files.dir().join(name)).unwrap().permissions().mode() & 0o777;

        assert_eq!(mode(IDENTITY_FILE), 0o600);
        assert_eq!(fs::metadata(files.dir()).unwrap().permissions().mode() & 0o777, 0o700);
    }
}
