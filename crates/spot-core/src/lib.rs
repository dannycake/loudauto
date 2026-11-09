//! Spotify library access for LOUDAUTO, through a librespot session used only for
//! metadata (playback is Spotify Soloist's job). See
//! `docs/superpowers/specs/2026-11-05-spotify-rs-design.md`.

pub mod context;
pub mod cover;
pub mod creds;
pub mod error;
pub mod files;
pub mod library;
pub mod model;
pub mod pages;
pub mod rootlist;
pub mod search;
pub mod session;
pub mod web;

pub use error::{Result, SpotError};
