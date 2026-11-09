//! The persisted-query hash for each [`Op`]. It starts from the fallback table and is
//! overridden by hashes scraped from the public web player, cached on disk as
//! `{"fetched_at_ms":…,"hashes":{"<op name>":"<sha256>",…}}`. A missing or unreadable
//! cache is never an error: the registry falls back and the next scrape rewrites it.

use std::{
    collections::{BTreeMap, HashMap},
    io::ErrorKind,
    path::PathBuf,
    sync::{
        Mutex as StdMutex, PoisonError, RwLock, RwLockReadGuard,
        atomic::{AtomicU64, Ordering},
    },
};

use serde::{Deserialize, Serialize};
use tokio::{sync::Mutex, time::Instant};

use super::ops::Op;
use crate::{
    Result,
    files::{self, write_atomic},
};

/// How long scraped hashes are trusted before the daily refresh is due.
pub const REFRESH_AFTER_MS: i64 = 24 * 60 * 60 * 1000;

/// The cache file, as written by [`HashRegistry::update`].
#[derive(Serialize, Deserialize)]
struct CacheFile {
    fetched_at_ms: i64,
    hashes: BTreeMap<String, String>,
}

#[derive(Default)]
struct State {
    /// When the last scrape was saved; None if there is no readable cache.
    fetched_at_ms: Option<i64>,
    hashes: BTreeMap<String, String>,
}

pub struct HashRegistry {
    path: PathBuf,
    state: RwLock<State>,
    /// Serializes `update`, so two concurrent saves can't leave the older snapshot on
    /// disk.
    write: Mutex<()>,
    refresh: Mutex<()>,
    /// Scrapes finished under `refresh`, whatever they found.
    scrapes: AtomicU64,
    /// When a heal scrape last failed to renew each op's hash.
    unrenewed: StdMutex<HashMap<Op, Instant>>,
}

impl HashRegistry {
    /// `$XDG_CACHE_HOME/loudauto/pathfinder-hashes.json`, else under `~/.cache`.
    pub fn default_path() -> PathBuf {
        default_path_from(|key| std::env::var(key).ok())
    }

    /// Reads the cache at `path`. A missing, unreadable or malformed file leaves only
    /// the fallback hashes, and the refresh due.
    pub async fn load(path: PathBuf) -> HashRegistry {
        let state = match tokio::fs::read(&path).await {
            Ok(bytes) => match serde_json::from_slice::<CacheFile>(&bytes) {
                Ok(file) => State {
                    fetched_at_ms: Some(file.fetched_at_ms),
                    hashes: file.hashes,
                },
                Err(e) => {
                    log::warn!("ignoring malformed hash cache {}: {e}", path.display());
                    State::default()
                }
            },
            Err(e) if e.kind() == ErrorKind::NotFound => State::default(),
            Err(e) => {
                log::warn!("ignoring unreadable hash cache {}: {e}", path.display());
                State::default()
            }
        };
        HashRegistry {
            path,
            state: RwLock::new(state),
            write: Mutex::new(()),
            refresh: Mutex::new(()),
            scrapes: AtomicU64::new(0),
            unrenewed: StdMutex::new(HashMap::new()),
        }
    }

    /// The scraped hash for `op` if there is one, else its fallback.
    pub fn hash(&self, op: Op) -> String {
        self.read()
            .hashes
            .get(op.name())
            .cloned()
            .unwrap_or_else(|| op.fallback_hash().to_owned())
    }

    /// True if `op`'s hash comes from a scrape (the cache), false if it is the fallback.
    pub fn is_cached(&self, op: Op) -> bool {
        self.read().hashes.contains_key(op.name())
    }

    /// Merges a scrape (op name to hash) and saves it. Names that aren't an [`Op`] are
    /// dropped; ops the scrape didn't find keep their cached hash. The new hashes are
    /// in use at once, even if the save then fails.
    pub async fn update(&self, found: BTreeMap<String, String>, now_ms: i64) -> Result<()> {
        let _write = self.write.lock().await;
        let file = {
            let mut state = self.state.write().unwrap_or_else(PoisonError::into_inner);
            for (name, hash) in found {
                if Op::ALL.iter().any(|op| op.name() == name) {
                    state.hashes.insert(name, hash);
                }
            }
            state.fetched_at_ms = Some(now_ms);
            CacheFile {
                fetched_at_ms: now_ms,
                hashes: state.hashes.clone(),
            }
        };
        write_atomic(&self.path, &serde_json::to_vec_pretty(&file)?).await
    }

