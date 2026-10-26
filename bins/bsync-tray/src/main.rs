//! BetterSync tray / menu-bar app: shows sync state, opens folders and the
//! status page, pauses syncing, and starts the daemon (and itself at login).

// The Linux stub uses only part of the shared modules.
#![cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
// No console window on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(any(windows, target_os = "macos"))]
mod app;
mod autostart;
mod icons;
mod status;

#[cfg(any(windows, target_os = "macos"))]
fn main() -> anyhow::Result<()> {
    app::run()
}

#[cfg(not(any(windows, target_os = "macos")))]
fn main() -> anyhow::Result<()> {
    // The tray targets Windows and macOS; on Linux use `bsync status` or the
    // status page, which the daemon serves at its control API address.
    let dir = status::config_dir();
    match status::Api::locate(&dir) {
        Some(api) => {
            println!("BetterSync has no tray icon on this platform.");
            println!("Status page: {}", api.status_page_url());
            if let Ok(s) = api.status() {
                println!("{}", s.summary());
            }
            if std::env::args().any(|a| a == "--autostart") {
                autostart::set_enabled(true)?;
                println!("Will start at login.");
            }
        }
        None => println!("The BetterSync daemon is not running. Start it with `bsyncd`."),
    }
    let _ = icons::rgba(status::TrayState::Idle);
    Ok(())
}
