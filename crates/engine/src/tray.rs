//! The tray icon, a StatusNotifierItem (through ksni): the app icon (`icon::app_icon`),
//! the track as its tooltip, and a menu of the player's commands. A click on the icon, or
//! "Open LOUDAUTO", opens the window or brings it to the front (`window`).
//!
//! - The menu's commands go the way the media keys' do (`Engine::on_local_requests`), so
//!   a heart from the tray is announced in a notification, as one from a key is.
//! - The heart does what its label says: `Like` or `Unlike` writes that state
//!   (`Request::SetLiked`); while the track's state isn't known, it offers to like or
//!   unlike, and toggles (`Request::ToggleHeart`), which asks Spotify first.
//! - The tray follows the player state MPRIS follows; ksni announces only what changed.

use std::{sync::Arc, time::Duration};

use engine_ipc::proto::{PlayerState, Request};
use ksni::{MenuItem, OfflineReason, ToolTip, TrayMethods, menu::StandardItem};
use log::{debug, info, warn};
use tokio::sync::{mpsc, watch};

use crate::{
    icon::{self, ICON_CANDIDATES},
    mpris::{caps, play_pause},
    window::WindowLauncher,
};

/// The StatusNotifierItem's id.
pub const TRAY_ID: &str = "loudauto";
/// Its title, and its tooltip while nothing plays.
pub const TRAY_TITLE: &str = "LOUDAUTO";

/// What a menu entry does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayAction {
    /// Sent as the media keys' requests are.
    Send(Request),
    /// Opens the window, or brings it to the front.
    OpenWindow,
}

/// One entry of the tray's menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuEntry {
    pub label: &'static str,
    /// Whether it can be picked now (the player allows it).
    pub enabled: bool,
    pub action: TrayAction,
}

/// `<title> — <artists>` while there is a track (the title alone without artists), else
/// `LOUDAUTO`.
pub fn tray_tooltip(p: &PlayerState) -> String {
    match &p.track {
        Some(t) if t.artists.is_empty() => t.title.clone(),
        Some(t) => format!("{} — {}", t.title, t.artists.join(", ")),
        None => TRAY_TITLE.into(),
    }
}

/// The heart's label while the current track's state isn't known.
const HEART_UNKNOWN: &str = "Like or unlike current track";

/// The menu for `p`: play or pause (as the play/pause key would), next, previous, the
/// current track's heart (`heart_entry`), open, quit. The player's entries are enabled
/// as MPRIS's `Can*` flags are; the heart, while there is a track.
pub fn tray_menu(p: &PlayerState) -> Vec<MenuEntry> {
    use TrayAction::{OpenWindow, Send};
    let has_track = p.track.is_some();
    let can = caps(&p.actions, has_track);
    let toggle = play_pause(&p.status);
    let toggle_label = if toggle == Request::Pause {
        "Pause"
    } else {
        "Play"
    };
    let entry = |label, enabled, action| MenuEntry {
        label,
        enabled,
        action,
    };
    vec![
        entry(toggle_label, can.can_play, Send(toggle)),
        entry("Next", can.can_go_next, Send(Request::Next)),
        entry("Previous", can.can_go_previous, Send(Request::Prev)),
        heart_entry(p),
        entry("Open LOUDAUTO", true, OpenWindow),
        entry("Quit", true, Send(Request::Quit)),
    ]
}

/// The current track's heart, which does what its label says: with the track's state
/// known, `Like` or `Unlike` writes that state on the track the label is about
/// (`SetLiked`), so a click can't write the opposite of what the owner read. While it
/// isn't known (before the liked set's first fill), the label promises neither and the
/// click toggles, which asks Spotify first. Without a track, it is disabled.
fn heart_entry(p: &PlayerState) -> MenuEntry {
    let (label, enabled, action) = match (&p.track, p.liked) {
        (Some(track), Some(liked)) => (
            if liked {
                "Unlike current track"
            } else {
                "Like current track"
            },
            true,
            Request::SetLiked {
                uri: track.uri.clone(),
                liked: !liked,
            },
        ),
        (track, _) => (
            HEART_UNKNOWN,
            track.is_some(),
            Request::ToggleHeart { uri: None },
        ),
    };
    MenuEntry {
        label,
        enabled,
        action: TrayAction::Send(action),
    }
}

