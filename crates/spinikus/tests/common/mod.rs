//! A control plane serving the agent listener on a free port, for tests with real agents.

// Each test file uses a different part of this.
#![allow(dead_code)]

use std::time::Duration;

use spinikus_control::{Control, NewServer, Principal};
use spinikus_core::{
    id::{ServerId, SessionId},
    secret::Secret,
    server::ServerStatus,
    user::UserFields,
};
use spinikus_store::{MasterKey, ServerRepository, SqliteStore, UserRepository, connect, migrate};
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
        let control = Control::new(SqliteStore::open(pool, &MasterKey::generate()).await.unwrap());
        let fields = UserFields { email: "admin@example.com".parse().unwrap(), name: "Admin".to_string(), admin: true };
        let user = control.store().create_user(fields, None).await.unwrap();
        let admin = Principal { user, credential: spinikus_control::Credential::Session(SessionId::new()) };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let url = format!("https://{address}");
        let (stop, stopped) = oneshot::channel::<()>();
        tokio::spawn(spinikus_control::agents::serve(control.clone(), listener, async move {
            let _ = stopped.await;
        }));
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
}
