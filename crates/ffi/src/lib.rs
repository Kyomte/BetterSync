//! Embedding the BetterSync daemon in an app (stretch goal S3).
//!
//! The app starts the daemon with a config directory, then talks to it
//! exactly like the CLI does: over the local control API (or by showing the
//! status page the daemon serves). So the foreign interface stays tiny:
//! start, stop, and where the API is.
//!
//! Two flavours of the same functions: a C ABI (`bsync_*`, declared in
//! `include/bsync.h`) for any host language (the iOS app uses it from
//! Swift), and JNI entry points for the Android app (`com.bettersync.Native`).
#![allow(unsafe_code)] // FFI boundary

use std::ffi::{CStr, CString, c_char};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use bsyncd::Daemon;

/// The running daemon (one per process).
static DAEMON: Mutex<Option<Daemon>> = Mutex::new(None);

/// How an app wants the daemon set up.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Folders paired devices share appear here (`<root>/<folder id>`)
    /// without any setup on the phone.
    pub folders_root: Option<PathBuf>,
    /// The app does LAN discovery itself and reports it with
    /// [`discovered`] (iOS: raw multicast needs a special entitlement,
    /// Bonjour through the system does not).
    pub app_discovery: bool,
}

/// Start the daemon with `config_dir` (created if needed). Returns the
/// status page URL (with the API token in its fragment).
pub fn start(config_dir: &Path) -> Result<String, String> {
    start_with(config_dir, &Options::default())
}

pub fn start_with(config_dir: &Path, options: &Options) -> Result<String, String> {
    let mut slot = DAEMON.lock().map_err(|e| e.to_string())?;
    if slot.is_none() {
        std::fs::create_dir_all(config_dir).map_err(|e| e.to_string())?;
        let mut cfg = bsyncd::Config::load_or_default(config_dir).map_err(|e| format!("{e:#}"))?;
        // A phone has no reason to expose the API beyond loopback, and a
        // fixed port could collide with another app.
        cfg.api_listen = "127.0.0.1:0".into();
        if let Some(root) = &options.folders_root {
            std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
            cfg.always_on.auto_accept_folders = true;
            cfg.always_on.folders_root = Some(root.clone());
            relocate_folders(&mut cfg, root);
        }
        if options.app_discovery {
            cfg.local_discovery = false;
        }
        cfg.save(config_dir).map_err(|e| format!("{e:#}"))?;
        *slot = Some(Daemon::start(config_dir).map_err(|e| format!("{e:#}"))?);
    }
    status_page_url(config_dir)
}

/// iOS may move the app's container on an update, which changes every
/// absolute path in it: a folder whose path is gone but whose name exists
/// under the current `root` moved with it.
fn relocate_folders(cfg: &mut bsyncd::Config, root: &Path) {
    for f in &mut cfg.folders {
        if f.path.exists() {
            continue;
        }
        let Some(name) = f.path.file_name() else {
            continue;
        };
        let moved = root.join(name);
        if moved.join(bsync_fs::BSYNC_DIR).is_dir() {
            tracing::info!(folder = %f.id, to = %moved.display(), "folder moved with the app");
            f.path = moved;
        }
    }
}

/// LAN addresses of `device`, found by the app (see
/// [`Options::app_discovery`]).
pub fn discovered(device: &str, addrs: &str) -> Result<(), String> {
    let device: bsync_core::DeviceId = device.parse().map_err(|_| "bad device ID")?;
    let addrs: Vec<SocketAddr> = addrs
        .split(',')
        .filter_map(|a| a.trim().parse().ok())
        .collect();
    if addrs.is_empty() {
        return Err("no usable address".into());
    }
    let slot = DAEMON.lock().map_err(|e| e.to_string())?;
    slot.as_ref()
        .ok_or("not running")?
        .discovered(device, addrs);
    Ok(())
}

/// Reconnect now (the app came back to the foreground, the network
/// changed).
pub fn network_changed() {
    if let Ok(slot) = DAEMON.lock()
        && let Some(d) = slot.as_ref()
    {
        d.network_changed();
    }
}

/// Stop the daemon if it runs.
pub fn stop() {
    if let Ok(mut slot) = DAEMON.lock() {
        slot.take();
    }
}

/// `http://127.0.0.1:<port>/#token=<token>` for the running daemon.
pub fn status_page_url(config_dir: &Path) -> Result<String, String> {
    let addr =
        std::fs::read_to_string(bsyncd::api::addr_path(config_dir)).map_err(|e| e.to_string())?;
    let token =
        std::fs::read_to_string(bsyncd::api::token_path(config_dir)).map_err(|e| e.to_string())?;
    Ok(format!("http://{}/#token={}", addr.trim(), token.trim()))
}