/// The tray as ksni serves it.
struct TrayIcon {
    player: PlayerState,
    icon: &'static str,
    requests: mpsc::Sender<Request>,
    launcher: Arc<WindowLauncher>,
}

impl TrayIcon {
    /// Runs a menu entry's action. ksni's callbacks must not wait, so nothing here does.
    fn run(&self, action: &TrayAction) {
        match action {
            TrayAction::Send(req) => {
                debug!("tray: {req:?}");
                if let Err(e) = self.requests.try_send(req.clone()) {
                    warn!("tray: {req:?} dropped: {e}");
                }
            }
            TrayAction::OpenWindow => self.open_window(),
        }
    }

    /// Opens the window, or brings it to the front, with this click's activation token.
    fn open_window(&self) {
        let token = activation_token();
        let launcher = self.launcher.clone();
        // `open_or_raise` logs its own failures.
        tokio::spawn(async move {
            let _ = launcher.open_or_raise(token).await;
        });
    }
}

/// The activation token of the click being handled. Plasma may hand it over just after
/// the click, so it is asked for once more a moment later, as ksni's own example does.
fn activation_token() -> Option<String> {
    ksni::current_xdg_activation_token().or_else(|| {
        std::thread::sleep(Duration::from_millis(1));
        ksni::current_xdg_activation_token()
    })
}

impl ksni::Tray for TrayIcon {
    fn id(&self) -> String {
        TRAY_ID.into()
    }

    fn title(&self) -> String {
        TRAY_TITLE.into()
    }

    fn icon_name(&self) -> String {
        self.icon.into()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: tray_tooltip(&self.player),
            ..ToolTip::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.open_window();
    }

    fn watcher_online(&self) {
        info!("tray: the tray host is up; showing the icon");
    }

    /// Keeps the tray waiting, so the icon shows once a tray host (re)starts.
    fn watcher_offline(&self, reason: OfflineReason) -> bool {
        match reason {
            OfflineReason::Error(e) => {
                warn!("tray: no tray host yet ({e}); the icon shows once one starts");
            }
            _ => info!("tray: the tray host went away; the icon comes back with it"),
        }
        true
    }

    /// `tray_menu`, with lines before the heart and before "Open LOUDAUTO".
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items = Vec::new();
        for entry in tray_menu(&self.player) {
            if matches!(
                entry.action,
                TrayAction::Send(Request::ToggleHeart { .. } | Request::SetLiked { .. })
                    | TrayAction::OpenWindow
            ) {
                items.push(MenuItem::Separator);
            }
            let action = entry.action;
            items.push(
                StandardItem {
                    label: entry.label.into(),
                    enabled: entry.enabled,
                    activate: Box::new(move |tray: &mut Self| tray.run(&action)),
                    ..StandardItem::default()
                }
                .into(),
            );
        }
        items
    }
}

