//! Soloist defaults to Vorbis 160 kbps with loudness normalization on. Its user prefs
//! file accepts the official desktop app's own keys; these values gave FLAC 1.4 Mbps live.
//! Write only while Soloist is stopped: it reads prefs at startup.

use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

/// `(key, value)` pairs the official app uses for lossless, normalization off.
pub const QUALITY_PREFS: [(&str, &str); 3] = [
    ("audio.play_bitrate_enumeration", "5"),
    ("audio.play_bitrate_non_metered_enumeration", "5"),
    ("audio.normalize_v2", "false"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefsOutcome {
    /// Not paired yet: Soloist creates `settings/Users/<user>-user/` at first login.
    NoUser,
    AlreadySet,
    Updated,
}

/// Ensure every `<data_dir>/settings/Users/*-user/prefs` holds `QUALITY_PREFS`.
pub fn ensure_quality_prefs(data_dir: &Path) -> std::io::Result<PrefsOutcome> {
    let users = data_dir.join("settings/Users");
    let entries = match fs::read_dir(&users) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(PrefsOutcome::NoUser),
        Err(e) => return Err(e),
    };
    let mut user_dirs: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() && entry.file_name().to_string_lossy().ends_with("-user") {
            user_dirs.push(entry.path());
        }
    }
    if user_dirs.is_empty() {
        return Ok(PrefsOutcome::NoUser);
    }
    let mut changed = false;
    for dir in user_dirs {
        changed |= ensure_file(&dir.join("prefs"))?;
    }
    Ok(if changed {
        PrefsOutcome::Updated
    } else {
        PrefsOutcome::AlreadySet
    })
}

/// `ensure_quality_prefs` on a blocking thread, so the runtime never waits on the disk.
pub async fn ensure_quality_prefs_async(data_dir: &Path) -> std::io::Result<PrefsOutcome> {
    let data_dir = data_dir.to_path_buf();
    tokio::task::spawn_blocking(move || ensure_quality_prefs(&data_dir))
        .await
        .map_err(std::io::Error::other)?
}

/// Returns whether the file changed.
fn ensure_file(path: &Path) -> std::io::Result<bool> {
    let before = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut lines: Vec<String> = before.lines().map(str::to_string).collect();
    for (key, value) in QUALITY_PREFS {
        let wanted = format!("{key}={value}");
        let prefix = format!("{key}=");
        match lines.iter().position(|l| l.starts_with(&prefix)) {
            Some(i) => lines[i] = wanted,
            None => lines.push(wanted),
        }
    }
    let after = lines.join("\n") + "\n";
    if after == before {
        return Ok(false);
    }
    fs::write(path, after)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn user_dir(root: &Path) -> std::path::PathBuf {
        let d = root.join("settings/Users/0uaktefav92xfmbx26j1narmh-user");
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn count(text: &str, line: &str) -> usize {
        text.lines().filter(|l| *l == line).count()
    }

    #[test]
    fn no_user_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            ensure_quality_prefs(dir.path()).unwrap(),
            PrefsOutcome::NoUser
        );
        assert!(!dir.path().join("settings").exists());
    }

    #[test]
    fn appends_missing_keys() {
        let dir = tempfile::tempdir().unwrap();
        let prefs = user_dir(dir.path()).join("prefs");
        fs::write(
            &prefs,
            "audio.gapless_migrated_to_ps=true\naudio.crossfade_migrated_to_ps=true\n",
        )
        .unwrap();
        assert_eq!(
            ensure_quality_prefs(dir.path()).unwrap(),
            PrefsOutcome::Updated
        );
        let text = fs::read_to_string(&prefs).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "audio.gapless_migrated_to_ps=true");
        assert_eq!(lines[1], "audio.crossfade_migrated_to_ps=true");
        for (k, v) in QUALITY_PREFS {
            assert_eq!(count(&text, &format!("{k}={v}")), 1, "{text}");
        }
    }

    #[test]
    fn replaces_differing_value() {
        let dir = tempfile::tempdir().unwrap();
        let prefs = user_dir(dir.path()).join("prefs");
        fs::write(&prefs, "audio.play_bitrate_enumeration=2\nother=1\n").unwrap();
        assert_eq!(
            ensure_quality_prefs(dir.path()).unwrap(),
            PrefsOutcome::Updated
        );
        let text = fs::read_to_string(&prefs).unwrap();
        assert_eq!(count(&text, "audio.play_bitrate_enumeration=5"), 1);
        assert_eq!(count(&text, "audio.play_bitrate_enumeration=2"), 0);
        assert_eq!(count(&text, "other=1"), 1);
    }

    #[test]
    fn idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let prefs = user_dir(dir.path()).join("prefs");
        fs::write(&prefs, "x=1\n").unwrap();
        ensure_quality_prefs(dir.path()).unwrap();
        let first = fs::read(&prefs).unwrap();
        assert_eq!(
            ensure_quality_prefs(dir.path()).unwrap(),
            PrefsOutcome::AlreadySet
        );
        assert_eq!(fs::read(&prefs).unwrap(), first);
    }

    #[tokio::test]
    async fn async_wrapper_writes_prefs() {
        let dir = tempfile::tempdir().unwrap();
        let prefs = user_dir(dir.path()).join("prefs");
        assert_eq!(
            ensure_quality_prefs_async(dir.path()).await.unwrap(),
            PrefsOutcome::Updated
        );
        let text = fs::read_to_string(&prefs).unwrap();
        for (k, v) in QUALITY_PREFS {
            assert_eq!(count(&text, &format!("{k}={v}")), 1);
        }
        assert_eq!(
            ensure_quality_prefs_async(dir.path()).await.unwrap(),
            PrefsOutcome::AlreadySet
        );
    }

    #[test]
    fn missing_prefs_file_in_user_dir() {
        let dir = tempfile::tempdir().unwrap();
        let prefs = user_dir(dir.path()).join("prefs");
        assert_eq!(
            ensure_quality_prefs(dir.path()).unwrap(),
            PrefsOutcome::Updated
        );
        let text = fs::read_to_string(&prefs).unwrap();
        for (k, v) in QUALITY_PREFS {
            assert_eq!(count(&text, &format!("{k}={v}")), 1);
        }
    }
}
