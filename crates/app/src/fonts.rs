//! Fonts: JetBrains Mono for the whole window (the owner, 2026-11-06), else the desktop's
//! UI font, and fallbacks for the symbols the window draws (hearts, transport, the warning
//! sign) and for Japanese, Korean and Chinese names, which egui's bundled fonts can't draw.
//!
//! JetBrains Mono is found by its exact family with `fc-list`, not `fc-match`: the owner's
//! fontconfig aliases the name to another font for the rest of the desktop, and wants
//! JetBrains Mono in this app only. Only its Regular face is loaded: egui draws no bold
//! (a heading is larger, not heavier), so no text here is bold. Without it, fontconfig
//! picks the desktop's font from `kdeglobals`, as before.
//!
//! The UI font and the symbol fonts are read while the window opens and installed before
//! the first frame; the CJK fonts (tens of MB) load on their own thread afterwards and are
//! added when ready, so they never delay the first frame.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use eframe::egui::{self, FontData, FontFamily, FontInsert, FontPriority, InsertFontFamily};
use log::{info, warn};
use skrifa::MetadataProvider;

/// The family when `kdeglobals` names none.
pub const DEFAULT_FAMILY: &str = "monospace";

/// The languages fallback fonts are looked up for, in the order they are added.
pub const CJK_LANGS: [&str; 3] = ["ja", "ko", "zh-cn"];

/// What `fc-match` prints for a match: the file, the face's index in it, and the
/// languages it covers (`aa|ja|ko|…`), a line each.
const FC_FORMAT: &str = "%{file}\n%{index}\n%{lang}\n";

/// A face fontconfig picked: its file, its index in the file (a `.ttc` holds several),
/// and the languages it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Face {
    pub path: PathBuf,
    pub index: u32,
    langs: Vec<String>,
}

impl Face {
    /// Whether fontconfig says this face covers `lang` (`ja`, `zh-cn`, …).
    pub fn covers(&self, lang: &str) -> bool {
        self.langs.iter().any(|l| l == lang)
    }
}

/// The desktop's UI font family, from `kdeglobals`.
pub fn ui_font_family() -> String {
    match kdeglobals_path() {
        Some(path) => ui_font_family_in(&path),
        None => DEFAULT_FAMILY.to_owned(),
    }
}

/// The family `[General] font=<family>,<size>,…` names in the `kdeglobals` at `path`;
/// `DEFAULT_FAMILY` when the file or the key is missing.
pub fn ui_font_family_in(path: &Path) -> String {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| family_in(&text))
        .unwrap_or_else(|| DEFAULT_FAMILY.to_owned())
}

/// The family of `[General]`'s `font` key, if it names one.
fn family_in(kdeglobals: &str) -> Option<String> {
    let mut in_general = false;
    for line in kdeglobals.lines().map(str::trim) {
        if line.starts_with('[') {
            in_general = line == "[General]";
        } else if in_general
            && let Some((key, value)) = line.split_once('=')
            && key.trim() == "font"
        {
            let family = value.split(',').next().unwrap_or("").trim();
            return (!family.is_empty()).then(|| family.to_owned());
        }
    }
    None
}

/// `$XDG_CONFIG_HOME/kdeglobals`, else `~/.config/kdeglobals`; `None` with neither set.
fn kdeglobals_path() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(config.join("kdeglobals"))
}

/// The face fontconfig picks for `pattern` (`fc-match -f '%{file}' <pattern>`, with the
/// face index and languages beside the file). `None` when fontconfig can't be asked.
pub fn resolve(pattern: &str) -> Option<Face> {
    let out = Command::new("fc-match")
        .args(["-f", FC_FORMAT, "--", pattern])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(out) if out.status.success() => parse_match(&String::from_utf8_lossy(&out.stdout)),
        Ok(out) => {
            info!("fc-match {pattern:?}: {}", out.status);
            None
        }
        Err(e) => {
            info!("fontconfig: can't run fc-match: {e}");
            None
        }
    }
}

/// `fc-match`'s answer, in `FC_FORMAT`.
fn parse_match(out: &str) -> Option<Face> {
    let mut lines = out.lines();
    let path = lines.next().filter(|p| !p.is_empty())?;
    let index = lines
        .next()
        .and_then(|i| i.trim().parse().ok())
        .unwrap_or(0);
    let langs = lines
        .next()
        .unwrap_or("")
        .split('|')
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    Some(Face {
        path: path.into(),
        index,
        langs,
    })
}

