//! The look: the author's Skunk dark scheme with Spotify's green in place of Skunk's
//! purple.
//! True black for OLED, white text, lines in a grey tinted toward purple; the green as ink
//! on black for what must stand out, and as the fill of a main button with black on it;
//! errors in a red box; corners softened 3 px. Cover art is the only full-colour imagery,
//! and nothing tints it. Every colour the window draws is one of these.

use eframe::egui::{
    Button, Color32, CornerRadius, Frame, Margin, Response, RichText, Shadow, Stroke, StrokeKind,
    Ui, Visuals,
};

/// The background: true black, for OLED.
pub const BG: Color32 = Color32::from_rgb(0x00, 0x00, 0x00);
/// Text and icons that are on.
pub const TEXT: Color32 = Color32::from_rgb(0xff, 0xff, 0xff);
/// Secondary text: artists, times, counts, "Loading…" (the text at 65 % over the black).
pub const SECONDARY: Color32 = Color32::from_rgb(0xa6, 0xa6, 0xa6);
/// Quiet, small marks: controls at rest, the speaker, a slider's played part.
pub const QUIET: Color32 = Color32::from_rgb(0x80, 0x80, 0x80);
/// Lines and borders: separators, outlines, the empty heart, a cover still loading,
/// controls that are off.
pub const LINES: Color32 = Color32::from_rgb(0x5c, 0x57, 0x66);
/// Spotify's green. As ink on black: the playing row (its title, ▶ and stripe), the liked
/// heart, toggles that are on, focus outlines, the shown tab's underline, the current lyric
/// line. As a fill: main buttons, with [`ON_ACCENT`] on it.
pub const ACCENT: Color32 = Color32::from_rgb(0x1e, 0xd7, 0x60);
/// Text and marks on an accent fill: black (white on this green is 1.9:1).
pub const ON_ACCENT: Color32 = Color32::from_rgb(0x00, 0x00, 0x00);
/// A selected row or entry: the accent at 18 % over the black, white text on it; rows are
/// large and the panel is OLED, so never the green at full strength.
pub const SELECTED: Color32 = Color32::from_rgb(0x05, 0x27, 0x11);
/// The sidebar, the bottom bar, the side panel, cards, inputs: Skunk's tile, the text at
/// 6 % over the black.
pub const SURFACE: Color32 = Color32::from_rgb(0x0f, 0x0f, 0x0f);
/// Under the pointer: the text at 11 % over the black.
pub const HOVER: Color32 = Color32::from_rgb(0x1c, 0x1c, 0x1c);
/// An error's words and border, on its box.
pub const ERROR_TEXT: Color32 = Color32::from_rgb(0xff, 0x8a, 0x93);
/// An error's box.
pub const ERROR_FILL: Color32 = Color32::from_rgb(0x4a, 0x0f, 0x14);
/// The layer that darkens an artist's header image under the name: black, not a tint, so
/// the image keeps its colours.
pub const SCRIM: Color32 = Color32::from_black_alpha(140);
/// What cover art is drawn with: no tint.
pub const UNTINTED: Color32 = Color32::WHITE;

/// Corners of buttons, cards, banners and inputs, in points: softened, never round.
pub const CORNER: u8 = 3;

/// A track row's height, in points.
pub const ROW_HEIGHT: f32 = 40.0;
/// A track row's cover thumbnail, in points.
pub const THUMB: f32 = 32.0;
/// The accent stripe down the playing row's (and the current page's) left edge.
pub const STRIPE_WIDTH: f32 = 3.0;
/// The outline keyboard focus adds: 1 px in the accent.
pub const FOCUS: Stroke = Stroke {
    width: 1.0,
    color: ACCENT,
};

/// Icons, a third bigger than the first look's (the owner, 2026-11-06: "a little bigger
/// for everything like the heart"): glyphs set in a track row, the playing row's ▶ and the
/// heart (were the text's 12.5).
pub const ICON_SMALL: f32 = 17.0;
/// The bottom bar's glyphs (the heart, previous, next; were 16), and the square of the
/// warning sign and the settings gear (were a line of text, about 16).
pub const ICON: f32 = 21.0;
/// The square the bottom bar's drawn icons fill: shuffle, repeat, the panel's toggle, the
/// speaker (were 14).
pub const ICON_DRAWN: f32 = 19.0;
/// Play and pause in the bottom bar (were 20).
pub const ICON_LARGE: f32 = 27.0;
/// A card's play button, across (was 36).
pub const PLAY_BUTTON: f32 = 48.0;

