//! Where librespot's reusable "stored credentials" live between runs. The bytes are
//! the JSON serialization of `librespot_core::authentication::Credentials`.

use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};

use async_trait::async_trait;

use crate::{Result, SpotError};

#[async_trait]
pub trait CredentialStore: Send + Sync {
    async fn load(&self) -> Result<Option<Vec<u8>>>;
    async fn save(&self, json: &[u8]) -> Result<()>;
    async fn clear(&self) -> Result<()>;
}

/// A single `0600` file. Used by tests and as a fallback when no Secret Service runs.
pub struct FileStore {
    pub path: PathBuf,
}

#[async_trait]
impl CredentialStore for FileStore {
    async fn load(&self) -> Result<Option<Vec<u8>>> {
        match fs::read(&self.path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    async fn save(&self, json: &[u8]) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&self.path)?;
        // `mode` only applies on creation; tighten a pre-existing file too.
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(json)?;
        Ok(())
    }
    async fn clear(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

/// One entry in the desktop Secret Service (KDE's ksecretd on this machine), via oo7.
pub struct SecretServiceStore {
    label: &'static str,
    kind: &'static str,
}

impl SecretServiceStore {
    /// librespot's reusable stored credentials: the Spotify login.
    pub const SPOTIFY_SESSION: SecretServiceStore = SecretServiceStore {
        label: "LOUDAUTO Spotify session",
        kind: "librespot-stored-credentials",
    };
    /// The Soloist API key, saved by `loudauto-cli set-key` (or copied from 1Password).
    pub const SOLOIST_KEY: SecretServiceStore = SecretServiceStore {
        label: "LOUDAUTO Soloist API key",
        kind: "soloist-api-key",
    };

    fn attributes(&self) -> [(&'static str, &'static str); 2] {
        [("application", "loudauto"), ("kind", self.kind)]
    }
}

#[async_trait]
impl CredentialStore for SecretServiceStore {
    async fn load(&self) -> Result<Option<Vec<u8>>> {
        let keyring = keyring().await?;
        let items = keyring
            .search_items(&self.attributes())
            .await
            .map_err(keyring_err)?;
        let Some(item) = items.first() else {
            return Ok(None);
        };
        let secret = item.secret().await.map_err(keyring_err)?;
        Ok(Some(secret.as_bytes().to_vec()))
    }
    async fn save(&self, json: &[u8]) -> Result<()> {
        let keyring = keyring().await?;
        keyring
            .create_item(
                self.label,
                &self.attributes(),
                oo7::Secret::blob(json),
                true,
            )
            .await
            .map_err(keyring_err)
    }
    async fn clear(&self) -> Result<()> {
        keyring()
            .await?
            .delete(&self.attributes())
            .await
            .map_err(keyring_err)
    }
}

async fn keyring() -> Result<oo7::Keyring> {
    let keyring = oo7::Keyring::new().await.map_err(keyring_err)?;
    keyring.unlock().await.map_err(keyring_err)?;
    Ok(keyring)
}

fn keyring_err(e: oo7::Error) -> SpotError {
    SpotError::Keyring(e.to_string())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// The login entry's identity is what finds an existing login: changing it logs people out.
    #[test]
    fn keyring_entries_are_distinct_and_the_login_entry_is_pinned() {
        let login = SecretServiceStore::SPOTIFY_SESSION;
        assert_eq!(login.label, "LOUDAUTO Spotify session");
        assert_eq!(
            login.attributes(),
            [
                ("application", "loudauto"),
                ("kind", "librespot-stored-credentials")
            ]
        );
        let key = SecretServiceStore::SOLOIST_KEY;
        assert_eq!(key.label, "LOUDAUTO Soloist API key");
        assert_eq!(
            key.attributes(),
            [("application", "loudauto"), ("kind", "soloist-api-key")]
        );
    }

    #[tokio::test]
    async fn file_store_roundtrip_and_mode() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore {
            path: dir.path().join("creds.json"),
        };
        assert_eq!(store.load().await.unwrap(), None);

        store.save(br#"{"username":"u"}"#).await.unwrap();
        assert_eq!(
            store.load().await.unwrap().as_deref(),
            Some(&br#"{"username":"u"}"#[..])
        );
        let mode = std::fs::metadata(&store.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        store.clear().await.unwrap();
        assert_eq!(store.load().await.unwrap(), None);
        store.clear().await.unwrap(); // clearing an empty store is fine
    }
}