/// A font file's bytes, if egui can parse them: egui panics on a font it can't read.
pub fn read(face: &Face) -> Option<FontData> {
    let bytes = match std::fs::read(&face.path) {
        Ok(bytes) => bytes,
        Err(e) => {
            warn!("font {}: {e}", face.path.display());
            return None;
        }
    };
    // The parse egui's own loading does, so what passes here can't make it panic.
    if let Err(e) = skrifa::FontRef::from_index(&bytes, face.index) {
        warn!(
            "font {} (face {}): not a font egui can read: {e}",
            face.path.display(),
            face.index
        );
        return None;
    }
    let mut data = FontData::from_owned(bytes);
    data.index = face.index;
    Some(data)
}

/// The UI font, read and ready to install.
pub struct UiFont {
    pub family: String,
    pub face: Face,
    pub data: FontData,
}

/// Resolves and reads the UI font. Blocking (a subprocess and a file read): call it off
/// the UI thread.
pub fn load_ui_font() -> Option<UiFont> {
    let (family, face, data) = ui_font_with(
        || exact_face(UI_FAMILY, "Regular"),
        ui_font_family,
        resolve,
        read,
    )?;
    Some(UiFont { family, face, data })
}

/// The window's font (the owner, 2026-11-06): JetBrains Mono, found by its exact family.
pub const UI_FAMILY: &str = "JetBrains Mono";

/// What `fc-list` prints for each face: its file, its index in the file and its styles
/// (`Medium,Regular`), tab-separated, a line each.
const FC_LIST_FORMAT: &str = "%{file}\t%{index}\t%{style}\n";

/// The face of exactly `family` in `style`, from `fc-list`; `None` when none is installed
/// or fontconfig can't be asked. Never `fc-match`, which follows aliases: the owner's
/// `fonts.conf` maps "JetBrains Mono" to another font for every other app.
pub fn exact_face(family: &str, style: &str) -> Option<Face> {
    let pattern = format!("{family}:style={style}");
    let out = Command::new("fc-list")
        .args(["-f", FC_LIST_FORMAT, "--", &pattern])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(out) if out.status.success() => parse_list(&String::from_utf8_lossy(&out.stdout), style),
        Ok(out) => {
            info!("fc-list {pattern:?}: {}", out.status);
            None
        }
        Err(e) => {
            info!("fontconfig: can't run fc-list: {e}");
            None
        }
    }
}

/// `fc-list`'s answer in [`FC_LIST_FORMAT`]: the face whose own style (the first of its
/// styles) is `style`, since `:style=Regular` lists Medium (`Medium,Regular`) and the
/// rest of the family too; the first by path when there are several.
fn parse_list(out: &str, style: &str) -> Option<Face> {
    out.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let path = fields.next().filter(|p| !p.is_empty())?;
            let index = fields.next()?.trim().parse().ok()?;
            let own = fields.next()?.split(',').next()?.trim();
            (own == style).then(|| Face {
                path: path.into(),
                index,
                langs: Vec::new(),
            })
        })
        .min_by(|a, b| a.path.cmp(&b.path))
}

/// The UI font's family, face and data: `exact`'s face when it has one that reads, else
/// fontconfig's answer (`resolve`) for the desktop's family.
fn ui_font_with<D>(
    exact: impl FnOnce() -> Option<Face>,
    desktop_family: impl FnOnce() -> String,
    resolve: impl Fn(&str) -> Option<Face>,
    read: impl Fn(&Face) -> Option<D>,
) -> Option<(String, Face, D)> {
    if let Some(face) = exact() {
        match read(&face) {
            Some(data) => return Some((UI_FAMILY.to_owned(), face, data)),
            None => warn!("{UI_FAMILY}: {} won't read", face.path.display()),
        }
    } else {
        info!("{UI_FAMILY} isn't installed; using the desktop's font");
    }
    let family = desktop_family();
    let face = resolve(&family)?;
    let data = read(&face)?;
    Some((family, face, data))
}