/// egui's dark visuals in the palette: white text on black, buttons outlined in
/// [`LINES`] with no fill of their own, [`HOVER`] under the pointer, the accent for focus
/// and (black on it) selected text, corners softened, and no shadows.
pub fn visuals() -> Visuals {
    let thin = |colour| Stroke::new(1.0, colour);
    let corner = CornerRadius::same(CORNER);
    let mut v = Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = BG;
    v.window_stroke = thin(LINES);
    v.window_shadow = Shadow::NONE;
    v.popup_shadow = Shadow::NONE;
    v.window_corner_radius = corner;
    v.menu_corner_radius = corner;
    v.extreme_bg_color = BG;
    v.faint_bg_color = SURFACE;
    v.code_bg_color = SURFACE;
    v.text_edit_bg_color = Some(SURFACE);
    v.weak_text_color = Some(SECONDARY);
    v.hyperlink_color = TEXT;
    v.warn_fg_color = TEXT;
    v.error_fg_color = ERROR_TEXT;
    // Selected text turns black on the green, as a main button's words are.
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = thin(ON_ACCENT);
    v.text_cursor.stroke.color = TEXT;
    v.ime_composition.active_underline_stroke.color = TEXT;
    v.ime_composition.inactive_underline_stroke.color = QUIET;

    let w = &mut v.widgets;
    // Labels, separators, frames.
    w.noninteractive.bg_fill = BG;
    w.noninteractive.weak_bg_fill = BG;
    w.noninteractive.bg_stroke = thin(LINES);
    w.noninteractive.fg_stroke = thin(TEXT);
    // A button at rest: Skunk's second button, a line round its words and no fill of its
    // own, so it sits as well on a surface or an error's box as on the background.
    w.inactive.bg_fill = SURFACE;
    w.inactive.weak_bg_fill = Color32::TRANSPARENT;
    w.inactive.bg_stroke = thin(LINES);
    w.inactive.fg_stroke = thin(TEXT);
    w.hovered.bg_fill = HOVER;
    w.hovered.weak_bg_fill = HOVER;
    w.hovered.bg_stroke = thin(LINES);
    w.hovered.fg_stroke = thin(TEXT);
    w.active.bg_fill = HOVER;
    w.active.weak_bg_fill = HOVER;
    // A focused widget is drawn in its active look: the focus outline.
    w.active.bg_stroke = FOCUS;
    w.active.fg_stroke = thin(TEXT);
    w.open.bg_fill = HOVER;
    w.open.weak_bg_fill = HOVER;
    w.open.bg_stroke = thin(LINES);
    w.open.fg_stroke = thin(TEXT);
    for state in [
        &mut w.noninteractive,
        &mut w.inactive,
        &mut w.hovered,
        &mut w.active,
        &mut w.open,
    ] {
        state.corner_radius = corner;
    }
    v
}

/// A page's main action (Log in, Try again, Retry, Start radio): the green with black
/// words on it; under keyboard focus, a ring in the accent a little outside it, since an
/// outline in the green on the green wouldn't show.
pub fn main_button(ui: &mut Ui, text: &str) -> Response {
    let button = Button::new(RichText::new(text).color(ON_ACCENT))
        .fill(ACCENT)
        .stroke(Stroke::NONE)
        .corner_radius(CORNER);
    let response = ui.add(button);
    if response.has_focus() {
        let ring = response.rect.expand(2.0);
        ui.painter()
            .rect_stroke(ring, CORNER + 2, FOCUS, StrokeKind::Outside);
    }
    response
}

/// A box with a 1 px line round it, for what is set apart but isn't an error (a
/// confirmation, the expiry warning, a log).
pub fn outline() -> Frame {
    Frame::new()
        .stroke(Stroke::new(1.0, LINES))
        .corner_radius(CORNER)
        .inner_margin(Margin::symmetric(12, 8))
}

/// An error's box: Skunk's red, its words and border in one light red, never white.
pub fn error_box() -> Frame {
    Frame::new()
        .fill(ERROR_FILL)
        .stroke(Stroke::new(1.0, ERROR_TEXT))
        .corner_radius(CORNER)
        .inner_margin(Margin::symmetric(12, 8))
}

