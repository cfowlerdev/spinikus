//! Reading the agent's certificate and the CA fingerprint.

use base64::{Engine, engine::general_purpose::STANDARD};
use jiff::Timestamp;
use sha2::{Digest, Sha256};

/// When a certificate is valid, read from its PEM.
pub(crate) fn validity(certificate_pem: &str) -> Option<(Timestamp, Timestamp)> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(certificate_pem.as_bytes()).ok()?;
    let (_, certificate) = x509_parser::parse_x509_certificate(&pem.contents).ok()?;
    let validity = certificate.validity();
    let not_before = Timestamp::from_second(validity.not_before.timestamp()).ok()?;
    let not_after = Timestamp::from_second(validity.not_after.timestamp()).ok()?;
    Some((not_before, not_after))
}

/// When to renew: once two thirds of the certificate's lifetime has passed.
pub(crate) fn renewal_due(not_before: Timestamp, not_after: Timestamp) -> Timestamp {
    let lifetime = not_after.duration_since(not_before);
    not_before + lifetime * 2 / 3
}

/// `sha256:<hex>` of a DER certificate, the format join commands carry.
pub(crate) fn fingerprint(certificate_der: &[u8]) -> String {
    let hex: String = Sha256::digest(certificate_der).iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256:{hex}")
}

/// Checks a fingerprint from a join command and normalizes it to lowercase.
pub(crate) fn parse_fingerprint(value: &str) -> Option<String> {
    let hex = value.trim().strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())).then(|| format!("sha256:{}", hex.to_ascii_lowercase()))
}

pub(crate) fn der_to_pem(der: &[u8]) -> String {
    let encoded = STANDARD.encode(der);
    let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
    for line in encoded.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
        pem.push('\n');
    }
    pem.push_str("-----END CERTIFICATE-----\n");
    pem
}

/// The time left until `at`, or zero if it has passed.
pub(crate) fn until(at: Timestamp) -> std::time::Duration {
    let left = at.duration_since(Timestamp::now());
    if left.is_negative() { std::time::Duration::ZERO } else { left.unsigned_abs() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renews_at_two_thirds_of_the_lifetime() {
        let not_before = Timestamp::from_second(0).unwrap();
        let not_after = Timestamp::from_second(30 * 86_400).unwrap();

        assert_eq!(renewal_due(not_before, not_after), Timestamp::from_second(20 * 86_400).unwrap());
    }

    #[test]
    fn parses_fingerprints() {
        let hex = "AB".repeat(32);

        assert_eq!(parse_fingerprint(&format!(" sha256:{hex} ")), Some(format!("sha256:{}", "ab".repeat(32))));
        assert_eq!(parse_fingerprint(&hex), None);
        assert_eq!(parse_fingerprint("sha256:abcd"), None);
        assert_eq!(parse_fingerprint(&format!("sha256:{}", "zz".repeat(32))), None);
    }

    #[test]
    fn converts_der_to_pem_and_back() {
        let key = rcgen::KeyPair::generate().unwrap();
        let certificate = rcgen::CertificateParams::new(vec!["a.example".to_string()]).unwrap().self_signed(&key).unwrap();

        let pem = der_to_pem(certificate.der());
        let (_, parsed) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).unwrap();

        assert_eq!(parsed.contents, certificate.der().to_vec());
        assert!(validity(&pem).is_some());
    }
}
