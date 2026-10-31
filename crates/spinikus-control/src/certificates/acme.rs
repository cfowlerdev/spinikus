//! Ordering certificates from an ACME certificate authority, such as Let's Encrypt, with
//! HTTP-01 challenges. [`AcmeClient`] is a trait so tests can issue certificates without
//! a CA; [`InstantAcme`] is the real client.

use std::{path::PathBuf, time::Duration};

use async_trait::async_trait;
use instant_acme::{
    Account, AccountBuilder, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, NewAccount, NewOrder, Order,
    OrderStatus, RetryPolicy,
};
use spinikus_core::{domain::Hostname, secret::Secret};

/// How long to wait for the CA to validate the challenges, and then to issue.
const ORDER_TIMEOUT: Duration = Duration::from_secs(90);

/// A certificate chain and the private key it was issued for, both PEM.
pub struct Issued {
    pub chain_pem: String,
    pub private_key_pem: Secret,
}

/// Why an account or certificate could not be had, as the CA or the connection to it put it.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct AcmeError(pub String);

impl From<instant_acme::Error> for AcmeError {
    fn from(error: instant_acme::Error) -> Self {
        Self(error.to_string())
    }
}

/// Serves HTTP-01 challenges while the CA validates them.
#[async_trait]
pub trait ChallengeResponder: Send + Sync {
    /// Serves `key_authorization` at `/.well-known/acme-challenge/<token>`, returning once
    /// the proxies serve it, or have had their chance to.
    async fn publish(&self, token: &str, key_authorization: &str);

    async fn withdraw(&self, token: &str);
}

#[async_trait]
pub trait AcmeClient: Send + Sync + 'static {
    /// Registers an account for `email` with the CA at `directory_url`, agreeing to its
    /// terms, and returns its credentials.
    async fn create_account(&self, directory_url: &str, email: &str) -> Result<Secret, AcmeError>;

    /// Orders a certificate for `hostname` with the account, answering the CA's HTTP-01
    /// challenge through `responder`.
    async fn issue(
        &self,
        credentials: &Secret,
        hostname: &Hostname,
        responder: &dyn ChallengeResponder,
    ) -> Result<Issued, AcmeError>;
}

/// The ACME client, over HTTPS with the system's web PKI roots, or a given root for a test CA.
#[derive(Default)]
pub struct InstantAcme {
    root: Option<PathBuf>,
}

impl InstantAcme {
    pub fn new() -> Self {
        Self::default()
    }

    /// Trusts only the CA certificate in the PEM file at `root`, such as a test CA's.
    pub fn with_root(root: PathBuf) -> Self {
        Self { root: Some(root) }
    }

    fn builder(&self) -> Result<AccountBuilder, AcmeError> {
        Ok(match &self.root {
            Some(root) => Account::builder_with_root(root)?,
            None => Account::builder()?,
        })
    }
}

#[async_trait]
impl AcmeClient for InstantAcme {
    async fn create_account(&self, directory_url: &str, email: &str) -> Result<Secret, AcmeError> {
        let contact = format!("mailto:{email}");
        let account = NewAccount { contact: &[&contact], terms_of_service_agreed: true, only_return_existing: false };
        let (_, credentials) = self.builder()?.create(&account, directory_url.to_string(), None).await?;
        Ok(Secret::new(serde_json::to_string(&credentials).expect("credentials serialize")))
    }

    async fn issue(
        &self,
        credentials: &Secret,
        hostname: &Hostname,
        responder: &dyn ChallengeResponder,
    ) -> Result<Issued, AcmeError> {
        let credentials: AccountCredentials = serde_json::from_str(credentials.expose())
            .map_err(|error| AcmeError(format!("unreadable account credentials: {error}")))?;
        let account = self.builder()?.from_credentials(credentials).await?;
        let identifiers = [Identifier::Dns(hostname.to_string())];
        let mut order = account.new_order(&NewOrder::new(&identifiers)).await?;
        let mut tokens = Vec::new();
        let result = complete(&mut order, responder, &mut tokens).await;
        for token in tokens {
            responder.withdraw(&token).await;
        }
        result
    }
}

