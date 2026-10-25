//! QUIC transport between paired devices.
//!
//! One connection per peer; each side opens one unidirectional stream and
//! sends length-prefixed [`Message`] frames on it, starting with `Hello`.
//! Incoming traffic is reported as [`NetEvent`]s on a channel; outgoing
//! messages go through [`QuicNet::transport`].

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bsync_core::{DeviceId, FolderId};
use bsync_proto::{Message, PROTOCOL_VERSION, frame_len};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use tokio::sync::mpsc;

use crate::tls::{PairedSet, client_config, device_id_from_cert, server_config};
use crate::{Identity, NetError, Transport};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetEvent {
    /// A paired peer finished the handshake and said hello.
    Connected {
        peer: DeviceId,
        name: String,
        addr: SocketAddr,
    },
    Disconnected {
        peer: DeviceId,
    },
    Message {
        peer: DeviceId,
        msg: Message,
    },
}

/// Something that wants [`NetEvent`]s.
pub type EventSink = Arc<dyn Fn(NetEvent) + Send + Sync>;

/// What to tell a peer in our `Hello`.
pub type FoldersFor = Arc<dyn Fn(DeviceId) -> Vec<FolderId> + Send + Sync>;

struct Conn {
    conn: quinn::Connection,
    tx: mpsc::UnboundedSender<Vec<u8>>,
    since: Instant,
}

/// Two connections to the same peer within this window are a simultaneous
/// dial; outside it, the newer one means the peer reconnected (e.g. after a
/// restart whose close never reached us).
const DIAL_RACE_WINDOW: Duration = Duration::from_secs(5);

struct Inner {
    identity: Identity,
    paired: PairedSet,
    endpoint: quinn::Endpoint,
    conns: Mutex<BTreeMap<DeviceId, Conn>>,
    events: EventSink,
    name: String,
    folders_for: FoldersFor,
}

/// A QUIC endpoint that both accepts and makes connections.
#[derive(Clone)]
pub struct QuicNet {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for QuicNet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuicNet")
            .field("device", &self.inner.identity.device_id())
            .finish_non_exhaustive()
    }
}

fn transport_config() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(10)));
    if let Ok(idle) = quinn::IdleTimeout::try_from(Duration::from_secs(30)) {
        t.max_idle_timeout(Some(idle));
    }
    Arc::new(t)
}

impl QuicNet {
    /// Listen on `addr` (port 0 picks a free port). Must be called inside a
    /// Tokio runtime.
    pub fn bind(
        addr: SocketAddr,
        identity: Identity,
        paired: PairedSet,
        name: String,
        folders_for: FoldersFor,
        events: EventSink,
    ) -> Result<Self, NetError> {
        let tls = server_config(&identity, &paired)?;
        let quic = QuicServerConfig::try_from(tls).map_err(NetError::tls)?;
        let mut server = quinn::ServerConfig::with_crypto(Arc::new(quic));
        server.transport_config(transport_config());
        let endpoint = quinn::Endpoint::server(server, addr)?;
        let net = Self {
            inner: Arc::new(Inner {
                identity,
                paired,
                endpoint,
                conns: Mutex::new(BTreeMap::new()),
                events,
                name,
                folders_for,
            }),
        };
        let acceptor = net.clone();
        tokio::spawn(async move { acceptor.accept_loop().await });
        Ok(net)
    }

    pub fn local_addr(&self) -> Result<SocketAddr, NetError> {
        Ok(self.inner.endpoint.local_addr()?)
    }

    pub fn device_id(&self) -> DeviceId {
        self.inner.identity.device_id()
    }

    pub fn paired(&self) -> &PairedSet {
        &self.inner.paired
    }

    fn conns(&self) -> MutexGuard<'_, BTreeMap<DeviceId, Conn>> {
        self.inner.conns.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_connected(&self, peer: &DeviceId) -> bool {
        self.conns().contains_key(peer)
    }

    pub fn connected_peers(&self) -> Vec<DeviceId> {
        self.conns().keys().copied().collect()
    }

    /// Round-trip time to a connected peer.
    pub fn rtt(&self, peer: &DeviceId) -> Option<Duration> {
        self.conns().get(peer).map(|c| c.conn.rtt())
    }

    /// Drop the connection to `peer` (e.g. after unpairing).
    pub fn disconnect(&self, peer: &DeviceId) {
        let removed = self.conns().remove(peer);
        if let Some(c) = removed {
            c.conn.close(0u32.into(), b"bye");
            (self.inner.events)(NetEvent::Disconnected { peer: *peer });
        }
    }

    /// The sending side, for the engines.
    pub fn transport(&self) -> Arc<dyn Transport> {
        Arc::new(QuicTransport { net: self.clone() })
    }

