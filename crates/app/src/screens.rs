//! The screens that take the main area's place while the engine can't play or browse
//! without the owner: logging in, pairing Soloist, and Soloist missing, expired or
//! failed (spec "First run, login, settings"). Also the banner warning of Soloist's
//! expiry, 14 days ahead.

use eframe::egui::{Label, RichText, ScrollArea, Ui, WidgetText};
use engine_ipc::{EngineState, LoginStep, Request, SoloistHealth};
use playback::error::{DOWNLOADS_URL, UPDATE_COMMAND};

use crate::{
    link::Requests,
    notices::{self, Sent},
    page_frame::warning_icon,
    theme,
};

/// The button that starts the device-code login.
pub const LOG_IN: &str = "Log in";
/// Under the login's URL and code.
pub const WAITING: &str = "Waiting for you to approve…";
/// Soloist has never been paired.
pub const NOT_PAIRED: &str =
    "Open the Spotify app on this PC and pick LOUDAUTO in the device list.";
pub const NOT_INSTALLED: &str = "Spotify Soloist isn't installed.";
pub const EXPIRED: &str = "This Soloist build has expired.";
/// The button that has the engine start Soloist again.
pub const TRY_AGAIN: &str = "Try again";
/// The expiry banner shows this many days ahead, and fewer.
pub const EXPIRY_WARNING_DAYS: u32 = 14;
/// A screen's column, at most this wide.
const MAX_WIDTH: f32 = 640.0;
/// Soloist's log tail is scrolled past this height.
const LOG_HEIGHT: f32 = 240.0;

/// What replaces the main area, borrowed from the engine's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen<'a> {
    /// No library session: the Log in button.
    LogIn,
    /// The device-code login under way.
    Approve {
        url: &'a str,
        code: &'a str,
    },
    /// The login failed, for this reason.
    LoginFailed(&'a str),
    NotPaired,
    NotInstalled {
        path: &'a str,
    },
    Expired {
        path: &'a str,
    },
    PlayerFailed {
        message: &'a str,
        log_tail: &'a str,
    },
}

/// What the owner asked of a screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenAction {
    LogIn,
    TryAgain,
}

impl ScreenAction {
    /// Sends the request, its outcome to be said through `sent`.
    pub fn send(self, link: &dyn Requests, sent: &mut Sent) {
        match self {
            ScreenAction::LogIn => sent.send(link, Request::StartLogin, "log in", None),
            ScreenAction::TryAgain => {
                sent.send(link, Request::RestartPlayer, "restart the player", None);
            }
        }
    }
}

/// The screen `state` calls for, if any.
pub fn blocking_screen(state: &EngineState) -> Option<Screen<'_>> {
    screen_of(Some(&state.login), Some(&state.soloist))
}

/// The screen for the login and Soloist's health, as far as the engine has said: the
/// login comes first, since nothing can be browsed without it. Soloist on its way back
/// (starting, reconnecting) is the bottom bar's to say.
pub fn screen_of<'a>(
    login: Option<&'a LoginStep>,
    soloist: Option<&'a SoloistHealth>,
) -> Option<Screen<'a>> {
    let login = login.and_then(|login| match login {
        LoginStep::LoggedIn => None,
        LoginStep::Needed => Some(Screen::LogIn),
        LoginStep::Prompt { url, code } => Some(Screen::Approve { url, code }),
        LoginStep::Failed(message) => Some(Screen::LoginFailed(message)),
    });
    login.or_else(|| match soloist? {
        SoloistHealth::NotPaired => Some(Screen::NotPaired),
        SoloistHealth::BinaryMissing { path } => Some(Screen::NotInstalled { path }),
        SoloistHealth::Expired { path } => Some(Screen::Expired { path }),
        SoloistHealth::Failed { message, log_tail } => {
            Some(Screen::PlayerFailed { message, log_tail })
        }
        SoloistHealth::Starting | SoloistHealth::Ready | SoloistHealth::Reconnecting { .. } => None,
    })
}

