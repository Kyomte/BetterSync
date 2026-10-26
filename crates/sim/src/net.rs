//! In-memory network for the simulator.
//!
//! Each ordered pair of devices is a FIFO link, like one QUIC stream: messages
//! between two devices arrive in order, but which link delivers next is up to
//! the seeded RNG. Partitioning a pair drops everything in flight on it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use bsync_core::DeviceId;
use bsync_net::{NetError, Transport};

use crate::SimRng;

#[derive(Debug, Default)]
struct Inner {
    links: BTreeMap<(DeviceId, DeviceId), VecDeque<Vec<u8>>>,
    /// Unordered pairs (smaller id first) that cannot talk.
    down: BTreeSet<(DeviceId, DeviceId)>,
}

fn pair(a: DeviceId, b: DeviceId) -> (DeviceId, DeviceId) {
    if a <= b { (a, b) } else { (b, a) }
}

/// The shared network. Clones refer to the same network.
#[derive(Debug, Clone, Default)]
pub struct SimNetwork {
    inner: Arc<Mutex<Inner>>,
}

/// A message taken off the network, ready to hand to the recipient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub from: DeviceId,
    pub to: DeviceId,
    pub msg: Vec<u8>,
}

impl SimNetwork {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The sending side of the network for device `id`.
    pub fn endpoint(&self, id: DeviceId) -> SimEndpoint {
        SimEndpoint {
            id,
            net: self.clone(),
        }
    }

    pub fn is_connected(&self, a: DeviceId, b: DeviceId) -> bool {
        a != b && !self.lock().down.contains(&pair(a, b))
    }

    /// Cut the link between `a` and `b`, dropping messages in flight.
    pub fn partition(&self, a: DeviceId, b: DeviceId) {
        let mut inner = self.lock();
        inner.down.insert(pair(a, b));
        inner.links.remove(&(a, b));
        inner.links.remove(&(b, a));
    }

    pub fn heal(&self, a: DeviceId, b: DeviceId) {
        self.lock().down.remove(&pair(a, b));
    }

    /// Number of messages in flight.
    pub fn in_flight(&self) -> usize {
        self.lock().links.values().map(VecDeque::len).sum()
    }

    /// Take the next message off a randomly chosen non-empty link.
    pub fn deliver_one(&self, rng: &mut SimRng) -> Option<Delivery> {
        let mut inner = self.lock();
        let busy: Vec<_> = inner
            .links
            .iter()
            .filter(|(_, q)| !q.is_empty())
            .map(|(k, _)| *k)
            .collect();
        if busy.is_empty() {
            return None;
        }
        let (from, to) = busy[rng.index(busy.len())];
        let queue = inner.links.get_mut(&(from, to))?;
        let msg = queue.pop_front()?;
        if queue.is_empty() {
            inner.links.remove(&(from, to));
        }
        Some(Delivery { from, to, msg })
    }
}

/// One device's handle for sending on a [`SimNetwork`].
#[derive(Debug, Clone)]
pub struct SimEndpoint {
    id: DeviceId,
    net: SimNetwork,
}

impl Transport for SimEndpoint {
    fn local_id(&self) -> DeviceId {
        self.id
    }

    fn send(&self, to: DeviceId, msg: Vec<u8>) -> Result<(), NetError> {
        if !self.net.is_connected(self.id, to) {
            return Err(NetError::Unreachable(to));
        }
        self.net
            .lock()
            .links
            .entry((self.id, to))
            .or_default()
            .push_back(msg);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> DeviceId {
        DeviceId::from_bytes([n; 32])
    }

    #[test]
    fn links_are_fifo_and_partitions_drop() {
        let net = SimNetwork::new();
        let a = net.endpoint(id(1));
        let mut rng = SimRng::new(1);
        a.send(id(2), b"1".to_vec()).unwrap();
        a.send(id(2), b"2".to_vec()).unwrap();
        assert_eq!(net.deliver_one(&mut rng).unwrap().msg, b"1");
        assert_eq!(net.deliver_one(&mut rng).unwrap().msg, b"2");
        assert!(net.deliver_one(&mut rng).is_none());

        a.send(id(2), b"lost".to_vec()).unwrap();
        net.partition(id(2), id(1));
        assert_eq!(net.in_flight(), 0);
        assert!(a.send(id(2), b"x".to_vec()).is_err());
        net.heal(id(1), id(2));
        a.send(id(2), b"ok".to_vec()).unwrap();
        assert_eq!(net.in_flight(), 1);
    }
}
