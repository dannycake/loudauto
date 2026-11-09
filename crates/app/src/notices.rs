//! What the window has to tell the owner that isn't a page: the engine's notices
//! (`Event::Notice`), and what came of a request the owner made (the bottom bar's
//! "Couldn't pause: …"), so nothing fails silently.
//!
//! A notice shows for [`SHOWN_FOR`] as one secondary line above the bottom bar, an error
//! as `⚠` and its words in the error's red box. The engine's errors are the exception:
//! they need the owner, so they stay, each a red box with a Dismiss button, until
//! dismissed. A request that failed is not one of them: trying again is the owner's whole
//! action, so it goes with the rest (spec "Look and feel", Task 29 ruling).

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use eframe::egui::{self, Role, Sense, TextStyle, TextWrapMode, Ui, WidgetInfo, WidgetText};
use engine_ipc::{Level, Reply, Request};

use crate::{
    link::{RequestId, Requests},
    page_frame::warning_icon,
    theme,
};

/// How long a notice shows.
pub const SHOWN_FOR: Duration = Duration::from_secs(6);
/// The button that puts an engine's error away.
pub const DISMISS: &str = "Dismiss";
/// Notices kept at most, of each kind; the oldest go first.
const KEPT: usize = 16;
/// The engine's errors shown at once, newest last; the rest wait their turn.
const ERRORS_SHOWN: usize = 3;

/// One notice: what it says, and since when.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub level: Level,
    pub text: String,
    pub at: Instant,
}

/// The notices, oldest first: those that show for [`SHOWN_FOR`], and the engine's errors,
/// which stay until dismissed.
#[derive(Debug, Default)]
pub struct Notices {
    brief: Vec<Entry>,
    errors: Vec<Entry>,
}

impl Notices {
    /// The engine's notice: an error stays until dismissed, the rest show for
    /// [`SHOWN_FOR`].
    pub fn push(&mut self, level: Level, text: impl Into<String>) {
        match level {
            Level::Error => add(&mut self.errors, level, text.into()),
            Level::Info => add(&mut self.brief, level, text.into()),
        }
    }

    /// What came of a request the owner made: shown for [`SHOWN_FOR`] whatever its
    /// level, since trying again is all a failure asks of the owner.
    pub fn brief(&mut self, level: Level, text: impl Into<String>) {
        add(&mut self.brief, level, text.into());
    }

    /// The notices showing for a while at `now`, oldest first.
    pub fn visible(&self, now: Instant) -> Vec<&Entry> {
        self.brief
            .iter()
            .filter(|e| left(e, now).is_some())
            .collect()
    }

    /// The engine's errors not yet dismissed, oldest first.
    pub fn errors(&self) -> &[Entry] {
        &self.errors
    }

    /// Puts away the engine's error `text`.
    pub fn dismiss(&mut self, text: &str) {
        self.errors.retain(|e| e.text != text);
    }

    /// How long until the next of those showing goes, for a repaint then.
    fn next_change(&self, now: Instant) -> Option<Duration> {
        self.brief.iter().filter_map(|e| left(e, now)).min()
    }
}

/// Adds `text` to `entries`, as of now. The same text again moves to the end and starts
/// anew, so a failure repeated (a volume drag, say) shows once.
fn add(entries: &mut Vec<Entry>, level: Level, text: String) {
    entries.retain(|e| e.text != text);
    entries.push(Entry {
        level,
        text,
        at: Instant::now(),
    });
    let over = entries.len().saturating_sub(KEPT);
    entries.drain(..over);
}

/// How much longer `entry` shows at `now`; `None` once it has gone.
fn left(entry: &Entry, now: Instant) -> Option<Duration> {
    SHOWN_FOR
        .checked_sub(now.saturating_duration_since(entry.at))
        .filter(|left| !left.is_zero())
}

/// `Couldn't <what>: <err>`; an error that already says `Couldn't …` (the heart's, from
/// the engine) is kept whole.
pub fn failure_text(what: &str, err: &str) -> String {
    if err.starts_with("Couldn't ") {
        err.to_owned()
    } else {
        format!("Couldn't {what}: {err}")
    }
}

