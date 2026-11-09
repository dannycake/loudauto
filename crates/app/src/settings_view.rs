//! The settings, opened by the gear in the sidebar's footer, kept small (spec "First
//! run, login, settings"): the output device, starting at login, the heart shortcut and
//! forgetting the Soloist key. No Log out: that stays a CLI action
//! (`loudauto-cli logout`), since a lost Soloist login can only be paired again from the
//! desktop app.
//!
//! What it asks of the desktop (the PipeWire sinks from `pactl`, System Settings'
//! shortcuts page) runs on threads of its own, through a [`Runner`] tests fake: the UI
//! thread never waits on a process.

use std::{
    process::{Command, Stdio},
    sync::{Arc, mpsc},
    thread,
};

use eframe::egui::{self, Checkbox, Label, RichText, Ui};
use engine_ipc::{Level, Request, Setting, Settings};
use log::warn;
use serde::Deserialize;

use crate::{
    link::Requests,
    notices::{self, Notices, Sent, failure_text},
    page_frame::{LOADING, warning_icon},
    theme,
};

pub const TITLE: &str = "Settings";
pub const OUTPUT_DEVICE: &str = "Output device";
/// The output device that follows the system's default.
pub const DEFAULT_DEVICE: &str = "Default";
pub const START_AT_LOGIN: &str = "Start at login";
pub const CHANGE_SHORTCUT: &str = "Change heart shortcut…";
pub const FORGET_KEY: &str = "Forget Soloist key";
/// The confirmation's question.
pub const FORGET_CONFIRM: &str = "Forget the saved Soloist key? Save it again with `loudauto-cli set-key` before Soloist next starts.";
pub const FORGET: &str = "Forget";
pub const CANCEL: &str = "Cancel";
/// Said once the engine has forgotten the key.
const FORGOT: &str = "Forgot the Soloist key.";
/// The settings' column, at most this wide.
const MAX_WIDTH: f32 = 640.0;

/// A PipeWire sink, as `pactl -f json list sinks` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Sink {
    /// What the setting holds: the node's name.
    pub name: String,
    /// What the owner reads.
    #[serde(default)]
    pub description: String,
}

/// What the settings ask of the desktop. Each call may block: the view makes it on a
/// thread of its own.
pub trait Runner: Send + Sync + 'static {
    /// The PipeWire sinks.
    fn sinks(&self) -> Result<Vec<Sink>, String>;
    /// Opens System Settings' shortcuts page; returns once it has closed, or failed to
    /// open.
    fn open_shortcuts(&self) -> Result<(), String>;
}

/// The desktop's own commands: `pactl` and `systemsettings`.
#[derive(Debug, Default)]
pub struct Commands;

impl Runner for Commands {
    fn sinks(&self) -> Result<Vec<Sink>, String> {
        let out = Command::new("pactl")
            .args(["-f", "json", "list", "sinks"])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("pactl: {e}"))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(format!("pactl {}: {}", out.status, stderr.trim()));
        }
        parse_sinks(&out.stdout)
    }

    /// Only a failure to start is the owner's to hear, at once: how System Settings
    /// later exits is logged.
    fn open_shortcuts(&self) -> Result<(), String> {
        let mut child = Command::new("systemsettings")
            .arg("kcm_keys")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("systemsettings: {e}"))?;
        // Reaped here, on the settings' thread, whenever it closes.
        match child.wait() {
            Ok(status) if !status.success() => warn!("systemsettings kcm_keys: {status}"),
            Ok(_) => {}
            Err(e) => warn!("can't wait for systemsettings: {e}"),
        }
        Ok(())
    }
}

/// `pactl -f json list sinks`'s output, as sinks.
pub fn parse_sinks(json: &[u8]) -> Result<Vec<Sink>, String> {
    serde_json::from_slice(json).map_err(|e| format!("pactl's list isn't JSON: {e}"))
}

/// The sinks, as far as `pactl` has said.
#[derive(Debug)]
enum Sinks {
    /// Being listed.
    Listing,
    Listed(Vec<Sink>),
    /// Why there is no list.
    Failed(String),
}

