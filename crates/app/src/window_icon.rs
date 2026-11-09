//! The window's icon: LOUDAUTO's icon from the active icon theme, the chain the engine's
//! tray and notifications use (`spotify`, `spotify-launcher`, `audio-x-generic`), as a
//! PNG; never artwork of our own. Plasma on Wayland draws the taskbar's and title bar's
//! icon from the `.desktop` file the app id `to.skunk.loudauto` names instead (winit
//! can't set a Wayland window's icon); this one shows where the platform takes it (X11).

use std::path::Path;

use eframe::egui::IconData;
use log::{info, warn};

/// The size asked of the icon theme, in pixels.
pub const SIZE: u32 = 128;

/// LOUDAUTO's icon, decoded; `None` when the active theme has no PNG of it. Blocking
/// (directory reads and a decode): call it off the UI thread.
pub fn load() -> Option<IconData> {
    let Some(path) = icon_theme::app_icon_png(SIZE) else {
        info!("window icon: the icon theme has no PNG of it");
        return None;
    };
    let icon = decode(&path)?;
    info!(
        "window icon: {} ({}×{})",
        path.display(),
        icon.width,
        icon.height
    );
    Some(icon)
}

/// The PNG at `path` as RGBA pixels; `None` (and why, logged) when it won't read.
pub fn decode(path: &Path) -> Option<IconData> {
    match image::open(path) {
        Ok(image) => {
            let rgba = image.into_rgba8();
            let (width, height) = rgba.dimensions();
            Some(IconData {
                rgba: rgba.into_raw(),
                width,
                height,
            })
        }
        Err(e) => {
            warn!("window icon {}: {e}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_png_to_rgba() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("icon.png");
        let mut pixels = image::RgbaImage::from_pixel(3, 2, image::Rgba([30, 215, 96, 255]));
        pixels.put_pixel(0, 0, image::Rgba([1, 2, 3, 4]));
        pixels.save(&path).unwrap();
        let icon = decode(&path).unwrap();
        assert_eq!((icon.width, icon.height), (3, 2));
        assert_eq!(icon.rgba.len(), 3 * 2 * 4);
        assert_eq!(icon.rgba[..8], [1, 2, 3, 4, 30, 215, 96, 255]);

        std::fs::write(&path, b"not a png").unwrap();
        assert!(decode(&path).is_none());
        assert!(decode(&dir.path().join("missing.png")).is_none());
    }

    /// On this machine: the theme's PNG reads as a square icon. Skipped quietly where the
    /// active theme has none.
    #[test]
    fn this_desktops_icon_loads() {
        let Some(path) = icon_theme::app_icon_png(SIZE) else {
            eprintln!("no PNG of LOUDAUTO's icon in the active theme; skipped");
            return;
        };
        let icon = load().unwrap_or_else(|| panic!("{} won't read", path.display()));
        assert_eq!(icon.width, icon.height, "{}", path.display());
        assert!(icon.width >= 64, "{}", path.display());
    }
}
