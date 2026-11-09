//! LOUDAUTO's window, `loudauto`: an egui client of the engine (`loudautod`), which it
//! reaches over `engine_ipc`. It draws only from memory and repaints only on input or a
//! change of state. See `docs/superpowers/specs/2026-11-05-loudauto-gui-design.md`.

pub mod art;
pub mod bottom_bar;
pub mod fonts;
pub mod link;
pub mod nav;
pub mod notices;
pub mod page_frame;
pub mod pages;
pub mod screens;
pub mod settings_view;
pub mod side_panel;
pub mod sidebar;
#[cfg(test)]
mod test_support;
pub mod theme;
pub mod track_list;
pub mod view_state;
pub mod window_icon;