/// What a runner's thread came back with.
#[derive(Debug)]
enum Done {
    /// The sinks, as listed by the thread started `asked`th.
    Sinks {
        asked: u64,
        result: Result<Vec<Sink>, String>,
    },
    /// System Settings closed, or didn't open.
    Shortcuts(Result<(), String>),
}

/// What the view keeps between frames.
pub struct SettingsView {
    runner: Arc<dyn Runner>,
    sinks: Sinks,
    /// How many times the sinks have been asked for: a slower, older listing that lands
    /// after a newer one is dropped.
    asked: u64,
    /// The Forget Soloist key confirmation is showing.
    confirming: bool,
    done_tx: mpsc::Sender<Done>,
    done: mpsc::Receiver<Done>,
}

impl SettingsView {
    pub fn new(runner: Arc<dyn Runner>) -> SettingsView {
        let (done_tx, done) = mpsc::channel();
        SettingsView {
            runner,
            sinks: Sinks::Listed(vec![]),
            asked: 0,
            confirming: false,
            done_tx,
            done,
        }
    }

    /// The view opens: the sinks are listed afresh, and no confirmation is left open.
    pub fn open(&mut self, ctx: &egui::Context) {
        self.confirming = false;
        self.asked += 1;
        let asked = self.asked;
        self.sinks = Sinks::Listing;
        self.run(ctx, move |runner| Done::Sinks {
            asked,
            result: runner.sinks(),
        });
    }

    /// Runs `job` with the runner on a thread of its own; its answer comes back through
    /// `done`, with a repaint.
    fn run(&self, ctx: &egui::Context, job: impl FnOnce(&dyn Runner) -> Done + Send + 'static) {
        let (runner, tx, ctx) = (self.runner.clone(), self.done_tx.clone(), ctx.clone());
        let spawned = thread::Builder::new()
            .name("loudauto-settings".into())
            .spawn(move || {
                // The view may have gone with the window: then nobody is waiting.
                let _ = tx.send(job(runner.as_ref()));
                ctx.request_repaint();
            });
        if let Err(e) = spawned {
            warn!("can't start the settings' thread: {e}");
        }
    }

    /// Takes in what the threads came back with; failures to open System Settings are
    /// said in `notices`, drawn earlier in the frame: hence a repaint.
    fn take_done(&mut self, ctx: &egui::Context, notices: &mut Notices) {
        while let Ok(done) = self.done.try_recv() {
            self.apply(done, notices);
            ctx.request_repaint();
        }
    }

    fn apply(&mut self, done: Done, notices: &mut Notices) {
        match done {
            Done::Sinks { asked, result } => {
                if asked != self.asked {
                    return;
                }
                self.sinks = match result {
                    Ok(sinks) => Sinks::Listed(sinks),
                    Err(e) => {
                        warn!("can't list the PipeWire sinks: {e}");
                        Sinks::Failed(e)
                    }
                };
            }
            Done::Shortcuts(Ok(())) => {}
            Done::Shortcuts(Err(e)) => {
                notices.brief(Level::Error, failure_text("open System Settings", &e));
            }
        }
    }

    /// Waits for one thread's answer and takes it in, failures dropped: for tests, whose
    /// fake runner answers at once.
    #[cfg(test)]
    fn wait(&mut self, notices: &mut Notices) {
        let done = self
            .done
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the runner's thread answers");
        self.apply(done, notices);
    }
}

/// What the view draws from and sends with, for a frame.
pub struct SettingsCx<'a> {
    pub link: &'a dyn Requests,
    /// The engine's settings; `None` until it has said.
    pub settings: Option<&'a Settings>,
    /// Where the view's requests are kept, for what came of them to be said.
    pub sent: &'a mut Sent,
    pub notices: &'a mut Notices,
}