/// Puts the tray up and keeps it in step with `state` until the engine is gone. With no
/// tray host yet (the engine started before Plasma's), it waits for one; with no session
/// bus, it is logged, and the engine runs on without a tray.
pub async fn run_tray(
    mut state: watch::Receiver<PlayerState>,
    requests: mpsc::Sender<Request>,
    launcher: Arc<WindowLauncher>,
) {
    // Its first call reads the icon themes: off the async threads.
    let icon = tokio::task::spawn_blocking(icon::app_icon)
        .await
        .unwrap_or(ICON_CANDIDATES[ICON_CANDIDATES.len() - 1]);
    let tray = TrayIcon {
        player: state.borrow_and_update().clone(),
        icon,
        requests,
        launcher,
    };
    // A tray host that isn't up yet (kded6, at login) is waited for, not an error.
    let handle = match tray.assume_sni_available(true).spawn().await {
        Ok(handle) => handle,
        Err(e) => {
            warn!("no tray icon: {e}");
            return;
        }
    };
    info!("tray: serving as {TRAY_ID}, icon {icon}");
    while state.changed().await.is_ok() {
        let player = state.borrow_and_update().clone();
        if handle.update(|tray| tray.player = player).await.is_none() {
            warn!("tray: the tray service stopped");
            return;
        }
    }
    debug!("tray: the engine is gone");
}

#[cfg(test)]
mod tests {
    use playback::{
        command::Repeat,
        event::{Actions, NowPlaying, PlayStatus, PlaybackEvent},
    };

    use super::*;
    use crate::{
        core::Core,
        engine::testing::{FakeLiker, settings, test_engine_with},
    };

    /// nerve's uri.
    const NERVE: &str = "spotify:track:5kCNaSNJ6AebIX5cM991Tk";

    /// "nerve" by 2hollis and brakence, playing, liked.
    fn playing() -> PlayerState {
        PlayerState {
            track: Some(NowPlaying {
                uri: NERVE.into(),
                title: "nerve".into(),
                artists: vec!["2hollis".into(), "brakence".into()],
                artist_uris: vec!["spotify:artist:a".into(), "spotify:artist:b".into()],
                album: Some("boy".into()),
                album_uri: Some("spotify:album:b".into()),
                duration_ms: Some(230_526),
                cover_url: None,
            }),
            status: PlayStatus::Playing,
            active_here: true,
            context_uri: Some("spotify:playlist:p".into()),
            position: None,
            volume: 40,
            shuffle: false,
            repeat: Repeat::Off,
            actions: Actions {
                pause: true,
                seek: true,
                skip_next: true,
                skip_prev: true,
                ..Actions::default()
            },
            liked: Some(true),
            quality: None,
        }
    }

    /// Nothing loaded: what the engine starts with.
    fn nothing() -> PlayerState {
        PlayerState {
            track: None,
            status: PlayStatus::Stopped,
            context_uri: None,
            actions: Actions::default(),
            liked: None,
            ..playing()
        }
    }

    /// The tray for `player`, its menu's requests going to `requests`.
    fn tray_icon(player: PlayerState, requests: mpsc::Sender<Request>) -> TrayIcon {
        TrayIcon {
            player,
            icon: "audio-x-generic",
            requests,
            launcher: Arc::new(WindowLauncher::new(
                Arc::new(crate::server::Windows::default()),
                "/bin/true".into(),
                crate::window::Launch::Direct,
            )),
        }
    }