/// The daemon's status as JSON (the same document as `GET /api/status`).
pub fn status_json(config_dir: &Path) -> Result<String, String> {
    let slot = DAEMON.lock().map_err(|e| e.to_string())?;
    let daemon = slot.as_ref().ok_or("not running")?;
    let status = daemon.status().map_err(|e| format!("{e:#}"))?;
    let config = bsyncd::Config::load(config_dir).map_err(|e| format!("{e:#}"))?;
    let dto = bsyncd::api::StatusDto::from_status(&status, &config);
    serde_json::to_string(&dto).map_err(|e| e.to_string())
}

// ----- C ABI -----

fn into_c(result: Result<String, String>) -> *mut c_char {
    let text = match result {
        Ok(s) => s,
        Err(e) => format!("error: {e}"),
    };
    CString::new(text.replace('\0', " "))
        .map(CString::into_raw)
        .unwrap_or(std::ptr::null_mut())
}

/// # Safety
/// `s` must be null or a valid NUL-terminated string.
unsafe fn str_arg(s: *const c_char) -> Result<String, String> {
    if s.is_null() {
        return Err("null argument".into());
    }
    // SAFETY: the caller promises a valid C string.
    let s = unsafe { CStr::from_ptr(s) };
    s.to_str().map(Into::into).map_err(|e| e.to_string())
}

/// # Safety
/// As [`str_arg`].
unsafe fn path_arg(s: *const c_char) -> Result<PathBuf, String> {
    // SAFETY: forwarded caller contract.
    unsafe { str_arg(s) }.map(Into::into)
}

/// Start the daemon. Returns the status page URL, or a string starting
/// with `error: `. Free the result with [`bsync_free_string`].
///
/// # Safety
/// `config_dir` must be a valid NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bsync_start(config_dir: *const c_char) -> *mut c_char {
    // SAFETY: forwarded caller contract.
    into_c(unsafe { path_arg(config_dir) }.and_then(|p| start(&p)))
}

/// Start the daemon for a phone app: folders shared by paired devices
/// appear under `folders_root`; if `app_discovery` is non-zero the app
/// reports LAN devices with [`bsync_discovered`] instead of the daemon's
/// own mDNS. Returns like [`bsync_start`].
///
/// # Safety
/// Both arguments must be valid NUL-terminated UTF-8 strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bsync_start_mobile(
    config_dir: *const c_char,
    folders_root: *const c_char,
    app_discovery: i32,
) -> *mut c_char {
    // SAFETY: forwarded caller contract.
    let r = unsafe { path_arg(config_dir) }.and_then(|dir| {
        // SAFETY: forwarded caller contract.
        let root = unsafe { path_arg(folders_root) }?;
        let options = Options {
            folders_root: Some(root),
            app_discovery: app_discovery != 0,
        };
        start_with(&dir, &options)
    });
    into_c(r)
}

/// Report LAN addresses (`ip:port`, comma-separated) of a device the app
/// discovered. Returns 0 on success.
///
/// # Safety
/// Both arguments must be valid NUL-terminated UTF-8 strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bsync_discovered(device_id: *const c_char, addrs: *const c_char) -> i32 {
    // SAFETY: forwarded caller contract.
    let r = unsafe { str_arg(device_id) }.and_then(|d| {
        // SAFETY: as above.
        let a = unsafe { str_arg(addrs) }?;
        discovered(&d, &a)
    });
    match r {
        Ok(()) => 0,
        Err(e) => {
            tracing::debug!("bsync_discovered: {e}");
            -1
        }
    }
}

/// Status JSON, or `error: …`. Free with [`bsync_free_string`].
///
/// # Safety
/// `config_dir` must be a valid NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bsync_status_json(config_dir: *const c_char) -> *mut c_char {
    // SAFETY: forwarded caller contract.
    into_c(unsafe { path_arg(config_dir) }.and_then(|p| status_json(&p)))
}

/// Reconnect to paired devices now, e.g. when the app returns to the
/// foreground.
#[unsafe(no_mangle)]
pub extern "C" fn bsync_network_changed() {
    network_changed();
}

#[unsafe(no_mangle)]
pub extern "C" fn bsync_stop() {
    stop();
}

/// Free a string returned by this library.
///
/// # Safety
/// `s` must come from this library and not be freed twice.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bsync_free_string(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: created by `CString::into_raw` above.
        drop(unsafe { CString::from_raw(s) });
    }
}

// ----- JNI (Android) -----

#[cfg(target_os = "android")]
mod android {
    use jni::JNIEnv;
    use jni::objects::{JClass, JString};
    use jni::sys::jstring;

    fn arg(env: &mut JNIEnv<'_>, s: &JString<'_>) -> Result<std::path::PathBuf, String> {
        env.get_string(s)
            .map(|s| String::from(s).into())
            .map_err(|e| e.to_string())
    }