/// Adds `data` to the proportional and monospace families, first or last.
pub fn install(ctx: &egui::Context, name: &str, data: FontData, priority: FontPriority) {
    let families = [FontFamily::Proportional, FontFamily::Monospace]
        .into_iter()
        .map(|family| InsertFontFamily { family, priority })
        .collect();
    ctx.add_font(FontInsert::new(name, data, families));
}

/// The symbols the window draws: the hearts, play, pause, previous, next, and the
/// warning sign. egui's bundled fonts have almost none of them.
pub const SYMBOLS: [char; 7] = ['♥', '♡', '▶', '⚠', '⏸', '⏮', '⏭'];

/// A fallback font for symbols, read and ready to install, and which of the symbols it
/// was added for.
pub struct SymbolFont {
    pub face: Face,
    pub data: FontData,
    pub covers: Vec<char>,
}

/// Which of `needed` a font covers, `has` being its charmap; in `needed`'s order.
pub fn covered(has: impl Fn(char) -> bool, needed: &[char]) -> Vec<char> {
    needed.iter().copied().filter(|&c| has(c)).collect()
}

/// Which of `needed` the font `data` has a glyph for.
pub fn covered_by(data: &FontData, needed: &[char]) -> Vec<char> {
    match skrifa::FontRef::from_index((*data.font).as_ref(), data.index) {
        Ok(font) => {
            let charmap = font.charmap();
            covered(|c| charmap.map(c).is_some(), needed)
        }
        Err(_) => Vec::new(),
    }
}

/// The fontconfig pattern for a font with every one of `chars`: `:charset=2665 26a0`.
fn charset_pattern(chars: &[char]) -> String {
    let hex: Vec<String> = chars
        .iter()
        .map(|&c| format!("{:x}", u32::from(c)))
        .collect();
    format!(":charset={}", hex.join(" "))
}

/// The fonts that give each of `needed` a glyph: fontconfig's pick for all of them,
/// then its pick for each one still missing; each file once (`skip`, the UI font's, not
/// at all), and only one that covers something still missing. `load` reads a face and
/// says which of the given characters it covers.
fn symbol_fonts_with<D>(
    needed: &[char],
    resolve: impl Fn(&str) -> Option<Face>,
    load: impl Fn(&Face, &[char]) -> Option<(D, Vec<char>)>,
    skip: Option<&Path>,
) -> Vec<(Face, D, Vec<char>)> {
    let mut missing = needed.to_vec();
    let mut seen: HashSet<PathBuf> = skip.map(Path::to_path_buf).into_iter().collect();
    let mut fonts = Vec::new();
    // All of them at once first; then, one at a time, those still missing.
    let mut wanted = vec![needed.to_vec()];
    wanted.extend(needed.iter().map(|&c| vec![c]));
    for chars in wanted {
        if !chars.iter().any(|c| missing.contains(c)) {
            continue;
        }
        let Some(face) = resolve(&charset_pattern(&chars)) else {
            continue;
        };
        if !seen.insert(face.path.clone()) {
            continue;
        }
        let Some((data, covers)) = load(&face, &missing) else {
            continue;
        };
        if covers.is_empty() {
            continue;
        }
        missing.retain(|c| !covers.contains(c));
        fonts.push((face, data, covers));
    }
    fonts
}

/// Resolves and reads fallback fonts for the [`SYMBOLS`] the UI font (`ui`) lacks.
/// Blocking (subprocesses and file reads): call it off the UI thread.
pub fn load_symbol_fonts(ui: Option<&UiFont>) -> Vec<SymbolFont> {
    let needed: Vec<char> = match ui {
        Some(ui) => {
            let has = covered_by(&ui.data, &SYMBOLS);
            SYMBOLS.into_iter().filter(|c| !has.contains(c)).collect()
        }
        None => SYMBOLS.to_vec(),
    };
    let load = |face: &Face, chars: &[char]| {
        let data = read(face)?;
        let covers = covered_by(&data, chars);
        Some((data, covers))
    };
    let skip = ui.map(|ui| ui.face.path.as_path());
    let fonts: Vec<SymbolFont> = symbol_fonts_with(&needed, resolve, load, skip)
        .into_iter()
        .map(|(face, data, covers)| SymbolFont { face, data, covers })
        .collect();
    let found: Vec<char> = fonts
        .iter()
        .flat_map(|f| f.covers.iter().copied())
        .collect();
    let still: String = needed.iter().filter(|c| !found.contains(c)).collect();
    if !still.is_empty() {
        info!("no font has {still}: those show as boxes");
    }
    fonts
}

