//! The certificates the proxy serves, chosen by the name the client asks for (SNI).

use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, RwLock},
};

use rustls::{
    crypto::ring::sign::any_supported_type,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};
use spinikus_core::certificate::CertificateBundle;

/// Certificates by hostname: exact names first, then wildcards one level up.
#[derive(Default)]
pub struct Certificates {
    exact: HashMap<String, Arc<CertifiedKey>>,
    /// By the part after `*.`.
    wildcard: HashMap<String, Arc<CertifiedKey>>,
}

impl Certificates {
    /// Loads the bundles. One that cannot be loaded is left out, with the reason.
    pub fn new(bundles: &[CertificateBundle]) -> (Self, Vec<String>) {
        let mut certificates = Self::default();
        let mut problems = Vec::new();
        for bundle in bundles {
            let key = match certified_key(bundle) {
                Ok(key) => Arc::new(key),
                Err(problem) => {
                    problems.push(format!("certificate {}: {problem}", bundle.id));
                    continue;
                }
            };
            for hostname in &bundle.hostnames {
                let name = hostname.as_str();
                match name.strip_prefix("*.") {
                    Some(parent) => certificates.wildcard.insert(parent.to_string(), key.clone()),
                    None => certificates.exact.insert(name.to_string(), key.clone()),
                };
            }
        }
        (certificates, problems)
    }

    pub fn len(&self) -> usize {
        self.exact.len() + self.wildcard.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The certificate for a hostname, without port, in any case.
    pub(crate) fn get(&self, name: &str) -> Option<&Arc<CertifiedKey>> {
        let name = name.to_ascii_lowercase();
        let name = name.strip_suffix('.').unwrap_or(&name);
        if let Some(key) = self.exact.get(name) {
            return Some(key);
        }
        let (_, parent) = name.split_once('.')?;
        self.wildcard.get(parent)
    }
}

fn certified_key(bundle: &CertificateBundle) -> Result<CertifiedKey, String> {
    let chain = CertificateDer::pem_slice_iter(bundle.chain_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("unreadable chain: {error}"))?;
    if chain.is_empty() {
        return Err("the chain has no certificates".to_string());
    }
    let key = PrivateKeyDer::from_pem_slice(bundle.private_key_pem.expose().as_bytes())
        .map_err(|_| "unreadable private key".to_string())?;
    let key = any_supported_type(&key).map_err(|error| format!("unsupported private key: {error}"))?;
    let certified = CertifiedKey::new(chain, key);
    certified.keys_match().map_err(|_| "the private key does not match the certificate".to_string())?;
    Ok(certified)
}

/// Picks the certificate for the TLS handshake from the proxy's current set. Without a
/// matching certificate, or without SNI, the handshake fails.
pub(crate) struct Resolver(pub(crate) Arc<RwLock<Arc<Certificates>>>);

impl fmt::Debug for Resolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resolver").finish_non_exhaustive()
    }
}

impl ResolvesServerCert for Resolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let name = client_hello.server_name()?;
        let certificates = self.0.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        certificates.get(name).cloned()
    }
}