/// Draws the settings into `ui`. A change goes to the engine, whose `Settings` event
/// the view then shows: nothing is kept here that the engine holds.
pub fn show(ui: &mut Ui, view: &mut SettingsView, cx: SettingsCx<'_>) {
    let SettingsCx {
        link,
        settings,
        sent,
        notices,
    } = cx;
    view.take_done(ui.ctx(), notices);
    ui.vertical(|ui| {
        ui.set_max_width(MAX_WIDTH);
        ui.spacing_mut().item_spacing.y = 8.0;
        ui.label(RichText::new(TITLE).heading().color(theme::TEXT));
        ui.add_space(8.0);

        ui.label(RichText::new(OUTPUT_DEVICE).color(theme::SECONDARY));
        let current = settings.map(|s| s.output_device.as_deref());
        if let Some(device) = output_device(ui, &view.sinks, current) {
            let req = Request::SetSetting(Setting::OutputDevice(device));
            sent.send(link, req, "change the output device", None);
        }
        ui.add_space(8.0);

        let mut autostart = settings.is_some_and(|s| s.autostart);
        let toggled = ui
            .add_enabled(
                settings.is_some(),
                Checkbox::new(&mut autostart, START_AT_LOGIN),
            )
            .changed();
        if toggled {
            let req = Request::SetSetting(Setting::Autostart(autostart));
            sent.send(link, req, "change Start at login", None);
        }
        ui.add_space(8.0);

        if ui.button(CHANGE_SHORTCUT).clicked() {
            view.run(ui.ctx(), |runner| Done::Shortcuts(runner.open_shortcuts()));
        }
        ui.add_space(8.0);

        if view.confirming {
            notices::outlined(ui, |ui| {
                ui.add(Label::new(FORGET_CONFIRM).wrap());
                ui.horizontal(|ui| {
                    if ui.button(FORGET).clicked() {
                        view.confirming = false;
                        let (what, done) = ("forget the Soloist key", Some(FORGOT));
                        sent.send(link, Request::ForgetSoloistKey, what, done);
                    }
                    if ui.button(CANCEL).clicked() {
                        view.confirming = false;
                    }
                });
            });
        } else if ui.button(FORGET_KEY).clicked() {
            view.confirming = true;
        }
    });
}

