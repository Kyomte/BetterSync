//! Embedding the BetterSync daemon in an app (stretch goal S3).
//!
//! The app starts the daemon with a config directory, then talks to it
//! exactly like the CLI does: over the local control API (or by showing the
//! status page the daemon serves). So the foreign interface stays tiny:
//! start, stop, and where the API is.
//!
//! Two flavours of the same functions: a C ABI (`bsync_*`) for any host
//! language, and JNI entry points for the Android app (`com.bettersync.Native`).
#![allow(unsafe_code)] // FFI boundary

use std::ffi::{CStr, CString, c_char};
use std::path::Path;
use std::sync::Mutex;

use bsyncd::Daemon;

/// The running daemon (one per process).
static DAEMON: Mutex<Option<Daemon>> = Mutex::new(None);

/// Start the daemon with `config_dir` (created if needed). Returns the
/// status page URL (with the API token in its fragment).
pub fn start(config_dir: &Path) -> Result<String, String> {
    let mut slot = DAEMON.lock().map_err(|e| e.to_string())?;
    if slot.is_none() {
        std::fs::create_dir_all(config_dir).map_err(|e| e.to_string())?;
        let mut cfg = bsyncd::Config::load_or_default(config_dir).map_err(|e| format!("{e:#}"))?;
        // A phone has no reason to expose the API beyond loopback, and a
        // fixed port could collide with another app.
        cfg.api_listen = "127.0.0.1:0".into();
        cfg.save(config_dir).map_err(|e| format!("{e:#}"))?;
        *slot = Some(Daemon::start(config_dir).map_err(|e| format!("{e:#}"))?);
    }
    status_page_url(config_dir)
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
/// `config_dir` must be a valid NUL-terminated UTF-8 string.
unsafe fn path_arg(config_dir: *const c_char) -> Result<std::path::PathBuf, String> {
    if config_dir.is_null() {
        return Err("null config dir".into());
    }
    // SAFETY: the caller promises a valid C string.
    let s = unsafe { CStr::from_ptr(config_dir) };
    s.to_str().map(Into::into).map_err(|e| e.to_string())
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

/// Status JSON, or `error: …`. Free with [`bsync_free_string`].
///
/// # Safety
/// `config_dir` must be a valid NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bsync_status_json(config_dir: *const c_char) -> *mut c_char {
    // SAFETY: forwarded caller contract.
    into_c(unsafe { path_arg(config_dir) }.and_then(|p| status_json(&p)))
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
    }
}
