//! QUIC transport with pinned device IDs.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use bsync_core::{DeviceId, FolderId};
use bsync_net::{Identity, NetEvent, PairedSet, QuicNet};
use bsync_proto::Message;
use tokio::sync::mpsc;

struct Peer {
    net: QuicNet,
    events: mpsc::UnboundedReceiver<NetEvent>,
    paired: PairedSet,
}

impl Peer {
    fn new(name: &str) -> Self {
        Self::with_identity(name, Identity::generate().unwrap())
    }

    fn with_identity(name: &str, identity: Identity) -> Self {
        let paired: PairedSet = Arc::new(RwLock::new(BTreeSet::new()));
        let (tx, events) = mpsc::unbounded_channel();
        let net = QuicNet::bind(
            "127.0.0.1:0".parse().unwrap(),
            identity,
            paired.clone(),
            name.into(),
            Arc::new(|_| vec![FolderId("f".into())]),
            Arc::new(move |e| {
                let _ = tx.send(e);
            }),
        )
        .unwrap();
        Self {
            net,
            events,
            paired,
        }
    }

    fn id(&self) -> DeviceId {
        self.net.device_id()
    }

    fn pair(&self, other: &Peer) {
        self.paired.write().unwrap().insert(other.id());
    }

    async fn next(&mut self) -> Option<NetEvent> {
        tokio::time::timeout(Duration::from_secs(5), self.events.recv())
            .await
            .ok()
            .flatten()
    }

    async fn expect_nothing(&mut self) {
        let got = tokio::time::timeout(Duration::from_millis(500), self.events.recv()).await;
        assert!(got.is_err(), "unexpected event: {got:?}");
    }
}

#[tokio::test]
async fn paired_devices_connect_and_exchange_messages() {
    let mut a = Peer::new("a");
    let mut b = Peer::new("b");
    a.pair(&b);
    b.pair(&a);
    a.net
        .connect(b.id(), b.net.local_addr().unwrap())
        .await
        .unwrap();

    let Some(NetEvent::Connected { peer, name, .. }) = a.next().await else {
        panic!("a did not connect")
    };
    assert_eq!((peer, name.as_str()), (b.id(), "b"));
    let Some(NetEvent::Connected { peer, .. }) = b.next().await else {
        panic!("b did not connect")
    };
    assert_eq!(peer, a.id());

    let ta = a.net.transport();
    ta.send(b.id(), Message::Ping.encode()).unwrap();
    ta.send(b.id(), Message::Pong.encode()).unwrap();
    assert_eq!(
        b.next().await,
        Some(NetEvent::Message {
            peer: a.id(),
            msg: Message::Ping
        })
    );
    assert_eq!(
        b.next().await,
        Some(NetEvent::Message {
            peer: a.id(),
            msg: Message::Pong
        })
    );
    b.net
        .transport()
        .send(a.id(), Message::Ping.encode())
        .unwrap();
    assert_eq!(
        a.next().await,
        Some(NetEvent::Message {
            peer: b.id(),
            msg: Message::Ping
        })
    );
    assert!(a.net.rtt(&b.id()).is_some());

    a.net.disconnect(&b.id());
    assert_eq!(
        a.next().await,
        Some(NetEvent::Disconnected { peer: b.id() })
    );
    assert_eq!(
        b.next().await,
        Some(NetEvent::Disconnected { peer: a.id() })
    );
}

#[tokio::test]
async fn unpaired_peer_is_rejected_both_ways() {
    let mut a = Peer::new("a");
    let mut b = Peer::new("b");
    // Only a trusts b.
    a.pair(&b);

    // a → b: b refuses a's client certificate.
    let r = a.net.connect(b.id(), b.net.local_addr().unwrap()).await;
    // The handshake may complete on a's side before b's rejection arrives;
    // either way no session is established.
    if r.is_ok() {
        a.next().await; // possibly a Disconnected
    }
    b.expect_nothing().await;
    assert!(!b.net.is_connected(&a.id()));

    // b → a: b will not even dial an unpaired device.
    assert!(
        b.net
            .connect(a.id(), a.net.local_addr().unwrap())
            .await
            .is_err()
    );
    a.expect_nothing().await;
    assert!(!a.net.is_connected(&b.id()));
}

