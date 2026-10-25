//! Deterministic multi-peer simulator for BetterSync.
//!
//! Provides an in-memory file system ([`MemFs`]), an in-memory network
//! ([`SimNetwork`]) and a seeded RNG ([`SimRng`]). Given the same seed, a
//! simulation run is exactly reproducible.

mod memfs;
mod net;
mod rng;

pub use memfs::MemFs;
pub use net::{Delivery, SimEndpoint, SimNetwork};
pub use rng::SimRng;

#[cfg(test)]
mod tests {
    use std::path::Path;

    use bsync_core::DeviceId;
    use bsync_fs::FileSystem;
    use bsync_net::Transport;

    use super::*;

    /// Two devices with their own file systems; device A ships a few random
    /// files to B over the simulated network. The same seed must give the
    /// same trace.
    fn run(seed: u64) -> Vec<(String, Vec<u8>)> {
        let mut rng = SimRng::new(seed);
        let net = SimNetwork::new();
        let (a, b) = (DeviceId::from_bytes([1; 32]), DeviceId::from_bytes([2; 32]));
        let (fs_a, fs_b) = (MemFs::new(), MemFs::new());
        let ep_a = net.endpoint(a);

        for i in 0..5 {
            let len = rng.index(64);
            let data = rng.bytes(len);
            let name = format!("/f{i}");
            fs_a.create_new(Path::new(&name), &data).unwrap();
            let content = fs_a.read(Path::new(&name)).unwrap();
            let mut msg = name.into_bytes();
            msg.push(0);
            msg.extend(content);
            ep_a.send(b, msg).unwrap();
        }

        let mut trace = Vec::new();
        while let Some(d) = net.deliver_one(&mut rng) {
            assert_eq!((d.from, d.to), (a, b));
            let split = d.msg.iter().position(|&c| c == 0).unwrap();
            let name = String::from_utf8(d.msg[..split].to_vec()).unwrap();
            fs_b.create_new(Path::new(&name), &d.msg[split + 1..])
                .unwrap();
            trace.push((name, d.msg[split + 1..].to_vec()));
        }
        assert_eq!(
            fs_a.snapshot(Path::new("/")).unwrap(),
            fs_b.snapshot(Path::new("/")).unwrap()
        );
        trace
    }

    #[test]
    fn trivial_two_device_run_is_deterministic() {
        assert_eq!(run(99), run(99));
        assert_ne!(run(1), run(2));
    }
}
