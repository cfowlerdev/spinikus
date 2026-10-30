//! TLS certificates for domains, and the ACME accounts that issue them.

use jiff::Timestamp;
use serde::Serialize;

use crate::{
    domain::Hostname,
    id::{AcmeAccountId, CertificateId},
    secret::Secret
};

/// A certificate and its chain. The private key is stored encrypted and read separately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Certificate {
    pub id: CertificateId,
    /// The names it is valid for (its subject alternative names).
    pub hostnames: Vec<Hostname>,
    /// The leaf certificate followed by its intermediates, PEM-encoded.
    pub chain_pem: String,
    pub not_before: Timestamp,
    pub not_after: Timestamp,
    /// The ACME account that issued it; `None` for an uploaded certificate.
    pub acme_account_id: Option<AcmeAccountId>,
    pub created_at: Timestamp
}

impl Certificate {
    /// Whether the certificate is valid for `hostname`, wildcards included.
    pub fn covers(&self, hostname: &Hostname) -> bool {
        self.hostnames.iter().any(|name| name.covers(hostname))
    }
}

/// A certificate to store. The caller reads the names and validity from the certificate itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCertificate {
    pub hostnames: Vec<Hostname>,
    pub chain_pem: String,
    pub private_key_pem: Secret,
    pub not_before: Timestamp,
    pub not_after: Timestamp,
    pub acme_account_id: Option<AcmeAccountId>
}

/// An account with an ACME certificate authority, such as Let's Encrypt. Its credentials
/// (the account key) are stored encrypted and read separately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AcmeAccount {
    pub id: AcmeAccountId,
    pub directory_url: String,
    pub email: String,
    pub created_at: Timestamp
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAcmeAccount {
    pub directory_url: String,
    pub email: String,
    /// The ACME client's serialized account credentials.
    pub credentials: Secret
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_any_of_its_names() {
        let certificate = Certificate {
            id: CertificateId::new(),
            hostnames: vec!["example.com".parse().unwrap(), "*.example.com".parse().unwrap()],
            chain_pem: "-----BEGIN CERTIFICATE-----".to_string(),
            not_before: Timestamp::UNIX_EPOCH,
            not_after: Timestamp::MAX,
            acme_account_id: None,
            created_at: Timestamp::UNIX_EPOCH
        };

        for covered in ["example.com", "www.example.com", "*.example.com"] {
            assert!(certificate.covers(&covered.parse().unwrap()), "{covered}");
        }
        assert!(!certificate.covers(&"a.b.example.com".parse().unwrap()));
    }
}
