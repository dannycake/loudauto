//! LOUDAUTO's files on disk: where its cache lives, and how a cache file is written so
//! that a reader never sees half of it.

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use tokio::io::AsyncWriteExt;

use crate::Result;

/// `$XDG_CACHE_HOME/loudauto`, else `~/.cache/loudauto`.
pub fn cache_dir() -> PathBuf {
    cache_dir_from(|key| std::env::var(key).ok())
}

/// [`cache_dir`], reading the environment through `get`. An empty `XDG_CACHE_HOME`
/// counts as unset.
pub fn cache_dir_from(get: impl Fn(&str) -> Option<String>) -> PathBuf {
    let cache = get("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(get("HOME").unwrap_or_default()).join(".cache"));
    cache.join("loudauto")
}

/// Writes a temp file beside `path` and renames it over `path`, so a reader sees the
/// old file or the new one, never a partial write. Creates `path`'s directory. The temp
/// name is unique within the process, so two writes never share one.
pub async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = parent_dir(path) {
        tokio::fs::create_dir_all(dir).await?;
    }
    let tmp = temp_beside(path);
    let written = async {
        let mut file = tokio::fs::File::create(&tmp).await?;
        file.write_all(bytes).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&tmp, path).await
    }
    .await;
    if written.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    Ok(written?)
}

/// [`write_atomic`] for a plain thread, outside any async runtime.
pub fn write_atomic_blocking(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = parent_dir(path) {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = temp_beside(path);
    let written = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// `path`'s directory, unless it is the current one.
fn parent_dir(path: &Path) -> Option<&Path> {
    path.parent().filter(|dir| !dir.as_os_str().is_empty())
}

/// A temp file name beside `path`, unique within the process.
fn temp_beside(path: &Path) -> PathBuf {
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let n = WRITES.fetch_add(1, Ordering::Relaxed);
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".{}.{n}.tmp", std::process::id()));
    path.with_file_name(tmp_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_dir_honours_xdg_cache_home() {
        let vars = |set: &'static [(&'static str, &'static str)]| {
            move |key: &str| {
                set.iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(
            cache_dir_from(vars(&[("XDG_CACHE_HOME", "/x/cache"), ("HOME", "/home/u")])),
            Path::new("/x/cache/loudauto")
        );
        assert_eq!(
            cache_dir_from(vars(&[("XDG_CACHE_HOME", ""), ("HOME", "/home/u")])),
            Path::new("/home/u/.cache/loudauto")
        );
    }

    #[tokio::test]
    async fn write_atomic_replaces_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/file.json");
        write_atomic(&path, b"one").await.unwrap();
        write_atomic(&path, b"two").await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["file.json"]);
    }

    #[test]
    fn write_atomic_blocking_replaces_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/file.bin");
        write_atomic_blocking(&path, b"one").unwrap();
        write_atomic_blocking(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["file.bin"]);
    }
}
