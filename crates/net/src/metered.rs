//! Is the current network metered (mobile data, a phone hotspot, roaming)?
//!
//! - Windows: the internet connection profile's `NetworkCostType`
//!   (WinRT), re-read whenever Windows reports a network change.
//! - macOS: `NWPathMonitor` (Network.framework): `isExpensive` or
//!   `isConstrained` (Low Data Mode).
//! - Elsewhere there is no detection; the configuration decides.
#![allow(unsafe_code)] // the macOS backend calls Network.framework's C API

/// Windows `NetworkCostType`: Unknown 0, Unrestricted 1, Fixed 2, Variable 3.
/// Fixed and variable plans are metered, and so is roaming or being over
/// the data limit.
pub fn is_metered_cost(cost_type: i32, roaming: bool, over_data_limit: bool) -> bool {
    matches!(cost_type, 2 | 3) || roaming || over_data_limit
}

/// Watches for changes until dropped.
pub struct MeteredWatcher {
    #[cfg(windows)]
    token: i64,
    #[cfg(target_os = "macos")]
    _monitor: macos::Monitor,
}

impl std::fmt::Debug for MeteredWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MeteredWatcher")
    }
}

/// Call `on_change` now and whenever the metered state may have changed.
/// `None` where the platform offers no detection.
#[cfg(windows)]
pub fn watch(on_change: impl Fn(bool) + Send + Sync + 'static) -> Option<MeteredWatcher> {
    use windows::Networking::Connectivity::{NetworkInformation, NetworkStatusChangedEventHandler};

    fn query() -> bool {
        let Ok(profile) = NetworkInformation::GetInternetConnectionProfile() else {
            return false; // offline
        };
        let Ok(cost) = profile.GetConnectionCost() else {
            return false;
        };
        let cost_type = cost.NetworkCostType().map_or(0, |t| t.0);
        is_metered_cost(
            cost_type,
            cost.Roaming().unwrap_or(false),
            cost.OverDataLimit().unwrap_or(false),
        )
    }

    on_change(query());
    let handler = NetworkStatusChangedEventHandler::new(move |_| {
        on_change(query());
        Ok(())
    });
    let token = NetworkInformation::NetworkStatusChanged(&handler).ok()?;
    Some(MeteredWatcher { token })
}

#[cfg(windows)]
impl Drop for MeteredWatcher {
    fn drop(&mut self) {
        let _ = windows::Networking::Connectivity::NetworkInformation::RemoveNetworkStatusChanged(
            self.token,
        );
    }
}

#[cfg(target_os = "macos")]
pub fn watch(on_change: impl Fn(bool) + Send + Sync + 'static) -> Option<MeteredWatcher> {
    macos::Monitor::start(on_change).map(|monitor| MeteredWatcher { _monitor: monitor })
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn watch(_on_change: impl Fn(bool) + Send + Sync + 'static) -> Option<MeteredWatcher> {
    None
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{c_char, c_void};

    use block2::RcBlock;

    type NwPath = *mut c_void;
    type NwPathMonitor = *mut c_void;
    type DispatchQueue = *mut c_void;

    #[link(name = "Network", kind = "framework")]
    unsafe extern "C" {
        fn nw_path_monitor_create() -> NwPathMonitor;
        fn nw_path_monitor_set_queue(monitor: NwPathMonitor, queue: DispatchQueue);
        fn nw_path_monitor_set_update_handler(
            monitor: NwPathMonitor,
            handler: &block2::Block<dyn Fn(NwPath)>,
        );
        fn nw_path_monitor_start(monitor: NwPathMonitor);
        fn nw_path_monitor_cancel(monitor: NwPathMonitor);
        fn nw_path_is_expensive(path: NwPath) -> bool;
        fn nw_path_is_constrained(path: NwPath) -> bool;
        fn nw_release(obj: *mut c_void);
    }

    // libdispatch is part of libSystem, which every program links.
    unsafe extern "C" {
        fn dispatch_queue_create(label: *const c_char, attr: *mut c_void) -> DispatchQueue;
    }

    pub struct Monitor(NwPathMonitor);

    // The monitor is only touched through Network.framework calls, which
    // are thread-safe.
    unsafe impl Send for Monitor {}
    unsafe impl Sync for Monitor {}

    impl Monitor {
        pub fn start(on_change: impl Fn(bool) + Send + Sync + 'static) -> Option<Self> {
            // SAFETY: plain C calls with valid arguments; the block is
            // copied by Network.framework, and `path` is valid for the
            // duration of the callback.
            unsafe {
                let monitor = nw_path_monitor_create();
                if monitor.is_null() {
                    return None;
                }
                let queue =
                    dispatch_queue_create(c"com.bettersync.metered".as_ptr(), std::ptr::null_mut());
                nw_path_monitor_set_queue(monitor, queue);
                let block = RcBlock::new(move |path: NwPath| {
                    let metered = !path.is_null()
                        && (nw_path_is_expensive(path) || nw_path_is_constrained(path));
                    on_change(metered);
                });
                nw_path_monitor_set_update_handler(monitor, &block);
                nw_path_monitor_start(monitor);
                Some(Self(monitor))
            }
        }
    }

    impl Drop for Monitor {
        fn drop(&mut self) {
            // SAFETY: the monitor was created by `start` and is released once.
            unsafe {
                nw_path_monitor_cancel(self.0);
                nw_release(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_types() {
        assert!(!is_metered_cost(0, false, false)); // unknown: assume not
        assert!(!is_metered_cost(1, false, false)); // unrestricted
        assert!(is_metered_cost(2, false, false)); // fixed plan
        assert!(is_metered_cost(3, false, false)); // pay per byte
        assert!(is_metered_cost(1, true, false)); // roaming
        assert!(is_metered_cost(1, false, true)); // over the data limit
    }
}
