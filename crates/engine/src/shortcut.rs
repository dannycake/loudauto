//! The global heart key, through the xdg-desktop-portal GlobalShortcuts portal. The
//! engine registers as the host app [`APP_ID`] (the portal wants a reverse-DNS id with a
//! matching `.desktop` file) and binds one shortcut, suggesting Meta+Shift+H: Plasma asks
//! the owner to accept or change it the first time, and it can be rebound in System
//! Settings → Shortcuts.
//!
//! - A press goes the way the tray's heart does (`Engine::on_local_requests`), so it
//!   always ends in a notification. It toggles the current track, which asks Spotify
//!   first while the track's state isn't known.
//! - Without the portal, if the owner declines, or if the portal's answer doesn't bind
//!   the heart, the engine runs on without the key and says so once, naming the fallback
//!   that always works: `loudauto-cli heart` as a custom shortcut.

use std::{fmt, sync::Arc};

use ashpd::desktop::{
    CreateSessionOptions, ResponseError,
    global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut},
};
use engine_ipc::{APP_ID, proto::Request};
use futures_util::StreamExt;
use log::{debug, info, warn};
use tokio::sync::mpsc;

use crate::engine::Engine;

/// The heart shortcut's id, within the engine's portal session.
pub const HEART_ID: &str = "heart";
/// What System Settings shows for it.
pub const HEART_DESCRIPTION: &str = "Like or unlike the current track";
/// Meta+Shift+H, in the notation of the XDG shortcuts spec.
pub const HEART_TRIGGER: &str = "LOGO+SHIFT+h";

/// Why the heart key is unavailable.
#[derive(Debug)]
pub enum Unavailable {
    /// The portal failed, or the owner declined its dialog.
    Portal(ashpd::Error),
    /// The portal answered the binding without the heart shortcut.
    NotBound,
    /// The portal's key presses ended.
    Stopped,
    /// The portal client panicked.
    Crashed,
}

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Portal(ashpd::Error::Response(ResponseError::Cancelled)) => {
                f.write_str("the shortcut dialog was cancelled")
            }
            Self::Portal(e) => e.fmt(f),
            Self::NotBound => f.write_str("the desktop didn't bind the shortcut"),
            Self::Stopped => f.write_str("the desktop portal stopped sending key presses"),
            Self::Crashed => f.write_str("the portal client crashed"),
        }
    }
}

impl From<ashpd::Error> for Unavailable {
    fn from(e: ashpd::Error) -> Self {
        Unavailable::Portal(e)
    }
}

/// What the owner is told when the heart key is unavailable.
pub fn unavailable_notice(why: &Unavailable) -> String {
    format!(
        "Global heart key unavailable: {why}. In System Settings → Shortcuts, add a custom \
         shortcut that runs: loudauto-cli heart"
    )
}

/// The engine's heart key: each press is sent to `requests` (answered by
/// `Engine::on_local_requests`). If the key is unavailable, that is logged and the owner
/// told once; the engine runs on without it.
pub async fn heart_key(engine: Arc<Engine>, requests: mpsc::Sender<Request>) {
    // Its own task, so even a panic in the portal client ends in the notice.
    let why = tokio::spawn(run_shortcut(move || press(&requests)))
        .await
        .unwrap_or_else(|e| {
            warn!("heart key: the portal client failed: {e}");
            Unavailable::Crashed
        });
    tell_unavailable(&engine, &why).await;
}

/// Binds the heart key and calls `heart` on each press, until the key is unavailable;
/// returns why. Binding waits while Plasma's dialog is open.
pub async fn run_shortcut(heart: impl Fn() + Send + Sync) -> Unavailable {
    match serve(&heart).await {
        Ok(()) => Unavailable::Stopped,
        Err(why) => why,
    }
}

/// The heart's key among the shortcuts the portal bound, given as `(id, trigger)`:
/// `None` when it is bound with no key assigned, `NotBound` when it isn't there.
fn heart_binding<'a>(
    bound: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<Option<&'a str>, Unavailable> {
    match bound.into_iter().find(|(id, _)| *id == HEART_ID) {
        Some((_, "")) => Ok(None),
        Some((_, trigger)) => Ok(Some(trigger)),
        None => Err(Unavailable::NotBound),
    }
}

/// The portal session: `Ok` once its key presses end.
async fn serve(heart: &(impl Fn() + Send + Sync)) -> Result<(), Unavailable> {
    ashpd::register_host_app(APP_ID.parse()?).await?;
    let portal = GlobalShortcuts::new().await?;
    // Held while presses come: the shortcut belongs to this session.
    let session = portal
        .create_session(CreateSessionOptions::default())
        .await?;
    let shortcut = NewShortcut::new(HEART_ID, HEART_DESCRIPTION).preferred_trigger(HEART_TRIGGER);
    let bound = portal
        .bind_shortcuts(&session, &[shortcut], None, BindShortcutsOptions::default())
        .await?
        .response()?;
    let shortcuts = bound.shortcuts();
    let pairs = shortcuts.iter().map(|s| (s.id(), s.trigger_description()));
    match heart_binding(pairs)? {
        Some(key) => info!("heart key: {key}"),
        None => info!("heart key: bound, with no key assigned"),
    }
    let mut presses = portal.receive_activated().await?;
    while let Some(activated) = presses.next().await {
        if activated.shortcut_id() == HEART_ID {
            debug!("heart key pressed");
            heart();
        }
    }
    drop(session);
    Ok(())
}