/// Where to put a new Soloist binary.
pub fn put_binary_at(path: &str) -> String {
    format!("Put the soloist binary at {path}.")
}

/// Draws `screen` into `ui`, a column from its top; what the owner clicked. What needs
/// fixing is `⚠` and its words in the error's red box; the action asked for is a main
/// button; what the owner copies (the login's URL and code, the download page, the path)
/// is selectable.
pub fn show(ui: &mut Ui, screen: Screen<'_>) -> Option<ScreenAction> {
    let mut action = None;
    ui.add_space(24.0);
    ui.vertical(|ui| {
        ui.set_max_width(MAX_WIDTH);
        ui.spacing_mut().item_spacing.y = 12.0;
        match screen {
            Screen::LogIn => {
                if theme::main_button(ui, LOG_IN).clicked() {
                    action = Some(ScreenAction::LogIn);
                }
            }
            Screen::Approve { url, code } => {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Go to");
                    copyable(ui, RichText::new(url).color(theme::TEXT));
                    ui.label("and enter");
                    copyable(ui, RichText::new(code).color(theme::TEXT).monospace());
                });
                ui.label(RichText::new(WAITING).color(theme::SECONDARY));
            }
            Screen::LoginFailed(message) => {
                let log_in = |ui: &mut Ui| theme::main_button(ui, LOG_IN).clicked();
                if notices::error_banner(ui, message, log_in) {
                    action = Some(ScreenAction::LogIn);
                }
            }
            Screen::NotPaired => {
                ui.add(Label::new(NOT_PAIRED).wrap());
            }
            Screen::NotInstalled { path } => action = install(ui, NOT_INSTALLED, path),
            Screen::Expired { path } => action = install(ui, EXPIRED, path),
            Screen::PlayerFailed { message, log_tail } => {
                notices::error_box(ui, |ui| {
                    ui.horizontal(|ui| {
                        warning_icon(ui);
                        ui.add(Label::new(message).wrap());
                    });
                    if !log_tail.is_empty() {
                        log_box(ui, log_tail);
                    }
                    if theme::main_button(ui, TRY_AGAIN).clicked() {
                        action = Some(ScreenAction::TryAgain);
                    }
                });
            }
        }
    });
    action
}

/// `heading`, the download page, where the binary goes, and Try again, in the error's red
/// box: what a missing or expired Soloist needs of the owner.
fn install(ui: &mut Ui, heading: &str, path: &str) -> Option<ScreenAction> {
    notices::error_box(ui, |ui| {
        ui.horizontal(|ui| {
            warning_icon(ui);
            ui.label(heading);
        });
        copyable(ui, UPDATE_COMMAND);
        copyable(ui, DOWNLOADS_URL);
        copyable(ui, put_binary_at(path));
        theme::main_button(ui, TRY_AGAIN)
            .clicked()
            .then_some(ScreenAction::TryAgain)
    })
}

/// Soloist's last lines, monospace and secondary on black in a thin outline, scrolled
/// when long.
fn log_box(ui: &mut Ui, tail: &str) {
    theme::outline().fill(theme::BG).show(ui, |ui| {
        ScrollArea::vertical()
            .id_salt("soloist-log-tail")
            .max_height(LOG_HEIGHT)
            .show(ui, |ui| {
                copyable(ui, RichText::new(tail).monospace().color(theme::SECONDARY));
            });
    });
}

/// Text the owner may select and copy, wrapped.
fn copyable(ui: &mut Ui, text: impl Into<WidgetText>) {
    ui.add(Label::new(text).selectable(true).wrap());
}

