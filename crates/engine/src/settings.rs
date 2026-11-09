//! The owner's settings, kept in `$XDG_CONFIG_HOME/loudauto/settings.json` (else
//! `~/.config/loudauto/settings.json`). A missing or unreadable file means the defaults:
//! volume 40, the output device from `LOUDAUTO_PIPEWIRE_DEVICE` (else the system
//! default), not started at login. Saves are debounced: a volume drag writes once,
//! 500 ms after it stops.

use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};

use engine_ipc::Settings;
use serde_json::Value;
use spot_core::files;
use tokio::sync::{mpsc, oneshot};

/// The volume before the owner has set one.
pub const DEFAULT_VOLUME: u8 = 40;
/// How long a change waits for the next before it is written.
pub const SAVE_DELAY: Duration = Duration::from_millis(500);

/// `$XDG_CONFIG_HOME/loudauto/settings.json`, else under `~/.config`.
pub fn default_path() -> PathBuf {
    path_from(|key| std::env::var(key).ok())
}

/// [`default_path`], reading the environment through `get`. An empty `XDG_CONFIG_HOME`
/// counts as unset.
pub fn path_from(get: impl Fn(&str) -> Option<String>) -> PathBuf {
    let config = get("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(get("HOME").unwrap_or_default()).join(".config"));
    config.join("loudauto").join("settings.json")
}

/// The settings before the owner changed any, from this process's environment.
pub fn defaults() -> Settings {
    defaults_from(|key| std::env::var(key).ok())
}

/// [`defaults`], reading the environment through `get`.
pub fn defaults_from(get: impl Fn(&str) -> Option<String>) -> Settings {
    Settings {
        volume: DEFAULT_VOLUME,
        output_device: get("LOUDAUTO_PIPEWIRE_DEVICE").filter(|v| !v.is_empty()),
        autostart: false,
    }
}

/// The settings in `path`, each one missing from it taken from `defaults`.
pub async fn load(path: &Path, defaults: Settings) -> Settings {
    match tokio::fs::read(path).await {
        Ok(bytes) => parse(&bytes, &defaults).unwrap_or_else(|| {
            log::warn!("{} is unreadable; using the defaults", path.display());
            defaults
        }),
        Err(e) if e.kind() == ErrorKind::NotFound => defaults,
        Err(e) => {
            log::warn!("couldn't read {}: {e}; using the defaults", path.display());
            defaults
        }
    }
}

/// The settings in `bytes` over `defaults`: each field the file has replaces the
/// default, so a file from an older build (fewer fields) still loads.
fn parse(bytes: &[u8], defaults: &Settings) -> Option<Settings> {
    let file: Value = serde_json::from_slice(bytes).ok()?;
    let mut merged = serde_json::to_value(defaults).ok()?;
    let (Some(merged_fields), Some(file_fields)) = (merged.as_object_mut(), file.as_object())
    else {
        return None;
    };
    for (key, value) in file_fields {
        if let Some(field) = merged_fields.get_mut(key) {
            *field = value.clone();
        }
    }
    // A volume past Soloist's 100 would be saved as one it never plays.
    if let Some(volume) = merged_fields.get_mut("volume")
        && volume.as_u64().is_some_and(|v| v > 100)
    {
        *volume = 100.into();
    }
    serde_json::from_value(merged).ok()
}

/// Writes `settings` to `path` (atomically: a reader sees the old file or the new one).
pub async fn save(path: &Path, settings: &Settings) -> spot_core::Result<()> {
    let mut json = serde_json::to_vec_pretty(settings)?;
    json.push(b'\n');
    files::write_atomic(path, &json).await
}

enum Msg {
    Save(Settings),
    Flush(oneshot::Sender<()>),
}

/// Writes settings to one file, [`SAVE_DELAY`] after the last change.
#[derive(Clone)]
pub struct Saver {
    tx: mpsc::UnboundedSender<Msg>,
}

impl Saver {
    /// A saver for `path`; spawns its task on the current runtime.
    pub fn new(path: PathBuf) -> Saver {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run_saver(path, rx));
        Saver { tx }
    }

    /// Writes `settings` once no newer ones have come for [`SAVE_DELAY`].
    pub fn save(&self, settings: Settings) {
        // The task ends only with the last `Saver`.
        let _ = self.tx.send(Msg::Save(settings));
    }

    /// Writes what is waiting now; for quitting.
    pub async fn flush(&self) {
        let (done, flushed) = oneshot::channel();
        if self.tx.send(Msg::Flush(done)).is_ok() {
            let _ = flushed.await;
        }
    }
}

