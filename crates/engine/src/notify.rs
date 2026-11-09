//! Desktop notifications (freedesktop Notifications): the heart key and tray always
//! answer with one, and serious errors show as one while the window is closed.

use notify_rust::{Hint, Notification};

use crate::icon;

/// Who the notification is from, as Plasma shows it.
pub const APP_NAME: &str = "LOUDAUTO";
/// The app id, so Plasma files the notification under LOUDAUTO's `.desktop` entry.
const DESKTOP_ENTRY: &str = "to.skunk.loudauto";

/// Shows a notification from LOUDAUTO on a blocking thread, so the D-Bus round trip (and
/// the first `icon::app_icon` lookup) never holds up the runtime. A failure (no
/// notification server, no session bus) is logged, never raised or panicked on.
pub fn show(summary: &str, body: &str) {
    let (summary, body) = (summary.to_owned(), body.to_owned());
    let send = move || {
        if let Err(e) = notification(&summary, &body, icon::app_icon()).show() {
            log::warn!("notification not shown: {e}");
        }
    };
    match tokio::runtime::Handle::try_current() {
        Ok(rt) => {
            rt.spawn_blocking(send);
        }
        Err(_) => log::warn!("notification not shown: no tokio runtime"),
    }
}

/// The notification `show` sends, with the theme icon `icon`.
fn notification(summary: &str, body: &str, icon: &str) -> Notification {
    let mut n = Notification::new();
    n.appname(APP_NAME)
        .icon(icon)
        .summary(summary)
        .body(body)
        .hint(Hint::DesktopEntry(DESKTOP_ENTRY.into()));
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_is_from_loudauto() {
        let n = notification("Liked: nerve — 2hollis", "", "spotify-launcher");
        assert_eq!(n.appname, "LOUDAUTO");
        assert_eq!(n.icon, "spotify-launcher");
        assert_eq!(n.summary, "Liked: nerve — 2hollis");
        assert_eq!(n.body, "");
        assert!(
            n.hints
                .contains(&Hint::DesktopEntry("to.skunk.loudauto".into())),
            "{:?}",
            n.hints
        );
    }
}