/// The expiry banner's words, `days` ahead: none until [`EXPIRY_WARNING_DAYS`].
pub fn expiry_text(days: u32) -> Option<String> {
    let unit = if days == 1 { "day" } else { "days" };
    (days <= EXPIRY_WARNING_DAYS)
        .then(|| {
            format!(
                "Soloist expires in {days} {unit}. Run {UPDATE_COMMAND}, or download a new build: {DOWNLOADS_URL}"
            )
        })
}

/// The expiry banner, when Soloist expires in [`EXPIRY_WARNING_DAYS`] or fewer: `⚠` and
/// the words in a thin outline in the line grey (a warning, not yet an error: that is the
/// Expired screen's red box), the URL selectable with them. Nothing otherwise.
pub fn expiry_banner(ui: &mut Ui, days: Option<u32>) {
    let Some(text) = days.and_then(expiry_text) else {
        return;
    };
    notices::outlined(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            warning_icon(ui);
            copyable(ui, text);
        });
    });
    ui.add_space(12.0);
}

#[cfg(test)]
mod tests {
    use eframe::egui::vec2;
    use egui_kittest::{Harness, kittest::Queryable};
    use engine_ipc::{PlayerState, SessionHealth, Settings};
    use playback::{
        command::Repeat,
        event::{Actions, PlayStatus},
    };

    use super::*;
    use crate::{page_frame::WARNING, test_support, theme};

    const PATH: &str = "/home/u/.local/share/loudauto/soloist/soloist";

    fn state(login: LoginStep, soloist: SoloistHealth) -> EngineState {
        EngineState {
            player: PlayerState {
                track: None,
                status: PlayStatus::Stopped,
                active_here: true,
                context_uri: None,
                position: None,
                volume: 40,
                shuffle: false,
                repeat: Repeat::Off,
                actions: Actions::default(),
                liked: None,
                quality: None,
            },
            queue: vec![],
            soloist,
            session: SessionHealth::Ready,
            login,
            settings: Settings {
                volume: 40,
                output_device: None,
                autostart: false,
            },
            username: None,
            soloist_expires_in_days: None,
        }
    }

    #[test]
    fn screen_priority() {
        let needed = state(LoginStep::Needed, SoloistHealth::NotPaired);
        assert_eq!(blocking_screen(&needed), Some(Screen::LogIn));
        let ready = state(LoginStep::LoggedIn, SoloistHealth::Ready);
        assert_eq!(blocking_screen(&ready), None);
        let paired = state(LoginStep::LoggedIn, SoloistHealth::NotPaired);
        assert_eq!(blocking_screen(&paired), Some(Screen::NotPaired));
        let prompt = LoginStep::Prompt {
            url: "https://www.spotify.com/pair".into(),
            code: "ABCD-EFGH".into(),
        };
        let missing = SoloistHealth::BinaryMissing { path: PATH.into() };
        assert_eq!(
            blocking_screen(&state(prompt, missing.clone())),
            Some(Screen::Approve {
                url: "https://www.spotify.com/pair",
                code: "ABCD-EFGH"
            })
        );
        let failed = LoginStep::Failed("Spotify refused the code.".into());
        assert_eq!(
            blocking_screen(&state(failed, SoloistHealth::Ready)),
            Some(Screen::LoginFailed("Spotify refused the code."))
        );
        assert_eq!(
            blocking_screen(&state(LoginStep::LoggedIn, missing)),
            Some(Screen::NotInstalled { path: PATH })
        );
        let expired = SoloistHealth::Expired { path: PATH.into() };
        assert_eq!(
            blocking_screen(&state(LoginStep::LoggedIn, expired)),
            Some(Screen::Expired { path: PATH })
        );
        let crashed = SoloistHealth::Failed {
            message: "Spotify Soloist keeps stopping (exit code 1).".into(),
            log_tail: "tail".into(),
        };
        assert_eq!(
            blocking_screen(&state(LoginStep::LoggedIn, crashed)),
            Some(Screen::PlayerFailed {
                message: "Spotify Soloist keeps stopping (exit code 1).",
                log_tail: "tail"
            })
        );
        // Soloist on its way back is the bottom bar's to say, not a screen.
        for passing in [
            SoloistHealth::Starting,
            SoloistHealth::Reconnecting {
                attempt: 2,
                retry_in_ms: 2000,
            },
        ] {
            assert_eq!(blocking_screen(&state(LoginStep::LoggedIn, passing)), None);
        }
        // Before the engine has said: nothing to block on.
        assert_eq!(screen_of(None, None), None);
        assert_eq!(
            screen_of(None, Some(&SoloistHealth::NotPaired)),
            Some(Screen::NotPaired)
        );
    }

