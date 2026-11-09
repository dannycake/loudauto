//! Which icon LOUDAUTO shows (the engine's tray and notifications, by name; the window's
//! icon, as a PNG file): `spotify`, else `spotify-launcher`, else `audio-x-generic`,
//! whichever the active icon theme can draw. Only what an installed theme or pixmaps dir
//! already has; never artwork of our own. Shared by the engine and the window.
//!
//! Plasma draws a name from the active theme (`kdeglobals` `[Icons] Theme=`), the
//! themes it inherits, then hicolor; a name only some other installed theme has (say
//! Papirus's `spotify`) shows as no icon at all, so only that chain counts.

use std::{
    collections::HashSet,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// In order of preference; the last is the fallback, which every theme has.
pub const ICON_CANDIDATES: [&str; 3] = ["spotify", "spotify-launcher", "audio-x-generic"];

/// How deep `theme_chain` follows `Inherits=`: the active theme is depth 0.
pub const MAX_THEME_DEPTH: usize = 10;

/// Plasma's icon theme when `kdeglobals` names none.
const DEFAULT_THEME: &str = "breeze";

/// The theme every chain ends with, as the icon theme spec says.
const FALLBACK_THEME: &str = "hicolor";

/// The file types an icon name resolves to.
const EXTENSIONS: [&str; 2] = ["svg", "png"];

/// The first of `candidates` that one of `themes` has under one of `dirs`, as
/// `<dir>/<theme>/<size>/apps/<name>.{svg,png}` or `<dir>/<theme>/apps/<size>/…` (any
/// size dir, `scalable` too), or that `<dir>/../pixmaps/<name>.{svg,png}` has; else the
/// last candidate (empty when there are none). Missing or unreadable dirs count as
/// empty. `themes` is the active chain, from `theme_chain`.
pub fn pick_icon_name(candidates: &[&str], dirs: &[PathBuf], themes: &[String]) -> String {
    candidates
        .iter()
        .find(|name| {
            let in_theme = themes
                .iter()
                .any(|theme| dirs.iter().any(|dir| in_theme_dir(&dir.join(theme), name)));
            in_theme || dirs.iter().any(|dir| in_pixmaps(dir, name))
        })
        .or(candidates.last())
        .map(|name| name.to_string())
        .unwrap_or_default()
}

/// The active icon theme and what it inherits: `[Icons] Theme=` from `kdeglobals`
/// (`breeze` when it says nothing), then each theme's `index.theme` `Inherits=` list
/// (from `read_index(theme)`), depth-first, each theme once, at most `MAX_THEME_DEPTH`
/// deep; then hicolor, unless it is already in the chain. A name that is a path (`..`,
/// `a/b`) is skipped.
pub fn theme_chain(
    kdeglobals: Option<&str>,
    read_index: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    let active = kdeglobals
        .and_then(|text| ini_value(text, "Icons", "Theme"))
        .unwrap_or(DEFAULT_THEME);
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![(active.to_string(), 0)];
    while let Some((theme, depth)) = stack.pop() {
        if depth > MAX_THEME_DEPTH || !is_theme_name(&theme) || !seen.insert(theme.clone()) {
            continue;
        }
        let inherits: Vec<String> = read_index(&theme)
            .as_deref()
            .and_then(|text| ini_value(text, "Icon Theme", "Inherits"))
            .map(|list| {
                list.split(',')
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        chain.push(theme);
        // Reversed, so the first inherited theme is the next one popped.
        stack.extend(inherits.into_iter().rev().map(|t| (t, depth + 1)));
    }
    if !chain.iter().any(|t| t == FALLBACK_THEME) {
        chain.push(FALLBACK_THEME.to_string());
    }
    chain
}

/// `theme`'s `index.theme`, from the first of `dirs` that has one.
pub fn read_index(dirs: &[PathBuf], theme: &str) -> Option<String> {
    dirs.iter()
        .find_map(|dir| std::fs::read_to_string(dir.join(theme).join("index.theme")).ok())
}

/// Where icon themes live: `$XDG_DATA_HOME/icons` (default `~/.local/share/icons`),
/// then `<entry>/icons` for each `$XDG_DATA_DIRS` entry (default
/// `/usr/local/share:/usr/share`).
pub fn icon_dirs() -> Vec<PathBuf> {
    icon_dirs_from(
        std::env::var_os("XDG_DATA_HOME"),
        std::env::var_os("HOME"),
        std::env::var_os("XDG_DATA_DIRS"),
    )
}

/// LOUDAUTO's icon name, resolved once: `ICON_CANDIDATES` against `icon_dirs()` and the
/// chain of `$XDG_CONFIG_HOME/kdeglobals` (default `~/.config/kdeglobals`). The first
/// call reads those files, so make it off the async threads. A theme switched while the
/// engine runs shows from its next start.
pub fn app_icon() -> &'static str {
    static ICON: OnceLock<String> = OnceLock::new();
    ICON.get_or_init(|| {
        let (dirs, themes) = active();
        let name = pick_icon_name(&ICON_CANDIDATES, &dirs, &themes);
        log::info!("icon: {name} (themes {})", themes.join(" → "));
        name
    })
}

/// `icon_dirs()` and the active theme chain, from `$XDG_CONFIG_HOME/kdeglobals` (default
/// `~/.config/kdeglobals`).
fn active() -> (Vec<PathBuf>, Vec<String>) {
    let dirs = icon_dirs();
    let kdeglobals = xdg_home(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
        ".config",
    )
    .and_then(|config| std::fs::read_to_string(config.join("kdeglobals")).ok());
    let themes = theme_chain(kdeglobals.as_deref(), |theme| read_index(&dirs, theme));
    (dirs, themes)
}

/// The PNG `name` is in the active chain (`themes`) under `dirs`, nearest `size` px (the
/// smallest at least that big, else the biggest), from the first theme that has one; else
/// `<dir>/../pixmaps/<name>.png`. `None` when it is SVG only, or nowhere.
pub fn png_file(name: &str, dirs: &[PathBuf], themes: &[String], size: u32) -> Option<PathBuf> {
    let file = format!("{name}.png");
    themes
        .iter()
        .find_map(|theme| {
            let found = dirs
                .iter()
                .flat_map(|dir| sized_pngs(&dir.join(theme), &file));
            nearest(found, size)
        })
        .or_else(|| {
            dirs.iter()
                .filter_map(|dir| dir.parent())
                .map(|share| share.join("pixmaps").join(&file))
                .find(|path| path.is_file())
        })
}

/// Every `<size>/apps/<file>` and `apps/<size>/<file>` under `theme_dir`, with the size
/// its dir names.
fn sized_pngs(theme_dir: &Path, file: &str) -> Vec<(Option<u32>, PathBuf)> {
    let mut out = Vec::new();
    for sub in subdirs(theme_dir) {
        let in_size_dirs: Vec<PathBuf> = if sub.file_name().is_some_and(|n| n == "apps") {
            subdirs(&sub).collect()
        } else {
            vec![sub.clone()]
        };
        for size_dir in in_size_dirs {
            let path = if size_dir == sub {
                sub.join("apps").join(file)
            } else {
                size_dir.join(file)
            };
            if path.is_file() {
                out.push((dir_size(&size_dir), path));
            }
        }
    }
    out
}

/// The pixels a size dir names: `48x48` and `48` are 48, `48x48@2` (or `@2x`) 96;
/// `None` for `scalable` and the like.
fn dir_size(dir: &Path) -> Option<u32> {
    let name = dir.file_name()?.to_str()?;
    let (base, scale) = match name.split_once('@') {
        Some((base, scale)) => (base, scale.trim_end_matches('x').parse().ok()?),
        None => (name, 1),
    };
    let side: u32 = base.split('x').next()?.parse().ok()?;
    side.checked_mul(scale)
}

/// The best of `found` for `want` px: the smallest at least that big, else the biggest,
/// one of no known size last; the first by path among equals.
fn nearest(found: impl IntoIterator<Item = (Option<u32>, PathBuf)>, want: u32) -> Option<PathBuf> {
    found
        .into_iter()
        .min_by_key(|(size, path)| {
            let rank = match *size {
                Some(s) if s >= want => (0, s),
                Some(s) => (1, u32::MAX - s),
                None => (2, 0),
            };
            (rank, path.clone())
        })
        .map(|(_, path)| path)
}

/// LOUDAUTO's icon as a PNG about `size` px big: the first of `ICON_CANDIDATES` the
/// active chain has as a PNG (an image can't be made of an SVG here). Reads the icon
/// dirs: call it off the UI thread.
pub fn app_icon_png(size: u32) -> Option<PathBuf> {
    let (dirs, themes) = active();
    ICON_CANDIDATES
        .iter()
        .find_map(|name| png_file(name, &dirs, &themes, size))
}

/// `icon_dirs` from the variables' values. Empty is unset and relative paths are
/// ignored, as the XDG Base Directory spec says.
fn icon_dirs_from(
    data_home: Option<OsString>,
    home: Option<OsString>,
    data_dirs: Option<OsString>,
) -> Vec<PathBuf> {
    let data_dirs: Vec<PathBuf> = data_dirs
        .filter(|v| !v.is_empty())
        .map(|v| {
            std::env::split_paths(&v)
                .filter(|p| p.is_absolute())
                .collect()
        })
        .unwrap_or_else(|| vec!["/usr/local/share".into(), "/usr/share".into()]);
    xdg_home(data_home, home, ".local/share")
        .into_iter()
        .chain(data_dirs)
        .map(|d| d.join("icons"))
        .collect()
}

/// An XDG base dir: `value` when it is an absolute path, else `$HOME/<default>`.
fn xdg_home(value: Option<OsString>, home: Option<OsString>, default: &str) -> Option<PathBuf> {
    let absolute = |v: Option<OsString>| v.map(PathBuf::from).filter(|p| p.is_absolute());
    absolute(value).or_else(|| absolute(home).map(|h| h.join(default)))
}

/// The value of `key` in `[section]` of an INI-style file, trimmed; `None` when absent
/// or empty. Comment lines (`#`, `;`) are skipped.
fn ini_value<'a>(text: &'a str, section: &str, key: &str) -> Option<&'a str> {
    let mut in_section = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_section = name == section;
        } else if in_section
            && let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            return Some(v.trim()).filter(|v| !v.is_empty());
        }
    }
    None
}