/// The notices, in panels along the bottom of `ui` (above the bottom bar, which is added
/// first): the newest of those showing for a while as one line, and above it the engine's
/// errors, each a red box. Nothing when there are none. A repaint is asked for when the
/// line is due to go.
pub fn show(ui: &mut Ui, notices: &mut Notices, now: Instant) {
    line(ui, notices, now);
    errors(ui, notices);
}

/// The newest notice showing for a while, as one line: secondary, or an error's `⚠` and
/// words in the red box.
fn line(ui: &mut Ui, notices: &Notices, now: Instant) {
    let Some(newest) = notices.visible(now).pop() else {
        return;
    };
    egui::Panel::bottom("notice-line")
        .resizable(false)
        .show_separator_line(false)
        .frame(
            egui::Frame::new()
                .fill(theme::BG)
                .inner_margin(egui::Margin::symmetric(12, 4)),
        )
        .show(ui, |ui| {
            let text = newest.text.as_str();
            if newest.level == Level::Error {
                error_box(ui, |ui| {
                    ui.horizontal(|ui| {
                        warning_icon(ui);
                        one_line(ui, text, theme::ERROR_TEXT);
                    });
                });
            } else {
                one_line(ui, text, theme::SECONDARY);
            }
        });
    if let Some(wait) = notices.next_change(now) {
        ui.ctx().request_repaint_after(wait);
    }
}

/// The engine's errors, newest last, each as `⚠`, its words and Dismiss in a red box.
fn errors(ui: &mut Ui, notices: &mut Notices) {
    let shown = notices.errors.len().saturating_sub(ERRORS_SHOWN);
    let shown = &notices.errors[shown..];
    if shown.is_empty() {
        return;
    }
    let mut dismissed = None;
    egui::Panel::bottom("notice-errors")
        .resizable(false)
        .show_separator_line(false)
        .frame(
            egui::Frame::new()
                .fill(theme::BG)
                .inner_margin(egui::Margin::symmetric(12, 4)),
        )
        .show(ui, |ui| {
            for entry in shown {
                if error_banner(ui, &entry.text, |ui| ui.button(DISMISS).clicked()) {
                    dismissed = Some(entry.text.clone());
                }
            }
        });
    if let Some(text) = dismissed {
        notices.dismiss(&text);
        ui.ctx().request_repaint();
    }
}

/// `text` on one line, cut short with "…" where it doesn't fit, in `colour`, with a label
/// for screen readers.
fn one_line(ui: &mut Ui, text: &str, colour: egui::Color32) {
    let galley = WidgetText::from(text).into_galley(
        ui,
        Some(TextWrapMode::Truncate),
        ui.available_width(),
        TextStyle::Body,
    );
    let (rect, response) = ui.allocate_exact_size(galley.size(), Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(Role::Label, true, text));
    ui.painter().galley(rect.min, galley, colour);
}

/// An error that needs the owner: `⚠` and `text` in the error's red box (spec "Look and
/// feel"), with what `button` adds at its right; whether that was clicked.
pub fn error_banner(ui: &mut Ui, text: &str, button: impl FnOnce(&mut Ui) -> bool) -> bool {
    error_box(ui, |ui| {
        egui::Sides::new()
            .shrink_left()
            .wrap()
            .show(
                ui,
                |ui| {
                    warning_icon(ui);
                    ui.label(text);
                },
                button,
            )
            .1
    })
}

/// `add` in the error's red box, its text, the warning sign and a second button's lines
/// in the error's light red; a main button keeps its own look.
pub fn error_box<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    theme::error_box()
        .show(ui, |ui| {
            theme::error_ink(ui);
            add(ui)
        })
        .inner
}

/// `add` in a 1 px outline in the line grey, on the background: set apart, not an error.
pub fn outlined<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    theme::outline().show(ui, add).inner
}

/// What the window asked of the engine on the owner's behalf (logging in, a setting),
/// for a notice of what came of it: what each request was, and what to say when it is
/// done, if anything.
#[derive(Debug, Default)]
pub struct Sent(HashMap<RequestId, Asked>);

#[derive(Debug, Clone, Copy)]
struct Asked {
    /// "log in": a failure says `Couldn't log in: …`.
    what: &'static str,
    /// Said once the engine has done it.
    done: Option<&'static str>,
}

impl Sent {
    /// Sends `req`, which is to `what`; `done` is said once it has worked.
    pub fn send(
        &mut self,
        link: &dyn Requests,
        req: Request,
        what: &'static str,
        done: Option<&'static str>,
    ) {
        self.0.insert(link.send(req), Asked { what, done });
    }