/// Inside an error's box: text (and the warning sign, which takes the text's colour) in
/// the error's light red, as are the lines round a second button. A second button puts
/// no grey on the red: under the pointer its line thickens instead (pressed or focused,
/// it shows the focus outline, as anywhere).
pub fn error_ink(ui: &mut Ui) {
    let v = ui.visuals_mut();
    v.override_text_color = Some(ERROR_TEXT);
    let w = &mut v.widgets;
    w.inactive.bg_stroke.color = ERROR_TEXT;
    w.hovered.bg_stroke = Stroke::new(2.0, ERROR_TEXT);
    for state in [&mut w.hovered, &mut w.active] {
        state.weak_bg_fill = Color32::TRANSPARENT;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `#rrggbb` as a colour.
    fn hex(s: &str) -> Color32 {
        let s = s.strip_prefix('#').unwrap();
        let byte = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).unwrap();
        Color32::from_rgb(byte(0), byte(2), byte(4))
    }

    /// WCAG's contrast ratio of two opaque colours.
    fn contrast(a: Color32, b: Color32) -> f32 {
        let luminance = |c: Color32| {
            let lin = |v: u8| {
                let v = f32::from(v) / 255.0;
                if v <= 0.040_45 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
        };
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn palette_values() {
        // The owner's Look 2 (2026-11-06): Skunk's dark scheme, Spotify's green for the
        // purple.
        assert_eq!(BG, hex("#000000"));
        assert_eq!(TEXT, hex("#ffffff"));
        assert_eq!(SECONDARY, hex("#a6a6a6"));
        assert_eq!(QUIET, hex("#808080"));
        assert_eq!(LINES, hex("#5c5766"));
        assert_eq!(ACCENT, hex("#1ed760"));
        assert_eq!(ON_ACCENT, hex("#000000"));
        assert_eq!(ERROR_TEXT, hex("#ff8a93"));
        assert_eq!(ERROR_FILL, hex("#4a0f14"));
        // Skunk's tile (the text at 6 %) and the hover (at 11 %), over the black.
        assert_eq!(SURFACE, hex("#0f0f0f"));
        assert_eq!(HOVER, hex("#1c1c1c"));
        assert_eq!(CORNER, 3);
        assert_eq!((ROW_HEIGHT, THUMB, STRIPE_WIDTH), (40.0, 32.0, 3.0));
    }

    /// A selected row is the accent at 18 % over the black, quiet enough for a large
    /// area on OLED, with white text on it.
    #[test]
    fn selection_is_a_quiet_accent() {
        let over_black = |c: Color32, a: f32| {
            let mix = |v: u8| (f32::from(v) * a).round() as u8;
            Color32::from_rgb(mix(c.r()), mix(c.g()), mix(c.b()))
        };
        assert_eq!(SELECTED, over_black(ACCENT, 0.18));
        assert!(contrast(TEXT, SELECTED) >= 7.0);
    }

    /// What is read on what: black on the green (white on it is 1.9:1), the green and the
    /// greys as ink on black, the error's words on its box.
    #[test]
    fn contrast_holds() {
        assert!(contrast(TEXT, ACCENT) < 2.0);
        assert!(contrast(ON_ACCENT, ACCENT) >= 7.0);
        assert!(contrast(ACCENT, BG) >= 7.0);
        assert!(contrast(ACCENT, SURFACE) >= 7.0);
        assert!(contrast(SECONDARY, SURFACE) >= 7.0);
        assert!(contrast(QUIET, SURFACE) >= 4.5);
        assert!(contrast(ERROR_TEXT, ERROR_FILL) >= 4.5);
    }

    #[test]
    fn visuals_follow_the_palette() {
        let v = visuals();
        assert!(v.dark_mode);
        assert_eq!(v.panel_fill, BG);
        assert_eq!(v.window_fill, BG);
        assert_eq!(v.window_stroke, Stroke::new(1.0, LINES));
        assert_eq!(v.extreme_bg_color, BG);
        assert_eq!(v.faint_bg_color, SURFACE);
        assert_eq!(v.text_edit_bg_color(), SURFACE);
        assert_eq!(v.widgets.noninteractive.bg_stroke, Stroke::new(1.0, LINES));
        // A button at rest: no fill of its own, a line round it.
        assert_eq!(v.widgets.inactive.weak_bg_fill, Color32::TRANSPARENT);
        assert_eq!(v.widgets.inactive.bg_stroke, Stroke::new(1.0, LINES));
        for state in [&v.widgets.hovered, &v.widgets.active, &v.widgets.open] {
            assert_eq!(state.bg_fill, HOVER);
            assert_eq!(state.weak_bg_fill, HOVER);
        }
        // Selected text is black on the green, as on a main button.
        assert_eq!(v.selection.bg_fill, ACCENT);
        assert_eq!(v.selection.stroke.color, ON_ACCENT);
        assert_eq!(v.text_color(), TEXT);
        assert_eq!(v.weak_text_color(), SECONDARY);
        assert_eq!(v.hyperlink_color, TEXT);
        assert_eq!(v.error_fg_color, ERROR_TEXT);
        assert_eq!(v.window_shadow, Shadow::NONE);
        assert_eq!(v.popup_shadow, Shadow::NONE);
    }

    /// Keyboard focus is a 1 px outline in the accent: egui draws a focused widget in its
    /// `active` look.
    #[test]
    fn focus_outline_is_the_accent() {
        assert_eq!(FOCUS, Stroke::new(1.0, hex("#1ed760")));
        assert_eq!(visuals().widgets.active.bg_stroke, FOCUS);
    }

    /// Corners are softened 3 px wherever egui draws a box: buttons, inputs, menus.
    #[test]
    fn corners_are_softened() {
        let v = visuals();
        let w = &v.widgets;
        for state in [
            &w.noninteractive,
            &w.inactive,
            &w.hovered,
            &w.active,
            &w.open,
        ] {
            assert_eq!(state.corner_radius, CornerRadius::same(CORNER));
        }
        assert_eq!(v.window_corner_radius, CornerRadius::same(CORNER));
        assert_eq!(v.menu_corner_radius, CornerRadius::same(CORNER));
    }

    /// A main button is black words on the green; focused, an accent ring just outside it
    /// (an outline on the green in the green wouldn't show).
    #[test]
    fn main_button_and_its_focus_ring() {
        use egui_kittest::{Harness, kittest::Queryable};

        use crate::test_support;

        let mut harness = Harness::new_ui(|ui| {
            main_button(ui, "Go");
        });
        harness.run();
        let rect = harness.get_by_label("Go").rect();
        test_support::assert_main_button(&harness, "Go", rect);
        let ring = |h: &Harness<'_>| {
            test_support::outlines(h)
                .into_iter()
                .filter(|(_, stroke)| *stroke == FOCUS)
                .map(|(r, _)| r)
                .collect::<Vec<_>>()
        };
        assert!(ring(&harness).is_empty());
        harness.get_by_label("Go").focus();
        harness.run();
        let rings = ring(&harness);
        assert_eq!(rings.len(), 1, "{rings:?}");
        let green: Vec<_> = test_support::fills(&harness)
            .into_iter()
            .filter(|(_, c)| *c == ACCENT)
            .map(|(r, _)| r)
            .collect();
        assert_eq!(green.len(), 1, "{green:?}");
        // Outside the fill, with a gap of black between them.
        let (ring, fill) = (rings[0], green[0]);
        assert!(ring.contains_rect(fill.expand(0.5)), "{ring:?} {fill:?}");
    }

    /// Every colour the visuals use is one of the palette's, or none (a resting button's
    /// fill): no blue cursor, no red error, no orange warning from egui's defaults.
    #[test]
    fn visuals_use_only_the_palette() {
        let palette = [
            BG,
            TEXT,
            SECONDARY,
            QUIET,
            LINES,
            ACCENT,
            ON_ACCENT,
            SELECTED,
            SURFACE,
            HOVER,
            ERROR_TEXT,
            ERROR_FILL,
            Color32::TRANSPARENT,
        ];
        let v = visuals();
        let w = &v.widgets;
        let mut colours = vec![
            v.panel_fill,
            v.window_fill,
            v.window_stroke.color,
            v.extreme_bg_color,
            v.faint_bg_color,
            v.code_bg_color,
            v.text_edit_bg_color(),
            v.hyperlink_color,
            v.warn_fg_color,
            v.error_fg_color,
            v.selection.bg_fill,
            v.selection.stroke.color,
            v.text_cursor.stroke.color,
            v.ime_composition.active_underline_stroke.color,
            v.ime_composition.inactive_underline_stroke.color,
            v.weak_text_color(),
        ];
        for state in [
            &w.noninteractive,
            &w.inactive,
            &w.hovered,
            &w.active,
            &w.open,
        ] {
            colours.extend([
                state.bg_fill,
                state.weak_bg_fill,
                state.bg_stroke.color,
                state.fg_stroke.color,
            ]);
        }
        for c in colours {
            assert!(palette.contains(&c), "{c:?} isn't in the palette");
        }
    }
}