/// A plain directory name: never `.`, `..` or anything with a slash.
fn is_theme_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/')
}

/// Whether `theme_dir` has `name` as an app icon, in either layout:
/// `<size>/apps/<name>` (hicolor, Papirus) or `apps/<size>/<name>` (breeze).
fn in_theme_dir(theme_dir: &Path, name: &str) -> bool {
    subdirs(theme_dir).any(|sub| {
        if sub.file_name().is_some_and(|n| n == "apps") {
            subdirs(&sub).any(|size| has_file(&size, name))
        } else {
            has_file(&sub.join("apps"), name)
        }
    })
}

/// Whether `<dir>/../pixmaps` has `name`.
fn in_pixmaps(dir: &Path, name: &str) -> bool {
    dir.parent()
        .is_some_and(|share| has_file(&share.join("pixmaps"), name))
}

/// Whether `dir` holds `<name>.svg` or `<name>.png`.
fn has_file(dir: &Path, name: &str) -> bool {
    EXTENSIONS
        .iter()
        .any(|ext| dir.join(format!("{name}.{ext}")).is_file())
}

/// The directories in `dir`; none when it can't be read.
fn subdirs(dir: &Path) -> impl Iterator<Item = PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// Creates `root/rel` (and its parents) holding `text`.
    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn touch(root: &Path, rel: &str) {
        write(root, rel, "");
    }

    fn themes(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    /// The pick against `dirs`, with the chain read from `kdeglobals` and the dirs' own
    /// `index.theme` files, as `app_icon` does.
    fn pick_active(dirs: &[PathBuf], kdeglobals: Option<&str>) -> String {
        let chain = theme_chain(kdeglobals, |theme| read_index(dirs, theme));
        pick_icon_name(&ICON_CANDIDATES, dirs, &chain)
    }

    /// The owner's desktop: breeze-dark (inherits breeze, hicolor), breeze (inherits
    /// hicolor), Papirus installed but not active.
    fn owners_desktop(icons: &Path) {
        write(
            icons,
            "breeze-dark/index.theme",
            "[Icon Theme]\nName=Breeze Dark\nInherits=breeze,hicolor\n",
        );
        write(
            icons,
            "breeze/index.theme",
            "[Icon Theme]\nInherits=hicolor\n",
        );
        write(icons, "hicolor/index.theme", "[Icon Theme]\nName=Hicolor\n");
        touch(icons, "breeze-dark/apps/48/org.kde.dolphin.svg");
        touch(icons, "Papirus/48x48/apps/spotify.svg");
        touch(icons, "hicolor/48x48/apps/spotify-launcher.png");
    }

    const BREEZE_DARK: &str = "[General]\nColorScheme=BreezeDark\n\n[Icons]\nTheme=breeze-dark\n\n[KDE]\nSingleClick=false\n";

    #[test]
    fn icon_follows_the_active_theme_not_any_theme() {
        let tmp = tempfile::tempdir().unwrap();
        let icons = tmp.path().join("share/icons");
        owners_desktop(&icons);
        let dirs = [icons];

        // Papirus's spotify.svg isn't drawn by breeze-dark: Plasma would show no icon.
        assert_eq!(pick_active(&dirs, Some(BREEZE_DARK)), "spotify-launcher");

        // With Papirus active, its spotify icon is the one drawn.
        assert_eq!(
            pick_active(&dirs, Some("[Icons]\nTheme=Papirus\n")),
            "spotify"
        );
    }

    #[test]
    fn icon_prefers_spotify() {
        let tmp = tempfile::tempdir().unwrap();
        let icons = tmp.path().join("share/icons");
        fs::create_dir_all(&icons).unwrap();
        let dirs = [icons.clone()];
        let chain = themes(&["breeze", "hicolor"]);
        let pick = || pick_icon_name(&ICON_CANDIDATES, &dirs, &chain);

        assert_eq!(pick(), "audio-x-generic", "nothing installed");
        touch(&icons, "hicolor/48x48/apps/spotify-launcher.png");
        assert_eq!(pick(), "spotify-launcher");
        touch(&icons, "hicolor/scalable/apps/spotify.svg");
        assert_eq!(pick(), "spotify");
    }

    #[test]
    fn icon_in_context_first_layout() {
        // KDE themes (breeze, Tokyonight) keep `<theme>/apps/<size>/`.
        let tmp = tempfile::tempdir().unwrap();
        let icons = tmp.path().join("icons");
        touch(&icons, "breeze/apps/48/spotify.svg");
        assert_eq!(
            pick_icon_name(&ICON_CANDIDATES, &[icons], &themes(&["breeze", "hicolor"])),
            "spotify"
        );
    }

    #[test]
    fn icon_looks_in_every_dir_and_only_for_icons() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, system) = (tmp.path().join("home/icons"), tmp.path().join("usr/icons"));
        // Wrong format, wrong context, wrong name: none of them count.
        touch(&home, "hicolor/48x48/apps/spotify.xpm");
        touch(&home, "hicolor/48x48/actions/spotify.png");
        touch(&home, "hicolor/apps/48/spotify-client.png");
        let missing = tmp.path().join("nowhere/icons");
        let dirs = [missing, home, system.clone()];
        let chain = themes(&["hicolor"]);
        assert_eq!(
            pick_icon_name(&ICON_CANDIDATES, &dirs, &chain),
            "audio-x-generic"
        );

        // A later dir's copy of a chain theme counts.
        touch(&system, "hicolor/256x256/apps/spotify-launcher.png");
        assert_eq!(
            pick_icon_name(&ICON_CANDIDATES, &dirs, &chain),
            "spotify-launcher"
        );
    }

    #[test]
    fn icon_from_pixmaps() {
        let tmp = tempfile::tempdir().unwrap();
        let share = tmp.path().join("share");
        let dirs = [share.join("icons")];
        let chain = themes(&["breeze", "hicolor"]);
        // `<dir>/../pixmaps`, beside an icons dir that holds no theme at all.
        touch(&share, "pixmaps/spotify-launcher.png");
        assert_eq!(
            pick_icon_name(&ICON_CANDIDATES, &dirs, &chain),
            "spotify-launcher"
        );
        touch(&share, "pixmaps/spotify.svg");
        assert_eq!(pick_icon_name(&ICON_CANDIDATES, &dirs, &chain), "spotify");
    }

    /// The window's icon: a PNG, in either layout, the size nearest the one asked for
    /// (bigger first), from the first theme in the chain that has one.
    #[test]
    fn png_file_nearest_size_in_the_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let icons = tmp.path().join("share/icons");
        let dirs = [icons.clone()];
        let chain = themes(&["breeze-dark", "breeze", "hicolor"]);
        let png = |size| png_file("spotify-launcher", &dirs, &chain, size);
        assert_eq!(png(128), None, "nothing installed");

        for size in ["22x22", "48x48", "256x256", "512x512"] {
            touch(&icons, &format!("hicolor/{size}/apps/spotify-launcher.png"));
        }
        touch(&icons, "hicolor/scalable/apps/spotify-launcher.svg");
        let at = |rel: &str| icons.join(rel);
        assert_eq!(
            png(128),
            Some(at("hicolor/256x256/apps/spotify-launcher.png"))
        );
        assert_eq!(png(48), Some(at("hicolor/48x48/apps/spotify-launcher.png")));
        assert_eq!(
            png(1024),
            Some(at("hicolor/512x512/apps/spotify-launcher.png"))
        );
        assert_eq!(png(16), Some(at("hicolor/22x22/apps/spotify-launcher.png")));

        // A theme earlier in the chain wins, in breeze's `apps/<size>/` layout too.
        touch(&icons, "breeze/apps/32/spotify-launcher.png");
        assert_eq!(png(128), Some(at("breeze/apps/32/spotify-launcher.png")));

        // SVG only: no PNG to draw. A theme outside the chain doesn't count.
        touch(&icons, "breeze/apps/48/spotify.svg");
        touch(&icons, "Papirus/64x64/apps/spotify.png");
        assert_eq!(png_file("spotify", &dirs, &chain, 128), None);
    }

    #[test]
    fn png_file_from_pixmaps() {
        let tmp = tempfile::tempdir().unwrap();
        let share = tmp.path().join("share");
        let dirs = [share.join("icons")];
        touch(&share, "pixmaps/spotify-launcher.png");
        assert_eq!(
            png_file("spotify-launcher", &dirs, &themes(&["hicolor"]), 128),
            Some(share.join("pixmaps/spotify-launcher.png"))
        );
    }

    #[test]
    fn icon_fallback_is_the_last_candidate() {
        assert_eq!(pick_icon_name(&["a", "b"], &[], &[]), "b");
    }

    #[test]
    fn icon_theme_chain_follows_inherits() {
        let index = |inherits: &str| Some(format!("[Icon Theme]\nInherits={inherits}\n"));
        // Depth-first, each theme once, hicolor kept where it is when inherited.
        let chain = theme_chain(Some("[Icons]\nTheme=a\n"), |theme| match theme {
            "a" => index("b, c"),
            "b" => index("d,a"),
            "c" => index("b,hicolor"),
            "d" => Some("[Icon Theme]\nName=D\n".into()),
            _ => None,
        });
        assert_eq!(chain, themes(&["a", "b", "d", "c", "hicolor"]));

        // hicolor goes last when nothing inherits it.
        let chain = theme_chain(Some("[Icons]\nTheme=a\n"), |theme| {
            (theme == "a").then(|| "[Icon Theme]\nInherits=b\n".to_string())
        });
        assert_eq!(chain, themes(&["a", "b", "hicolor"]));

        // A path is never a theme name.
        let chain = theme_chain(Some("[Icons]\nTheme=a\n"), |theme| {
            (theme == "a").then(|| "[Icon Theme]\nInherits=../x,b/c,..,b\n".to_string())
        });
        assert_eq!(chain, themes(&["a", "b", "hicolor"]));
    }

    #[test]
    fn icon_theme_chain_depth_is_capped() {
        // t0 inherits t1 inherits t2 … without end.
        let chain = theme_chain(Some("[Icons]\nTheme=t0\n"), |theme| {
            let n: u32 = theme[1..].parse().unwrap();
            Some(format!("[Icon Theme]\nInherits=t{}\n", n + 1))
        });
        assert_eq!(chain.len(), MAX_THEME_DEPTH + 2, "{chain:?}");
        assert_eq!(chain.last().map(String::as_str), Some("hicolor"));
    }

    #[test]
    fn icon_theme_defaults_to_breeze() {
        let none = |_: &str| None;
        assert_eq!(theme_chain(None, none), themes(&["breeze", "hicolor"]));
        // No [Icons] section, or a Theme key elsewhere, or an empty one.
        for text in [
            "[General]\nTheme=nope\n",
            "[Icons]\nTheme=\n",
            "[Icons]\n# Theme=nope\n[KDE]\nTheme=nope\n",
        ] {
            assert_eq!(
                theme_chain(Some(text), none),
                themes(&["breeze", "hicolor"]),
                "{text}"
            );
        }
        // Spaces around the key and value are allowed.
        assert_eq!(
            theme_chain(Some("[Icons]\n Theme = breeze-dark \n"), none),
            themes(&["breeze-dark", "hicolor"])
        );
    }

    #[test]
    fn icon_missing_kdeglobals_and_nothing_anywhere() {
        let tmp = tempfile::tempdir().unwrap();
        let icons = tmp.path().join("share/icons");
        fs::create_dir_all(&icons).unwrap();
        let dirs = [icons];
        assert_eq!(pick_active(&dirs, None), "audio-x-generic");

        // Missing kdeglobals is breeze, whose own index brings in hicolor.
        write(
            &dirs[0],
            "breeze/index.theme",
            "[Icon Theme]\nInherits=hicolor\n",
        );
        touch(&dirs[0], "breeze/apps/22/spotify-launcher.svg");
        assert_eq!(pick_active(&dirs, None), "spotify-launcher");
    }

    #[test]
    fn icon_index_from_the_first_dir_that_has_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, system) = (tmp.path().join("home/icons"), tmp.path().join("usr/icons"));
        write(&system, "breeze/index.theme", "system");
        assert_eq!(
            read_index(&[home.clone(), system.clone()], "breeze").as_deref(),
            Some("system")
        );
        write(&home, "breeze/index.theme", "home");
        assert_eq!(
            read_index(&[home.clone(), system.clone()], "breeze").as_deref(),
            Some("home")
        );
        assert_eq!(read_index(&[home, system], "Papirus"), None);
    }

    #[test]
    fn icon_dirs_follow_xdg() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(
            icon_dirs_from(os("/data"), os("/home/d"), os("/a:/b/")),
            [
                PathBuf::from("/data/icons"),
                PathBuf::from("/a/icons"),
                PathBuf::from("/b/icons")
            ]
        );
        let defaults = [
            PathBuf::from("/home/d/.local/share/icons"),
            PathBuf::from("/usr/local/share/icons"),
            PathBuf::from("/usr/share/icons"),
        ];
        assert_eq!(icon_dirs_from(None, os("/home/d"), None), defaults);
        // Empty is unset, and relative entries are ignored, as the XDG spec says.
        assert_eq!(icon_dirs_from(os(""), os("/home/d"), os("")), defaults);
        assert_eq!(
            icon_dirs_from(os("rel"), os("/home/d"), os("rel:/usr/share")),
            [
                PathBuf::from("/home/d/.local/share/icons"),
                PathBuf::from("/usr/share/icons")
            ]
        );
        assert_eq!(
            icon_dirs_from(None, None, os("/usr/share")),
            [PathBuf::from("/usr/share/icons")],
            "no home: no user dir"
        );
    }
}