    /// Whether the reply to request `id` is for these.
    pub fn wants(&self, id: RequestId) -> bool {
        self.0.contains_key(&id)
    }

    /// The reply to request `id`: what to say of it, if anything.
    pub fn on_reply(
        &mut self,
        id: RequestId,
        result: Result<Reply, String>,
    ) -> Option<(Level, String)> {
        let asked = self.0.remove(&id)?;
        match result {
            Ok(_) => asked.done.map(|done| (Level::Info, done.to_owned())),
            Err(e) => Some((Level::Error, failure_text(asked.what, &e))),
        }
    }

    /// The link (re)connected: nothing sent before is waited for.
    pub fn clear(&mut self) {
        self.0.clear();
    }
}

#[cfg(test)]
mod tests {
    use eframe::egui::vec2;
    use egui_kittest::{Harness, kittest::Queryable};

    use super::*;
    use crate::{
        page_frame::WARNING,
        test_support::{self, FakeLink, texts},
    };

    fn shown(notices: &Notices, now: Instant) -> Vec<&str> {
        notices
            .visible(now)
            .into_iter()
            .map(|e| e.text.as_str())
            .collect()
    }

    /// A notice shows for six seconds; the same one twice shows once, as the newest. A
    /// failed request is one, whatever its level.
    #[test]
    fn notices_show_for_six_seconds() {
        let mut notices = Notices::default();
        notices.brief(Level::Error, "Couldn't pause: The player is restarting.");
        notices.push(Level::Info, "Something else");
        notices.brief(Level::Error, "Couldn't pause: The player is restarting.");
        let now = Instant::now();
        assert_eq!(
            shown(&notices, now),
            [
                "Something else",
                "Couldn't pause: The player is restarting."
            ]
        );
        assert_eq!(notices.visible(now)[1].level, Level::Error);
        assert!(notices.errors().is_empty());
        let wait = notices.next_change(now).unwrap();
        assert!(wait <= SHOWN_FOR && !wait.is_zero(), "{wait:?}");
        let later = now + SHOWN_FOR + Duration::from_millis(1);
        assert!(shown(&notices, later).is_empty());
        assert_eq!(notices.next_change(later), None);

        for i in 0..KEPT + 4 {
            notices.push(Level::Info, format!("n{i}"));
        }
        assert_eq!(notices.brief.len(), KEPT);
        assert_eq!(notices.brief[0].text, "n4");
    }

