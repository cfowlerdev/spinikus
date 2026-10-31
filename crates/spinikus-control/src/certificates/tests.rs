//! The certificate manager against a fake CA, over the deployment tests' setup: a live app
//! on a server with a fake agent.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use jiff::Timestamp;
use rcgen::{CertificateParams, KeyPair};
use serde_json::json;
use spinikus_core::{domain::Hostname, secret::Secret};
use spinikus_proto::agent::v1::{self as pb, control_message};
use spinikus_store::CertificateRepository;
use tokio::sync::mpsc;

use super::{
    CertificateTimings,
    acme::{AcmeClient, AcmeError, ChallengeResponder, Issued},
};
use crate::{
    Control, DeploymentTimings, NewDomain,
    deployments::tests::{Setup, connect, deploy, setup_with},
    settings::ACME_EMAIL,
};

/// Issues self-signed certificates, or fails while `fail` is set, answering a challenge first.
#[derive(Default)]
struct FakeAcme {
    accounts: AtomicUsize,
    issued: Mutex<Vec<String>>,
    fail: Mutex<Option<String>>,
    /// How long ago issued certificates start; 90-day certificates issued 70 days ago are
    /// due for renewal.
    backdate: Mutex<Duration>,
}

#[async_trait]
impl AcmeClient for FakeAcme {
    async fn create_account(&self, _directory_url: &str, email: &str) -> Result<Secret, AcmeError> {
        self.accounts.fetch_add(1, Ordering::SeqCst);
        Ok(Secret::new(format!("account of {email}")))
    }

    async fn issue(
        &self,
        credentials: &Secret,
        hostname: &Hostname,
        responder: &dyn ChallengeResponder,
    ) -> Result<Issued, AcmeError> {
        assert_eq!(credentials.expose(), "account of ops@example.com");
        let token = format!("token-{hostname}");
        responder.publish(&token, "key-authorization").await;
        responder.withdraw(&token).await;
        if let Some(problem) = self.fail.lock().unwrap().clone() {
            return Err(AcmeError(problem));
        }
        self.issued.lock().unwrap().push(hostname.to_string());
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec![hostname.to_string()]).unwrap();
        let start = time::OffsetDateTime::now_utc() - *self.backdate.lock().unwrap();
        params.not_before = start;
        params.not_after = start + Duration::from_secs(90 * 24 * 60 * 60);
        let certificate = params.self_signed(&key).unwrap();
        Ok(Issued { chain_pem: certificate.pem(), private_key_pem: Secret::new(key.serialize_pem()) })
    }
}

struct Harness {
    setup: Setup,
    acme: Arc<FakeAcme>,
    /// What the fake agent received; it confirms challenges as they arrive.
    received: mpsc::UnboundedReceiver<pb::ControlMessage>,
}

/// A live app with a public domain that redirects another, and certificates turned on.
async fn harness() -> Harness {
    let acme = Arc::new(FakeAcme::default());
    let timings = CertificateTimings { tick: Duration::from_secs(60), challenge_wait: Duration::from_secs(5) };
    let setup = setup_with(DeploymentTimings::default(), |control| control.with_acme_client(acme.clone(), timings)).await;
    let control = &setup.control;
    let apex = NewDomain { hostname: "shop.example.com".to_string(), redirect_to: None };
    let apex = control.create_domain(&setup.admin, setup.service.id, apex).await.unwrap();
    let www = NewDomain { hostname: "www.shop.example.com".to_string(), redirect_to: Some(apex.id) };
    control.create_domain(&setup.admin, setup.service.id, www).await.unwrap();
    let mut agent = connect(control, setup.server.id);
    deploy(&setup, &mut agent).await;
    control.advance_deployments().await;

    let (forward, received) = mpsc::unbounded_channel();
    let (control, server) = (control.clone(), setup.server.id);
    tokio::spawn(async move {
        while let Some(message) = agent.recv().await {
            if let Some(control_message::Message::AcmeChallenges(set)) = &message.message {
                control.challenges_applied(server, set.revision);
            }
            let _ = forward.send(message);
        }
    });
    Harness { setup, acme, received }
}

async fn turn_on(control: &Control, setup: &Setup) {
    control.set_setting(&setup.admin, ACME_EMAIL, 0, json!("ops@example.com")).await.unwrap();
}

