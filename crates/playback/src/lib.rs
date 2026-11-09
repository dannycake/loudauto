//! Playback for LOUDAUTO. Audio comes from Spotify Soloist, Spotify's official headless
//! player: this crate launches it, keeps it on lossless, and drives it over its local
//! WebSocket API. It never touches decrypted audio. See
//! `docs/superpowers/specs/2026-11-05-spotify-rs-design.md`.

pub mod backend;
pub mod client;
pub mod command;
pub mod config;
pub mod error;
pub mod event;
pub mod prefs;
pub mod quality;
pub mod restart;
pub mod supervisor;
pub mod wire;

pub use error::{Result, SoloistError};