#[tokio::test]
async fn impostor_at_expected_address_is_rejected() {
    let a = Peer::new("a");
    let mut b = Peer::new("b");
    let c = Peer::new("c");
    a.pair(&b);
    a.pair(&c);
    b.pair(&a);
    c.pair(&a);
    // a wants to reach c but dials b's address: b's certificate is not c's.
    assert!(
        a.net
            .connect(c.id(), b.net.local_addr().unwrap())
            .await
            .is_err()
    );
    b.expect_nothing().await;
}

/// Regression: a peer that disappeared without closing its connection (crash,
/// killed process) and came back was refused as a "duplicate" until the old
/// connection timed out.
#[test]
fn restarted_peer_is_accepted_while_old_connection_lingers() {
    let main = tokio::runtime::Runtime::new().unwrap();
    let a_identity = Identity::generate().unwrap();
    // The bug only showed when the restarted peer, as the dialer, was not
    // the "preferred" side of the duplicate rule: a < b.
    let key = loop {
        let k = Identity::generate().unwrap();
        if a_identity.device_id() < k.device_id() {
            break k.pkcs8().to_vec();
        }
    };
    let mut a = main.block_on(async { Peer::with_identity("a", a_identity) });
    let a_addr = a.net.local_addr().unwrap();

    // b runs on its own runtime, which we kill without any shutdown.
    let crashed = tokio::runtime::Runtime::new().unwrap();
    let b = crashed.block_on(async {
        let b = Peer::with_identity("b", Identity::from_pkcs8(&key).unwrap());
        b.pair(&a);
        b
    });
    a.pair(&b);
    let b_id = b.id();
    crashed.block_on(b.net.connect(a.id(), a_addr)).unwrap();
    main.block_on(async {
        assert!(matches!(a.next().await, Some(NetEvent::Connected { .. })));
    });
    drop(b);
    crashed.shutdown_background();

    // b comes back with the same identity and dials again.
    main.block_on(async {
        let b2 = Peer::with_identity("b", Identity::from_pkcs8(&key).unwrap());
        b2.pair(&a);
        assert_eq!(b2.id(), b_id);
        // Let the dial-race window pass, as a real restart would.
        tokio::time::sleep(Duration::from_millis(5100)).await;
        b2.net.connect(a.id(), a_addr).await.unwrap();
        let mut connected = false;
        while let Some(e) = a.next().await {
            if matches!(e, NetEvent::Connected { peer, .. } if peer == b_id) {
                connected = true;
                break;
            }
        }
        assert!(connected, "a refused the restarted peer");
        assert!(a.net.is_connected(&b_id));
    });
}

#[tokio::test]
async fn dialer_tries_addresses_until_one_works() {
    let mut a = Peer::new("a");
    let mut b = Peer::new("b");
    a.pair(&b);
    b.pair(&a);
    let dialer = bsync_net::Dialer::start(a.net.clone());
    // A dead address first (nothing listens there), then the real one.
    let dead = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    dialer.set_devices(
        [(
            b.id(),
            vec![dead.to_string(), b.net.local_addr().unwrap().to_string()],
        )]
        .into(),
    );
    // The dead address costs one dial timeout (5 s) before the real one.
    let first = tokio::time::timeout(Duration::from_secs(15), b.events.recv()).await;
    assert!(
        matches!(first, Ok(Some(NetEvent::Connected { .. }))),
        "{first:?}"
    );
    assert!(matches!(a.next().await, Some(NetEvent::Connected { .. })));

    // When the connection drops, the dialer reconnects.
    b.net.disconnect(&a.id());
    let mut reconnected = false;
    for _ in 0..6 {
        let e = tokio::time::timeout(Duration::from_secs(15), a.events.recv()).await;
        if let Ok(Some(NetEvent::Connected { .. })) = e {
            reconnected = true;
            break;
        }
    }
    assert!(reconnected);
}
