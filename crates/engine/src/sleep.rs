//! Resume from sleep. logind sends `PrepareForSleep(true)` on the system bus before the
//! machine sleeps and `PrepareForSleep(false)` once it has woken. On waking, the session
//! supervisor reconnects the session (`SessionHint::Woke`), and the wake signal lets the
//! Soloist loop ask Soloist for fresh state.
//!
//! The D-Bus part is kept to `listen`; the zbus is the one mpris-server re-exports, so
//! this needs no zbus dependency of its own.

use futures_util::StreamExt;
use log::{info, warn};
use mpris_server::zbus::{self, Connection, Proxy};
use tokio::sync::{mpsc, watch};

use crate::session::SessionHint;

const LOGIN1: &str = "org.freedesktop.login1";
const LOGIN1_PATH: &str = "/org/freedesktop/login1";
const LOGIN1_MANAGER: &str = "org.freedesktop.login1.Manager";

/// Watches logind in the background. Each resume sends `SessionHint::Woke` to `hints`
/// and bumps the returned receiver: its value counts resumes, and `changed()` wakes
/// every clone of it. Without a system bus or logind it logs a warning, and
/// `changed()` returns `Err` because no wake will come.
pub fn watch_sleep(hints: mpsc::Sender<SessionHint>) -> watch::Receiver<u64> {
    let (wake, woken) = watch::channel(0);
    tokio::spawn(async move {
        match listen(&hints, &wake).await {
            Ok(()) => warn!("stopped watching for resume from sleep: the system bus closed"),
            Err(e) => warn!("not watching for resume from sleep: {e}"),
        }
    });
    woken
}

async fn listen(hints: &mpsc::Sender<SessionHint>, wake: &watch::Sender<u64>) -> zbus::Result<()> {
    let bus = Connection::system().await?;
    let manager = Proxy::new(&bus, LOGIN1, LOGIN1_PATH, LOGIN1_MANAGER).await?;
    let mut signals = manager.receive_signal("PrepareForSleep").await?;
    while let Some(signal) = signals.next().await {
        match signal.body().deserialize::<bool>() {
            Ok(start) => on_prepare_for_sleep(start, hints, wake).await,
            Err(e) => warn!("unreadable PrepareForSleep signal: {e}"),
        }
    }
    Ok(())
}

/// One `PrepareForSleep(start)`. Only the resume (`start == false`) does anything.
async fn on_prepare_for_sleep(
    start: bool,
    hints: &mpsc::Sender<SessionHint>,
    wake: &watch::Sender<u64>,
) {
    if start {
        info!("going to sleep");
        return;
    }
    info!("resumed from sleep");
    // `send_modify` updates the value even while no one subscribes.
    wake.send_modify(|wakes| *wakes = wakes.wrapping_add(1));
    // A closed channel means the supervisor has stopped: nothing to reconnect.
    let _ = hints.send(SessionHint::Woke).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resume_reconnects_the_session_and_fires_wake() {
        let (hints, mut hints_rx) = mpsc::channel(4);
        let (wake, mut woken) = watch::channel(0);
        on_prepare_for_sleep(false, &hints, &wake).await;
        assert_eq!(hints_rx.try_recv(), Ok(SessionHint::Woke));
        assert!(woken.has_changed().unwrap());
        assert_eq!(*woken.borrow_and_update(), 1);

        on_prepare_for_sleep(false, &hints, &wake).await;
        assert_eq!(*woken.borrow_and_update(), 2, "every resume fires again");
    }

    #[tokio::test]
    async fn going_to_sleep_does_nothing() {
        let (hints, mut hints_rx) = mpsc::channel(4);
        let (wake, woken) = watch::channel(0);
        on_prepare_for_sleep(true, &hints, &wake).await;
        assert!(hints_rx.try_recv().is_err());
        assert!(!woken.has_changed().unwrap());
    }
}
