//! LOUDAUTO's engine, `loudautod`: the long-running process that owns playback, MPRIS,
//! the tray and the caches. Windows and `loudauto-cli` reach it over `engine_ipc`.
//! See `docs/superpowers/specs/2026-11-05-loudauto-gui-design.md`.

pub mod core;
pub mod desktop;
pub mod engine;
pub mod heart;
pub mod hinting;
/// Which icon name LOUDAUTO shows; shared with the window, which draws it as its icon.
pub use icon_theme as icon;
pub mod mpris;
pub mod notify;
pub mod pages;
pub mod reason;
pub mod requests;
pub mod run;
pub mod server;
pub mod session;
pub mod settings;
pub mod shortcut;
pub mod sleep;
pub mod soloist;
pub mod tray;
pub mod window;
