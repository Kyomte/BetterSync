//! LAN discovery over mDNS: every device announces `_bsync._udp.local.`
//! with its device ID in a TXT record, and listens for the others.
//! Discovery only suggests addresses; the TLS handshake still decides who
//! is accepted (invariant 6).

use std::collections::BTreeSet;
use std::net::SocketAddr;

use bsync_core::DeviceId;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

use crate::NetError;

pub const SERVICE: &str = "_bsync._udp.local.";

/// Announces this device and reports others until dropped.
pub struct Discovery {
    daemon: ServiceDaemon,
}

impl std::fmt::Debug for Discovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Discovery")
    }
}

impl Discovery {
    /// Announce `me` on `port` and call `found` (on a background thread)
    /// with each other device's ID and addresses.
    pub fn start(
        me: DeviceId,
        port: u16,
        found: impl Fn(DeviceId, Vec<SocketAddr>) + Send + 'static,
    ) -> Result<Self, NetError> {
        let mdns = |e: mdns_sd::Error| NetError::Connect(format!("mDNS: {e}"));
        let daemon = ServiceDaemon::new().map_err(mdns)?;
        // Instance labels are limited to 63 bytes: use the undashed ID.
        let instance: String = me.to_string().replace('-', "");
        let host = format!("bsync-{}.local.", instance[..16].to_lowercase());
        let id = me.to_string();
        let info = ServiceInfo::new(
            SERVICE,
            &instance,
            &host,
            "",
            port,
            &[("id", id.as_str())][..],
        )
        .map_err(mdns)?
        .enable_addr_auto();
        daemon.register(info).map_err(mdns)?;
        let events = daemon.browse(SERVICE).map_err(mdns)?;
        std::thread::Builder::new()
            .name("bsync-mdns".into())
            .spawn(move || {
                while let Ok(event) = events.recv() {
                    let ServiceEvent::ServiceResolved(svc) = event else {
                        continue;
                    };
                    let Some(peer) = svc
                        .txt_properties
                        .get_property_val_str("id")
                        .and_then(|s| s.parse::<DeviceId>().ok())
                    else {
                        continue;
                    };
                    if peer == me {
                        continue;
                    }
                    let addrs: BTreeSet<SocketAddr> = svc
                        .addresses
                        .iter()
                        .map(|ip| SocketAddr::new(ip.to_ip_addr(), svc.port))
                        .collect();
                    if !addrs.is_empty() {
                        found(peer, addrs.into_iter().collect());
                    }
                }
            })?;
        Ok(Self { daemon })
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}
