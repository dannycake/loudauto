//! What the kittest tests share: a harness with the window's real fonts, a link that
//! records what it is sent, the shapes a frame drew, and the web player's fixtures.

use std::cell::{Cell, RefCell};

use eframe::egui::{Color32, Rect, Shape, Vec2, epaint::TextShape};
use egui_kittest::{Harness, HarnessBuilder};
use engine_ipc::Request;

use crate::{
    fonts,
    link::{RequestId, Requests},
    theme,
};

/// The glyphs the window draws that egui's own fonts lack: the hearts, and the bottom
/// bar's transport.
pub const GLYPHS: [char; 6] = ['▶', '♥', '♡', '⏸', '⏮', '⏭'];

/// A harness `size` points big with the window's real fonts (the UI font and the
/// symbol fallbacks), and whether they have [`GLYPHS`]. Where fontconfig finds none,
/// or `tofu`, missing glyphs draw as boxes instead of failing the test.
pub fn builder<S>(size: Vec2, tofu: bool) -> (HarnessBuilder<S>, bool) {
    let ui = fonts::load_ui_font();
    let symbols = fonts::load_symbol_fonts(ui.as_ref());
    let covered = |c: char| {
        ui.as_ref()
            .is_some_and(|ui| !fonts::covered_by(&ui.data, &[c]).is_empty())
            || symbols.iter().any(|s| s.covers.contains(&c))
    };
    let missing: String = GLYPHS.into_iter().filter(|&c| !covered(c)).collect();
    let glyphs = missing.is_empty();
    let mut builder = Harness::builder()
        .with_size(size)
        .with_context_setup(move |ctx| fonts::install_ui_fonts(ctx, ui, symbols));
    if !glyphs {
        eprintln!("fontconfig found no font for {missing}; glyph checks skipped");
    }
    if !glyphs || tofu {
        builder = builder.allow_missing_glyphs();
    }
    (builder, glyphs)
}

fn walk_shapes<S>(harness: &Harness<'_, S>, mut each: impl FnMut(&Shape)) {
    fn walk(shape: &Shape, each: &mut impl FnMut(&Shape)) {
        match shape {
            Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, each)),
            shape => each(shape),
        }
    }
    for clipped in &harness.output().shapes {
        walk(&clipped.shape, &mut each);
    }
}

/// Every text drawn last frame: its text, the colour it shows in (its first section's,
/// else the painter's) and rect.
pub fn texts<S>(harness: &Harness<'_, S>) -> Vec<(String, Color32, Rect)> {
    let mut out = vec![];
    walk_shapes(harness, |shape| {
        if let Shape::Text(TextShape {
            pos,
            galley,
            fallback_color,
            override_text_color,
            ..
        }) = shape
        {
            let section = galley
                .job
                .sections
                .first()
                .map(|s| s.format.color)
                .filter(|c| *c != Color32::PLACEHOLDER);
            let shown = override_text_color.or(section).unwrap_or(*fallback_color);
            out.push((
                galley.text().to_owned(),
                shown,
                galley.rect.translate(pos.to_vec2()),
            ));
        }
    });
    out
}

/// The font size each text drawn last frame was set in (its first section's), with its
/// text and rect.
pub fn text_sizes<S>(harness: &Harness<'_, S>) -> Vec<(String, f32, Rect)> {
    let mut out = vec![];
    walk_shapes(harness, |shape| {
        if let Shape::Text(TextShape { pos, galley, .. }) = shape
            && let Some(section) = galley.job.sections.first()
        {
            out.push((
                galley.text().to_owned(),
                section.format.font_id.size,
                galley.rect.translate(pos.to_vec2()),
            ));
        }
    });
    out
}

/// The size `text` was set in last frame, if it was drawn once; panics if it was drawn
/// more than once at different sizes.
pub fn size_of<S>(harness: &Harness<'_, S>, text: &str) -> Option<f32> {
    let sizes: Vec<f32> = text_sizes(harness)
        .into_iter()
        .filter(|(t, _, _)| t == text)
        .map(|(_, size, _)| size)
        .collect();
    assert!(
        sizes.windows(2).all(|w| w[0] == w[1]),
        "{text} drawn at {sizes:?}"
    );
    sizes.first().copied()
}

/// The bounds of what was drawn last frame inside `area`, text left out: a drawn icon's
/// extent.
pub fn drawn_within<S>(harness: &Harness<'_, S>, area: Rect) -> Rect {
    let mut bounds = Rect::NOTHING;
    walk_shapes(harness, |shape| {
        if matches!(shape, Shape::Text(_)) {
            return;
        }
        let rect = shape.visual_bounding_rect();
        if area.contains_rect(rect) {
            bounds = bounds.union(rect);
        }
    });
    bounds
}