    /// Carry 1: the engine's errors stay until dismissed, and aren't crowded out by the
    /// notices that come and go.
    #[test]
    fn engine_errors_stay_until_dismissed() {
        let mut notices = Notices::default();
        notices.push(Level::Error, "Soloist lost its audio device.");
        for i in 0..KEPT + 4 {
            notices.brief(Level::Error, format!("Couldn't pause: {i}"));
        }
        let later = Instant::now() + SHOWN_FOR * 100;
        assert!(notices.visible(later).is_empty());
        assert_eq!(notices.next_change(later), None, "no repaint for an error");
        let texts: Vec<_> = notices.errors().iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["Soloist lost its audio device."]);
        notices.dismiss("Soloist lost its audio device.");
        assert!(notices.errors().is_empty());
    }

    /// The newest notice is one line in the secondary grey; with none, there is no line.
    /// An error's is `⚠` and its words in the error's red box.
    #[test]
    fn newest_notice_is_a_dim_line() {
        let mut harness = Harness::new_ui_state(
            |ui, notices: &mut Notices| show(ui, notices, Instant::now()),
            Notices::default(),
        );
        harness.run();
        assert!(texts(&harness).is_empty());
        harness.state_mut().push(Level::Info, "first");
        harness.state_mut().push(Level::Info, "second");
        harness.run();
        harness.get_by_label("second");
        assert!(harness.query_by_label("first").is_none());
        assert!(harness.query_by_label(WARNING).is_none());
        let drawn: Vec<_> = texts(&harness)
            .into_iter()
            .map(|(t, c, _)| (t, c))
            .collect();
        assert_eq!(drawn, [("second".to_string(), theme::SECONDARY)]);
        assert!(
            !test_support::fills(&harness)
                .iter()
                .any(|(_, c)| *c == theme::ERROR_FILL)
        );
        harness
            .state_mut()
            .brief(Level::Error, "Couldn't pause: gone.");
        harness.run();
        let words = harness.get_by_label("Couldn't pause: gone.").rect();
        harness.get_by_label(WARNING);
        test_support::assert_in_red_box(&harness, "Couldn't pause: gone.", words);
    }

    /// The engine's error is `⚠` and its words in the error's red box (Look 2), over the
    /// line; Dismiss puts it away.
    #[test]
    fn engine_error_is_a_red_box() {
        let (builder, _) = test_support::builder(vec2(800.0, 300.0), false);
        let mut harness = builder.build_ui_state(
            |ui, notices: &mut Notices| {
                *ui.visuals_mut() = theme::visuals();
                show(ui, notices, Instant::now());
            },
            Notices::default(),
        );
        harness
            .state_mut()
            .push(Level::Error, "Soloist lost its audio device.");
        harness.state_mut().push(Level::Info, "Brief");
        harness.run();
        let error = harness
            .get_by_label("Soloist lost its audio device.")
            .rect();
        let brief = harness.get_by_label("Brief").rect();
        assert!(error.bottom() <= brief.top(), "{error:?} {brief:?}");
        harness.get_by_label(WARNING);
        test_support::assert_in_red_box(&harness, "Soloist lost its audio device.", error);
        // Dismiss is a second button: no fill of its own on the red, at rest or under the
        // pointer, only its light red line.
        let only_red = |harness: &Harness<'_, Notices>| {
            let fills = test_support::fills(harness);
            let red = fills
                .iter()
                .find(|(r, c)| *c == theme::ERROR_FILL && r.contains_rect(error))
                .map(|(r, _)| *r)
                .expect("a red box");
            let inside: Vec<_> = fills
                .into_iter()
                .filter(|(r, _)| *r != red && red.contains_rect(*r))
                .filter(|(_, c)| *c != theme::ACCENT)
                .collect();
            assert_eq!(inside, [], "fills on the red box");
        };
        only_red(&harness);
        harness.get_by_label(DISMISS).hover();
        harness.run();
        only_red(&harness);
        harness.get_by_label(DISMISS).click();
        harness.run();
        assert!(
            harness
                .query_by_label("Soloist lost its audio device.")
                .is_none()
        );
        assert!(harness.state().errors().is_empty());

        // A few at once, the newest; the rest show as those are put away.
        for i in 0..ERRORS_SHOWN + 2 {
            harness.state_mut().push(Level::Error, format!("e{i}"));
        }
        harness.run();
        assert_eq!(harness.query_all_by_label(DISMISS).count(), ERRORS_SHOWN);
        assert!(harness.query_by_label("e1").is_none());
        harness.get_by_label("e2");
    }

    /// A request's failure says `Couldn't <what>: …`; one that worked says its `done`,
    /// if it has one; an answer to something else isn't taken.
    #[test]
    fn sent_says_what_came_of_it() {
        let link = FakeLink::default();
        let mut sent = Sent::default();
        sent.send(&link, Request::StartLogin, "log in", None);
        let login = link.last_id();
        sent.send(
            &link,
            Request::ForgetSoloistKey,
            "forget the Soloist key",
            Some("Forgot the Soloist key."),
        );
        let forget = link.last_id();
        assert!(sent.wants(login) && sent.wants(forget) && !sent.wants(99));
        assert_eq!(
            sent.on_reply(login, Err("The engine is busy.".into())),
            Some((Level::Error, "Couldn't log in: The engine is busy.".into()))
        );
        assert_eq!(
            sent.on_reply(forget, Ok(Reply::Ok)),
            Some((Level::Info, "Forgot the Soloist key.".into()))
        );
        assert!(!sent.wants(login) && !sent.wants(forget));
        sent.send(&link, Request::RestartPlayer, "restart the player", None);
        assert_eq!(sent.on_reply(link.last_id(), Ok(Reply::Ok)), None);
        sent.send(&link, Request::RestartPlayer, "restart the player", None);
        sent.clear();
        assert!(!sent.wants(link.last_id()));
        assert_eq!(
            failure_text("pause", "Couldn't pause: no."),
            "Couldn't pause: no."
        );
    }
}