/// Installs the UI font in front of egui's own, and the symbol fonts right after it.
/// With no UI font, egui's own lead and the symbol fonts follow them.
pub fn install_ui_fonts(ctx: &egui::Context, ui: Option<UiFont>, symbols: Vec<SymbolFont>) {
    for font in &symbols {
        let covers: String = font.covers.iter().collect();
        info!(
            "symbol font for {covers}: {} (face {})",
            font.face.path.display(),
            font.face.index
        );
    }
    let mut named: Vec<(String, FontData)> = symbols
        .into_iter()
        .enumerate()
        .map(|(i, font)| (format!("symbols-{i}"), font.data))
        .collect();
    match ui {
        Some(ui) => {
            // Each goes in front of those before it: the symbol fonts last to first, then
            // the UI font.
            named.reverse();
            for (name, data) in named {
                install(ctx, &name, data, FontPriority::Highest);
            }
            install_ui_font(ctx, ui);
        }
        None => {
            for (name, data) in named {
                install(ctx, &name, data, FontPriority::Lowest);
            }
        }
    }
}

/// Installs the UI font in front of egui's own.
pub fn install_ui_font(ctx: &egui::Context, font: UiFont) {
    info!(
        "UI font: {} ({}, face {})",
        font.family,
        font.face.path.display(),
        font.face.index
    );
    install(ctx, "ui", font.data, FontPriority::Highest);
}

/// On a thread of its own: finds the CJK fonts, adds them as the last fallbacks, then
/// asks for a repaint. `ui_font` is skipped, being installed already.
pub fn load_cjk_later(ctx: egui::Context, ui_font: Option<PathBuf>) {
    let spawned = std::thread::Builder::new()
        .name("cjk-fonts".into())
        .spawn(move || {
            let faces = cjk_faces(resolve, ui_font.as_deref());
            if faces.is_empty() {
                info!(
                    "no CJK font installed (fontconfig has none for {}): such names show as \
                     boxes; noto-fonts-cjk has them",
                    CJK_LANGS.join(", ")
                );
                return;
            }
            let mut added = false;
            for (lang, face) in faces {
                if let Some(data) = read(&face) {
                    info!(
                        "CJK font for {lang}: {} (face {})",
                        face.path.display(),
                        face.index
                    );
                    install(&ctx, &format!("cjk-{lang}"), data, FontPriority::Lowest);
                    added = true;
                }
            }
            if added {
                ctx.request_repaint();
            }
        });
    if let Err(e) = spawned {
        warn!("CJK fonts: no thread to load them on: {e}");
    }
}

