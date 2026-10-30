//! Domains: hostnames that route to a service, or redirect to another of its domains.

use std::{fmt, str::FromStr};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::id::{CertificateId, DomainId, ServiceId};

/// Longest allowed hostname, the DNS limit.
pub const MAX_HOSTNAME_LEN: usize = 253;

/// A hostname the proxy serves, routed to a service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Domain {
    pub id: DomainId,
    pub service_id: ServiceId,
    /// Unique among domains that are not deleted.
    pub hostname: Hostname,
    /// When set, requests are redirected to that domain of the same service instead of
    /// proxied, for example `www.example.com` to `example.com`.
    pub redirect_to: Option<DomainId>,
    /// The certificate serving this hostname, once one is issued. Set by the certificate
    /// manager, not by users.
    pub certificate_id: Option<CertificateId>,
    pub revision: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// The fields a user sets when creating a domain, or replaces when updating one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainFields {
    pub hostname: Hostname,
    pub redirect_to: Option<DomainId>,
}

/// A DNS hostname, such as `app.example.com`, or a wildcard covering one level of
/// subdomains, such as `*.example.com`. Parsing lowercases it and drops a trailing dot.
/// Internationalized names are given in their `xn--` form. IP addresses are not hostnames.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hostname(String);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("`{value}` is not a valid hostname: {problem}")]
pub struct HostnameError {
    value: String,
    problem: &'static str,
}

impl Hostname {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_wildcard(&self) -> bool {
        self.0.starts_with("*.")
    }

    /// Whether a certificate for `self` is valid for `hostname`: the same name, or `self`
    /// is a wildcard and `hostname` is exactly one level below it. `*.example.com` covers
    /// `app.example.com` and `*.example.com`, but not `example.com` or `a.b.example.com`.
    pub fn covers(&self, hostname: &Hostname) -> bool {
        if self == hostname {
            return true;
        }
        match (self.0.strip_prefix("*."), hostname.0.split_once('.')) {
            (Some(suffix), Some((label, rest))) => label != "*" && rest == suffix,
            _ => false,
        }
    }
}

impl FromStr for Hostname {
    type Err = HostnameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.strip_suffix('.').unwrap_or(value).to_ascii_lowercase();
        let (wildcard, name) = match normalized.strip_prefix("*.") {
            Some(name) => (true, name),
            None => (false, normalized.as_str()),
        };
        let labels: Vec<&str> = name.split('.').collect();

        let problem = if name.is_empty() {
            Some("it is empty")
        } else if normalized.len() > MAX_HOSTNAME_LEN {
            Some("it is longer than 253 characters")
        } else if labels.len() < 2 {
            Some("it needs at least two labels, such as example.com")
        } else if labels.iter().any(|label| label.is_empty() || label.len() > 63) {
            Some("each dot-separated label must be 1 to 63 characters")
        } else if !name.bytes().all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.')) {
            Some(if name.contains('*') {
                "a wildcard must be the whole first label, as in *.example.com"
            } else {
                "only letters, digits, hyphens and dots are allowed"
            })
        } else if labels.iter().any(|label| label.starts_with('-') || label.ends_with('-')) {
            Some("a label cannot start or end with a hyphen")
        } else if labels.last().is_some_and(|tld| tld.bytes().all(|byte| byte.is_ascii_digit())) {
            Some("IP addresses are not allowed")
        } else {
            None
        };

        match problem {
            Some(problem) => Err(HostnameError { value: value.to_string(), problem }),
            None if wildcard => Ok(Self(format!("*.{name}"))),
            None => Ok(Self(name.to_string())),
        }
    }
}

impl fmt::Display for Hostname {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for Hostname {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Hostname {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(value: &str) -> Hostname {
        value.parse().unwrap()
    }

    #[test]
    fn accepts_and_normalizes_hostnames() {
        let cases = [
            ("example.com", "example.com"),
            ("App.Example.COM.", "app.example.com"),
            ("*.example.com", "*.example.com"),
            ("xn--bcher-kva.example", "xn--bcher-kva.example"),
            ("a-1.b2.co", "a-1.b2.co"),
        ];
        for (input, expected) in cases {
            assert_eq!(input.parse::<Hostname>().map(|host| host.to_string()), Ok(expected.to_string()), "{input}");
        }
    }

    #[test]
    fn rejects_invalid_hostnames() {
        let too_long = format!("{}.com", vec!["a".repeat(63); 4].join("."));
        for input in [
            "",
            ".",
            "localhost",
            "*.com",
            "a..com",
            ".example.com",
            "-a.com",
            "a-.com",
            "a_b.com",
            "a b.com",
            "*foo.example.com",
            "app.*.example.com",
            "*.*.example.com",
            "192.168.1.1",
            "bücher.example",
            &format!("{}.com", "a".repeat(64)),
            &too_long,
        ] {
            assert!(input.parse::<Hostname>().is_err(), "{input:?} should be rejected");
        }
    }

    #[test]
    fn wildcards_cover_one_level() {
        let wildcard = host("*.example.com");

        assert!(wildcard.is_wildcard());
        assert!(wildcard.covers(&host("app.example.com")));
        assert!(wildcard.covers(&host("*.example.com")));
        assert!(!wildcard.covers(&host("example.com")));
        assert!(!wildcard.covers(&host("a.b.example.com")));
        assert!(!wildcard.covers(&host("app.example.org")));
        assert!(host("app.example.com").covers(&host("app.example.com")));
        assert!(!host("app.example.com").covers(&host("*.example.com")));
        assert!(!host("*.a.example.com").covers(&host("*.example.com")));
    }
}