    fn ret(env: &mut JNIEnv<'_>, r: Result<String, String>) -> jstring {
        let text = r.unwrap_or_else(|e| format!("error: {e}"));
        env.new_string(text)
            .map(|s| s.into_raw())
            .unwrap_or(std::ptr::null_mut())
    }

    /// `com.bettersync.Native.start(configDir: String): String`
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_bettersync_Native_start<'l>(
        mut env: JNIEnv<'l>,
        _class: JClass<'l>,
        config_dir: JString<'l>,
    ) -> jstring {
        let r = arg(&mut env, &config_dir).and_then(|p| super::start(&p));
        ret(&mut env, r)
    }

    /// `com.bettersync.Native.statusJson(configDir: String): String`
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_bettersync_Native_statusJson<'l>(
        mut env: JNIEnv<'l>,
        _class: JClass<'l>,
        config_dir: JString<'l>,
    ) -> jstring {
        let r = arg(&mut env, &config_dir).and_then(|p| super::status_json(&p));
        ret(&mut env, r)
    }

    /// `com.bettersync.Native.stop()`
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_bettersync_Native_stop<'l>(
        _env: JNIEnv<'l>,
        _class: JClass<'l>,
    ) {
        super::stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_status_stop_through_the_c_abi() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg");
        // Keep the test off the default ports.
        let c = bsyncd::Config {
            listen: "127.0.0.1:0".into(),
            local_discovery: false,
            tailscale_discovery: false,
            ..Default::default()
        };
        c.save(&cfg).unwrap();

        let c_path = CString::new(cfg.to_str().unwrap()).unwrap();
        // SAFETY: valid C string; results freed below.
        let url = unsafe { bsync_start(c_path.as_ptr()) };
        let url_text = unsafe { CStr::from_ptr(url) }.to_str().unwrap().to_owned();
        unsafe { bsync_free_string(url) };
        assert!(
            url_text.starts_with("http://127.0.0.1:") && url_text.contains("/#token="),
            "{url_text}"
        );

        let json = unsafe { bsync_status_json(c_path.as_ptr()) };
        let json_text = unsafe { CStr::from_ptr(json) }.to_str().unwrap().to_owned();
        unsafe { bsync_free_string(json) };
        let v: serde_json::Value = serde_json::from_str(&json_text).unwrap();
        assert!(v["device_id"].as_str().unwrap().len() > 50);

        // Starting again returns the same daemon's page.
        assert_eq!(start(&cfg).unwrap(), url_text);
        bsync_stop();
        assert!(status_json(&cfg).unwrap_err().contains("not running"));

        // The phone flavour: shared folders land under the given root, and
        // the app reports LAN devices itself.
        let root = dir.path().join("Documents");
        let c_root = CString::new(root.to_str().unwrap()).unwrap();
        let url = unsafe { bsync_start_mobile(c_path.as_ptr(), c_root.as_ptr(), 1) };
        let text = unsafe { CStr::from_ptr(url) }.to_str().unwrap().to_owned();
        unsafe { bsync_free_string(url) };
        assert!(text.starts_with("http://127.0.0.1:"), "{text}");
        let saved = bsyncd::Config::load(&cfg).unwrap();
        assert!(saved.always_on.auto_accept_folders);
        assert_eq!(
            saved.always_on.folders_root.as_deref(),
            Some(root.as_path())
        );
        assert!(!saved.local_discovery);
        assert!(root.is_dir());

        // A container move between launches carries the folder along.
        bsync_stop();
        let old = dir.path().join("old-container/Documents/photos");
        std::fs::create_dir_all(root.join("photos/.bsync")).unwrap();
        let mut moved = bsyncd::Config::load(&cfg).unwrap();
        moved.folders.push(bsyncd::config::FolderConfig {
            id: "photos".into(),
            path: old,
            devices: vec![],
            paused: false,
        });
        moved.save(&cfg).unwrap();
        let url = unsafe { bsync_start_mobile(c_path.as_ptr(), c_root.as_ptr(), 1) };
        unsafe { bsync_free_string(url) };
        let saved = bsyncd::Config::load(&cfg).unwrap();
        assert_eq!(saved.folders[0].path, root.join("photos"));

        let id = CString::new(v["device_id"].as_str().unwrap()).unwrap();
        let addrs = CString::new("192.168.1.20:22027, [fe80::1]:22027").unwrap();
        assert_eq!(unsafe { bsync_discovered(id.as_ptr(), addrs.as_ptr()) }, 0);
        bsync_network_changed();
        let bad = CString::new("not-an-id").unwrap();
        assert_eq!(
            unsafe { bsync_discovered(bad.as_ptr(), addrs.as_ptr()) },
            -1
        );
        let none = CString::new("nonsense").unwrap();
        assert_eq!(unsafe { bsync_discovered(id.as_ptr(), none.as_ptr()) }, -1);
        bsync_stop();
    }
}