fn drain(received: &mut mpsc::UnboundedReceiver<pb::ControlMessage>) -> (Vec<pb::AcmeChallenges>, Vec<pb::SetCertificates>) {
    let (mut challenges, mut certificates) = (Vec::new(), Vec::new());
    while let Ok(message) = received.try_recv() {
        match message.message {
            Some(control_message::Message::AcmeChallenges(set)) => challenges.push(set),
            Some(control_message::Message::SetCertificates(set)) => certificates.push(set),
            _ => {}
        }
    }
    (challenges, certificates)
}

#[tokio::test]
async fn gets_certificates_for_public_routed_hostnames_and_sends_them() {
    let Harness { setup, acme, mut received } = harness().await;
    let control = &setup.control;

    control.issue_certificates().await.unwrap();
    let issued_while_off = acme.issued.lock().unwrap().len();
    turn_on(control, &setup).await;
    let started = std::time::Instant::now();
    control.issue_certificates().await.unwrap();
    let took = started.elapsed();
    control.issue_certificates().await.unwrap();
    // Let the fake agent forward what it got.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (challenges, _) = drain(&mut received);
    control.advance_deployments().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (_, sent) = drain(&mut received);
    control.certificates_applied(setup.server.id, sent[0].revision).await;
    control.advance_deployments().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (_, again) = drain(&mut received);
    let listed = control.list_domains(&setup.admin, setup.service.id).await.unwrap();

    assert_eq!(issued_while_off, 0);
    // `web-production-shop.localhost` is not public.
    assert_eq!(*acme.issued.lock().unwrap(), ["shop.example.com", "www.shop.example.com"]);
    assert_eq!(acme.accounts.load(Ordering::SeqCst), 1);
    assert!(took < Duration::from_secs(2), "waited for confirmations that came: {took:?}");
    // Published, then withdrawn, for each hostname.
    let counts: Vec<_> = challenges.iter().map(|set| set.challenges.len()).collect();
    assert_eq!(counts, [1, 0, 1, 0]);
    assert_eq!(challenges[0].challenges[0].token, "token-shop.example.com");
    assert_eq!(challenges[0].challenges[0].key_authorization, "key-authorization");
    assert!(challenges.windows(2).all(|pair| pair[0].revision < pair[1].revision));
    assert_eq!(sent.len(), 1);
    let names: Vec<_> = sent[0].certificates.iter().map(|certificate| certificate.hostnames.join(",")).collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"shop.example.com".to_string()) && names.contains(&"www.shop.example.com".to_string()));
    assert!(sent[0].certificates.iter().all(|certificate| certificate.private_key_pem.contains("PRIVATE KEY")));
    assert!(again.is_empty(), "{again:?}");
    assert_eq!(listed.certificates.len(), 2);
    assert!(listed.certificates.iter().all(|record| record.certificate_id.is_some() && record.failures == 0));
}

#[tokio::test]
async fn failures_are_retried_later() {
    let Harness { setup, acme, .. } = harness().await;
    let control = &setup.control;
    turn_on(control, &setup).await;
    *acme.fail.lock().unwrap() = Some("urn:ietf:params:acme:error:dns: no such host".to_string());

    control.issue_certificates().await.unwrap();
    control.issue_certificates().await.unwrap();
    let records = control.store().list_hostname_certificates().await.unwrap();

    assert_eq!(records.len(), 2);
    for record in records {
        assert_eq!((record.failures, record.certificate_id), (1, None));
        assert_eq!(record.last_error.as_deref(), Some("urn:ietf:params:acme:error:dns: no such host"));
        let retry_in = record.retry_at.unwrap().duration_since(Timestamp::now());
        assert!(retry_in > jiff::SignedDuration::from_mins(4) && retry_in <= jiff::SignedDuration::from_mins(5), "{retry_in:?}");
    }
    assert!(acme.issued.lock().unwrap().is_empty());
    assert_eq!(super::retry_delay(2), Duration::from_secs(15 * 60));
    assert_eq!(super::retry_delay(9), Duration::from_secs(24 * 60 * 60));
}

#[tokio::test]
async fn renews_certificates_two_thirds_through_their_lifetime() {
    let Harness { setup, acme, .. } = harness().await;
    let control = &setup.control;
    turn_on(control, &setup).await;
    *acme.backdate.lock().unwrap() = Duration::from_secs(70 * 24 * 60 * 60);

    control.issue_certificates().await.unwrap();
    control.issue_certificates().await.unwrap();

    assert_eq!(acme.issued.lock().unwrap().len(), 4);
}