    /// True if no scrape was ever saved, or the last one is more than 24 h old.
    pub fn is_due(&self, now_ms: i64) -> bool {
        match self.read().fetched_at_ms {
            None => true,
            Some(at) => now_ms.saturating_sub(at) > REFRESH_AFTER_MS,
        }
    }

    /// Held across scrape and update, so concurrent hash failures scrape once.
    pub fn refresh_lock(&self) -> &Mutex<()> {
        &self.refresh
    }

    /// How many scrapes have finished under `refresh_lock`, whatever they found. Read
    /// it before taking the lock: if it moved while you waited, a scrape already ran.
    pub(crate) fn scrape_generation(&self) -> u64 {
        self.scrapes.load(Ordering::SeqCst)
    }

    /// Counts a finished scrape. Call it holding `refresh_lock`, after any `update`.
    pub(crate) fn scrape_finished(&self) {
        self.scrapes.fetch_add(1, Ordering::SeqCst);
    }

    /// Notes that a heal scrape at `at` found no new hash for `op`.
    pub(crate) fn mark_unrenewed(&self, op: Op, at: Instant) {
        self.unrenewed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(op, at);
    }

    /// When a heal scrape last found no new hash for `op`.
    pub(crate) fn unrenewed_at(&self, op: Op) -> Option<Instant> {
        self.unrenewed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&op)
            .copied()
    }

    // Nothing panics while holding the lock, so a poisoned one is still consistent.
    fn read(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }
}