/// The colours of the marks drawn last frame inside `area`, text left out: a drawn
/// icon's lines and fills.
pub fn marks_within<S>(harness: &Harness<'_, S>, area: Rect) -> Vec<Color32> {
    let mut out = vec![];
    walk_shapes(harness, |shape| {
        if !area.contains_rect(shape.visual_bounding_rect()) {
            return;
        }
        let (fill, stroke) = match shape {
            Shape::Path(p) => match &p.stroke.color {
                eframe::epaint::ColorMode::Solid(c) => (p.fill, *c),
                eframe::epaint::ColorMode::UV(_) => (p.fill, Color32::TRANSPARENT),
            },
            Shape::LineSegment { stroke, .. } => (Color32::TRANSPARENT, stroke.color),
            Shape::Circle(c) => (c.fill, c.stroke.color),
            Shape::Rect(r) => (r.fill, r.stroke.color),
            _ => return,
        };
        out.extend(
            [fill, stroke]
                .into_iter()
                .filter(|c| *c != Color32::TRANSPARENT),
        );
    });
    out
}

/// Every circle drawn last frame: its centre, radius and fill.
pub fn circles<S>(harness: &Harness<'_, S>) -> Vec<(eframe::egui::Pos2, f32, Color32)> {
    let mut out = vec![];
    walk_shapes(harness, |shape| {
        if let Shape::Circle(c) = shape {
            out.push((c.center, c.radius, c.fill));
        }
    });
    out
}

/// Every filled rect drawn last frame.
pub fn fills<S>(harness: &Harness<'_, S>) -> Vec<(Rect, Color32)> {
    let mut out = vec![];
    walk_shapes(harness, |shape| {
        if let Shape::Rect(r) = shape
            && r.fill != Color32::TRANSPARENT
        {
            out.push((r.rect, r.fill));
        }
    });
    out
}

/// Every rect outline drawn last frame, with its stroke.
pub fn outlines<S>(harness: &Harness<'_, S>) -> Vec<(Rect, eframe::egui::Stroke)> {
    let mut out = vec![];
    walk_shapes(harness, |shape| {
        if let Shape::Rect(r) = shape
            && r.stroke.width > 0.0
            && r.stroke.color != Color32::TRANSPARENT
        {
            out.push((r.rect, r.stroke));
        }
    });
    out
}

/// `text`, drawn at `rect`, is in the error's light red inside its red box, outlined in
/// the same light red.
pub fn assert_in_red_box<S>(harness: &Harness<'_, S>, text: &str, rect: Rect) {
    let drawn = texts(harness)
        .into_iter()
        .find(|(t, _, r)| t == text && r.contains(rect.center()))
        .map(|(_, c, _)| c);
    assert_eq!(drawn, Some(theme::ERROR_TEXT), "{text}");
    let boxed = outlines(harness)
        .into_iter()
        .any(|(r, s)| r.contains_rect(rect) && s.color == theme::ERROR_TEXT);
    assert!(boxed, "no light red line round {text}");
    let filled = fills(harness)
        .into_iter()
        .any(|(r, c)| r.contains_rect(rect) && c == theme::ERROR_FILL);
    assert!(filled, "no red box under {text}");
}

/// The button `label`, drawn at `rect`, is a main one: black words on the green.
pub fn assert_main_button<S>(harness: &Harness<'_, S>, label: &str, rect: Rect) {
    let drawn = texts(harness)
        .into_iter()
        .find(|(t, _, r)| t == label && rect.contains(r.center()))
        .map(|(_, c, _)| c);
    assert_eq!(drawn, Some(theme::ON_ACCENT), "{label}");
    let filled = fills(harness)
        .into_iter()
        .any(|(r, c)| r.expand(1.0).contains_rect(rect) && c == theme::ACCENT);
    assert!(filled, "{label} isn't on the green");
}

/// A link that records what it is sent.
#[derive(Debug, Default)]
pub struct FakeLink {
    sent: RefCell<Vec<Request>>,
    last_id: Cell<RequestId>,
}

impl Requests for FakeLink {
    fn send(&self, req: Request) -> RequestId {
        self.sent.borrow_mut().push(req);
        self.last_id.set(self.last_id.get() + 1);
        self.last_id.get()
    }
}

impl FakeLink {
    /// The requests sent since the last call.
    pub fn take(&self) -> Vec<Request> {
        self.sent.borrow_mut().drain(..).collect()
    }

    /// The id of the request sent last.
    pub fn last_id(&self) -> RequestId {
        self.last_id.get()
    }
}

/// The web player's answer `name`, as captured in spot-core's fixtures.
pub fn fixture(name: &str) -> serde_json::Value {
    let path = format!(
        "{}/../spot-core/tests/fixtures/web/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}
