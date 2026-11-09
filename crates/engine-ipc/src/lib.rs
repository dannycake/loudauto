//! LOUDAUTO's engine protocol. The engine (`loudautod`) owns the Spotify session and
//! playback; the window (`loudauto`) and `loudauto-cli` reach it over a Unix socket.
//! This crate is what all three share: the messages (`proto`), their framing (`frame`)
//! and the client (`client`). See `docs/superpowers/specs/2026-11-05-loudauto-gui-design.md`.

pub mod client;
pub mod frame;
pub mod proto;

use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
};

pub use client::{EngineClient, IpcError};
pub use proto::{
    ClientMsg, EngineState, Event, Level, LoginStep, Notice, Page, PageReply, PageRequest,
    PlayerState, Reply, Request, Role, ServerMsg, SessionHealth, Setting, Settings, SoloistHealth,
};

/// The protocol both ends speak; a client refuses an engine that answers another.
/// 2: `LikedState` answers may be unknown; `SetLiked`, `Username`, `LikedSetReady`.
pub const PROTOCOL_VERSION: u32 = 2;

/// The application id: Wayland app id, `.desktop` file name, MPRIS `DesktopEntry` and
/// portal host app id.
pub const APP_ID: &str = "to.skunk.loudauto";

/// Where the engine listens: `$XDG_RUNTIME_DIR/loudauto/engine.sock`, or
/// `/run/user/<uid>/loudauto/engine.sock` when `XDG_RUNTIME_DIR` is unset. The engine
/// creates the directory with mode 0700 and the socket with mode 0600.
pub fn socket_path() -> PathBuf {
    socket_path_in(
        std::env::var_os("XDG_RUNTIME_DIR"),
        nix::unistd::getuid().as_raw(),
    )
}

/// `socket_path` for a given `XDG_RUNTIME_DIR` and uid. The XDG spec requires the
/// directory be absolute, so an empty or relative value counts as unset.
fn socket_path_in(runtime_dir: Option<OsString>, uid: u32) -> PathBuf {
    let runtime_dir = runtime_dir
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{uid}")));
    runtime_dir.join("loudauto").join("engine.sock")
}

/// Create the socket's directory, private to this user (mode 0700), tightening it if it
/// already exists. For the engine, before it binds.
pub fn ensure_socket_dir(socket: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let dir = socket.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "socket path has no directory")
    })?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Version 2: `LikedState` answers may be unknown (`null`), and `SetLiked`,
    /// `Username` and `LikedSetReady` are new. A version 1 window can't read them, so
    /// it must hear a mismatch instead.
    #[test]
    fn protocol_is_version_2() {
        assert_eq!(PROTOCOL_VERSION, 2);
    }

    #[test]
    fn socket_path_under_runtime_dir() {
        assert_eq!(
            socket_path_in(Some("/run/user/1000".into()), 1000),
            PathBuf::from("/run/user/1000/loudauto/engine.sock")
        );
        assert_eq!(
            socket_path_in(Some("/tmp/rt".into()), 1000),
            PathBuf::from("/tmp/rt/loudauto/engine.sock")
        );
    }

    #[test]
    fn socket_path_without_usable_runtime_dir() {
        let fallback = PathBuf::from("/run/user/1234/loudauto/engine.sock");
        assert_eq!(socket_path_in(None, 1234), fallback);
        // The XDG spec requires an absolute path; anything else is ignored.
        assert_eq!(socket_path_in(Some("".into()), 1234), fallback);
        assert_eq!(socket_path_in(Some("relative".into()), 1234), fallback);
    }

    #[test]
    fn socket_path_ends_in_engine_sock() {
        assert!(socket_path().ends_with("loudauto/engine.sock"));
    }

    #[test]
    fn socket_dir_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("loudauto/engine.sock");
        ensure_socket_dir(&sock).unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&tmp.path().join("loudauto")), 0o700);

        // An existing directory with looser permissions is tightened.
        std::fs::set_permissions(
            tmp.path().join("loudauto"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        ensure_socket_dir(&sock).unwrap();
        assert_eq!(mode(&tmp.path().join("loudauto")), 0o700);
    }
}
