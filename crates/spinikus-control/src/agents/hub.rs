//! The connected agents, and the channel to each one's control stream.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use spinikus_core::id::ServerId;
use spinikus_proto::agent::v1::ControlMessage;
use tokio::sync::{mpsc, oneshot};

/// Messages queued for an agent before sending blocks.
pub(crate) const OUTBOUND_BUFFER: usize = 64;

/// The agents connected to this control plane. Cheap to clone; clones share the registry.
#[derive(Clone, Default)]
pub struct AgentHub {
    connections: Arc<Mutex<HashMap<ServerId, Connection>>>,
    /// Held while a connection starts or ends, across both the registry change and the
    /// database write, so a reconnect can never be overwritten by the old connection's disconnect.
    transitions: Arc<tokio::sync::Mutex<()>>,
}

struct Connection {
    id: ConnectionId,
    sender: mpsc::Sender<ControlMessage>,
    /// Taken and dropped to tell the connection's reader to stop, which then unregisters
    /// the connection (closing its stream) and records the disconnect.
    stop: Option<oneshot::Sender<()>>,
}

/// Tells apart successive connections from the same server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConnectionId(u64);

/// The server's agent is not connected to this control plane.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the agent of server `{0}` is not connected")]
pub struct NotConnected(pub ServerId);

impl AgentHub {
    pub(crate) async fn transition(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.transitions.lock().await
    }

    /// Registers a new connection from `server`, replacing (and closing) any earlier one. The
    /// receiver resolves when this connection is replaced or disconnected.
    pub(crate) fn register(
        &self,
        server: ServerId,
        sender: mpsc::Sender<ControlMessage>,
    ) -> (ConnectionId, oneshot::Receiver<()>) {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = ConnectionId(NEXT_ID.fetch_add(1, Ordering::Relaxed));
        let (stop, stopped) = oneshot::channel();
        let replaced = self.lock().insert(server, Connection { id, sender, stop: Some(stop) });
        if replaced.is_some() {
            tracing::info!(server = %server, "agent reconnected, closing its previous connection");
        }
        (id, stopped)
    }

    /// Removes the connection when it ends. Returns false if a newer connection from the
    /// same server has already replaced it, so the server is still connected.
    pub(crate) fn unregister(&self, server: ServerId, id: ConnectionId) -> bool {
        let mut connections = self.lock();
        match connections.get(&server) {
            Some(connection) if connection.id == id => {
                connections.remove(&server);
                true
            }
            _ => false,
        }
    }

    /// Closes the server's connection, such as when the server is revoked. Returns whether it was connected.
    pub fn disconnect(&self, server: ServerId) -> bool {
        self.lock().get_mut(&server).is_some_and(|connection| connection.stop.take().is_some())
    }

    /// Closes every connection, such as when the control plane shuts down.
    pub fn disconnect_all(&self) {
        for connection in self.lock().values_mut() {
            connection.stop.take();
        }
    }

    pub fn is_connected(&self, server: ServerId) -> bool {
        self.lock().contains_key(&server)
    }

    pub fn connected_servers(&self) -> Vec<ServerId> {
        let mut servers: Vec<_> = self.lock().keys().copied().collect();
        servers.sort();
        servers
    }

    /// Queues a message on the server's control stream, waiting while its buffer is full.
    pub async fn send(&self, server: ServerId, message: ControlMessage) -> Result<(), NotConnected> {
        let sender = self.lock().get(&server).map(|connection| connection.sender.clone()).ok_or(NotConnected(server))?;
        sender.send(message).await.map_err(|_| NotConnected(server))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ServerId, Connection>> {
        self.connections.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
