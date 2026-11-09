//! What the engine does to the desktop outside itself: desktop notifications, starting
//! at login (`systemctl --user`), and the keyring's copy of the Soloist key. Behind the
//! [`Desktop`] trait so tests never notify, run `systemctl` or touch the keyring.

use async_trait::async_trait;
use playback::{SoloistError, config::KeyCache};
use spot_core::creds::{CredentialStore, SecretServiceStore};

use crate::notify;

/// The systemd user unit that runs the engine.
pub const SERVICE: &str = "loudauto.service";

#[async_trait]
pub trait Desktop: Send + Sync {
    /// A desktop notification from LOUDAUTO.
    fn notify(&self, summary: &str, body: &str);
    /// Starts the engine at login, or stops doing so. `Err` is the message to show.
    async fn set_autostart(&self, on: bool) -> Result<(), String>;
    /// Forgets the keyring's copy of the Soloist key. `Err` is the message to show.
    async fn forget_soloist_key(&self) -> Result<(), String>;
}

/// The real desktop.
pub struct LiveDesktop;

#[async_trait]
impl Desktop for LiveDesktop {
    fn notify(&self, summary: &str, body: &str) {
        notify::show(summary, body);
    }

    async fn set_autostart(&self, on: bool) -> Result<(), String> {
        let verb = if on { "enable" } else { "disable" };
        let output = tokio::process::Command::new("systemctl")
            .args(["--user", verb, SERVICE])
            .output()
            .await
            .map_err(|e| autostart_failure(&format!("couldn't run systemctl: {e}")))?;
        if output.status.success() {
            log::info!("autostart {verb}d");
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let said = last_line(&stderr);
        log::warn!("systemctl --user {verb} {SERVICE}: {said}");
        Err(autostart_failure(&format!("systemctl said: {said}")))
    }

    async fn forget_soloist_key(&self) -> Result<(), String> {
        KwalletKey.forget().await.map_err(|e| e.to_string())
    }
}

/// `Couldn't change Start at login: <reason>`.
fn autostart_failure(reason: &str) -> String {
    format!("Couldn't change Start at login: {reason}")
}

/// The last non-empty line of `text`, trimmed (systemctl's error is its last line).
fn last_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or("no reason given")
}

/// The Soloist key's copy in the keyring (Secret Service: KWallet, GNOME Keyring, ...).
/// Keyring trouble on `load`/`save` only means looking the key up again; on `forget` it
/// is reported.
pub struct KwalletKey;

#[async_trait]
impl KeyCache for KwalletKey {
    async fn load(&self) -> Option<String> {
        match SecretServiceStore::SOLOIST_KEY.load().await {
            Ok(bytes) => bytes.and_then(|b| String::from_utf8(b).ok()),
            Err(e) => {
                log::warn!("no keyring copy of the Soloist key: {e}");
                None
            }
        }
    }

    async fn save(&self, key: &str) {
        if let Err(e) = SecretServiceStore::SOLOIST_KEY.save(key.as_bytes()).await {
            log::warn!("could not keep the Soloist key in the keyring: {e}");
        }
    }

    async fn forget(&self) -> playback::Result<()> {
        SecretServiceStore::SOLOIST_KEY
            .clear()
            .await
            .map_err(|e| SoloistError::Key(format!("couldn't forget the keyring's copy: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemctl_reason_is_its_last_line() {
        assert_eq!(
            last_line(
                "Created symlink.\nFailed to enable unit: Unit file loudauto.service does not exist.\n\n"
            ),
            "Failed to enable unit: Unit file loudauto.service does not exist."
        );
        assert_eq!(last_line("  \n"), "no reason given");
        assert_eq!(
            autostart_failure("systemctl said: x"),
            "Couldn't change Start at login: systemctl said: x"
        );
    }

    #[test]
    fn service_file_runs_the_installed_engine() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packaging")
            .join(SERVICE);
        let unit =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let lines: Vec<&str> = unit.lines().collect();
        for line in [
            "ExecStart=%h/.local/bin/loudautod",
            "Restart=on-failure",
            "PartOf=graphical-session.target",
            "WantedBy=graphical-session.target",
        ] {
            assert!(lines.contains(&line), "{line} is missing");
        }
    }
}
