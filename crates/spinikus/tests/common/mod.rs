//! A control plane serving the agent listener on a free port, for tests with real agents.

// Each test file uses a different part of this.
#![allow(dead_code)]

use std::time::Duration;

use serde_json::json;
use spinikus_control::{Control, DeploymentTimings, NewEnvironment, NewProject, NewServer, NewService, Principal};
use spinikus_core::{
    deployment::DeploymentStatus,
    environment::EnvironmentKind,
    id::{DeploymentId, ServerId, ServiceId, SessionId},
    secret::Secret,
    server::ServerStatus,
    service::ServiceKind,
    source::{ImageSource, SourceSpec},
    user::UserFields,
};
use spinikus_store::{DeploymentRepository, MasterKey, ServerRepository, SqliteStore, UserRepository, connect, migrate};
use tokio::{net::TcpListener, sync::oneshot};

pub struct ControlPlane {
    _dir: tempfile::TempDir,
    pub control: Control,
    pub admin: Principal,
    pub url: String,
    /// The agent listener's address.
    pub address: std::net::SocketAddr,
    pub fingerprint: String,
    _stop: oneshot::Sender<()>,
}

impl ControlPlane {
    pub async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let pool = connect(&dir.path().join("spinikus.db")).await.unwrap();
        migrate(&pool).await.unwrap();
        let timings = DeploymentTimings { tick: Duration::from_millis(50), ..Default::default() };
        let control =
            Control::new(SqliteStore::open(pool, &MasterKey::generate()).await.unwrap()).with_deployment_timings(timings);
        let fields = UserFields { email: "admin@example.com".parse().unwrap(), name: "Admin".to_string(), admin: true };
        let user = control.store().create_user(fields, None).await.unwrap();
        let admin = Principal { user, credential: spinikus_control::Credential::Session(SessionId::new()) };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let url = format!("https://{address}");
        let (stop, stopped) = oneshot::channel::<()>();
        let (stop_runner, runner_stopped) = oneshot::channel::<()>();
        tokio::spawn(spinikus_control::agents::serve(control.clone(), listener, async move {
            let _ = stopped.await;
            let _ = stop_runner.send(());
        }));
        let runner = control.clone();
        tokio::spawn(async move {
            runner
                .run_deployments(async move {
                    let _ = runner_stopped.await;
                })
                .await
        });
        let fingerprint = control.certificate_authority().await.unwrap().fingerprint();
        Self { _dir: dir, control, admin, url, address, fingerprint, _stop: stop }
    }

    pub async fn join_token(&self) -> (ServerId, Secret) {
        let server = self.control.create_server(&self.admin, NewServer { name: "web-1".to_string() }).await.unwrap();
        (server.id, self.control.create_join_token(&self.admin, server.id).await.unwrap().secret)
    }

    pub async fn status(&self, server: ServerId) -> ServerStatus {
        self.control.store().get_server(server).await.unwrap().status
    }

    pub async fn wait_for_status(&self, server: ServerId, status: ServerStatus) {
        for _ in 0..250 {
            if self.status(server).await == status {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the server never became {status}");
    }

    /// An app service placed on `server`, deploying `image`, with these runtime settings.
    pub async fn service(&self, server: ServerId, image: &str, runtime: serde_json::Value) -> ServiceId {
        let project =
            self.control.create_project(&self.admin, NewProject { name: "Shop".to_string(), slug: None }).await.unwrap();
        let new = NewEnvironment { name: "Production".to_string(), slug: None, kind: EnvironmentKind::Production };
        let environment = self.control.create_environment(&self.admin, project.id, new).await.unwrap();
        let new = NewService {
            name: "Web".to_string(),
            slug: None,
            kind: ServiceKind::App,
            config: json!({ "runtime": runtime }).as_object().cloned().unwrap(),
            server_id: Some(server),
        };
        let service = self.control.create_service(&self.admin, environment.id, new).await.unwrap();
        let source = SourceSpec::Image(ImageSource { image: image.to_string() });
        self.control.create_source(&self.admin, service.id, source).await.unwrap();
        service.id
    }

    pub async fn wait_for_deployment(&self, deployment: DeploymentId, status: DeploymentStatus, within: Duration) {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let current = self.control.store().get_deployment(deployment).await.unwrap();
            if current.status == status {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline && !current.status.is_terminal(),
                "the deployment is {} ({:?}), expected {status}: {:?}",
                current.status,
                current.reason,
                self.control.store().list_deployment_events(deployment).await.unwrap().last().unwrap().message,
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
