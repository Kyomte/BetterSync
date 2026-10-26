//! Keeps paired devices connected: tries every known address (static ones
//! from the config, e.g. Tailscale, and ones found by mDNS), backs off
//! exponentially on failure, prefers the address with the lowest measured
//! round-trip time, and moves an existing connection to a LAN address when
//! one turns up.
//!
//! It sleeps until something can change: a backoff expiring, a connection
//! dropping, new addresses, or the periodic upgrade check.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use bsync_core::DeviceId;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::QuicNet;

const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const DIAL_TIMEOUT: Duration = Duration::from_secs(5);
/// How often a connected device is checked for a better (LAN) address.
const UPGRADE_EVERY: Duration = Duration::from_secs(60);

/// Where an address came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// Announced on the local network via mDNS.
    Lan,
    /// Configured by the user (e.g. a Tailscale address).
    Static,
    /// A machine on the tailnet that may or may not run a paired device;
    /// tried last, the TLS handshake tells.
    Tailnet,
}

#[derive(Debug, Default)]
struct DeviceState {
    static_addrs: Vec<String>,
    discovered: Vec<SocketAddr>,
    /// Tailnet addresses this device was reached at.
    learned: Vec<SocketAddr>,
    failures: u32,
    next_attempt: Option<Instant>,
    next_upgrade: Option<Instant>,
}

#[derive(Debug, Default)]
struct Inner {
    devices: BTreeMap<DeviceId, DeviceState>,
    /// Last measured round-trip time per address.
    rtt: BTreeMap<SocketAddr, Duration>,
    /// Addresses of other machines on the tailnet (from `tailscale status`).
    tailnet: Vec<SocketAddr>,
}

/// Is `ip` on a local network (private, link-local or loopback)? Tailscale's
/// 100.64.0.0/10 counts as remote.
pub fn is_lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique local
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link local
        }
    }
}

/// Order candidate addresses: known-fast first (lowest RTT), then LAN
/// addresses, then the rest in the order given. Duplicates are dropped.
pub fn order_candidates(
    candidates: &[(SocketAddr, Source)],
    rtt: &BTreeMap<SocketAddr, Duration>,
) -> Vec<SocketAddr> {
    let mut seen = Vec::new();
    let mut keyed: Vec<_> = candidates
        .iter()
        .enumerate()
        .filter(|(_, (a, _))| {
            if seen.contains(a) {
                false
            } else {
                seen.push(*a);
                true
            }
        })
        .map(|(i, (a, src))| {
            let measured = rtt.get(a).copied().unwrap_or(Duration::MAX);
            let lan = !(is_lan(a.ip()) || *src == Source::Lan);
            (measured, lan, i, *a)
        })
        .collect();
    keyed.sort();
    keyed.into_iter().map(|(_, _, _, a)| a).collect()
}

/// Exponential backoff after `failures` failed rounds.
pub fn backoff(failures: u32) -> Duration {
    MIN_BACKOFF
        .saturating_mul(1u32 << failures.min(16))
        .min(MAX_BACKOFF)
}

#[derive(Debug)]
pub struct Dialer {
    net: QuicNet,
    inner: Mutex<Inner>,
    wake: Notify,
}

