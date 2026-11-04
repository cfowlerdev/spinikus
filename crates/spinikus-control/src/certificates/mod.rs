//! HTTPS certificates for the hostnames the proxies serve.
//!
//! With an ACME email set (`acme_email`, which accepts the CA's terms), the certificate
//! manager ([`Control::run_certificates`]) gets a certificate for each public hostname in
//! the route tables, one per hostname with an HTTP-01 challenge, and renews it once two
//! thirds of its lifetime have passed. Names that cannot be public, such as `localhost`
//! ones, and wildcards (which need DNS-01) are skipped. A failure is recorded on the
//! hostname and retried later, backing off from 5 minutes to a day.
//!
//! Each runner pass sends every server the certificates of the hostnames it routes
//! ([`Control::sync_certificates`]), as a snapshot like its route table.

pub mod acme;
mod challenges;
#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    future::Future,
    sync::Arc,
    time::Duration,
};

use jiff::{SignedDuration, Timestamp};
use spinikus_core::{
    certificate::{Certificate, HostnameCertificate, NewAcmeAccount, NewCertificate},
    domain::Hostname,
    id::{CertificateId, ServerId},
    notification::{Message, NotificationEvent},
    route::Route,
};
use spinikus_proto::agent::v1::{self as pb, control_message};
use spinikus_store::{CertificateRepository, ServerCertificateRepository, StoredCertificates};
use tokio::{
    sync::{Mutex, Notify},
    time::Instant,
};

use self::{
    acme::{AcmeClient, AcmeError},
    challenges::{AgentResponder, Challenges},
};
use crate::{Control, ControlError, settings::AcmeSettings};