    /// A screen in a harness, and what was clicked on it.
    struct Shown {
        screen: Screen<'static>,
        clicked: Vec<ScreenAction>,
    }

    fn on_screen(screen: Screen<'static>) -> Harness<'static, Shown> {
        let (builder, _) = test_support::builder(vec2(900.0, 600.0), false);
        let mut harness = builder.build_ui_state(
            |ui, s: &mut Shown| s.clicked.extend(show(ui, s.screen)),
            Shown {
                screen,
                clicked: vec![],
            },
        );
        harness.run();
        harness
    }

    fn banner_harness(days: Option<u32>) -> Harness<'static, Option<u32>> {
        let (builder, _) = test_support::builder(vec2(900.0, 200.0), false);
        let mut harness =
            builder.build_ui_state(|ui, days: &mut Option<u32>| expiry_banner(ui, *days), days);
        harness.run();
        harness
    }

    #[test]
    fn expiry_banner_at_14_days() {
        let at_14 = format!(
            "Soloist expires in 14 days. Run {UPDATE_COMMAND}, or download a new build: {DOWNLOADS_URL}"
        );
        assert_eq!(expiry_text(14).as_deref(), Some(at_14.as_str()));
        assert_eq!(expiry_text(15), None);
        assert_eq!(
            expiry_text(1),
            Some(format!(
                "Soloist expires in 1 day. Run {UPDATE_COMMAND}, or download a new build: {DOWNLOADS_URL}"
            ))
        );
        let harness = banner_harness(Some(14));
        harness.get_by_label(&at_14);
        harness.get_by_label(WARNING);
        let banner = harness.get_by_label(&at_14).rect();
        // A warning, not yet an error: outlined in the line grey, no red box.
        assert!(
            test_support::outlines(&harness)
                .iter()
                .any(|(r, s)| r.contains_rect(banner) && s.color == theme::LINES),
            "outlined"
        );
        assert!(
            !test_support::fills(&harness)
                .iter()
                .any(|(_, c)| *c == theme::ERROR_FILL),
            "a red box"
        );
        let harness = banner_harness(Some(15));
        assert!(
            harness
                .query_by_label_contains("Soloist expires in")
                .is_none()
        );
        assert!(harness.query_by_label(WARNING).is_none());
        let harness = banner_harness(None);
        assert!(
            harness
                .query_by_label_contains("Soloist expires in")
                .is_none()
        );
    }

    #[test]
    fn missing_binary_names_path_and_url() {
        let mut harness = on_screen(Screen::NotInstalled { path: PATH });
        let words = harness.get_by_label(NOT_INSTALLED).rect();
        test_support::assert_in_red_box(&harness, NOT_INSTALLED, words);
        harness.get_by_label(WARNING);
        harness.get_by_label(UPDATE_COMMAND);
        harness.get_by_label(DOWNLOADS_URL);
        harness.get_by_label(&format!("Put the soloist binary at {PATH}."));
        // Task 16 carry: the engine waits for a restart once the binary is in place.
        harness.get_by_label(TRY_AGAIN).click();
        harness.run();
        assert_eq!(harness.state().clicked, [ScreenAction::TryAgain]);

        let mut harness = on_screen(Screen::Expired { path: PATH });
        harness.get_by_label(EXPIRED);
        harness.get_by_label(UPDATE_COMMAND);
        harness.get_by_label(DOWNLOADS_URL);
        harness.get_by_label(&format!("Put the soloist binary at {PATH}."));
        harness.get_by_label(TRY_AGAIN).click();
        harness.run();
        assert_eq!(harness.state().clicked, [ScreenAction::TryAgain]);
    }