    /// Close every connection and wait (briefly) until peers were told.
    pub async fn shutdown(&self) {
        self.inner.endpoint.close(0u32.into(), b"shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(1), self.inner.endpoint.wait_idle()).await;
    }

    async fn accept_loop(self) {
        while let Some(incoming) = self.inner.endpoint.accept().await {
            let net = self.clone();
            tokio::spawn(async move {
                match incoming.await {
                    Ok(conn) => net.run_connection(conn, false).await,
                    Err(e) => tracing::debug!("incoming connection failed: {e}"),
                }
            });
        }
    }

    /// Connect to `peer` at `addr`. Fails unless the remote end proves it is
    /// `peer` and `peer` is paired.
    pub async fn connect(&self, peer: DeviceId, addr: SocketAddr) -> Result<(), NetError> {
        if !self
            .inner
            .paired
            .read()
            .map(|p| p.contains(&peer))
            .unwrap_or(false)
        {
            return Err(NetError::NotPaired(peer));
        }
        let tls = client_config(&self.inner.identity, &self.inner.paired, peer)?;
        let quic = QuicClientConfig::try_from(tls).map_err(NetError::tls)?;
        let mut client = quinn::ClientConfig::new(Arc::new(quic));
        client.transport_config(transport_config());
        let conn = self
            .inner
            .endpoint
            .connect_with(client, addr, "bsync")
            .map_err(|e| NetError::Connect(e.to_string()))?
            .await
            .map_err(|e| NetError::Connect(e.to_string()))?;
        let net = self.clone();
        tokio::spawn(async move { net.run_connection(conn, true).await });
        Ok(())
    }

    fn peer_of(conn: &quinn::Connection) -> Option<DeviceId> {
        let certs = conn
            .peer_identity()?
            .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
            .ok()?;
        device_id_from_cert(certs.first()?)
    }

    /// With two connections between the same pair (both dialed at once),
    /// both sides keep the one dialed by the smaller device ID.
    fn preferred(&self, peer: DeviceId, initiated_by_me: bool) -> bool {
        initiated_by_me == (self.device_id() < peer)
    }

    async fn run_connection(self, conn: quinn::Connection, initiated_by_me: bool) {
        let Some(peer) = Self::peer_of(&conn) else {
            conn.close(1u32.into(), b"no identity");
            return;
        };
        // The TLS verifier already checked this; check again in case the
        // pairing changed during the handshake.
        if !self
            .inner
            .paired
            .read()
            .map(|p| p.contains(&peer))
            .unwrap_or(false)
        {
            conn.close(1u32.into(), b"not paired");
            return;
        }
        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        {
            let mut conns = self.conns();
            if let Some(existing) = conns.get(&peer) {
                let racing = existing.conn.close_reason().is_none()
                    && existing.since.elapsed() < DIAL_RACE_WINDOW;
                if racing && !self.preferred(peer, initiated_by_me) {
                    conn.close(0u32.into(), b"duplicate");
                    return;
                }
                if let Some(old) = conns.remove(&peer) {
                    old.conn.close(0u32.into(), b"replaced");
                    (self.inner.events)(NetEvent::Disconnected { peer });
                }
            }
            conns.insert(
                peer,
                Conn {
                    conn: conn.clone(),
                    tx: tx.clone(),
                    since: Instant::now(),
                },
            );
        }
        let hello = Message::Hello {
            protocol_version: PROTOCOL_VERSION,
            device_id: self.device_id(),
            device_name: self.inner.name.clone(),
            folders: (self.inner.folders_for)(peer),
        };
        let _ = tx.send(hello.encode());

        let writer = tokio::spawn(write_loop(conn.clone(), rx));
        let result = self.read_loop(&conn, peer).await;
        if let Err(e) = result {
            tracing::debug!("connection to {peer:?} ended: {e}");
        }
        writer.abort();
        conn.close(0u32.into(), b"closed");
        let was_current = {
            let mut conns = self.conns();
            let current = conns
                .get(&peer)
                .is_some_and(|c| c.conn.stable_id() == conn.stable_id());
            if current {
                conns.remove(&peer);
            }
            current
        };
        if was_current {
            (self.inner.events)(NetEvent::Disconnected { peer });
        }
    }

    async fn read_loop(&self, conn: &quinn::Connection, peer: DeviceId) -> Result<(), NetError> {
        let mut stream = conn
            .accept_uni()
            .await
            .map_err(|e| NetError::Connect(e.to_string()))?;
        let mut hello_seen = false;
        loop {
            let mut header = [0u8; 4];
            stream
                .read_exact(&mut header)
                .await
                .map_err(|e| NetError::Connect(e.to_string()))?;
            let len = frame_len(header).map_err(|e| NetError::Protocol(e.to_string()))?;
            let mut body = vec![0u8; len];
            stream
                .read_exact(&mut body)
                .await
                .map_err(|e| NetError::Connect(e.to_string()))?;
            let msg = Message::decode(&body).map_err(|e| NetError::Protocol(e.to_string()))?;
            if !hello_seen {
                let Message::Hello {
                    protocol_version,
                    device_id,
                    device_name,
                    ..
                } = msg
                else {
                    return Err(NetError::Protocol("expected Hello".into()));
                };
                if protocol_version != PROTOCOL_VERSION {
                    return Err(NetError::Protocol(format!(
                        "peer speaks protocol {protocol_version}, we speak {PROTOCOL_VERSION}"
                    )));
                }
                if device_id != peer {
                    return Err(NetError::Protocol("Hello names another device".into()));
                }
                hello_seen = true;
                (self.inner.events)(NetEvent::Connected {
                    peer,
                    name: device_name,
                    addr: conn.remote_address(),
                });
                continue;
            }
            (self.inner.events)(NetEvent::Message { peer, msg });
        }
    }
}

async fn write_loop(conn: quinn::Connection, mut rx: mpsc::UnboundedReceiver<Vec<u8>>) {
    let Ok(mut stream) = conn.open_uni().await else {
        return;
    };
    while let Some(body) = rx.recv().await {
        let mut frame = Vec::with_capacity(4 + body.len());
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(&body);
        if stream.write_all(&frame).await.is_err() {
            return;
        }
    }
}

/// [`Transport`] over a [`QuicNet`].
#[derive(Debug, Clone)]
pub struct QuicTransport {
    net: QuicNet,
}

impl Transport for QuicTransport {
    fn local_id(&self) -> DeviceId {
        self.net.device_id()
    }

    fn send(&self, to: DeviceId, msg: Vec<u8>) -> Result<(), NetError> {
        let conns = self.net.conns();
        let conn = conns.get(&to).ok_or(NetError::Unreachable(to))?;
        conn.tx.send(msg).map_err(|_| NetError::Unreachable(to))
    }
}