/// The output devices as radio buttons: Default, then each sink by its description
/// (else its name), and the device set if it isn't listed (unplugged, say). `current`
/// is the engine's setting, `None` before it has said: nothing can be picked then. The
/// device picked, when it isn't the current one.
fn output_device(
    ui: &mut Ui,
    sinks: &Sinks,
    current: Option<Option<&str>>,
) -> Option<Option<String>> {
    let mut picked = None;
    ui.add_enabled_ui(current.is_some(), |ui| {
        let set = current.flatten();
        let mut radio = |ui: &mut Ui, device: Option<&str>, text: &str| {
            let on = current == Some(device);
            if ui.radio(on, text).clicked() && !on {
                picked = Some(device.map(str::to_owned));
            }
        };
        radio(ui, None, DEFAULT_DEVICE);
        let listed = match sinks {
            Sinks::Listed(sinks) => sinks.as_slice(),
            Sinks::Listing | Sinks::Failed(_) => &[],
        };
        for sink in listed {
            let text = if sink.description.is_empty() {
                &sink.name
            } else {
                &sink.description
            };
            radio(ui, Some(&sink.name), text);
        }
        if let Some(set) = set.filter(|set| !listed.iter().any(|s| s.name == *set)) {
            radio(ui, Some(set), set);
        }
    });
    match sinks {
        Sinks::Listing => {
            ui.label(RichText::new(LOADING).color(theme::SECONDARY));
        }
        Sinks::Failed(e) => {
            notices::error_box(ui, |ui| {
                ui.horizontal(|ui| {
                    warning_icon(ui);
                    ui.add(Label::new(failure_text("list the outputs", e)).wrap());
                });
            });
        }
        Sinks::Listed(_) => {}
    }
    picked
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use eframe::egui::vec2;
    use egui_kittest::{Harness, kittest::Queryable};

    use super::*;
    use crate::test_support::{self, FakeLink};

    /// A runner that never runs anything: canned sinks, and a count of System Settings
    /// opened.
    #[derive(Default)]
    struct FakeRunner {
        sinks: Mutex<Option<Result<Vec<Sink>, String>>>,
        shortcuts: Mutex<Vec<Result<(), String>>>,
        opened: Mutex<u32>,
    }

    impl Runner for FakeRunner {
        fn sinks(&self) -> Result<Vec<Sink>, String> {
            self.sinks
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| Ok(vec![]))
        }

        fn open_shortcuts(&self) -> Result<(), String> {
            *self.opened.lock().unwrap() += 1;
            self.shortcuts.lock().unwrap().pop().unwrap_or(Ok(()))
        }
    }

    fn sink(name: &str, description: &str) -> Sink {
        Sink {
            name: name.into(),
            description: description.into(),
        }
    }

    struct Shown {
        view: SettingsView,
        settings: Option<Settings>,
        link: FakeLink,
        sent: Sent,
        notices: Notices,
    }

    fn settings(device: Option<&str>, autostart: bool) -> Settings {
        Settings {
            volume: 40,
            output_device: device.map(Into::into),
            autostart,
        }
    }

    fn harness(runner: Arc<FakeRunner>, settings: Option<Settings>) -> Harness<'static, Shown> {
        let (builder, _) = test_support::builder(vec2(900.0, 700.0), false);
        let mut harness = builder.build_ui_state(
            |ui, s: &mut Shown| {
                let cx = SettingsCx {
                    link: &s.link,
                    settings: s.settings.as_ref(),
                    sent: &mut s.sent,
                    notices: &mut s.notices,
                };
                show(ui, &mut s.view, cx);
            },
            Shown {
                view: SettingsView::new(runner),
                settings,
                link: FakeLink::default(),
                sent: Sent::default(),
                notices: Notices::default(),
            },
        );
        let ctx = harness.ctx.clone();
        let s = harness.state_mut();
        s.view.open(&ctx);
        s.view.wait(&mut s.notices);
        harness.run();
        harness
    }

    #[test]
    fn no_logout_in_settings() {
        let harness = harness(Arc::default(), Some(settings(None, false)));
        harness.get_by_label(TITLE);
        harness.get_by_label(OUTPUT_DEVICE);
        harness.get_by_label(DEFAULT_DEVICE);
        harness.get_by_label(START_AT_LOGIN);
        harness.get_by_label(CHANGE_SHORTCUT);
        harness.get_by_label(FORGET_KEY);
        assert!(harness.query_by_label_contains("Log out").is_none());
        assert!(harness.query_by_label_contains("Logout").is_none());
        assert!(harness.query_by_label_contains("log out").is_none());
    }

    /// Each setting sends what it says: an output device picked (Default is none), Start
    /// at login flipped, the key forgotten only once confirmed. The shortcut button runs
    /// the runner, never `systemsettings` here.
    #[test]
    fn settings_send_what_they_say() {
        let runner = Arc::new(FakeRunner::default());
        *runner.sinks.lock().unwrap() = Some(Ok(vec![
            sink("alsa_output.usb-dac.analog-stereo", "USB DAC Analog Stereo"),
            sink("easyeffects_sink", "Easy Effects Sink"),
            sink("bare_sink", ""),
        ]));
        let mut harness = harness(runner.clone(), Some(settings(None, false)));
        // A sink without a description goes by its name.
        harness.get_by_label("bare_sink");
        harness.get_by_label("Easy Effects Sink").click();
        harness.run();
        let set = |s| Request::SetSetting(s);
        assert_eq!(
            harness.state().link.take(),
            [set(Setting::OutputDevice(Some("easyeffects_sink".into())))]
        );
        // The engine says it took it; Default goes back to none.
        harness.state_mut().settings = Some(settings(Some("easyeffects_sink"), false));
        harness.run();
        harness.get_by_label(DEFAULT_DEVICE).click();
        harness.run();
        harness.get_by_label(START_AT_LOGIN).click();
        harness.run();
        assert_eq!(
            harness.state().link.take(),
            [
                set(Setting::OutputDevice(None)),
                set(Setting::Autostart(true))
            ]
        );

        // Forget asks first; Cancel sends nothing.
        harness.get_by_label(FORGET_KEY).click();
        harness.run();
        harness.get_by_label(FORGET_CONFIRM);
        harness.get_by_label(CANCEL).click();
        harness.run();
        assert!(harness.query_by_label(FORGET_CONFIRM).is_none());
        assert_eq!(harness.state().link.take(), []);
        harness.get_by_label(FORGET_KEY).click();
        harness.run();
        harness.get_by_label(FORGET).click();
        harness.run();
        assert_eq!(harness.state().link.take(), [Request::ForgetSoloistKey]);
        assert!(harness.query_by_label(FORGET_CONFIRM).is_none());
        let s = harness.state_mut();
        let said = s.sent.on_reply(s.link.last_id(), Ok(engine_ipc::Reply::Ok));
        assert_eq!(
            said,
            Some((Level::Info, "Forgot the Soloist key.".to_string()))
        );

        // The shortcut page, through the runner, off the UI thread; its failure is said.
        runner
            .shortcuts
            .lock()
            .unwrap()
            .push(Err("systemsettings: not found".into()));
        // One frame: the click starts the thread after the frame took in what was done.
        harness.get_by_label(CHANGE_SHORTCUT).click();
        harness.step();
        let s = harness.state_mut();
        s.view.wait(&mut s.notices);
        harness.run();
        assert_eq!(*runner.opened.lock().unwrap(), 1);
        let shown: Vec<_> = harness
            .state()
            .notices
            .visible(std::time::Instant::now())
            .into_iter()
            .map(|e| (e.level, e.text.clone()))
            .collect();
        assert_eq!(
            shown,
            [(
                Level::Error,
                "Couldn't open System Settings: systemsettings: not found".to_string()
            )]
        );
    }

    /// The current device is marked, even when it isn't listed (unplugged); a listing
    /// that failed says so; before the engine has said, nothing can be changed.
    #[test]
    fn output_device_states() {
        use egui::accesskit::Toggled;
        use egui_kittest::kittest::NodeT;
        let runner = Arc::new(FakeRunner::default());
        *runner.sinks.lock().unwrap() = Some(Err("pactl: not found".into()));
        let mut harness = harness(runner, Some(settings(Some("gone_sink"), true)));
        harness.get_by_label("Couldn't list the outputs: pactl: not found");
        let toggled = |h: &Harness<'_, Shown>, label: &str| {
            h.get_by_label(label).accesskit_node().toggled() == Some(Toggled::True)
        };
        assert!(toggled(&harness, "gone_sink"));
        assert!(!toggled(&harness, DEFAULT_DEVICE));
        assert!(toggled(&harness, START_AT_LOGIN));
        harness.get_by_label("gone_sink").click();
        harness.run();
        assert_eq!(harness.state().link.take(), [], "already the device");

        harness.state_mut().settings = None;
        harness.run();
        harness.get_by_label(START_AT_LOGIN).click();
        harness.get_by_label(DEFAULT_DEVICE).click();
        harness.run();
        assert_eq!(harness.state().link.take(), []);
    }

    #[test]
    fn pactl_sinks_parse() {
        let json = br#"[
            {"index": 52, "state": "IDLE", "name": "alsa_output.usb-dac.analog-stereo",
             "description": "USB DAC Analog Stereo", "properties": {"a": "b"}},
            {"index": 74, "state": "SUSPENDED", "name": "easyeffects_sink"}
        ]"#;
        assert_eq!(
            parse_sinks(json),
            Ok(vec![
                sink("alsa_output.usb-dac.analog-stereo", "USB DAC Analog Stereo"),
                sink("easyeffects_sink", ""),
            ])
        );
        assert!(parse_sinks(b"Connection failure").is_err());
    }
}
