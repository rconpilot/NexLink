//! "Start at login" on Linux — the counterpart of the Windows
//! `autostart` module (HKCU Run key), behind the same API.
//!
//! Writes an XDG autostart entry
//! (`$XDG_CONFIG_HOME/autostart/nexlink.desktop`, defaulting to
//! `~/.config/autostart/`) so the desktop session launches NexLink at
//! login. As on Windows, [`is_enabled`] is true only when the entry
//! exists *and* points at the exe that is currently running — a stale
//! entry left by a moved or older install counts as off (turning the
//! checkbox on overwrites it).
//!
//! Caveat, the Linux mirror of the Windows UAC note: the entry starts
//! the app as the user, but creating the TUN device needs root, so
//! Connect will ask for a privileged start (the elevation check in
//! `start_tunnel` reports it) until a privileged-helper setup exists.

use std::path::PathBuf;

/// The autostart entry's file name inside `autostart/`.
const ENTRY_NAME: &str = "nexlink.desktop";

/// `$XDG_CONFIG_HOME/autostart/nexlink.desktop` (or the `~/.config`
/// default when XDG_CONFIG_HOME is unset).
fn entry_path() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or_else(|| {
            "Could not find the config directory ($XDG_CONFIG_HOME and $HOME are both unset)."
                .to_string()
        })?;
    Ok(base.join("autostart").join(ENTRY_NAME))
}

fn exe_path() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("Could not find the NexLink exe path ({e})."))
}

/// Is autostart on *for this exe*? A missing entry is "off", not an
/// error.
pub fn is_enabled() -> Result<bool, String> {
    let path = entry_path()?;
    let exe = exe_path()?;
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Ok(false);
    };
    let wanted = format!("Exec={}", exe.display());
    Ok(contents.lines().any(|line| line.trim() == wanted))
}

/// Turn autostart on (write the entry) or off (delete it).
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let path = entry_path()?;
    if enabled {
        let exe = exe_path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("Could not create {} ({e}).", dir.display()))?;
        }
        let contents = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=NexLink\n\
             Comment=Share your phone's internet with this PC\n\
             Exec={}\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n",
            exe.display()
        );
        std::fs::write(&path, contents)
            .map_err(|e| format!("Could not write {} ({e}).", path.display()))?;
    } else {
        // Deleting an entry that is already gone is not an error.
        let _ = std::fs::remove_file(&path);
    }
    Ok(())
}