fn default_path_from(get: impl Fn(&str) -> Option<String>) -> PathBuf {
    files::cache_dir_from(get).join("pathfinder-hashes.json")
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::Path, sync::Arc};

    use super::*;
    use crate::web::ops::Op;

    const HOUR_MS: i64 = 60 * 60 * 1000;

    fn hex(c: char) -> String {
        std::iter::repeat_n(c, 64).collect()
    }

    fn found(pairs: &[(&str, String)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, hash)| (name.to_string(), hash.clone()))
            .collect()
    }

    fn write_cache(path: &Path, fetched_at_ms: i64, home: &str) {
        std::fs::write(
            path,
            format!(r#"{{"fetched_at_ms":{fetched_at_ms},"hashes":{{"home":"{home}"}}}}"#),
        )
        .unwrap();
    }

    fn assert_all_fallback(reg: &HashRegistry) {
        for op in Op::ALL {
            assert_eq!(reg.hash(op), op.fallback_hash(), "{op:?}");
        }
    }

    #[tokio::test]
    async fn fallback_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let reg = HashRegistry::load(dir.path().join("pathfinder-hashes.json")).await;
        assert_all_fallback(&reg);
        assert!(reg.is_due(0), "never fetched is due");
    }

    #[tokio::test]
    async fn cached_overrides_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pathfinder-hashes.json");
        write_cache(&path, 1, &hex('a'));
        let reg = HashRegistry::load(path).await;
        assert_eq!(reg.hash(Op::Home), hex('a'));
        for op in Op::ALL.into_iter().filter(|&op| op != Op::Home) {
            assert_eq!(reg.hash(op), op.fallback_hash(), "{op:?}");
        }
        assert!(!reg.is_due(1 + HOUR_MS), "fetched an hour ago");
    }

    #[tokio::test]
    async fn is_cached_only_for_scraped_ops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pathfinder-hashes.json");
        write_cache(&path, 1, &hex('a'));
        let reg = HashRegistry::load(path).await;
        assert!(reg.is_cached(Op::Home));
        for op in Op::ALL.into_iter().filter(|&op| op != Op::Home) {
            assert!(!reg.is_cached(op), "{op:?}");
        }
    }

    #[tokio::test]
    async fn garbage_file_is_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pathfinder-hashes.json");
        for garbage in [
            "",
            "not json {",
            r#"{"hashes":5}"#,
            r#"{"hashes":{"home":"x"}}"#,
        ] {
            std::fs::write(&path, garbage).unwrap();
            let reg = HashRegistry::load(path.clone()).await;
            assert_all_fallback(&reg);
            assert!(reg.is_due(0), "{garbage:?} counts as never fetched");
        }
    }

    #[tokio::test]
    async fn unreadable_path_is_fallback() {
        // A directory where the file should be: the read fails with something other
        // than NotFound.
        let dir = tempfile::tempdir().unwrap();
        let reg = HashRegistry::load(dir.path().to_path_buf()).await;
        assert_all_fallback(&reg);
        assert!(reg.is_due(0));
    }

    #[tokio::test]
    async fn update_merges_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pathfinder-hashes.json");
        write_cache(&path, 1, &hex('a'));
        let reg = HashRegistry::load(path.clone()).await;
        reg.update(
            found(&[("searchDesktop", hex('b')), ("someOtherOp", hex('c'))]),
            5,
        )
        .await
        .unwrap();
        assert_eq!(reg.hash(Op::SearchDesktop), hex('b'), "in memory at once");

        let reloaded = HashRegistry::load(path.clone()).await;
        assert_eq!(reloaded.hash(Op::SearchDesktop), hex('b'));
        assert_eq!(reloaded.hash(Op::Home), hex('a'), "an op not found is kept");
        assert_eq!(
            reloaded.hash(Op::FetchPlaylist),
            Op::FetchPlaylist.fallback_hash()
        );
        assert!(!reloaded.is_due(5 + HOUR_MS), "fetched_at_ms persisted");

        let file: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(file["fetched_at_ms"], 5);
        assert!(
            file["hashes"].get("someOtherOp").is_none(),
            "only our ops are kept"
        );
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            names,
            ["pathfinder-hashes.json"],
            "no temp file left behind"
        );
    }

    #[tokio::test]
    async fn update_creates_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache/loudauto/pathfinder-hashes.json");
        let reg = HashRegistry::load(path.clone()).await;
        reg.update(found(&[("home", hex('a'))]), 7).await.unwrap();
        assert_eq!(HashRegistry::load(path).await.hash(Op::Home), hex('a'));
    }

    #[tokio::test]
    async fn concurrent_updates_keep_both() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pathfinder-hashes.json");
        let reg = Arc::new(HashRegistry::load(path.clone()).await);
        let (a, b) = (reg.clone(), reg.clone());
        let (ra, rb) = tokio::join!(
            async move { a.update(found(&[("home", hex('a'))]), 1).await },
            async move { b.update(found(&[("recents", hex('b'))]), 2).await },
        );
        ra.unwrap();
        rb.unwrap();
        let reloaded = HashRegistry::load(path).await;
        assert_eq!(reloaded.hash(Op::Home), hex('a'));
        assert_eq!(reloaded.hash(Op::Recents), hex('b'));
    }

    #[tokio::test]
    async fn is_due_after_24h() {
        let dir = tempfile::tempdir().unwrap();
        let reg = HashRegistry::load(dir.path().join("pathfinder-hashes.json")).await;
        reg.update(BTreeMap::new(), 0).await.unwrap();
        assert!(!reg.is_due(23 * HOUR_MS));
        assert!(!reg.is_due(24 * HOUR_MS));
        assert!(reg.is_due(24 * HOUR_MS + 1));
        // A fetch saved later than the caller's `now_ms` (a 412 heal that ran while the
        // caller waited for `refresh_lock`) is fresh, not due: no second scrape.
        assert!(!reg.is_due(-1));
    }

    #[tokio::test]
    async fn refresh_lock_is_shared() {
        let dir = tempfile::tempdir().unwrap();
        let reg = HashRegistry::load(dir.path().join("pathfinder-hashes.json")).await;
        let _held = reg.refresh_lock().lock().await;
        assert!(reg.refresh_lock().try_lock().is_err());
    }

    #[test]
    fn default_path_honours_xdg_cache_home() {
        let vars = |set: &'static [(&'static str, &'static str)]| {
            move |key: &str| {
                set.iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(
            default_path_from(vars(&[("XDG_CACHE_HOME", "/x/cache"), ("HOME", "/home/u")])),
            Path::new("/x/cache/loudauto/pathfinder-hashes.json")
        );
        assert_eq!(
            default_path_from(vars(&[("XDG_CACHE_HOME", ""), ("HOME", "/home/u")])),
            Path::new("/home/u/.cache/loudauto/pathfinder-hashes.json")
        );
        assert_eq!(
            default_path_from(vars(&[("HOME", "/home/u")])),
            Path::new("/home/u/.cache/loudauto/pathfinder-hashes.json")
        );
    }
}
