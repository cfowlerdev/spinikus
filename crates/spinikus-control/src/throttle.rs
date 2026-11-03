//! Limits on failed sign-ins, kept in memory. An address gets [`MAX_FAILURES`] wrong
//! passwords per email in [`WINDOW`], and [`MAX_ADDRESS_FAILURES`] for all emails together;
//! after that its sign-ins are refused, without checking the password, until the window
//! that began with its first failure ends. Keying by email and address together means
//! someone guessing can't lock the real user out from elsewhere. A successful sign-in clears
//! that email's failures from the address.
//!
//! The counts are lost on restart, which an attacker can't cause, and are per control plane,
//! which is the only one.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

/// Wrong passwords for one email from one address before it must wait.
pub const MAX_FAILURES: u32 = 5;

/// Wrong passwords from one address, for any emails, before it must wait.
pub const MAX_ADDRESS_FAILURES: u32 = 50;

/// How long failures count.
pub const WINDOW: Duration = Duration::from_secs(15 * 60);

/// Past this many entries, expired ones are dropped when a failure is recorded.
const PRUNE_AT: usize = 10_000;

#[derive(Default)]
pub(crate) struct SignInThrottle {
    failures: Mutex<HashMap<Key, Failures>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    /// An email (lowercased) from an address.
    Email(String, String),
    /// An address, for all emails.
    Address(String),
}

struct Failures {
    count: u32,
    since: Instant,
}

impl Failures {
    fn expired(&self, now: Instant) -> bool {
        now.duration_since(self.since) >= WINDOW
    }
}

impl SignInThrottle {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Key, Failures>> {
        self.failures.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn keys(email: &str, address: Option<&str>) -> [Key; 2] {
        // Without an address (tests, or a client the server can't see), all such share one.
        let address = address.unwrap_or("unknown").to_string();
        [Key::Email(email.trim().to_lowercase(), address.clone()), Key::Address(address)]
    }

    /// `Err` with how long to wait if this email may not be tried from this address now.
    pub(crate) fn check(&self, email: &str, address: Option<&str>) -> Result<(), Duration> {
        self.check_at(email, address, Instant::now())
    }

    fn check_at(&self, email: &str, address: Option<&str>, now: Instant) -> Result<(), Duration> {
        let failures = self.lock();
        let [by_email, by_address] = Self::keys(email, address);
        let wait = |key: &Key, limit: u32| {
            failures
                .get(key)
                .filter(|failures| failures.count >= limit && !failures.expired(now))
                .map(|failures| WINDOW - now.duration_since(failures.since))
        };
        match wait(&by_email, MAX_FAILURES).into_iter().chain(wait(&by_address, MAX_ADDRESS_FAILURES)).max() {
            Some(wait) => Err(wait),
            None => Ok(()),
        }
    }

    pub(crate) fn failed(&self, email: &str, address: Option<&str>) {
        self.failed_at(email, address, Instant::now());
    }

    fn failed_at(&self, email: &str, address: Option<&str>, now: Instant) {
        let mut failures = self.lock();
        if failures.len() >= PRUNE_AT {
            failures.retain(|_, failures| !failures.expired(now));
        }
        for key in Self::keys(email, address) {
            let entry = failures.entry(key).or_insert(Failures { count: 0, since: now });
            if entry.expired(now) {
                *entry = Failures { count: 0, since: now };
            }
            entry.count += 1;
        }
    }

    pub(crate) fn succeeded(&self, email: &str, address: Option<&str>) {
        let [by_email, _] = Self::keys(email, address);
        self.lock().remove(&by_email);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn makes_an_address_wait_after_too_many_wrong_passwords() {
        let throttle = SignInThrottle::default();
        let start = Instant::now();
        let here = Some("203.0.113.7");
        for _ in 0..MAX_FAILURES {
            assert_eq!(throttle.check_at("ada@example.com", here, start), Ok(()));
            throttle.failed_at("Ada@Example.com", here, start);
        }

        let wait = throttle.check_at("ada@example.com ", here, start + Duration::from_secs(60));
        assert_eq!(wait, Err(WINDOW - Duration::from_secs(60)));
        // Not from elsewhere, nor for another email.
        assert_eq!(throttle.check_at("ada@example.com", Some("198.51.100.2"), start), Ok(()));
        assert_eq!(throttle.check_at("bob@example.com", here, start), Ok(()));
        // Until the window ends.
        assert_eq!(throttle.check_at("ada@example.com", here, start + WINDOW), Ok(()));
        throttle.failed_at("ada@example.com", here, start + WINDOW);
        assert_eq!(throttle.check_at("ada@example.com", here, start + WINDOW), Ok(()), "counting starts again");
    }

    #[test]
    fn limits_an_address_across_emails_and_clears_on_success() {
        let throttle = SignInThrottle::default();
        let now = Instant::now();
        let here = Some("203.0.113.7");
        for n in 0..MAX_ADDRESS_FAILURES {
            throttle.failed_at(&format!("user{n}@example.com"), here, now);
        }
        assert!(throttle.check_at("new@example.com", here, now).is_err());
        assert_eq!(throttle.check_at("new@example.com", None, now), Ok(()));

        let other = SignInThrottle::default();
        for _ in 0..MAX_FAILURES - 1 {
            other.failed_at("ada@example.com", here, now);
        }
        other.succeeded("ada@example.com", here);
        other.failed_at("ada@example.com", here, now);
        assert_eq!(other.check_at("ada@example.com", here, now), Ok(()), "a success starts the count again");
    }
}