/// One press: the current track's heart, sent the tray's way. It never waits: with
/// the queue full, the press is dropped (and logged).
fn press(requests: &mpsc::Sender<Request>) {
    if let Err(e) = requests.try_send(Request::ToggleHeart { uri: None }) {
        warn!("heart key: press dropped: {e}");
    }
}

/// Logs why the key is unavailable and tells the owner, once.
async fn tell_unavailable(engine: &Arc<Engine>, why: &Unavailable) {
    warn!("no global heart key: {why}");
    engine.error_notice(unavailable_notice(why)).await;
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::PathBuf};

    use engine_ipc::proto::{Event, Level, Notice};

    use super::*;
    use crate::{
        core::Core,
        engine::testing::{settings, test_engine},
    };

    /// The workspace's `packaging` directory.
    fn packaging() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packaging")
    }

    /// The `[Desktop Entry]` group's keys and values.
    fn desktop_entry(text: &str) -> HashMap<&str, &str> {
        let mut lines = text.lines().map(str::trim);
        assert_eq!(lines.next(), Some("[Desktop Entry]"));
        lines
            .take_while(|line| !line.starts_with('['))
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| line.split_once('=').unwrap_or_else(|| panic!("{line}")))
            .collect()
    }

    #[test]
    fn desktop_file_matches_app_id() {
        assert_eq!(APP_ID, "to.skunk.loudauto");
        let path = packaging().join("to.skunk.loudauto.desktop");
        assert_eq!(
            path.file_name().unwrap().to_str(),
            Some(format!("{APP_ID}.desktop").as_str())
        );
        let text =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let entry = desktop_entry(&text);
        assert_eq!(entry.get("Type"), Some(&"Application"));
        assert_eq!(entry.get("Name"), Some(&"LOUDAUTO"));
        // Look 2: the taskbar's and title bar's icon. One name can't say the spotify →
        // spotify-launcher chain; spotify-launcher is in hicolor here, and the icon spec's
        // dash fallback (spotify-launcher → spotify) finds Papirus's.
        assert_eq!(entry.get("Icon"), Some(&"spotify-launcher"));
        // So the compositor ties the window (app id `to.skunk.loudauto`) to this file.
        assert_eq!(entry.get("StartupWMClass"), Some(&APP_ID));
        assert_eq!(entry.get("Exec"), Some(&"loudauto"));
    }

    #[test]
    fn shortcut_app_id_is_one_the_portal_takes() {
        assert!(APP_ID.parse::<ashpd::AppID>().is_ok());
    }

    #[test]
    fn shortcut_unavailable_notice_names_the_fallback() {
        let cancelled = Unavailable::Portal(ashpd::Error::Response(ResponseError::Cancelled));
        assert_eq!(
            unavailable_notice(&cancelled),
            "Global heart key unavailable: the shortcut dialog was cancelled. In System \
             Settings → Shortcuts, add a custom shortcut that runs: loudauto-cli heart"
        );
        assert_eq!(
            unavailable_notice(&Unavailable::Portal(ashpd::Error::NoResponse)),
            "Global heart key unavailable: Portal request didn't receive a response. In \
             System Settings → Shortcuts, add a custom shortcut that runs: loudauto-cli heart"
        );
    }

    #[tokio::test]
    async fn shortcut_press_goes_the_trays_way() {
        let (requests, mut rx) = mpsc::channel(1);
        press(&requests);
        assert_eq!(rx.recv().await, Some(Request::ToggleHeart { uri: None }));
        // A full queue drops the press rather than waiting.
        press(&requests);
        press(&requests);
        assert_eq!(rx.recv().await, Some(Request::ToggleHeart { uri: None }));
        assert!(rx.try_recv().is_err());
    }

    /// Deferred Task 21 minor: a bind response without the heart shortcut means no key
    /// will ever be pressed: the key is unavailable (and the owner told the fallback),
    /// rather than waited on in silence.
    #[test]
    fn a_bind_without_the_heart_is_unavailable() {
        assert!(matches!(
            heart_binding([("heart", "Meta+Shift+H")]),
            Ok(Some("Meta+Shift+H"))
        ));
        assert!(matches!(heart_binding([("heart", "")]), Ok(None)));
        assert!(matches!(
            heart_binding([("other", "Meta+X")]),
            Err(Unavailable::NotBound)
        ));
        assert!(matches!(
            heart_binding(std::iter::empty()),
            Err(Unavailable::NotBound)
        ));
        assert_eq!(
            unavailable_notice(&Unavailable::NotBound),
            "Global heart key unavailable: the desktop didn't bind the shortcut. In System \
             Settings → Shortcuts, add a custom shortcut that runs: loudauto-cli heart"
        );
    }

    #[tokio::test]
    async fn shortcut_unavailable_is_told_once_as_an_error() {
        let t = test_engine(Core::new(settings())).await;
        let (_, mut events) = t.engine.welcome().await;
        tell_unavailable(&t.engine, &Unavailable::Stopped).await;
        let text = "Global heart key unavailable: the desktop portal stopped sending key \
                    presses. In System Settings → Shortcuts, add a custom shortcut that runs: \
                    loudauto-cli heart";
        assert_eq!(
            events.recv().await.unwrap(),
            Event::Notice(Notice {
                id: 1,
                level: Level::Error,
                text: text.into(),
            })
        );
        assert!(events.try_recv().is_err());
        // No window is open: a desktop notification too.
        assert_eq!(t.desktop.notified(), [text]);
    }
}