async fn complete(order: &mut Order, responder: &dyn ChallengeResponder, tokens: &mut Vec<String>) -> Result<Issued, AcmeError> {
    let mut authorizations = order.authorizations();
    while let Some(authorization) = authorizations.next().await {
        let mut authorization = authorization?;
        match authorization.status {
            AuthorizationStatus::Pending => {}
            AuthorizationStatus::Valid => continue,
            status => return Err(AcmeError(format!("the authorization is {status:?}"))),
        }
        let mut challenge = authorization
            .challenge(ChallengeType::Http01)
            .ok_or_else(|| AcmeError("the CA offers no HTTP-01 challenge".to_string()))?;
        responder.publish(&challenge.token, challenge.key_authorization().as_str()).await;
        tokens.push(challenge.token.clone());
        challenge.set_ready().await?;
    }

    let retries = RetryPolicy::new().timeout(ORDER_TIMEOUT);
    if order.poll_ready(&retries).await? != OrderStatus::Ready {
        return Err(AcmeError(challenge_problem(order).await));
    }
    let private_key_pem = Secret::new(order.finalize().await?);
    let chain_pem = order.poll_certificate(&retries).await?;
    Ok(Issued { chain_pem, private_key_pem })
}

/// Why the CA did not validate the challenges, as far as it says.
async fn challenge_problem(order: &mut Order) -> String {
    let mut authorizations = order.authorizations();
    while let Some(Ok(mut authorization)) = authorizations.next().await {
        if let Ok(state) = authorization.refresh().await
            && let Some(problem) = state.challenges.iter().find_map(|challenge| challenge.error.as_ref())
        {
            return format!("the CA could not validate the challenge: {problem}");
        }
    }
    "the CA could not validate the challenge".to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    #[async_trait]
    impl ChallengeResponder for Recorder {
        async fn publish(&self, token: &str, key_authorization: &str) {
            assert!(key_authorization.starts_with(&format!("{token}.")), "{key_authorization}");
            self.0.lock().unwrap().push(format!("publish {token}"));
        }

        async fn withdraw(&self, token: &str) {
            self.0.lock().unwrap().push(format!("withdraw {token}"));
        }
    }

    /// Against Pebble, Let's Encrypt's test CA, with validation turned off:
    ///
    /// ```sh
    /// docker run -d --name pebble -p 14000:14000 -e PEBBLE_VA_ALWAYS_VALID=1 -e PEBBLE_VA_NOSLEEP=1 ghcr.io/letsencrypt/pebble
    /// docker cp pebble:/test/certs/pebble.minica.pem /tmp/pebble.minica.pem
    /// PEBBLE_ROOT=/tmp/pebble.minica.pem cargo test -p spinikus-control -- --ignored pebble
    /// ```
    #[tokio::test]
    #[ignore = "needs Pebble; see the comment for how to run it"]
    async fn orders_a_certificate_from_pebble() {
        let root = std::env::var("PEBBLE_ROOT").expect("PEBBLE_ROOT names Pebble's root certificate");
        let directory = std::env::var("PEBBLE_DIRECTORY").unwrap_or("https://localhost:14000/dir".to_string());
        let client = InstantAcme::with_root(root.into());
        let hostname: Hostname = "app.example.com".parse().unwrap();
        let recorder = Recorder::default();

        let credentials = client.create_account(&directory, "ops@example.com").await.unwrap();
        let issued = client.issue(&credentials, &hostname, &recorder).await.unwrap();

        let (_, pem) = x509_parser::pem::parse_x509_pem(issued.chain_pem.as_bytes()).unwrap();
        let (_, certificate) = x509_parser::parse_x509_certificate(&pem.contents).unwrap();
        let names = certificate.subject_alternative_name().unwrap().unwrap().value.general_names.clone();
        assert_eq!(format!("{names:?}"), r#"[DNSName("app.example.com")]"#);
        assert!(issued.private_key_pem.expose().contains("PRIVATE KEY"));
        let events = recorder.0.lock().unwrap();
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(events[0].starts_with("publish ") && events[1].starts_with("withdraw "), "{events:?}");
    }
}