/// How long to wait after each failure in a row before trying a hostname again.
const RETRY_DELAYS: [Duration; 5] = [
    Duration::from_secs(5 * 60),
    Duration::from_secs(15 * 60),
    Duration::from_secs(60 * 60),
    Duration::from_secs(6 * 60 * 60),
    Duration::from_secs(24 * 60 * 60),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CertificateTimings {
    /// How often to look for certificates to get or renew, besides when routes change.
    pub tick: Duration,
    /// How long to wait for agents to confirm they serve a challenge.
    pub challenge_wait: Duration,
}

impl Default for CertificateTimings {
    fn default() -> Self {
        Self { tick: Duration::from_secs(60), challenge_wait: Duration::from_secs(10) }
    }
}

pub(crate) struct Certificates {
    acme: Arc<dyn AcmeClient>,
    timings: CertificateTimings,
    wake: Notify,
    challenges: Challenges,
    /// The certificates revision last sent to each server's agent, and when.
    sent: Mutex<HashMap<ServerId, (u64, Instant)>>,
    /// Certificates already reported as expiring soon, so each is reported once.
    expiring: std::sync::Mutex<HashSet<(Hostname, CertificateId)>>,
}

impl Certificates {
    pub(crate) fn new(acme: Arc<dyn AcmeClient>, timings: CertificateTimings) -> Self {
        Self {
            acme,
            timings,
            wake: Notify::new(),
            challenges: Challenges::default(),
            sent: Mutex::default(),
            expiring: std::sync::Mutex::default(),
        }
    }
}

/// How close to expiring a certificate is reported, if it hasn't been renewed.
pub(crate) const EXPIRING_WITHIN: SignedDuration = SignedDuration::from_hours(14 * 24);

/// Hostnames that can get a certificate: public names, not wildcards.
pub(crate) fn certifiable(hostname: &Hostname) -> bool {
    hostname.is_public() && !hostname.is_wildcard()
}

fn retry_delay(failures: u32) -> Duration {
    let index = (failures.max(1) as usize - 1).min(RETRY_DELAYS.len() - 1);
    RETRY_DELAYS[index]
}

impl Control {
    pub(crate) fn certificates(&self) -> &Certificates {
        &self.certificates
    }

    /// Gets and renews certificates until `shutdown` resolves. One per control plane.
    pub async fn run_certificates(&self, shutdown: impl Future<Output = ()>) {
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            if let Err(error) = self.issue_certificates().await {
                tracing::error!(%error, "failed to update certificates");
            }
            tokio::select! {
                () = shutdown.as_mut() => return,
                () = self.certificates().wake.notified() => {}
                () = tokio::time::sleep(self.certificates().timings.tick) => {}
            }
        }
    }

    /// Makes the certificate manager look for certificates to get now.
    pub(crate) fn wake_certificates(&self) {
        self.certificates().wake.notify_one();
    }

    /// Gets a certificate for each routed hostname that has none or is due for renewal,
    /// one at a time, unless issuing is off.
    pub(crate) async fn issue_certificates(&self) -> Result<(), ControlError> {
        let Some(settings) = self.acme_settings().await? else {
            return Ok(());
        };
        let hostnames: BTreeSet<Hostname> =
            self.build_routes().await?.into_values().flatten().map(|route| route.hostname).filter(certifiable).collect();
        let records: HashMap<Hostname, HostnameCertificate> = self
            .store()
            .list_hostname_certificates()
            .await?
            .into_iter()
            .map(|record| (record.hostname.clone(), record))
            .collect();
        let certificates = self.certificates_by_id().await?;
        for hostname in hostnames {
            let record = records.get(&hostname);
            let now = Timestamp::now();
            let current = record.and_then(|record| record.certificate_id).and_then(|id| certificates.get(&id));
            if let Some(certificate) = current
                && certificate.not_after - EXPIRING_WITHIN <= now
            {
                self.notify_expiring(&hostname, certificate, record.and_then(|record| record.last_error.as_deref())).await;
            }
            if record.and_then(|record| record.retry_at).is_some_and(|retry_at| retry_at > now) {
                continue;
            }
            if current.is_some_and(|certificate| certificate.renewal_due() > now) {
                continue;
            }
            self.issue_certificate(&settings, &hostname, record.map_or(0, |record| record.failures)).await?;
        }
        Ok(())
    }

    /// Tries to get a certificate for `hostname`, which has failed `failures` times in a
    /// row, recording the certificate or the failure.
    async fn issue_certificate(&self, settings: &AcmeSettings, hostname: &Hostname, failures: u32) -> Result<(), ControlError> {
        tracing::info!(%hostname, "requesting a certificate");
        let issued = self.order_certificate(settings, hostname).await;
        match issued {
            Ok(new) => {
                let certificate = self.store().record_certificate_issued(hostname, new).await?;
                tracing::info!(%hostname, certificate = %certificate.id, not_after = %certificate.not_after, "got a certificate");
                // The runner sends it to the servers that route the hostname.
                self.wake_deployments();
            }
            Err(error) => {
                let delay = retry_delay(failures + 1);
                let retry_at = Timestamp::now() + SignedDuration::try_from(delay).expect("delays fit");
                tracing::warn!(%hostname, %error, retry_in = ?delay, "failed to get a certificate");
                self.store().record_certificate_failure(hostname, &error.0, retry_at).await?;
                // Once per run of failures: it is retried with backoff, often.
                if failures == 0 {
                    self.notify(Message {
                        event: NotificationEvent::CertificateFailed,
                        title: format!("No HTTPS certificate for {hostname}"),
                        text: format!(
                            "Getting one failed: {}\n\nCheck that its DNS points at the server and that port 80 is open. \
                             Spinikus keeps trying.",
                            error.0
                        ),
                        path: None,
                        details: BTreeMap::from([("hostname".to_string(), hostname.to_string())]),
                    })
                    .await;
                }
            }
        }
        Ok(())
    }

    /// Reports a certificate that expires within [`EXPIRING_WITHIN`], once.
    async fn notify_expiring(&self, hostname: &Hostname, certificate: &Certificate, last_error: Option<&str>) {
        let key = (hostname.clone(), certificate.id);
        if !self.certificates().expiring.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(key) {
            return;
        }
        let renewal = match last_error {
            Some(error) => format!("Renewing it failed: {error}"),
            None => "It hasn't been renewed yet.".to_string(),
        };
        let expires = certificate.not_after.round(jiff::Unit::Second).unwrap_or(certificate.not_after);
        self.notify(Message {
            event: NotificationEvent::CertificateExpiring,
            title: format!("The HTTPS certificate for {hostname} expires {expires}"),
            text: format!("{renewal} Spinikus keeps trying; once it expires, browsers refuse the site."),
            path: None,
            details: BTreeMap::from([
                ("hostname".to_string(), hostname.to_string()),
                ("certificate_id".to_string(), certificate.id.to_string()),
            ]),
        })
        .await;
    }

    async fn order_certificate(&self, settings: &AcmeSettings, hostname: &Hostname) -> Result<NewCertificate, AcmeError> {
        let store_error = |error: spinikus_store::StoreError| {
            tracing::error!(%error, "failed to read or store the ACME account");
            AcmeError("the ACME account could not be read or stored".to_string())
        };
        let account = match self.store().find_acme_account(&settings.directory_url, &settings.email).await.map_err(store_error)? {
            Some(account) => account,
            None => {
                let credentials = self.certificates().acme.create_account(&settings.directory_url, &settings.email).await?;
                let new =
                    NewAcmeAccount { directory_url: settings.directory_url.clone(), email: settings.email.clone(), credentials };
                let account = self.store().create_acme_account(new).await.map_err(store_error)?;
                tracing::info!(directory = settings.directory_url, "registered an ACME account");
                account
            }
        };
        let credentials = self.store().acme_account_credentials(account.id).await.map_err(store_error)?;
        let issued = self.certificates().acme.issue(&credentials, hostname, &AgentResponder(self)).await?;
        let (not_before, not_after) =
            validity(&issued.chain_pem).ok_or_else(|| AcmeError("the CA returned an unreadable certificate".to_string()))?;
        Ok(NewCertificate {
            hostnames: vec![hostname.clone()],
            chain_pem: issued.chain_pem,
            private_key_pem: issued.private_key_pem,
            not_before,
            not_after,
            acme_account_id: Some(account.id),
        })
    }

    pub(crate) async fn certificates_by_id(&self) -> Result<HashMap<CertificateId, Certificate>, ControlError> {
        let certificates = self.store().list_certificates().await?;
        Ok(certificates.into_iter().map(|certificate| (certificate.id, certificate)).collect())
    }

    /// Stores each server's certificates, those of the hostnames it routes that have not
    /// expired, and sends them to agents that have not applied them.
    pub(crate) async fn sync_certificates(&self, tables: &HashMap<ServerId, Vec<Route>>) -> Result<(), ControlError> {
        let by_hostname: HashMap<Hostname, CertificateId> = self
            .store()
            .list_hostname_certificates()
            .await?
            .into_iter()
            .filter_map(|record| Some((record.hostname, record.certificate_id?)))
            .collect();
        let certificates = self.certificates_by_id().await?;
        let now = Timestamp::now();
        let mut sent = self.certificates().sent.lock().await;
        for (server, routes) in tables {
            let mut ids: Vec<CertificateId> = routes
                .iter()
                .filter_map(|route| by_hostname.get(&route.hostname))
                .filter(|id| certificates.get(id).is_some_and(|certificate| certificate.not_after > now))
                .copied()
                .collect();
            ids.sort();
            ids.dedup();
            let stored = self.store().save_server_certificates(*server, ids).await?;
            if stored.applied() {
                sent.remove(server);
                continue;
            }
            if let Some((revision, at)) = sent.get(server)
                && *revision == stored.revision
                && at.elapsed() < self.runner().timings.resend_after
            {
                continue;
            }
            let message = self.set_certificates(&stored, &certificates).await?;
            if self.agents().send(*server, message).await.is_ok() {
                tracing::debug!(server = %server, revision = stored.revision, "sent certificates");
                sent.insert(*server, (stored.revision, Instant::now()));
            }
        }
        Ok(())
    }

    async fn set_certificates(
        &self,
        stored: &StoredCertificates,
        certificates: &HashMap<CertificateId, Certificate>,
    ) -> Result<pb::ControlMessage, ControlError> {
        let mut bundles = Vec::new();
        for id in &stored.certificate_ids {
            let Some(certificate) = certificates.get(id) else {
                continue;
            };
            let key = self.store().certificate_private_key(*id).await?;
            bundles.push(pb::Certificate {
                certificate_id: id.to_string(),
                hostnames: certificate.hostnames.iter().map(ToString::to_string).collect(),
                chain_pem: certificate.chain_pem.clone(),
                private_key_pem: key.expose().to_string(),
            });
        }
        let set = pb::SetCertificates { revision: stored.revision, certificates: bundles };
        Ok(pb::ControlMessage { message: Some(control_message::Message::SetCertificates(set)) })
    }

    /// The agent serves the certificates with `revision`, now or before it connected.
    pub(crate) async fn certificates_applied(&self, server: ServerId, revision: u64) {
        if let Err(error) = self.store().record_certificates_applied(server, revision).await {
            tracing::error!(server = %server, %error, "failed to record applied certificates");
        }
    }

    /// A connecting agent may have lost what it was sent.
    pub(crate) async fn certificates_connected(&self, server: ServerId, revision: u64) {
        self.certificates().sent.lock().await.remove(&server);
        self.certificates_applied(server, revision).await;
        self.resend_challenges(server).await;
    }
}

/// When the first certificate of a PEM chain is valid.
fn validity(chain_pem: &str) -> Option<(Timestamp, Timestamp)> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(chain_pem.as_bytes()).ok()?;
    let (_, certificate) = x509_parser::parse_x509_certificate(&pem.contents).ok()?;
    let validity = certificate.validity();
    let not_before = Timestamp::from_second(validity.not_before.timestamp()).ok()?;
    let not_after = Timestamp::from_second(validity.not_after.timestamp()).ok()?;
    Some((not_before, not_after))
}