/// The saver's task: holds the newest settings until none newer came for
/// [`SAVE_DELAY`], or a flush, or the last `Saver` going.
async fn run_saver(path: PathBuf, mut rx: mpsc::UnboundedReceiver<Msg>) {
    let mut pending: Option<Settings> = None;
    loop {
        let msg = if pending.is_some() {
            match tokio::time::timeout(SAVE_DELAY, rx.recv()).await {
                Ok(msg) => msg,
                Err(_quiet) => {
                    write(&path, pending.take()).await;
                    continue;
                }
            }
        } else {
            rx.recv().await
        };
        match msg {
            Some(Msg::Save(settings)) => pending = Some(settings),
            Some(Msg::Flush(done)) => {
                write(&path, pending.take()).await;
                let _ = done.send(());
            }
            None => {
                write(&path, pending.take()).await;
                return;
            }
        }
    }
}

async fn write(path: &Path, settings: Option<Settings>) {
    let Some(settings) = settings else { return };
    match save(path, &settings).await {
        Ok(()) => log::debug!("settings saved"),
        Err(e) => log::warn!("couldn't save the settings to {}: {e}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn vars(set: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        |key| {
            set.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn settings_path_honours_xdg_config_home() {
        assert_eq!(
            path_from(vars(&[("XDG_CONFIG_HOME", "/x/config"), ("HOME", "/h")])),
            PathBuf::from("/x/config/loudauto/settings.json")
        );
        assert_eq!(
            path_from(vars(&[("XDG_CONFIG_HOME", ""), ("HOME", "/h")])),
            PathBuf::from("/h/.config/loudauto/settings.json")
        );
        assert_eq!(
            path_from(vars(&[("HOME", "/h")])),
            PathBuf::from("/h/.config/loudauto/settings.json")
        );
    }

    #[tokio::test]
    async fn settings_defaults_and_roundtrip() {
        // Defaults: volume 40, the env's device (an empty one is unset), not at login.
        let plain = Settings {
            volume: 40,
            output_device: None,
            autostart: false,
        };
        assert_eq!(defaults_from(vars(&[])), plain);
        assert_eq!(
            defaults_from(vars(&[("LOUDAUTO_PIPEWIRE_DEVICE", "")])),
            plain
        );
        let with_device = defaults_from(vars(&[("LOUDAUTO_PIPEWIRE_DEVICE", "easyeffects_sink")]));
        assert_eq!(
            with_device.output_device.as_deref(),
            Some("easyeffects_sink")
        );
        assert_eq!((with_device.volume, with_device.autostart), (40, false));

        // No file: the defaults.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("loudauto/settings.json");
        assert_eq!(load(&path, with_device.clone()).await, with_device);

        // What is saved loads back, an explicit system default (`null`) included.
        let mine = Settings {
            volume: 73,
            output_device: None,
            autostart: true,
        };
        save(&path, &mine).await.unwrap();
        assert_eq!(load(&path, with_device.clone()).await, mine);

        // A field the file lacks comes from the defaults; one it has unknown is ignored.
        std::fs::write(&path, r#"{"volume": 12, "theme": "dark"}"#).unwrap();
        let partial = load(&path, with_device.clone()).await;
        assert_eq!(
            partial,
            Settings {
                volume: 12,
                ..with_device.clone()
            }
        );

        // An unreadable file is the defaults; a volume past 100 is clamped.
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load(&path, with_device.clone()).await, with_device);
        std::fs::write(&path, r#"{"volume": 250}"#).unwrap();
        assert_eq!(load(&path, with_device.clone()).await.volume, 100);
    }

    /// Waits up to 5 s for `path` to hold `want`.
    async fn wait_for_file(path: &Path, want: &Settings) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if load(path, defaults_from(vars(&[]))).await == *want && path.exists() {
                return;
            }
            assert!(Instant::now() < deadline, "settings never saved");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn saver_writes_the_last_change_after_a_pause() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let saver = Saver::new(path.clone());
        let with_volume = |volume| Settings {
            volume,
            output_device: None,
            autostart: false,
        };
        saver.save(with_volume(10));
        tokio::time::sleep(Duration::from_millis(100)).await;
        saver.save(with_volume(20));
        tokio::time::sleep(Duration::from_millis(100)).await;
        saver.save(with_volume(30));
        let last = Instant::now();
        // 600 ms after the first change but 400 ms after the last: nothing written yet.
        // (A slow machine only skips the check, never fails it.)
        tokio::time::sleep(Duration::from_millis(400)).await;
        if last.elapsed() < SAVE_DELAY - Duration::from_millis(20) {
            assert!(
                !path.exists(),
                "saved before the last change was 500 ms old"
            );
        }
        wait_for_file(&path, &with_volume(30)).await;
    }

    #[tokio::test]
    async fn saver_flush_writes_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let saver = Saver::new(path.clone());
        let mine = Settings {
            volume: 55,
            output_device: Some("easyeffects_sink".into()),
            autostart: false,
        };
        saver.save(mine.clone());
        saver.flush().await;
        assert_eq!(load(&path, defaults_from(vars(&[]))).await, mine);
    }
}
