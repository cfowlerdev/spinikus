//! The internal certificate authority. It signs agents' client certificates, whose subject
//! is the server ID, and the agent listener's certificate. Its key is stored in the
//! database, sealed under the master key.

use jiff::{SignedDuration, Timestamp};
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose,
    IsCa, Issuer, KeyPair, KeyUsagePurpose, SanType, SerialNumber,
};
use sha2::{Digest, Sha256};
use spinikus_core::{enrollment::CONTROL_PLANE_SERVER_NAME, id::ServerId, secret::Secret};
use spinikus_store::{EnrollmentRepository, StoredAuthority};
use time::OffsetDateTime;

use crate::{Control, ControlError};

/// How long the CA itself is valid.
const AUTHORITY_LIFETIME: SignedDuration = SignedDuration::from_hours(10 * 365 * 24);

/// How long the agent listener's certificate is valid. It is issued anew on every start.
const LISTENER_CERTIFICATE_LIFETIME: SignedDuration = SignedDuration::from_hours(365 * 24);

/// Certificates start a little in the past, so agents whose clocks lag still accept them.
const CLOCK_SKEW: SignedDuration = SignedDuration::from_mins(5);

pub struct CertificateAuthority {
    certificate_pem: String,
    certificate_der: Vec<u8>,
    issuer: Issuer<'static, KeyPair>,
}

impl CertificateAuthority {
    /// A new CA with a fresh key, and its stored form.
    fn generate(now: Timestamp) -> Result<(Self, StoredAuthority), rcgen::Error> {
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, "Spinikus internal CA");
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
        set_validity(&mut params, now, AUTHORITY_LIFETIME);
        let certificate = params.self_signed(&key)?;
        let stored = StoredAuthority { certificate_pem: certificate.pem(), private_key_pem: Secret::new(key.serialize_pem()) };
        Ok((Self::load(&stored)?, stored))
    }

    fn load(stored: &StoredAuthority) -> Result<Self, rcgen::Error> {
        let key = KeyPair::from_pem(stored.private_key_pem.expose())?;
        let issuer = Issuer::from_ca_cert_pem(&stored.certificate_pem, key)?;
        let der = certificate_der(&stored.certificate_pem).ok_or(rcgen::Error::CouldNotParseCertificate)?;
        Ok(Self { certificate_pem: stored.certificate_pem.clone(), certificate_der: der, issuer })
    }

    pub fn certificate_pem(&self) -> &str {
        &self.certificate_pem
    }

    /// The SHA-256 fingerprint of the CA certificate, as `sha256:<hex>`. Join commands carry
    /// it, so a new agent can check it is talking to the right control plane before trusting it.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.certificate_der)
    }

    /// Signs a client certificate for `server`'s agent. Only the CSR's public key is used:
    /// the subject is always the server ID, so an agent cannot ask for another identity.
    pub fn issue_agent_certificate(
        &self,
        server: ServerId,
        csr: &AgentCsr,
        now: Timestamp,
        lifetime: SignedDuration,
    ) -> Result<String, ControlError> {
        let mut params = leaf_params(&server.to_string(), now, lifetime);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let certificate = params
            .signed_by(&csr.0.public_key, &self.issuer)
            .map_err(|error| ControlError::internal("failed to sign a certificate", error))?;
        Ok(certificate.pem())
    }

    /// A certificate and key for the agent listener, for [`CONTROL_PLANE_SERVER_NAME`].
    pub fn issue_listener_certificate(&self, now: Timestamp) -> Result<(String, Secret), ControlError> {
        let issue = || {
            let key = KeyPair::generate()?;
            let mut params = leaf_params(CONTROL_PLANE_SERVER_NAME, now, LISTENER_CERTIFICATE_LIFETIME);
            params.subject_alt_names = vec![SanType::DnsName(CONTROL_PLANE_SERVER_NAME.try_into()?)];
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
            let certificate = params.signed_by(&key, &self.issuer)?;
            Ok::<_, rcgen::Error>((certificate.pem(), Secret::new(key.serialize_pem())))
        };
        issue().map_err(|error| ControlError::internal("failed to issue the agent listener's certificate", error))
    }
}

/// A parsed certificate signing request whose signature checks out.
pub struct AgentCsr(CertificateSigningRequestParams);

impl AgentCsr {
    /// Parses a DER CSR and checks its signature, which proves the agent holds the key.
    pub fn from_der(der: &[u8]) -> Result<Self, ControlError> {
        CertificateSigningRequestParams::from_der(&der.into())
            .map(Self)
            .map_err(|error| ControlError::Invalid { field: "csr", problem: error.to_string() })
    }
}

/// The server ID in an agent's client certificate (its subject's common name), if it has one.
pub fn server_in_certificate(certificate_der: &[u8]) -> Option<ServerId> {
    let (_, certificate) = x509_parser::parse_x509_certificate(certificate_der).ok()?;
    let common_name = certificate.subject().iter_common_name().next()?.as_str().ok()?;
    common_name.parse().ok()
}

/// The SHA-256 fingerprint of a DER certificate, as `sha256:<hex>`.
pub fn fingerprint(certificate_der: &[u8]) -> String {
    let digest = Sha256::digest(certificate_der);
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256:{hex}")
}