    /// The other screens: each says what to do, and its button asks for it.
    #[test]
    fn screens_say_what_to_do() {
        let mut harness = on_screen(Screen::LogIn);
        let button = harness.get_by_label(LOG_IN).rect();
        test_support::assert_main_button(&harness, LOG_IN, button);
        harness.get_by_label(LOG_IN).click();
        harness.run();
        assert_eq!(harness.state().clicked, [ScreenAction::LogIn]);

        let harness = on_screen(Screen::Approve {
            url: "https://www.spotify.com/pair",
            code: "ABCD-EFGH",
        });
        harness.get_by_label("Go to");
        harness.get_by_label("https://www.spotify.com/pair");
        harness.get_by_label("and enter");
        harness.get_by_label("ABCD-EFGH");
        harness.get_by_label(WAITING);
        assert!(harness.query_by_label(LOG_IN).is_none());

        let mut harness = on_screen(Screen::LoginFailed("Spotify refused the code."));
        let words = harness.get_by_label("Spotify refused the code.").rect();
        test_support::assert_in_red_box(&harness, "Spotify refused the code.", words);
        // The warning sign, a third bigger than a line of text.
        let warning = harness.get_by_label(WARNING).rect();
        assert_eq!(warning.size(), vec2(theme::ICON, theme::ICON));
        let button = harness.get_by_label(LOG_IN).rect();
        test_support::assert_main_button(&harness, LOG_IN, button);
        harness.get_by_label(LOG_IN).click();
        harness.run();
        assert_eq!(harness.state().clicked, [ScreenAction::LogIn]);

        let harness = on_screen(Screen::NotPaired);
        harness.get_by_label(NOT_PAIRED);
        assert!(harness.query_by_label(TRY_AGAIN).is_none());

        let mut harness = on_screen(Screen::PlayerFailed {
            message: "Spotify Soloist keeps stopping (exit code 1).",
            log_tail: "E: no audio device\nE: giving up",
        });
        let words = harness
            .get_by_label("Spotify Soloist keeps stopping (exit code 1).")
            .rect();
        test_support::assert_in_red_box(
            &harness,
            "Spotify Soloist keeps stopping (exit code 1).",
            words,
        );
        harness.get_by_label(WARNING);
        harness.get_by_label("E: no audio device\nE: giving up");
        let button = harness.get_by_label(TRY_AGAIN).rect();
        test_support::assert_main_button(&harness, TRY_AGAIN, button);
        harness.get_by_label(TRY_AGAIN).click();
        harness.run();
        assert_eq!(harness.state().clicked, [ScreenAction::TryAgain]);
        // No tail, no box.
        let harness = on_screen(Screen::PlayerFailed {
            message: "Spotify Soloist lost its connection.",
            log_tail: "",
        });
        harness.get_by_label("Spotify Soloist lost its connection.");
        assert_eq!(test_support::outlines(&harness).len(), 1);
    }

    /// The actions' requests, and what their failure says.
    #[test]
    fn screen_actions_send() {
        let link = test_support::FakeLink::default();
        let mut sent = Sent::default();
        ScreenAction::LogIn.send(&link, &mut sent);
        ScreenAction::TryAgain.send(&link, &mut sent);
        assert_eq!(link.take(), [Request::StartLogin, Request::RestartPlayer]);
        let said = sent.on_reply(link.last_id(), Err("The engine is stopping.".into()));
        assert_eq!(
            said.map(|(_, text)| text).as_deref(),
            Some("Couldn't restart the player: The engine is stopping.")
        );
    }
}