/// The faces to add for `CJK_LANGS`: each that covers its language, once per file. Every
/// face of a pan-CJK collection (Noto Sans CJK) has every CJK glyph, and egui takes a glyph
/// from the first font that has it, so a second face of the same file would never be used.
fn cjk_faces(
    resolve: impl Fn(&str) -> Option<Face>,
    skip: Option<&Path>,
) -> Vec<(&'static str, Face)> {
    let mut seen: HashSet<PathBuf> = skip.map(Path::to_path_buf).into_iter().collect();
    CJK_LANGS
        .into_iter()
        .filter_map(|lang| {
            let face = resolve(&format!(":lang={lang}"))?;
            (face.covers(lang) && seen.insert(face.path.clone())).then_some((lang, face))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face(path: &str, langs: &str) -> Face {
        Face {
            path: path.into(),
            index: 0,
            langs: langs.split('|').map(str::to_owned).collect(),
        }
    }

    #[test]
    fn kde_font_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kdeglobals");
        std::fs::write(
            &path,
            "[Colors:View]\nfont=Wrong,9\n\n[General]\nfixed=Hack,10\n\
             font=JetBrains Mono,10,-1,5,400,0,0,0,0,0,0,0,0,0,0,1\nmenuFont=Wrong,10\n\n\
             [WM]\nfont=Wrong,10\n",
        )
        .unwrap();
        assert_eq!(ui_font_family_in(&path), "JetBrains Mono");

        assert_eq!(ui_font_family_in(&dir.path().join("missing")), "monospace");
        // A [General] without a font, or with an empty family.
        std::fs::write(&path, "[General]\nfixed=Hack,10\n[WM]\nfont=Wrong,10\n").unwrap();
        assert_eq!(ui_font_family_in(&path), "monospace");
        std::fs::write(&path, "[General]\nfont=,10\n").unwrap();
        assert_eq!(ui_font_family_in(&path), "monospace");
    }

    /// `fc-list "JetBrains Mono:style=Regular"` lists Medium, Light and the rest too (their
    /// styles are `Medium,Regular`, …): only the face whose own style is Regular counts.
    #[test]
    fn exact_face_from_fc_list() {
        let out = "/usr/share/fonts/TTF/JetBrainsMono-Medium.ttf\t0\tMedium,Regular\n\
                   /usr/share/fonts/TTF/JetBrainsMono-Thin.ttf\t0\tThin,Regular\n\
                   /usr/share/fonts/TTF/JetBrainsMono-Regular.ttf\t0\tRegular\n\
                   /usr/share/fonts/TTF/JetBrainsMono-ExtraBold.ttf\t0\tExtraBold,Regular\n";
        let face = parse_list(out, "Regular").unwrap();
        assert_eq!(
            face.path,
            Path::new("/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf")
        );
        assert_eq!(face.index, 0);
        assert_eq!(parse_list(out, "Bold"), None);
        assert_eq!(
            parse_list("/fonts/JBM.ttc\t2\tBold\n", "Bold").map(|f| (f.path, f.index)),
            Some(("/fonts/JBM.ttc".into(), 2))
        );
        // Two copies: the first by path, whatever order fontconfig lists them in.
        let two =
            "/usr/share/fonts/b/JBM.ttf\t0\tRegular\n/usr/share/fonts/a/JBM.ttf\t0\tRegular\n";
        assert_eq!(
            parse_list(two, "Regular").unwrap().path,
            Path::new("/usr/share/fonts/a/JBM.ttf")
        );
        // Not installed: fc-list prints nothing.
        assert_eq!(parse_list("", "Regular"), None);
        assert_eq!(parse_list("garbage\n", "Regular"), None);
    }

    /// JetBrains Mono when it is installed and reads; else today's way, fontconfig's
    /// answer for the desktop's family (which the owner's fonts.conf aliases elsewhere).
    #[test]
    fn ui_font_is_jetbrains_mono_else_the_desktop_font() {
        let jbm = face("/fonts/JetBrainsMono-Regular.ttf", "");
        let desktop = |family: &str| {
            assert_eq!(family, "JetBrains Mono");
            Some(face("/fonts/AtkynsonMono.otf", "en"))
        };
        let reads = |f: &Face| Some(f.path.clone());
        let got = ui_font_with(
            || Some(jbm.clone()),
            || "JetBrains Mono".into(),
            desktop,
            reads,
        );
        assert_eq!(
            got.map(|(family, f, _)| (family, f.path)),
            Some((UI_FAMILY.to_owned(), jbm.path.clone()))
        );

        // Not installed, or a file that won't read: the desktop's font.
        let got = ui_font_with(|| None, || "JetBrains Mono".into(), desktop, reads);
        assert_eq!(
            got.map(|(_, f, _)| f.path),
            Some("/fonts/AtkynsonMono.otf".into())
        );
        let only_desktop = |f: &Face| (f.path != jbm.path).then(|| f.path.clone());
        let got = ui_font_with(
            || Some(jbm.clone()),
            || "JetBrains Mono".into(),
            desktop,
            only_desktop,
        );
        assert_eq!(
            got.map(|(_, f, _)| f.path),
            Some("/fonts/AtkynsonMono.otf".into())
        );
        // Neither: egui's own fonts.
        assert!(ui_font_with(|| None, || "x".into(), |_| None, reads).is_none());
    }

    /// On this machine: JetBrains Mono is the UI font, by its exact family, not what
    /// fontconfig's alias for the name picks. Skipped quietly where it isn't installed.
    #[test]
    fn ui_font_here_is_jetbrains_mono() {
        let Some(face) = exact_face(UI_FAMILY, "Regular") else {
            eprintln!("JetBrains Mono isn't installed; skipped");
            return;
        };
        let ui = load_ui_font().expect("a UI font");
        assert_eq!(ui.family, UI_FAMILY);
        assert_eq!(ui.face, face);
        let file = face
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(file.starts_with("JetBrainsMono-Regular."), "{file}");
    }

    #[test]
    fn fc_match_output_parse() {
        let got =
            parse_match("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc\n1\naa|ja|ko|zh-cn\n");
        assert_eq!(
            got,
            Some(Face {
                path: "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc".into(),
                index: 1,
                langs: vec!["aa".into(), "ja".into(), "ko".into(), "zh-cn".into()],
            })
        );
        let got = got.unwrap();
        assert!(got.covers("ja") && got.covers("zh-cn"));
        assert!(!got.covers("zh") && !got.covers("zh-tw"));
        assert_eq!(parse_match(""), None);
        assert_eq!(parse_match("\n0\nen\n"), None);
    }

    #[test]
    fn cjk_faces_cover_their_language_once_per_file() {
        let noto = "/fonts/NotoSansCJK-Regular.ttc";
        let all = |_: &str| Some(face(noto, "en|ja|ko|zh-cn"));
        assert_eq!(
            cjk_faces(all, None),
            vec![("ja", face(noto, "en|ja|ko|zh-cn"))]
        );

        // No CJK font installed: fontconfig falls back to one that covers none of them.
        let none = |_: &str| Some(face("/fonts/Atkinson.otf", "en|fr"));
        assert_eq!(cjk_faces(none, None), vec![]);
        assert_eq!(cjk_faces(|_: &str| None, None), vec![]);

        // Separate fonts per language; the UI font is skipped.
        let split = |pattern: &str| match pattern {
            ":lang=ja" => Some(face("/fonts/ipa.ttf", "ja")),
            ":lang=ko" => Some(face("/fonts/nanum.ttf", "ko")),
            ":lang=zh-cn" => Some(face("/fonts/ui.ttf", "zh-cn")),
            _ => None,
        };
        assert_eq!(
            cjk_faces(split, Some(Path::new("/fonts/ui.ttf"))),
            vec![
                ("ja", face("/fonts/ipa.ttf", "ja")),
                ("ko", face("/fonts/nanum.ttf", "ko"))
            ]
        );
    }

    #[test]
    fn symbol_coverage_from_charmap() {
        assert_eq!(covered(|c| "♥▶x".contains(c), &SYMBOLS), vec!['♥', '▶']);
        assert_eq!(covered(|_| true, &SYMBOLS), SYMBOLS.to_vec());
        assert_eq!(covered(|_| false, &SYMBOLS), Vec::<char>::new());
        assert_eq!(charset_pattern(&['♥', '⚠', '⏭']), ":charset=2665 26a0 23ed");
    }

    /// One font for all the symbols if fontconfig has one; else one per symbol still
    /// missing, each file once, the UI font's never, and none that adds nothing.
    #[test]
    fn symbol_fonts_fill_the_gaps() {
        use std::cell::RefCell;

        // What each fake font covers.
        let charmaps = [
            ("/fonts/dejavu.ttf", "♥♡▶⚠"),
            ("/fonts/adwaita.ttf", "♥♡▶⚠⏸⏮"),
            ("/fonts/symbols2.ttf", "⏭"),
            ("/fonts/latin.ttf", "abc"),
            ("/fonts/ui.ttf", "♥♡▶⚠⏸⏮⏭"),
        ];
        let load = |face: &Face, needed: &[char]| {
            let (_, has) = charmaps.iter().find(|(p, _)| face.path == Path::new(p))?;
            Some(((), covered(|c| has.contains(c), needed)))
        };
        let added = |fonts: Vec<(Face, (), Vec<char>)>| -> Vec<(String, String)> {
            fonts
                .into_iter()
                .map(|(f, (), c)| (f.path.display().to_string(), c.into_iter().collect()))
                .collect()
        };
        let pair = |p: &str, c: &str| (p.to_string(), c.to_string());

        let asked = RefCell::new(Vec::new());
        let resolve = |pattern: &str| {
            asked.borrow_mut().push(pattern.to_string());
            let path = match pattern {
                ":charset=2665 2661 25b6 26a0 23f8 23ee 23ed" => "/fonts/dejavu.ttf",
                ":charset=23f8" => "/fonts/adwaita.ttf",
                ":charset=23ed" => "/fonts/symbols2.ttf",
                _ => "/fonts/latin.ttf",
            };
            Some(face(path, "en"))
        };
        assert_eq!(
            added(symbol_fonts_with(&SYMBOLS, resolve, load, None)),
            vec![
                pair("/fonts/dejavu.ttf", "♥♡▶⚠"),
                pair("/fonts/adwaita.ttf", "⏸⏮"),
                pair("/fonts/symbols2.ttf", "⏭"),
            ]
        );
        // ⏮ came with ⏸'s font, so it wasn't asked for alone.
        assert_eq!(
            asked.take(),
            vec![
                ":charset=2665 2661 25b6 26a0 23f8 23ee 23ed",
                ":charset=23f8",
                ":charset=23ed"
            ]
        );

        // One font has them all: it alone is added.
        let all = |_: &str| Some(face("/fonts/ui.ttf", "en"));
        assert_eq!(
            added(symbol_fonts_with(&SYMBOLS, all, load, None)),
            vec![pair("/fonts/ui.ttf", "♥♡▶⚠⏸⏮⏭")]
        );
        // ... unless it is the UI font, already installed.
        let fonts = symbol_fonts_with(&SYMBOLS, all, load, Some(Path::new("/fonts/ui.ttf")));
        assert_eq!(added(fonts), vec![]);

        // fontconfig gone, or with nothing that has them.
        assert_eq!(
            added(symbol_fonts_with(&SYMBOLS, |_| None, load, None)),
            vec![]
        );
        let latin = |_: &str| Some(face("/fonts/latin.ttf", "en"));
        assert_eq!(
            added(symbol_fonts_with(&SYMBOLS, latin, load, None)),
            vec![]
        );
        // Nothing needed, nothing asked.
        let never = |_: &str| -> Option<Face> { panic!("asked fontconfig for nothing") };
        assert_eq!(added(symbol_fonts_with(&[], never, load, None)), vec![]);
    }

    /// On this machine: once the fonts are loaded the symbols have glyphs in both
    /// families, and the symbol fonts sit right after the UI font. Skipped quietly where
    /// fontconfig finds no symbol font.
    #[test]
    fn symbols_have_glyphs_once_loaded() {
        let ui = load_ui_font();
        let ui_name = ui.as_ref().map(|_| "ui");
        let symbols = load_symbol_fonts(ui.as_ref());
        if symbols.is_empty() {
            eprintln!("fontconfig found no symbol font; skipped");
            return;
        }
        for font in &symbols {
            let covers: String = font.covers.iter().collect();
            eprintln!("symbol font for {covers}: {}", font.face.path.display());
        }
        let names: Vec<String> = (0..symbols.len()).map(|i| format!("symbols-{i}")).collect();
        let ctx = egui::Context::default();
        install_ui_fonts(&ctx, ui, symbols);
        // The fonts apply at the start of a pass.
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            let font = egui::FontId::new(14.0, family.clone());
            for c in ['♥', '♡', '▶', '⚠'] {
                assert!(
                    ctx.fonts_mut(|f| f.has_glyph(&font, c)),
                    "{c} in {family:?}"
                );
            }
            let order = ctx.fonts_mut(|f| f.definitions().families[&family].clone());
            let at = |name: &str| order.iter().position(|n| n == name).unwrap();
            if let Some(ui) = ui_name {
                assert_eq!(at(ui), 0, "{order:?}");
            }
            for (i, name) in names.iter().enumerate() {
                assert_eq!(at(name), i + usize::from(ui_name.is_some()), "{order:?}");
            }
        }
    }

    #[test]
    fn unreadable_fonts_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let garbage = dir.path().join("garbage.ttf");
        std::fs::write(&garbage, b"not a font at all").unwrap();
        assert!(read(&face(garbage.to_str().unwrap(), "en")).is_none());
        assert!(read(&face("/nonexistent/font.ttf", "en")).is_none());

        // egui's own Hack font, written to a file, reads.
        let defs = egui::FontDefinitions::default();
        let hack = dir.path().join("Hack.ttf");
        std::fs::write(&hack, defs.font_data["Hack"].bytes()).unwrap();
        let data = read(&face(hack.to_str().unwrap(), "en")).unwrap();
        assert_eq!(data.bytes(), defs.font_data["Hack"].bytes());
        // A face index past the file's faces is refused too.
        let mut past = face(hack.to_str().unwrap(), "en");
        past.index = 3;
        assert!(read(&past).is_none());
    }
}