fn leaf_params(common_name: &str, now: Timestamp, lifetime: SignedDuration) -> CertificateParams {
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params.distinguished_name.push(DnType::CommonName, common_name);
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.serial_number = Some(SerialNumber::from_slice(&serial()));
    params.use_authority_key_identifier_extension = true;
    set_validity(&mut params, now, lifetime);
    params
}

/// A random positive 128-bit serial number.
fn serial() -> [u8; 16] {
    let mut serial: [u8; 16] = rand::random();
    serial[0] &= 0x7f;
    serial
}

fn set_validity(params: &mut CertificateParams, now: Timestamp, lifetime: SignedDuration) {
    params.not_before = offset_date_time(now - CLOCK_SKEW);
    params.not_after = offset_date_time(now + lifetime);
}

fn offset_date_time(at: Timestamp) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(at.as_second()).expect("timestamps fit in OffsetDateTime")
}

/// The DER bytes of the first certificate in a PEM string.
pub fn certificate_der(pem: &str) -> Option<Vec<u8>> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).ok()?;
    (pem.label == "CERTIFICATE").then_some(pem.contents)
}

impl Control {
    /// The internal CA, created on first use.
    pub async fn certificate_authority(&self) -> Result<&CertificateAuthority, ControlError> {
        self.authority
            .get_or_try_init(|| async {
                let invalid = |error| ControlError::internal("the stored certificate authority is invalid", error);
                if let Some(stored) = self.store().get_certificate_authority().await? {
                    return CertificateAuthority::load(&stored).map_err(invalid);
                }
                let (authority, stored) = CertificateAuthority::generate(Timestamp::now())
                    .map_err(|error| ControlError::internal("failed to create the certificate authority", error))?;
                let kept = self.store().create_certificate_authority(stored.clone()).await?;
                if kept == stored {
                    tracing::info!(fingerprint = authority.fingerprint(), "created the internal certificate authority");
                    return Ok(authority);
                }
                // Another process created one first; use that.
                CertificateAuthority::load(&kept).map_err(invalid)
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use rcgen::CertificateParams;
    use x509_parser::{extensions::ParsedExtension, prelude::X509Certificate};

    use super::*;
    use crate::test_support::control;

    fn csr(key: &KeyPair) -> Vec<u8> {
        let mut params = CertificateParams::new(vec!["evil.example.com".to_string()]).unwrap();
        params.distinguished_name.push(DnType::CommonName, "srv_someone_else");
        params.serialize_request(key).unwrap().der().to_vec()
    }

    fn parse(pem: &str) -> Vec<u8> {
        certificate_der(pem).unwrap()
    }

    fn client_auth_only(certificate: &X509Certificate<'_>) -> bool {
        certificate.extensions().iter().any(|extension| match extension.parsed_extension() {
            ParsedExtension::ExtendedKeyUsage(usage) => usage.client_auth && !usage.server_auth,
            _ => false,
        })
    }

    #[tokio::test]
    async fn the_authority_is_created_once_and_kept() {
        let (_dir, control) = control().await;

        let first = control.certificate_authority().await.unwrap().fingerprint();
        let reloaded = Control::new(control.store().clone()).certificate_authority().await.unwrap().fingerprint();

        assert_eq!(first, reloaded);
        assert!(first.starts_with("sha256:") && first.len() == "sha256:".len() + 64, "{first}");
    }

    #[tokio::test]
    async fn agent_certificates_name_the_server_whatever_the_csr_asks_for() {
        let (_dir, control) = control().await;
        let authority = control.certificate_authority().await.unwrap();
        let server = ServerId::new();
        let key = KeyPair::generate().unwrap();
        let now = Timestamp::now();

        let csr = AgentCsr::from_der(&csr(&key)).unwrap();
        let pem = authority.issue_agent_certificate(server, &csr, now, SignedDuration::from_hours(24)).unwrap();
        let again = authority.issue_agent_certificate(server, &csr, now, SignedDuration::from_hours(24)).unwrap();
        let der = parse(&pem);
        let (_, certificate) = x509_parser::parse_x509_certificate(&der).unwrap();

        assert_eq!(server_in_certificate(&der), Some(server));
        assert!(certificate.subject_alternative_name().unwrap().is_none(), "the CSR's names are ignored");
        assert!(client_auth_only(&certificate));
        assert_eq!(certificate.validity().not_after.timestamp(), (now + SignedDuration::from_hours(24)).as_second());
        let second_der = parse(&again);
        let (_, second) = x509_parser::parse_x509_certificate(&second_der).unwrap();
        assert_ne!(certificate.serial, second.serial, "serials are random, even for the same key");
        let ca_der = parse(authority.certificate_pem());
        let (_, ca) = x509_parser::parse_x509_certificate(&ca_der).unwrap();
        certificate.verify_signature(Some(ca.public_key())).unwrap();
    }

    #[test]
    fn rejects_malformed_and_forged_csrs() {
        let key = KeyPair::generate().unwrap();
        let mut forged = csr(&key);
        let last = forged.len() - 1;
        forged[last] ^= 0xff;

        assert!(matches!(AgentCsr::from_der(b"not a csr"), Err(ControlError::Invalid { field: "csr", .. })));
        assert!(matches!(AgentCsr::from_der(&forged), Err(ControlError::Invalid { field: "csr", .. })));
    }
}
