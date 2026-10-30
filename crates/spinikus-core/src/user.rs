//! User accounts.

use std::{fmt, str::FromStr};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::id::UserId;

/// Longest allowed email address.
pub const MAX_EMAIL_LEN: usize = 254;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct User {
    pub id: UserId,
    /// Unique among all users, disabled ones included.
    pub email: Email,
    pub name: String,
    /// An instance admin: may do anything, in every project, regardless of memberships.
    pub admin: bool,
    /// Whether the user can sign in with a password, rather than only through SSO.
    pub has_password: bool,
    pub revision: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// Disabled users cannot sign in. Users are disabled rather than deleted, so the audit
    /// log and deployment history keep pointing at them.
    pub disabled_at: Option<Timestamp>
}

impl User {
    pub fn is_active(&self) -> bool {
        self.disabled_at.is_none()
    }
}

/// What a new user starts from. The password is set separately, if at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserFields {
    pub email: Email,
    pub name: String,
    pub admin: bool
}

/// An email address, lowercased. Only checked loosely: one `@` with something on each side
/// and no whitespace. Whether it is deliverable is for a confirmation email to find out.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Email(String);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("`{value}` is not a valid email address: {problem}")]
pub struct EmailError {
    value: String,
    problem: &'static str
}

impl Email {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Email {
    type Err = EmailError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let email = value.trim().to_lowercase();
        let problem = match email.split_once('@') {
            _ if email.len() > MAX_EMAIL_LEN => Some("it is longer than 254 characters"),
            _ if email.chars().any(|c| c.is_whitespace() || c.is_control()) => Some("it cannot contain whitespace"),
            None => Some("it needs an @"),
            Some((local, domain)) if local.is_empty() || domain.is_empty() => Some("it needs text on both sides of the @"),
            Some((_, domain)) if domain.contains('@') => Some("it can only contain one @"),
            Some(_) => None
        };

        match problem {
            Some(problem) => Err(EmailError { value: value.to_string(), problem }),
            None => Ok(Self(email))
        }
    }
}

impl fmt::Display for Email {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for Email {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Email {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// A password hash in PHC string format, such as `$argon2id$v=19$…`. Hashing and verifying
/// happen in the service layer; this only carries the result. `Debug` hides it.
#[derive(Clone, PartialEq, Eq)]
pub struct PasswordHash(Zeroizing<String>);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("not a password hash in PHC string format")]
pub struct PasswordHashError;

impl PasswordHash {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for PasswordHash {
    type Err = PasswordHashError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let algorithm = value.strip_prefix('$').and_then(|rest| rest.split('$').next()).unwrap_or_default();
        if algorithm.is_empty() || value.split('$').count() < 3 || value.chars().any(char::is_whitespace) {
            return Err(PasswordHashError);
        }
        Ok(Self(Zeroizing::new(value.to_string())))
    }
}

impl fmt::Debug for PasswordHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PasswordHash([redacted])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_emails() {
        assert_eq!("  Ada@Example.COM ".parse::<Email>().unwrap().as_str(), "ada@example.com");
        assert_eq!("root@localhost".parse::<Email>().unwrap().as_str(), "root@localhost");
    }

    #[test]
    fn rejects_invalid_emails() {
        for value in ["", "ada", "@example.com", "ada@", "ada@b@c", "a da@example.com", &format!("{}@x.io", "a".repeat(250))] {
            assert!(value.parse::<Email>().is_err(), "{value:?} should be rejected");
        }
    }

    #[test]
    fn accepts_phc_hashes_only() {
        let hash: PasswordHash = "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".parse().unwrap();

        assert!(hash.as_str().starts_with("$argon2id$"));
        assert_eq!(format!("{hash:?}"), "PasswordHash([redacted])");
        for value in ["", "hunter2", "$", "$argon2id", "$$x", "$argon2id$ x"] {
            assert!(value.parse::<PasswordHash>().is_err(), "{value:?} should be rejected");
        }
    }
}
