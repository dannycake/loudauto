//! Where Soloist lives and how it is launched. The API key is passed only as `-k`.

use std::path::PathBuf;

use crate::{Result, SoloistError};

/// An optional 1Password reference (`op://vault/item/field`) to read the Soloist API key
/// from when neither `SOLOIST_API_KEY` nor the keyring has it.
pub const OP_REFERENCE_VAR: &str = "LOUDAUTO_SOLOIST_OP_REF";

/// What to do when no key is found anywhere.
pub const NO_KEY: &str =
    "No Soloist API key. Save yours with `loudauto-cli set-key`, or set SOLOIST_API_KEY.";

#[derive(Debug, Clone, PartialEq)]
pub struct SoloistConfig {
    pub binary: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub device_name: String,
    /// PipeWire node to play into (`-d`); `None` = system default output.
    pub pipewire_device: Option<String>,
    /// Extra environment for the child process. Only tests set this (fake binary modes).
    pub env: Vec<(String, String)>,
}

impl SoloistConfig {
    pub fn from_env() -> Self {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    /// Defaults, overridden by `LOUDAUTO_SOLOIST_BIN` and `LOUDAUTO_PIPEWIRE_DEVICE`;
    /// honours `XDG_DATA_HOME` / `XDG_CACHE_HOME`.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Self {
        let home = PathBuf::from(get("HOME").unwrap_or_default());
        let xdg = |var: &str, fallback: &str| {
            get(var)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(fallback))
        };
        let data = xdg("XDG_DATA_HOME", ".local/share").join("loudauto");
        let cache = xdg("XDG_CACHE_HOME", ".cache").join("loudauto");
        Self {
            binary: get("LOUDAUTO_SOLOIST_BIN")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| data.join("soloist/soloist")),
            data_dir: data.join("soloist-data"),
            cache_dir: cache.join("soloist"),
            device_name: "LOUDAUTO".into(),
            pipewire_device: get("LOUDAUTO_PIPEWIRE_DEVICE").filter(|v| !v.is_empty()),
            env: Vec::new(),
        }
    }

    pub fn args(&self, api_key: &str) -> Vec<String> {
        let mut args = vec![
            "-n".to_string(),
            self.device_name.clone(),
            "-k".to_string(),
            api_key.to_string(),
            "-D".to_string(),
            self.data_dir.display().to_string(),
            "-C".to_string(),
            self.cache_dir.display().to_string(),
            "-w".to_string(),
            "127.0.0.1:0".to_string(),
            // Lossless tracks are ~40 MB each; Soloist's default cache size is unlimited.
            "-z".to_string(),
            CACHE_SIZE_MB.to_string(),
            // The per-track `codec:` / `bitrate:` lines the quality parser reads are verbose-only.
            "-v".to_string(),
        ];
        if let Some(dev) = &self.pipewire_device {
            args.extend(["-d".to_string(), dev.clone()]);
        }
        args
    }
}

/// Soloist's audio cache cap (`-z`), in MB.
pub const CACHE_SIZE_MB: u32 = 1024;

/// Where the key is kept between launches: the desktop keyring (Secret Service) in the app.
/// `loudauto-cli set-key` saves it there; a key found through 1Password is copied there.
#[async_trait::async_trait]
pub trait KeyCache: Send + Sync {
    /// The cached key, if any. A failure counts as a miss.
    async fn load(&self) -> Option<String>;
    /// Keep a copy. A failure only costs another lookup next launch.
    async fn save(&self, key: &str);
    /// Drop the copy, so the next launch looks the key up again (after a key rotation).
    /// Unlike `load` and `save`, a failure is reported: the owner asked for it.
    async fn forget(&self) -> Result<()>;
}

/// Non-empty `SOLOIST_API_KEY` wins; then the keyring's copy; otherwise `op_read` (1Password,
/// when configured), whose answer is kept in the keyring. `op_read` may block on a desktop
/// prompt, so it runs off the runtime.
pub async fn resolve_api_key(
    env_value: Option<String>,
    cache: &dyn KeyCache,
    op_read: impl FnOnce() -> Result<String> + Send + 'static,
) -> Result<String> {
    let usable = |v: String| Some(v.trim().to_string()).filter(|v| !v.is_empty());
    if let Some(key) = env_value.and_then(usable) {
        return Ok(key);
    }
    if let Some(key) = cache.load().await.and_then(usable) {
        return Ok(key);
    }
    let fetched = tokio::task::spawn_blocking(op_read)
        .await
        .map_err(|e| SoloistError::Key(format!("key lookup did not finish: {e}")))??;
    let key = usable(fetched).ok_or_else(|| SoloistError::Key("empty Soloist API key".into()))?;
    cache.save(&key).await;
    Ok(key)
}

/// The last place to look: `op read` of `LOUDAUTO_SOLOIST_OP_REF` when it is set, else
/// [`NO_KEY`]. Errors carry op's stderr, never the key.
pub fn op_read_key() -> Result<String> {
    lookup_key(std::env::var(OP_REFERENCE_VAR).ok())
}

