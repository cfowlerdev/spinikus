//! Password policy and hashing. Hashing is Argon2id with the crate's defaults (the OWASP
//! recommendation), run on the blocking pool since each hash takes tens of milliseconds.

use std::sync::LazyLock;

use argon2::{
    Argon2,
    password_hash::{PasswordHasher, PasswordVerifier, phc},
};
use spinikus_core::{secret::Secret, user::PasswordHash};

use crate::ControlError;

pub const MIN_PASSWORD_LEN: usize = 12;
pub const MAX_PASSWORD_LEN: usize = 1024;

/// Checks a new password. Only length is enforced: long passphrases beat composition rules.
pub fn check_policy(password: &Secret) -> Result<(), ControlError> {
    let length = password.expose().chars().count();
    if length < MIN_PASSWORD_LEN {
        return Err(ControlError::Invalid { field: "password", problem: format!("use at least {MIN_PASSWORD_LEN} characters") });
    }
    if length > MAX_PASSWORD_LEN {
        return Err(ControlError::Invalid { field: "password", problem: format!("use at most {MAX_PASSWORD_LEN} characters") });
    }
    Ok(())
}

pub async fn hash(password: Secret) -> PasswordHash {
    tokio::task::spawn_blocking(move || hash_blocking(&password)).await.expect("password hashing does not panic")
}

fn hash_blocking(password: &Secret) -> PasswordHash {
    let hash = Argon2::default().hash_password(password.expose().as_bytes()).expect("Argon2 hashes any password");
    hash.to_string().parse().expect("Argon2 produces a PHC string")
}

/// Whether `password` matches `hash`. With no hash (no such user, or no password), it checks
/// against a dummy hash anyway, so the response time does not reveal whether the account exists.
pub async fn verify(password: Secret, hash: Option<PasswordHash>) -> bool {
    tokio::task::spawn_blocking(move || {
        let exists = hash.is_some();
        let hash = hash.unwrap_or_else(|| DUMMY_HASH.clone());
        let matches = phc::PasswordHash::new(hash.as_str())
            .is_ok_and(|parsed| Argon2::default().verify_password(password.expose().as_bytes(), &parsed).is_ok());
        exists && matches
    })
    .await
    .expect("password verification does not panic")
}

static DUMMY_HASH: LazyLock<PasswordHash> = LazyLock::new(|| hash_blocking(&Secret::new("spinikus dummy password")));

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hashes_and_verifies() {
        let hash = hash(Secret::new("correct horse battery staple")).await;

        assert!(hash.as_str().starts_with("$argon2id$"));
        assert!(verify(Secret::new("correct horse battery staple"), Some(hash.clone())).await);
        assert!(!verify(Secret::new("wrong horse battery staple"), Some(hash)).await);
        assert!(!verify(Secret::new("spinikus dummy password"), None).await, "the dummy hash never signs anyone in");
    }

    #[test]
    fn enforces_length() {
        assert!(check_policy(&Secret::new("short")).is_err());
        assert!(check_policy(&Secret::new("a".repeat(MAX_PASSWORD_LEN + 1))).is_err());
        assert!(check_policy(&Secret::new("twelve chars")).is_ok());
        assert!(check_policy(&Secret::new("ü".repeat(MIN_PASSWORD_LEN))).is_ok(), "counts characters, not bytes");
    }
}
