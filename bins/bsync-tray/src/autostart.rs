//! Start the tray app at login.
//!
//! - Windows: a value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
//!   (When testing from a sandboxed shell, launch via `explorer.exe`, or the
//!   registry write may be virtualised.)
//! - macOS: `~/Library/LaunchAgents/com.bettersync.tray.plist`.
//! - Linux: `~/.config/autostart/bettersync.desktop` (XDG).
//!
//! The entry generators for every platform are compiled and unit-tested on
//! all of them, so each build leaves some unused.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::Result;

pub const LABEL: &str = "com.bettersync.tray";
#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const RUN_VALUE: &str = "BetterSync";

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The LaunchAgent property list that starts `exe` at login.
pub fn launch_agent_plist(exe: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#,
        xml_escape(&exe.display().to_string())
    )
}

/// The XDG autostart entry that starts `exe` at login.
pub fn desktop_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=BetterSync\nComment=Peer-to-peer folder sync\nExec=\"{}\"\nX-GNOME-Autostart-enabled=true\n",
        exe.display()
    )
}

/// The command line stored in the Windows Run key (quoted: paths may contain
/// spaces).
pub fn run_key_command(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

/// Where the login item lives, for the file-based platforms.
pub fn login_item_path(home: &Path) -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        Some(
            home.join("Library/LaunchAgents")
                .join(format!("{LABEL}.plist")),
        )
    } else if cfg!(windows) {
        None
    } else {
        Some(home.join(".config/autostart/bettersync.desktop"))
    }
}

fn home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))
}

pub fn is_enabled() -> bool {
    #[cfg(windows)]
    {
        let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
        hkcu.open_subkey(RUN_KEY)
            .and_then(|k| k.get_value::<String, _>(RUN_VALUE))
            .is_ok()
    }
    #[cfg(not(windows))]
    {
        home()
            .ok()
            .and_then(|h| login_item_path(&h))
            .is_some_and(|p| p.exists())
    }
}

pub fn set_enabled(on: bool) -> Result<()> {
    let exe = std::env::current_exe()?;
    #[cfg(windows)]
    {
        let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
        let (key, _) = hkcu.create_subkey(RUN_KEY)?;
        if on {
            key.set_value(RUN_VALUE, &run_key_command(&exe))?;
        } else {
            let _ = key.delete_value(RUN_VALUE);
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let home = home()?;
        set_enabled_in(&home, &exe, on)
    }
}

/// File-based implementation, separated for tests.
#[cfg(not(windows))]
pub fn set_enabled_in(home: &Path, exe: &Path, on: bool) -> Result<()> {
    let Some(path) = login_item_path(home) else {
        return Ok(());
    };
    if on {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let content = if cfg!(target_os = "macos") {
            launch_agent_plist(exe)
        } else {
            desktop_entry(exe)
        };
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, &path)?;
    } else {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_entries_quote_and_escape() {
        let exe = Path::new("/Applications/Better Sync & Co.app/Contents/MacOS/bsync-tray");
        let plist = launch_agent_plist(exe);
        assert!(plist.contains("<string>com.bettersync.tray</string>"));
        assert!(plist.contains("Better Sync &amp; Co.app"));
        assert!(plist.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(desktop_entry(exe).contains("Exec=\"/Applications/Better Sync & Co.app"));
        assert_eq!(
            run_key_command(Path::new(r"C:\Program Files\BetterSync\bsync-tray.exe")),
            r#""C:\Program Files\BetterSync\bsync-tray.exe""#
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn enable_and_disable_login_item() {
        let home = tempfile::tempdir().unwrap();
        let exe = Path::new("/opt/bettersync/bsync-tray");
        set_enabled_in(home.path(), exe, true).unwrap();
        let path = login_item_path(home.path()).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("/opt/bettersync/bsync-tray")
        );
        set_enabled_in(home.path(), exe, false).unwrap();
        assert!(!path.exists());
        set_enabled_in(home.path(), exe, false).unwrap();
    }
}