fn lookup_key(reference: Option<String>) -> Result<String> {
    let Some(reference) = reference.filter(|r| !r.trim().is_empty()) else {
        return Err(SoloistError::Key(NO_KEY.into()));
    };
    let out = std::process::Command::new("op")
        .args(["read", reference.trim()])
        .output()
        .map_err(|e| SoloistError::Key(format!("could not run 1Password CLI `op`: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(SoloistError::Key(format!(
            "`op read` of {OP_REFERENCE_VAR} failed: {}",
            stderr.trim()
        )));
    }
    String::from_utf8(out.stdout).map_err(|_| SoloistError::Key("key is not UTF-8".into()))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn config(dev: Option<&str>) -> SoloistConfig {
        SoloistConfig {
            binary: "/opt/soloist".into(),
            data_dir: "/d".into(),
            cache_dir: "/c".into(),
            device_name: "LOUDAUTO".into(),
            pipewire_device: dev.map(str::to_string),
            env: vec![],
        }
    }

    #[test]
    fn args_without_device() {
        assert_eq!(
            config(None).args("KEY"),
            [
                "-n",
                "LOUDAUTO",
                "-k",
                "KEY",
                "-D",
                "/d",
                "-C",
                "/c",
                "-w",
                "127.0.0.1:0",
                "-z",
                "1024",
                "-v"
            ]
        );
    }

    #[test]
    fn args_with_device() {
        assert_eq!(
            config(Some("easyeffects_sink")).args("KEY"),
            [
                "-n",
                "LOUDAUTO",
                "-k",
                "KEY",
                "-D",
                "/d",
                "-C",
                "/c",
                "-w",
                "127.0.0.1:0",
                "-z",
                "1024",
                "-v",
                "-d",
                "easyeffects_sink"
            ]
        );
    }

    #[test]
    fn default_paths() {
        let vars = HashMap::from([("HOME", "/home/u")]);
        let c = SoloistConfig::from_vars(|k| vars.get(k).map(|v| v.to_string()));
        assert_eq!(
            c.binary,
            PathBuf::from("/home/u/.local/share/loudauto/soloist/soloist")
        );
        assert_eq!(
            c.data_dir,
            PathBuf::from("/home/u/.local/share/loudauto/soloist-data")
        );
        assert_eq!(
            c.cache_dir,
            PathBuf::from("/home/u/.cache/loudauto/soloist")
        );
        assert_eq!(c.device_name, "LOUDAUTO");
        assert_eq!(c.pipewire_device, None);
        assert!(c.env.is_empty());
    }

    #[test]
    fn overrides_and_xdg() {
        let vars = HashMap::from([
            ("HOME", "/home/u"),
            ("XDG_DATA_HOME", "/x/data"),
            ("XDG_CACHE_HOME", "/x/cache"),
            ("LOUDAUTO_SOLOIST_BIN", "/opt/soloist"),
            ("LOUDAUTO_PIPEWIRE_DEVICE", "easyeffects_sink"),
        ]);
        let c = SoloistConfig::from_vars(|k| vars.get(k).map(|v| v.to_string()));
        assert_eq!(c.binary, PathBuf::from("/opt/soloist"));
        assert_eq!(c.data_dir, PathBuf::from("/x/data/loudauto/soloist-data"));
        assert_eq!(c.cache_dir, PathBuf::from("/x/cache/loudauto/soloist"));
        assert_eq!(c.pipewire_device.as_deref(), Some("easyeffects_sink"));
    }

    /// An in-memory stand-in for the keyring's copy.
    #[derive(Default)]
    struct Mem(std::sync::Mutex<Option<String>>);

    #[async_trait::async_trait]
    impl KeyCache for Mem {
        async fn load(&self) -> Option<String> {
            self.0.lock().unwrap().clone()
        }
        async fn save(&self, key: &str) {
            *self.0.lock().unwrap() = Some(key.to_string());
        }
        async fn forget(&self) -> Result<()> {
            *self.0.lock().unwrap() = None;
            Ok(())
        }
    }

    fn cached(key: &str) -> Mem {
        Mem(std::sync::Mutex::new(Some(key.to_string())))
    }

    #[tokio::test]
    async fn env_key_wins_and_is_not_cached() {
        let cache = Mem::default();
        let k = resolve_api_key(Some(" k \n".into()), &cache, || panic!("must not ask op"))
            .await
            .unwrap();
        assert_eq!(k, "k");
        assert_eq!(cache.load().await, None);
    }

    #[tokio::test]
    async fn cached_key_skips_1password() {
        let k = resolve_api_key(None, &cached(" from-cache\n"), || panic!("must not ask op"))
            .await
            .unwrap();
        assert_eq!(k, "from-cache");
    }

    #[tokio::test]
    async fn key_from_1password_is_cached_for_next_time() {
        let cache = Mem::default();
        let k = resolve_api_key(Some(String::new()), &cache, || Ok(" from-op\n".into()))
            .await
            .unwrap();
        assert_eq!(k, "from-op");
        assert_eq!(cache.load().await.as_deref(), Some("from-op"));
        // A blank cached copy is a miss, not a key.
        let k = resolve_api_key(None, &cached("  "), || Ok("from-op".into()))
            .await
            .unwrap();
        assert_eq!(k, "from-op");
    }

    #[test]
    fn no_key_anywhere_says_how_to_save_one() {
        for reference in [None, Some("  ".to_string())] {
            match lookup_key(reference) {
                Err(SoloistError::Key(m)) => {
                    assert!(m.contains("loudauto-cli set-key"), "{m}");
                    assert!(m.contains("SOLOIST_API_KEY"), "{m}");
                }
                other => panic!("expected a key error, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn empty_key_is_error_and_not_cached() {
        let cache = Mem::default();
        assert!(matches!(
            resolve_api_key(None, &cache, || Ok("  ".into())).await,
            Err(SoloistError::Key(_))
        ));
        assert_eq!(cache.load().await, None);
    }
}