impl Dialer {
    /// Start dialing in the background. Must be called inside a Tokio runtime.
    pub fn start(net: QuicNet) -> Arc<Self> {
        let d = Arc::new(Self {
            net,
            inner: Mutex::new(Inner::default()),
            wake: Notify::new(),
        });
        let run = Arc::clone(&d);
        tokio::spawn(async move { run.run().await });
        d
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Replace the configured devices and their static addresses.
    pub fn set_devices(&self, devices: BTreeMap<DeviceId, Vec<String>>) {
        {
            let mut inner = self.lock();
            inner.devices.retain(|id, _| devices.contains_key(id));
            for (id, addrs) in devices {
                let st = inner.devices.entry(id).or_default();
                if st.static_addrs != addrs {
                    st.static_addrs = addrs;
                    st.failures = 0;
                    st.next_attempt = None;
                }
            }
        }
        self.wake.notify_one();
    }

    /// Addresses announced by `device` on the LAN.
    pub fn discovered(&self, device: DeviceId, addrs: Vec<SocketAddr>) {
        {
            let mut inner = self.lock();
            let Some(st) = inner.devices.get_mut(&device) else {
                return;
            };
            if st.discovered == addrs {
                return;
            }
            st.discovered = addrs;
            st.failures = 0;
            st.next_attempt = None;
            st.next_upgrade = None;
        }
        self.wake.notify_one();
    }

    /// Machines on the tailnet (from `tailscale status`), tried for paired
    /// devices that are not reachable otherwise.
    pub fn set_tailnet(&self, addrs: Vec<SocketAddr>) {
        {
            let mut inner = self.lock();
            if inner.tailnet == addrs {
                return;
            }
            inner.tailnet = addrs;
            for st in inner.devices.values_mut() {
                st.failures = 0;
                st.next_attempt = None;
            }
        }
        self.wake.notify_one();
    }

    /// The network may have changed (a phone app came back to the
    /// foreground): forget the backoff and try every device now.
    pub fn retry_now(&self) {
        {
            let mut inner = self.lock();
            for st in inner.devices.values_mut() {
                st.failures = 0;
                st.next_attempt = None;
            }
        }
        self.wake.notify_one();
    }

    /// A connection dropped (or anything else that warrants a look).
    pub fn poke(&self) {
        self.wake.notify_one();
    }

    async fn candidates(&self, device: DeviceId) -> Vec<SocketAddr> {
        let (statics, discovered, learned, tailnet, rtt) = {
            let inner = self.lock();
            let Some(st) = inner.devices.get(&device) else {
                return Vec::new();
            };
            // Tailnet machines already known to be some other device are
            // not worth a try.
            let others: Vec<SocketAddr> = inner
                .devices
                .iter()
                .filter(|(id, _)| **id != device)
                .flat_map(|(_, d)| d.learned.iter().copied())
                .collect();
            let tailnet: Vec<SocketAddr> = inner
                .tailnet
                .iter()
                .filter(|a| !others.contains(a))
                .copied()
                .collect();
            (
                st.static_addrs.clone(),
                st.discovered.clone(),
                st.learned.clone(),
                tailnet,
                inner.rtt.clone(),
            )
        };
        let mut cands: Vec<(SocketAddr, Source)> =
            discovered.into_iter().map(|a| (a, Source::Lan)).collect();
        for s in statics {
            if let Ok(a) = s.parse::<SocketAddr>() {
                cands.push((a, Source::Static));
            } else if let Ok(found) = tokio::net::lookup_host(s.as_str()).await {
                cands.extend(found.map(|a| (a, Source::Static)));
            }
        }
        cands.extend(learned.into_iter().map(|a| (a, Source::Static)));
        cands.extend(tailnet.into_iter().map(|a| (a, Source::Tailnet)));
        order_candidates(&cands, &rtt)
    }

    async fn try_connect(&self, device: DeviceId, addrs: &[SocketAddr]) -> bool {
        for addr in addrs {
            let attempt = tokio::time::timeout(DIAL_TIMEOUT, self.net.connect(device, *addr)).await;
            match attempt {
                Ok(Ok(())) => {
                    // Give the handshake a moment to register, then record RTT.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let mut inner = self.lock();
                    if let Some(rtt) = self.net.rtt(&device) {
                        inner.rtt.insert(*addr, rtt);
                    }
                    if inner.tailnet.contains(addr)
                        && let Some(st) = inner.devices.get_mut(&device)
                        && !st.learned.contains(addr)
                    {
                        st.learned.push(*addr);
                    }
                    return true;
                }
                Ok(Err(e)) => tracing::debug!(%addr, "dial failed: {e}"),
                Err(_) => tracing::debug!(%addr, "dial timed out"),
            }
        }
        false
    }

    async fn run(self: Arc<Self>) {
        loop {
            let now = Instant::now();
            let devices: Vec<DeviceId> = self.lock().devices.keys().copied().collect();
            let mut next_wake = now + UPGRADE_EVERY;
            for device in devices {
                let connected = self.net.is_connected(&device);
                let (due, upgrade_due) = {
                    let inner = self.lock();
                    let Some(st) = inner.devices.get(&device) else {
                        continue;
                    };
                    (
                        st.next_attempt.is_none_or(|t| t <= now),
                        st.next_upgrade.is_none_or(|t| t <= now),
                    )
                };
                if !connected && due {
                    let addrs = self.candidates(device).await;
                    let ok = !addrs.is_empty() && self.try_connect(device, &addrs).await;
                    let mut inner = self.lock();
                    if let Some(st) = inner.devices.get_mut(&device) {
                        if ok {
                            st.failures = 0;
                            st.next_attempt = None;
                            st.next_upgrade = Some(Instant::now() + UPGRADE_EVERY);
                        } else {
                            let wait = backoff(st.failures);
                            st.failures = st.failures.saturating_add(1);
                            st.next_attempt = Some(Instant::now() + wait);
                        }
                    }
                } else if connected && upgrade_due {
                    // On a remote address while a LAN one is known? Move.
                    let current = self.net.remote_address(&device);
                    let addrs = self.candidates(device).await;
                    let better = addrs.first().copied().filter(|best| {
                        current.is_some_and(|c| !is_lan(c.ip()) && is_lan(best.ip()) && c != *best)
                    });
                    if let Some(best) = better {
                        tracing::info!(%best, "moving connection to a LAN address");
                        self.try_connect(device, &[best]).await;
                    }
                    if let Some(st) = self.lock().devices.get_mut(&device) {
                        st.next_upgrade = Some(Instant::now() + UPGRADE_EVERY);
                    }
                }
                if let Some(st) = self.lock().devices.get(&device) {
                    for t in [st.next_attempt, st.next_upgrade].into_iter().flatten() {
                        next_wake = next_wake.min(t);
                    }
                }
            }
            let changed = self.net.changed();
            tokio::select! {
                () = tokio::time::sleep_until(next_wake.max(Instant::now() + Duration::from_millis(100))) => {}
                () = self.wake.notified() => {}
                () = changed.notified() => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn lan_detection() {
        assert!(is_lan("192.168.1.5".parse().unwrap()));
        assert!(is_lan("10.0.0.1".parse().unwrap()));
        assert!(is_lan("fe80::1".parse().unwrap()));
        assert!(!is_lan("100.101.102.103".parse().unwrap())); // Tailscale
        assert!(!is_lan("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn candidates_prefer_measured_rtt_then_lan() {
        let ts = a("100.64.0.2:22027");
        let lan = a("192.168.1.9:22027");
        let other = a("203.0.113.5:22027");
        let cands = [
            (ts, Source::Static),
            (other, Source::Static),
            (lan, Source::Lan),
            (ts, Source::Static),
        ];
        assert_eq!(
            order_candidates(&cands, &BTreeMap::new()),
            vec![lan, ts, other]
        );
        let rtt = BTreeMap::from([
            (ts, Duration::from_millis(5)),
            (lan, Duration::from_millis(20)),
        ]);
        assert_eq!(order_candidates(&cands, &rtt), vec![ts, lan, other]);
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff(0), Duration::from_secs(1));
        assert_eq!(backoff(3), Duration::from_secs(8));
        assert_eq!(backoff(30), MAX_BACKOFF);
    }
}