    fn labels(menu: &[MenuEntry]) -> Vec<&'static str> {
        menu.iter().map(|e| e.label).collect()
    }

    #[test]
    fn menu_labels() {
        assert_eq!(
            labels(&tray_menu(&playing())),
            [
                "Pause",
                "Next",
                "Previous",
                "Unlike current track",
                "Open LOUDAUTO",
                "Quit"
            ]
        );
        let paused = PlayerState {
            status: PlayStatus::Paused,
            liked: Some(false),
            actions: Actions {
                play: true,
                ..playing().actions
            },
            ..playing()
        };
        assert_eq!(
            labels(&tray_menu(&paused)),
            [
                "Play",
                "Next",
                "Previous",
                "Like current track",
                "Open LOUDAUTO",
                "Quit"
            ]
        );
        // Not known yet (before the liked set's first fill): a label that promises
        // neither.
        let unknown = PlayerState {
            liked: None,
            ..playing()
        };
        assert_eq!(
            labels(&tray_menu(&unknown))[3],
            "Like or unlike current track"
        );
        // Nothing loaded: the same label, disabled.
        assert_eq!(
            labels(&tray_menu(&nothing()))[3],
            "Like or unlike current track"
        );
    }

    /// Final re-review N1: the heart does what its label says. A known state is written
    /// as the label names it (`SetLiked`, on the track the label is about), never
    /// toggled from a state the engine may have wrong; an unknown one promises neither,
    /// and toggles, which reads Spotify first.
    #[test]
    fn the_heart_does_what_its_label_says() {
        use TrayAction::Send;
        let heart = |liked| tray_menu(&PlayerState { liked, ..playing() })[3].clone();
        assert_eq!(
            heart(Some(true)),
            MenuEntry {
                label: "Unlike current track",
                enabled: true,
                action: Send(Request::SetLiked {
                    uri: NERVE.into(),
                    liked: false
                }),
            }
        );
        assert_eq!(
            heart(Some(false)),
            MenuEntry {
                label: "Like current track",
                enabled: true,
                action: Send(Request::SetLiked {
                    uri: NERVE.into(),
                    liked: true
                }),
            }
        );
        let neutral = |enabled| MenuEntry {
            label: "Like or unlike current track",
            enabled,
            action: Send(Request::ToggleHeart { uri: None }),
        };
        assert_eq!(heart(None), neutral(true));
        // Nothing loaded: nothing to like, whatever the state says.
        assert_eq!(tray_menu(&nothing())[3], neutral(false));
        let stale = PlayerState {
            liked: Some(true),
            ..nothing()
        };
        assert_eq!(tray_menu(&stale)[3], neutral(false));
    }

    #[test]
    fn menu_actions_are_the_media_keys_requests() {
        use TrayAction::{OpenWindow, Send};
        let actions = |p: &PlayerState| -> Vec<TrayAction> {
            tray_menu(p).into_iter().map(|e| e.action).collect()
        };
        // The heart: as its label says (`the_heart_does_what_its_label_says`).
        let heart = Send(Request::SetLiked {
            uri: NERVE.into(),
            liked: false,
        });
        assert_eq!(
            actions(&playing()),
            [
                Send(Request::Pause),
                Send(Request::Next),
                Send(Request::Prev),
                heart,
                OpenWindow,
                Send(Request::Quit),
            ]
        );
        // Buffering counts as playing, as for the play/pause key.
        let buffering = PlayerState {
            status: PlayStatus::Buffering,
            ..playing()
        };
        assert_eq!(actions(&buffering)[0], Send(Request::Pause));
        let paused = PlayerState {
            status: PlayStatus::Paused,
            ..playing()
        };
        assert_eq!(actions(&paused)[0], Send(Request::Play));
    }

    #[test]
    fn menu_offers_only_what_the_player_allows() {
        let enabled = |p: &PlayerState| -> Vec<bool> {
            tray_menu(p).into_iter().map(|e| e.enabled).collect()
        };
        assert_eq!(enabled(&playing()), [true; 6]);
        // The heart while its state isn't known yet: still there to press.
        let unknown = PlayerState {
            liked: None,
            ..playing()
        };
        assert_eq!(enabled(&unknown), [true; 6]);
        // Nothing loaded: only opening the window and quitting.
        assert_eq!(
            enabled(&nothing()),
            [false, false, false, false, true, true]
        );
        // The last track of a context with repeat off: no next.
        let last = PlayerState {
            actions: Actions {
                skip_next: false,
                ..playing().actions
            },
            ..playing()
        };
        assert_eq!(enabled(&last), [true, false, true, true, true, true]);
    }

    /// The menu as the tray host shows it: lines before the heart, whichever request it
    /// sends, and before "Open LOUDAUTO".
    #[tokio::test]
    async fn menu_lines_set_the_heart_apart() {
        use ksni::Tray as _;
        let shown = |liked| -> Vec<String> {
            let player = PlayerState { liked, ..playing() };
            let items = tray_icon(player, mpsc::channel(1).0).menu();
            items
                .into_iter()
                .map(|item| match item {
                    MenuItem::Standard(entry) => entry.label,
                    MenuItem::Separator => "--".into(),
                    other => panic!("{other:?}"),
                })
                .collect()
        };
        for (liked, heart) in [
            (Some(true), "Unlike current track"),
            (Some(false), "Like current track"),
            (None, "Like or unlike current track"),
        ] {
            assert_eq!(
                shown(liked),
                [
                    "Pause",
                    "Next",
                    "Previous",
                    "--",
                    heart,
                    "--",
                    "Open LOUDAUTO",
                    "Quit"
                ]
            );
        }
    }

    /// Final re-review N1, the owner's case: a click on the tray's heart reaches the
    /// account as its label said, the media keys' way, and is announced in a
    /// notification. Nerve is liked on the account in the first and last cases; in the
    /// last the engine doesn't know it yet, so the entry promises neither, and the
    /// toggle reads Spotify before it writes.
    #[tokio::test]
    async fn a_heart_click_writes_what_its_label_said() {
        use ksni::Tray as _;
        let unliked = "Removed from Liked Songs: nerve — 2hollis, brakence";
        let liked = "Liked: nerve — 2hollis, brakence";
        // (the liked set, if filled; the label; the write; the notification)
        for (liked_set, label, write, told) in [
            (Some(vec![NERVE]), "Unlike current track", false, unliked),
            (Some(vec![]), "Like current track", true, liked),
            (None, "Like or unlike current track", false, unliked),
        ] {
            let on_account = liked_set.clone().unwrap_or(vec![NERVE]);
            let mut core = Core::new(settings());
            core.on_playback(&PlaybackEvent::Status(PlayStatus::Playing));
            core.on_playback(&PlaybackEvent::Track(playing().track.unwrap()));
            if let Some(set) = liked_set {
                core.set_liked_set(set.into_iter().map(String::from));
            }
            let liker = FakeLiker {
                on_account: on_account.into_iter().map(String::from).collect(),
                ..FakeLiker::default()
            };
            let t = test_engine_with(core, liker).await;
            let (requests, rx) = mpsc::channel(4);
            tokio::spawn(t.engine.clone().on_local_requests(rx));
            let player = t.engine.core.lock().await.state.player.clone();
            let mut tray = tray_icon(player, requests);

            let heart = tray
                .menu()
                .into_iter()
                .find_map(|item| match item {
                    MenuItem::Standard(entry) if entry.label == label => Some(entry),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no {label:?} in the menu"));
            assert!(heart.enabled, "{label}");
            (heart.activate)(&mut tray);
            tokio::time::timeout(Duration::from_secs(5), async {
                while t.desktop.notified().is_empty() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("the click is announced");

            assert_eq!(t.desktop.notified(), [told], "{label}");
            assert_eq!(
                *t.liker.calls.lock().unwrap(),
                [(NERVE.to_string(), write)],
                "{label}"
            );
        }
    }

    #[tokio::test]
    async fn tray_waits_for_a_tray_host() {
        use ksni::Tray as _;
        let tray = tray_icon(nothing(), mpsc::channel(1).0);
        // Not up yet at login, or gone for a Plasma restart: keep waiting either way.
        assert!(tray.watcher_offline(OfflineReason::Error(ksni::Error::WontShow)));
        assert!(tray.watcher_offline(OfflineReason::No));
        assert_eq!(tray.id(), "loudauto");
        assert_eq!(tray.title(), "LOUDAUTO");
    }

    #[test]
    fn tray_tooltip_names_the_track() {
        assert_eq!(tray_tooltip(&playing()), "nerve — 2hollis, brakence");
        assert_eq!(tray_tooltip(&nothing()), "LOUDAUTO");
        let mut no_artists = playing();
        no_artists.track.as_mut().unwrap().artists.clear();
        assert_eq!(tray_tooltip(&no_artists), "nerve");
    }
}
