//! `loudauto`, LOUDAUTO's window: a thin client of the engine, which it starts if it isn't
//! running. Logs to stderr at `info` unless `RUST_LOG` says otherwise.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use app::{
    art::Art,
    bottom_bar::{self, BarAction, BarCx, BottomBar},
    fonts,
    link::{Link, LinkMsg, Requests},
    notices::{self, Notices, Sent},
    page_frame::Pages,
    pages::{self, Env, PageViews},
    screens::{self, Screen},
    settings_view::{self, Runner, SettingsCx, SettingsView},
    side_panel::{self, PanelCx, SidePanel},
    sidebar::{self, SidebarAction},
    theme,
    view_state::{self, ViewState},
    window_icon,
};
use eframe::egui::{self, Theme};
use engine_ipc::{
    Event, Level, LoginStep, PageRequest, PlayerState, SessionHealth, Settings, SoloistHealth,
};
use log::{info, warn};
use playback::event::NowPlaying;

/// While disconnected after a connection, over the window's last frame.
const RECONNECTING: &str = "Reconnecting to the LOUDAUTO engine…";
/// Before the first connection, once `CONNECT_GRACE` has passed.
const CONNECTING: &str = "Connecting to the LOUDAUTO engine…";
/// How long the window says nothing before its first connection: an engine already
/// running answers well within it, so its opening never flashes a banner.
const CONNECT_GRACE: Duration = Duration::from_millis(500);
/// Set to `1`, the window logs how many frames it drew every [`FRAME_LOG_EVERY`].
const DEBUG_REPAINT: &str = "LOUDAUTO_DEBUG_REPAINT";
/// How much wall time each `frames last 10 s: N` line counts.
const FRAME_LOG_EVERY: Duration = Duration::from_secs(10);

fn main() -> eframe::Result {
    let started = Instant::now();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    // LOUDAUTO's icon from the icon theme, for the window, looked up meanwhile.
    let window_icon = std::thread::spawn(window_icon::load);
    // The UI font (JetBrains Mono) and the symbol fonts are looked up and read while the
    // window opens.
    let startup_fonts = std::thread::spawn(|| {
        let ui = fonts::load_ui_font();
        let symbols = fonts::load_symbol_fonts(ui.as_ref());
        (ui, symbols)
    });
    // The view the last window left, so a raise looks like the same window coming forward.
    let view_path = ViewState::path();
    let view = view_path
        .as_deref()
        .map(ViewState::load)
        .unwrap_or_default();
    let frames = FrameLog::from_env();
    let mut viewport = egui::ViewportBuilder::default()
        .with_app_id(engine_ipc::APP_ID)
        .with_title("LOUDAUTO")
        .with_inner_size(view.size)
        .with_min_inner_size(view_state::MIN_SIZE);
    // Plasma on Wayland takes the icon from the desktop file the app id names; this one is
    // for where the platform takes a window's own (X11).
    match window_icon.join() {
        Ok(Some(icon)) => viewport = viewport.with_icon(icon),
        Ok(None) => {}
        Err(_) => warn!("the window icon lookup panicked; no window icon"),
    }
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "LOUDAUTO",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            // Dark whatever the desktop's light or dark preference.
            ctx.set_theme(egui::ThemePreference::Dark);
            ctx.set_visuals_of(Theme::Dark, theme::visuals());
            let ui_font_path = match startup_fonts.join() {
                Ok((ui, symbols)) => {
                    if ui.is_none() {
                        info!("no UI font; using egui's own");
                    }
                    let path = ui.as_ref().map(|font| font.face.path.clone());
                    fonts::install_ui_fonts(&ctx, ui, symbols);
                    path
                }
                Err(_) => {
                    warn!("the font lookup panicked; using egui's own");
                    None
                }
            };
            fonts::load_cjk_later(ctx.clone(), ui_font_path);
            let runner = Arc::new(settings_view::Commands);
            Ok(Box::new(Window {
                shell: Shell::new(view, Instant::now(), Art::new(ctx.clone()), runner),
                link: Link::start(ctx),
                view_path,
                started: Some(started),
                frames,
            }))
        }),
    )
}

/// Whether `LOUDAUTO_DEBUG_REPAINT` is `1`, the variable read through `get`.
fn debug_repaint_enabled(get: impl Fn(&str) -> Option<String>) -> bool {
    get(DEBUG_REPAINT).as_deref() == Some("1")
}

/// The window's frames since they were last taken: the UI thread ticks it, the frame
/// log's thread takes it.
#[derive(Default)]
struct FrameCounter(AtomicU64);

impl FrameCounter {
    fn tick(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    /// The frames since the last take, starting the count again.
    fn take(&self) -> u64 {
        self.0.swap(0, Ordering::Relaxed)
    }
}

/// `LOUDAUTO_DEBUG_REPAINT=1`, for the footprint check: the window ticks a counter each
/// time it runs, and a thread of its own logs `frames last 10 s: N` every 10 s of wall
/// time, so a window that isn't repainted at all still says 0. The thread has no egui
/// `Context` and the tick asks for nothing, so counting never causes a frame. Off, the
/// default, there is no counter and no thread.
struct FrameLog {
    counter: Option<Arc<FrameCounter>>,
}

impl FrameLog {
    /// On if the environment says `LOUDAUTO_DEBUG_REPAINT=1`, logging at `info`.
    fn from_env() -> FrameLog {
        if !debug_repaint_enabled(|name| std::env::var(name).ok()) {
            return FrameLog::off();
        }
        match FrameLog::start(FRAME_LOG_EVERY, |line| info!("{line}")) {
            Ok((frames, _thread)) => {
                info!("{DEBUG_REPAINT}=1: logging the frames every 10 s");
                frames
            }
            Err(e) => {
                warn!("can't start the frame counter: {e}");
                FrameLog::off()
            }
        }
    }

    fn off() -> FrameLog {
        FrameLog { counter: None }
    }

    /// Counts frames, a thread handing `log` the line for each `every` until this frame
    /// log is dropped. The line names [`FRAME_LOG_EVERY`] whatever `every` is: only the
    /// tests pass another.
    fn start(
        every: Duration,
        mut log: impl FnMut(String) + Send + 'static,
    ) -> std::io::Result<(FrameLog, JoinHandle<()>)> {
        let counter = Arc::new(FrameCounter::default());
        let taker = Arc::downgrade(&counter);
        let thread = std::thread::Builder::new()
            .name("frame-log".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(every);
                    let Some(counter) = taker.upgrade() else {
                        return;
                    };
                    log(format!("frames last 10 s: {}", counter.take()));
                }
            })?;
        let frames = FrameLog {
            counter: Some(counter),
        };
        Ok((frames, thread))
    }

    /// The window ran once.
    fn frame(&self) {
        if let Some(counter) = &self.counter {
            counter.tick();
        }
    }
}

struct Window {
    link: Link,
    shell: Shell,
    /// Where the view is saved; `None` with no home to save it in.
    view_path: Option<PathBuf>,
    /// When the process started, until the first frame is logged.
    started: Option<Instant>,
    /// `LOUDAUTO_DEBUG_REPAINT=1`'s frame count; nothing otherwise.
    frames: FrameLog,
}

impl Window {
    fn save_view(&self) {
        let Some(path) = &self.view_path else {
            return;
        };
        match self.shell.view.save(path) {
            Ok(()) => info!("saved the view to {}", path.display()),
            Err(e) => warn!("can't save the view to {}: {e}", path.display()),
        }
    }
}

impl eframe::App for Window {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Each time the window runs: every egui pass, and each logic-only tick while
        // it's minimized.
        self.frames.frame();
        let now = Instant::now();
        for msg in self.link.poll() {
            if self.shell.on_link(&self.link, msg, now) {
                // The engine is closing this window for a fresh one, which reads the view
                // as it starts: save it now rather than wait for the close to reach eframe.
                self.save_view();
            }
        }
        self.shell.view.nav.keys(ctx);
        self.shell.view.size = ctx.input(|i| i.viewport_rect().size()).into();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.shell.draw(ui, &self.link, Instant::now());
        if let Some(started) = self.started.take() {
            info!("first frame at {} ms", started.elapsed().as_millis());
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_view();
    }
}

/// The window apart from its link: what it knows of the connection, its view, the
/// pages, the player, the bottom bar, the cover art and the settings. The engine's state
/// is kept here once, as it last said it; the pages, the bar, the screens and the
/// settings draw from it.
struct Shell {
    connection: Connection,
    view: ViewState,
    pages: Pages,
    views: PageViews,
    art: Art,
    player: PlayerState,
    /// Soloist's health; `None` until the engine has said.
    soloist: Option<SoloistHealth>,
    /// What plays next.
    queue: Vec<NowPlaying>,
    /// The account's name, from the engine.
    username: Option<String>,
    /// The session's health; `None` until the engine has said.
    session: Option<SessionHealth>,
    bar: BottomBar,
    /// The right panel: the queue, the lyrics and the credits.
    side: SidePanel,
    /// The engine's notices and what failed, for above the bar.
    notices: Notices,
    /// The login; `None` until the engine has said.
    login: Option<LoginStep>,
    /// The engine's settings; `None` until it has said.
    settings: Option<Settings>,
    /// The days Soloist's build has left, once the engine has read them.
    expires_in_days: Option<u32>,
    settings_view: SettingsView,
    /// The page the settings were opened over: they show in place of the main area while
    /// it is still the current page. Any navigation closes them (`settings_shown`).
    settings_over: Option<PageRequest>,
    /// The window's own requests (the screens', the settings'), for what came of them.
    sent: Sent,
}

impl Shell {
    fn new(view: ViewState, now: Instant, art: Art, runner: Arc<dyn Runner>) -> Shell {
        Shell {
            connection: Connection::new(now),
            view,
            pages: Pages::new(),
            views: PageViews::default(),
            art,
            player: pages::idle_player(),
            soloist: None,
            queue: Vec::new(),
            username: None,
            session: None,
            bar: BottomBar::default(),
            side: SidePanel::default(),
            notices: Notices::default(),
            login: None,
            settings: None,
            expires_in_days: None,
            settings_view: SettingsView::new(runner),
            settings_over: None,
            sent: Sent::default(),
        }
    }

    /// Takes in a message from the link; true when it is the engine asking the window
    /// to close.
    fn on_link(&mut self, link: &impl Requests, msg: LinkMsg, now: Instant) -> bool {
        match msg {
            LinkMsg::Connected(state) => {
                self.connection.connected();
                self.pages.on_connected(link, self.view.nav.current());
                self.views.on_connected();
                self.bar.on_connected();
                self.side.on_connected();
                self.sent.clear();
                self.player = state.player;
                self.soloist = Some(state.soloist);
                self.queue = state.queue;
                self.side.set_queue(&self.queue);
                bottom_bar::prefetch_next(&mut self.art, &self.queue);
                self.username = state.username;
                self.session = Some(state.session);
                self.login = Some(state.login);
                self.settings = Some(state.settings);
                self.expires_in_days = state.soloist_expires_in_days;
            }
            LinkMsg::Disconnected => {
                self.connection.disconnected(now);
                self.pages.on_disconnected();
                self.side.on_disconnected();
                // Soloist went with the engine: nothing plays, and nothing takes commands,
                // until the engine says otherwise.
                let now_ms = bottom_bar::now_ms();
                let halted = self.player.halted(now_ms);
                self.bar.on_player(&self.player, &halted, now, now_ms);
                self.player = halted;
                self.soloist = None;
            }
            LinkMsg::Reply { id, result } if self.pages.wants(id) => {
                self.pages.on_reply(id, result);
                if self.pages.take_recovered() {
                    self.refresh_failing(link);
                }
            }
            LinkMsg::Reply { id, result } if self.views.wants(id) => {
                if let Some(text) = self.views.on_reply(id, result) {
                    self.notices.brief(Level::Error, text);
                }
            }
            LinkMsg::Reply { id, result } if self.side.wants(id) => {
                self.side.on_reply(id, result);
            }
            LinkMsg::Reply { id, result } if self.bar.wants(id) => {
                if let Some(text) = self.bar.on_reply(id, result, now) {
                    self.notices.brief(Level::Error, text);
                }
            }
            LinkMsg::Reply { id, result } if self.sent.wants(id) => {
                if let Some((level, text)) = self.sent.on_reply(id, result) {
                    self.notices.brief(level, text);
                }
            }
            LinkMsg::Event(Event::PageUpdated { req, reply }) => {
                self.pages.on_event(req, reply);
                if self.pages.take_recovered() {
                    self.refresh_failing(link);
                }
            }
            LinkMsg::Event(Event::Player(player)) => {
                self.bar
                    .on_player(&self.player, &player, now, bottom_bar::now_ms());
                self.player = player;
            }
            LinkMsg::Event(Event::Soloist(health)) => self.soloist = Some(health),
            LinkMsg::Event(Event::Queue(queue)) => {
                self.queue = queue;
                self.side.set_queue(&self.queue);
                bottom_bar::prefetch_next(&mut self.art, &self.queue);
            }
            LinkMsg::Event(Event::LikedChanged { uri, liked }) => {
                self.views.liked_set.set(uri, liked);
            }
            LinkMsg::Event(Event::LikedSetReady) => {
                self.views.liked_set.on_liked_set_ready();
                self.side.on_liked_set_ready();
            }
            LinkMsg::Event(Event::Username(name)) => self.username = Some(name),
            LinkMsg::Event(Event::Session(health)) => {
                let ready = health == SessionHealth::Ready
                    && self.session.as_ref() != Some(&SessionHealth::Ready);
                self.session = Some(health);
                if ready {
                    // What was asked for before the session was ready failed for want of it.
                    self.refresh_failing(link);
                }
            }
            LinkMsg::Event(Event::Notice(notice)) => self.notices.push(notice.level, notice.text),
            LinkMsg::Event(Event::Login(step)) => {
                let logged_in = step == LoginStep::LoggedIn
                    && self.login.as_ref() != Some(&LoginStep::LoggedIn);
                self.login = Some(step);
                if logged_in {
                    // What was asked for before the login failed for want of it.
                    self.refresh_failing(link);
                }
            }
            LinkMsg::Event(Event::Settings(settings)) => self.settings = Some(settings),
            LinkMsg::Event(Event::Expiry { days }) => self.expires_in_days = Some(days),
            LinkMsg::Event(Event::CloseWindow) => return true,
            LinkMsg::Reply { .. } => {}
        }
        false
    }

    /// Asks again for the current page and the sidebar if they failed, or their refresh
    /// did: once the session or the network is back.
    fn refresh_failing(&mut self, link: &impl Requests) {
        let reqs = [self.view.nav.current().clone(), PageRequest::Sidebar];
        self.pages.refresh_failing(link, &reqs);
    }

    /// The banner while disconnected, the bottom bar (and the notices above it), the
    /// sidebar, the right panel while open, and the main area: the settings while open,
    /// else the screen the engine's state calls for, else the current page.
    fn draw(&mut self, ui: &mut egui::Ui, link: &impl Requests, now: Instant) {
        banner(ui, &self.connection, now);
        // One clock for the frame: the lyrics follow the bar's position.
        let now_ms = bottom_bar::now_ms();
        // Before the side panels, so the bar spans the window.
        let cx = BarCx {
            link,
            art: &mut self.art,
            player: &self.player,
            soloist: self.soloist.as_ref(),
            now,
            now_ms,
            panel: self.view.panel.is_some(),
        };
        match bottom_bar::show(ui, &mut self.bar, cx) {
            Some(BarAction::GoAlbum(uri)) => self.view.nav.go(PageRequest::Album { uri }),
            Some(BarAction::GoArtist(uri)) => self.view.nav.go(PageRequest::Artist { uri }),
            Some(BarAction::TogglePanel) => self.side.toggle(&mut self.view.panel),
            None => {}
        }
        notices::show(ui, &mut self.notices, now);
        let settings_open = settings_shown(&mut self.settings_over, self.view.nav.current());
        let (view, pages) = (&mut self.view, &mut self.pages);
        pages.ensure(link, &PageRequest::Sidebar);
        let action = egui::Panel::left("sidebar")
            .resizable(false)
            .exact_size(sidebar::WIDTH)
            .frame(
                egui::Frame::new()
                    .fill(theme::SURFACE)
                    .inner_margin(egui::Margin::symmetric(0, 12)),
            )
            .show(ui, |ui| {
                let playlists = pages.get(&PageRequest::Sidebar);
                sidebar::show(ui, view.nav.current(), settings_open, playlists)
            })
            .inner;
        match action {
            Some(SidebarAction::Go(req)) => {
                // From the settings, the current page's entry goes back to the page as
                // it was (a search keeps its words).
                if !(settings_open && sidebar::is_current(&req, view.nav.current())) {
                    view.nav.go(req);
                }
                self.settings_over = None;
            }
            Some(SidebarAction::Retry) => pages.retry(link, &PageRequest::Sidebar),
            Some(SidebarAction::Settings) => {
                if settings_open {
                    self.settings_over = None;
                } else {
                    self.settings_over = Some(view.nav.current().clone());
                    self.settings_view.open(ui.ctx());
                }
            }
            None => {}
        }
        let cx = PanelCx {
            link,
            view,
            art: &mut self.art,
            player: &self.player,
            ready: self.soloist == Some(SoloistHealth::Ready),
            bar: &mut self.bar,
            views: &mut self.views,
            now,
            now_ms,
        };
        side_panel::show(ui, &mut self.side, cx);
        // A queue row may have opened a page: the settings close for it, this frame.
        let settings_open = settings_shown(&mut self.settings_over, view.nav.current());
        // A page row's `Show credits` opens the panel on the Credits tab, or picks another
        // track while it is open; the panel, drawn before the pages, shows it next frame.
        let panel = (view.panel, self.views.credits.clone());
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::same(24)),
            )
            .show(ui, |ui| {
                let screen = screens::screen_of(self.login.as_ref(), self.soloist.as_ref());
                // The missing and expired screens say what the banner would.
                if !matches!(
                    screen,
                    Some(Screen::NotInstalled { .. } | Screen::Expired { .. })
                ) {
                    screens::expiry_banner(ui, self.expires_in_days);
                }
                if settings_open {
                    let cx = SettingsCx {
                        link,
                        settings: self.settings.as_ref(),
                        sent: &mut self.sent,
                        notices: &mut self.notices,
                    };
                    settings_view::show(ui, &mut self.settings_view, cx);
                } else if let Some(screen) = screen {
                    if let Some(action) = screens::show(ui, screen) {
                        action.send(link, &mut self.sent);
                    }
                } else {
                    let env = Env {
                        link,
                        view,
                        art: &mut self.art,
                        player: &self.player,
                        username: self.username.as_deref(),
                        now,
                    };
                    pages::show(ui, pages, &mut self.views, env);
                }
            });
        if (self.view.panel, &self.views.credits) != (panel.0, &panel.1) {
            ui.ctx().request_repaint();
        }
    }
}

/// Whether the settings show: they were opened over `current`. Any navigation since
/// (the sidebar, the bar, a row, Back) closes them, for good: coming back to that page
/// doesn't reopen them.
fn settings_shown(over: &mut Option<PageRequest>, current: &PageRequest) -> bool {
    if over.as_ref().is_some_and(|page| page != current) {
        *over = None;
    }
    over.is_some()
}

/// What the window knows of its link to the engine.
#[derive(Debug)]
struct Connection {
    connected: bool,
    ever_connected: bool,
    /// When the window began waiting for a connection: its start, before the first.
    waiting_since: Instant,
}

/// What `Connection` has the window say.
#[derive(Debug, PartialEq)]
enum Banner {
    None,
    Show(&'static str),
    /// Nothing yet; perhaps after this long.
    Later(Duration),
}

impl Connection {
    fn new(now: Instant) -> Connection {
        Connection {
            connected: false,
            ever_connected: false,
            waiting_since: now,
        }
    }

    fn connected(&mut self) {
        self.connected = true;
        self.ever_connected = true;
    }

    fn disconnected(&mut self, now: Instant) {
        self.connected = false;
        self.waiting_since = now;
    }

    fn banner(&self, now: Instant) -> Banner {
        if self.connected {
            Banner::None
        } else if self.ever_connected {
            Banner::Show(RECONNECTING)
        } else {
            let waited = now.saturating_duration_since(self.waiting_since);
            match CONNECT_GRACE.checked_sub(waited) {
                Some(left) if !left.is_zero() => Banner::Later(left),
                _ => Banner::Show(CONNECTING),
            }
        }
    }
}

/// The banner. While disconnected the window keeps what it last drew, under it.
fn banner(ui: &mut egui::Ui, connection: &Connection, now: Instant) {
    match connection.banner(now) {
        Banner::Show(text) => {
            egui::Panel::top("engine-banner")
                .resizable(false)
                .frame(
                    egui::Frame::new()
                        .fill(theme::SURFACE)
                        .inner_margin(egui::Margin::symmetric(12, 6)),
                )
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| ui.label(text));
                });
        }
        // The one repaint that isn't input or a change of state: the grace running out.
        Banner::Later(left) => ui.ctx().request_repaint_after(left),
        Banner::None => {}
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use app::{
        link::RequestId,
        page_frame::LOADING,
        screens,
        settings_view::{self, Runner, Sink},
        track_list,
        view_state::PanelTab,
    };
    use eframe::egui::accesskit::Toggled;
    use egui_kittest::{
        Harness,
        kittest::{NodeT, Queryable},
    };
    use engine_ipc::{
        EngineState, LoginStep, Notice, Page, PageReply, PlayerState, Reply, Request,
        SessionHealth, Setting, Settings, SoloistHealth,
    };
    use playback::{
        command::Repeat,
        error::DOWNLOADS_URL,
        event::{Actions, PlayStatus},
    };
    use spot_core::{
        model::{PlaylistRef, RootEntry},
        pages::{LikedPage, PlaylistPage, TrackRow},
    };

    use super::*;

    /// A link that records what it is sent.
    #[derive(Default)]
    struct FakeLink {
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
        fn take(&self) -> Vec<Request> {
            self.sent.borrow_mut().drain(..).collect()
        }
    }

    fn page(req: PageRequest) -> Request {
        Request::Page {
            req,
            prefetch: false,
        }
    }

    fn engine_state() -> EngineState {
        EngineState {
            player: PlayerState {
                track: None,
                status: PlayStatus::Paused,
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
            soloist: SoloistHealth::Ready,
            session: SessionHealth::Ready,
            login: LoginStep::LoggedIn,
            settings: Settings {
                volume: 40,
                output_device: None,
                autostart: false,
            },
            username: None,
            soloist_expires_in_days: None,
        }
    }

    fn reply(page: Page) -> PageReply {
        PageReply {
            page,
            fetched_at_ms: 1,
            stale: false,
            error: None,
        }
    }

    struct Wired {
        shell: Shell,
        link: FakeLink,
    }

    /// Settings that never run `pactl` or `systemsettings`: no sinks.
    struct NoRunner;

    impl Runner for NoRunner {
        fn sinks(&self) -> Result<Vec<Sink>, String> {
            Ok(vec![])
        }

        fn open_shortcuts(&self) -> Result<(), String> {
            Ok(())
        }
    }

    /// The shell on `view`, its covers never loaded, its settings running nothing.
    fn shell(view: ViewState) -> Shell {
        let art = Art::offline(egui::Context::default());
        Shell::new(view, Instant::now(), art, Arc::new(NoRunner))
    }

    /// The window takes in `msg`.
    fn link_msg(harness: &mut Harness<'_, Wired>, msg: LinkMsg) {
        let w = harness.state_mut();
        w.shell.on_link(&w.link, msg, Instant::now());
    }

    fn event(harness: &mut Harness<'_, Wired>, event: Event) {
        link_msg(harness, LinkMsg::Event(event));
    }

    /// The engine fails the request the window sent last.
    fn fail_last(harness: &mut Harness<'_, Wired>, error: &str) {
        let id = harness.state().link.last_id.get();
        let result = Err(error.into());
        link_msg(harness, LinkMsg::Reply { id, result });
    }

    const SOLOIST_PATH: &str = "/home/u/.local/share/loudauto/soloist/soloist";

    /// Task 29: the login and Soloist's states replace the main area with their screen,
    /// whose buttons ask the engine, a failure said above the bar; under a screen the
    /// bar doesn't say it is reconnecting (carry 4); a missing binary has Try again
    /// (carry 5). Ready, the page is back.
    #[test]
    fn screens_replace_the_main_area() {
        let mut view = ViewState::default();
        view.nav.go(PageRequest::Liked);
        let mut harness = window(shell(view));
        let mut state = engine_state();
        state.login = LoginStep::Needed;
        state.soloist = SoloistHealth::NotPaired;
        link_msg(&mut harness, LinkMsg::Connected(Box::new(state)));
        harness.run();
        harness.state().link.take();
        // The page isn't drawn: only the playlists are loading.
        assert_eq!(harness.query_all_by_label(LOADING).count(), 1);
        assert!(
            harness.query_by_label(screens::NOT_PAIRED).is_none(),
            "login first"
        );
        harness.get_by_label(screens::LOG_IN).click();
        harness.run();
        assert_eq!(harness.state().link.take(), [Request::StartLogin]);

        // What was asked for before the login fails for want of it.
        let sidebar_id = harness.state().link.last_id.get() - 1;
        for id in [sidebar_id - 1, sidebar_id] {
            let result = Err("Not logged in.".into());
            link_msg(&mut harness, LinkMsg::Reply { id, result });
        }
        let prompt = LoginStep::Prompt {
            url: "https://www.spotify.com/pair".into(),
            code: "ABCD-EFGH".into(),
        };
        event(&mut harness, Event::Login(prompt));
        harness.run();
        harness.get_by_label("ABCD-EFGH");
        harness.get_by_label(screens::WAITING);

        // Logged in, the pages that failed are asked for again, once.
        event(&mut harness, Event::Login(LoginStep::LoggedIn));
        event(&mut harness, Event::Login(LoginStep::LoggedIn));
        assert_eq!(
            harness.state().link.take(),
            [page(PageRequest::Liked), page(PageRequest::Sidebar)]
        );
        harness.run();
        harness.get_by_label(screens::NOT_PAIRED);
        assert!(
            harness
                .query_by_label(bottom_bar::RECONNECTING_PLAYER)
                .is_none()
        );

        let missing = SoloistHealth::BinaryMissing {
            path: SOLOIST_PATH.into(),
        };
        event(&mut harness, Event::Soloist(missing));
        harness.run();
        harness.get_by_label(screens::NOT_INSTALLED);
        harness.get_by_label(DOWNLOADS_URL);
        assert!(
            harness
                .query_by_label(bottom_bar::RECONNECTING_PLAYER)
                .is_none()
        );
        assert!(
            harness
                .get_by_label(bottom_bar::PLAY)
                .accesskit_node()
                .is_disabled()
        );
        harness.get_by_label(screens::TRY_AGAIN).click();
        harness.run();
        assert_eq!(harness.state().link.take(), [Request::RestartPlayer]);
        fail_last(&mut harness, "The engine is stopping.");
        harness.run();
        harness.get_by_label("Couldn't restart the player: The engine is stopping.");
        assert!(harness.state().shell.notices.errors().is_empty(), "brief");

        event(&mut harness, Event::Soloist(SoloistHealth::Ready));
        harness.run();
        assert!(harness.query_by_label(screens::NOT_INSTALLED).is_none());
        assert_eq!(harness.query_all_by_label(LOADING).count(), 2);
    }

    /// Carries 1 and 2: the engine's notices reach the line above the bar, its errors
    /// staying until dismissed; the expiry banner shows 14 days ahead, and not over the
    /// expired screen.
    #[test]
    fn engine_notices_and_expiry() {
        let mut harness = window(shell(ViewState::default()));
        let mut state = engine_state();
        state.soloist_expires_in_days = Some(20);
        link_msg(&mut harness, LinkMsg::Connected(Box::new(state)));
        harness.run();
        assert!(
            harness
                .query_by_label_contains("Soloist expires in")
                .is_none()
        );

        let notice = |id, level, text: &str| {
            Event::Notice(Notice {
                id,
                level,
                text: text.into(),
            })
        };
        let info = "The new output device is used from LOUDAUTO's next start.";
        event(&mut harness, notice(1, Level::Info, info));
        event(
            &mut harness,
            notice(2, Level::Error, "Soloist lost its audio device."),
        );
        event(&mut harness, Event::Expiry { days: 9 });
        harness.run();
        harness.get_by_label(info);
        harness.get_by_label("Soloist lost its audio device.");
        let banner = screens::expiry_text(9).expect("9 days is within the warning");
        harness.get_by_label(&banner);
        let later = Instant::now() + notices::SHOWN_FOR * 2;
        let store = &harness.state().shell.notices;
        assert!(store.visible(later).is_empty());
        assert_eq!(store.errors().len(), 1);
        harness.get_by_label(notices::DISMISS).click();
        harness.run();
        assert!(
            harness
                .query_by_label("Soloist lost its audio device.")
                .is_none()
        );

        let expired = SoloistHealth::Expired {
            path: SOLOIST_PATH.into(),
        };
        event(&mut harness, Event::Soloist(expired));
        harness.run();
        harness.get_by_label(screens::EXPIRED);
        assert!(harness.query_by_label(&banner).is_none());
    }

    /// Carry 3: a page's request that failed is said above the bar, briefly.
    #[test]
    fn page_failures_are_said() {
        let mut view = ViewState::default();
        view.nav.go(PageRequest::Liked);
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .allow_missing_glyphs()
            .build_ui_state(
                |ui, w: &mut Wired| w.shell.draw(ui, &w.link, Instant::now()),
                Wired {
                    shell: shell(view),
                    link: FakeLink::default(),
                },
            );
        link_msg(&mut harness, LinkMsg::Connected(Box::new(engine_state())));
        let liked_id = harness.state().link.last_id.get() - 1;
        let liked = Page::Liked(LikedPage {
            total: 1,
            tracks: vec![TrackRow {
                uri: "spotify:track:alpha".into(),
                uid: None,
                title: "Alpha".into(),
                artists: vec![],
                album: None,
                duration_ms: 60_000,
                images: vec![],
                added_at: None,
            }],
        });
        let result = Ok(Reply::Page(reply(liked)));
        link_msg(
            &mut harness,
            LinkMsg::Reply {
                id: liked_id,
                result,
            },
        );
        harness.run();
        harness.state().link.take();
        harness.get_by_label("Alpha").click_secondary();
        harness.run();
        harness.get_by_label(track_list::ADD_TO_QUEUE).click();
        harness.run();
        let queued = Request::AddToQueue {
            uri: "spotify:track:alpha".into(),
        };
        assert_eq!(harness.state().link.take(), [queued]);
        fail_last(&mut harness, "Spotify didn't answer.");
        harness.run();
        harness.get_by_label("Couldn't add to the queue: Spotify didn't answer.");
        assert!(harness.state().shell.notices.errors().is_empty(), "brief");
    }

    /// The gear opens the settings in place of the main area (a screen's too); what they
    /// change goes to the engine, whose settings they show; a page's entry goes back.
    #[test]
    fn gear_opens_the_settings() {
        // The sidebar's entry, not the settings' heading.
        let click_gear = |harness: &Harness<'static, Wired>| {
            let gear = egui::accesskit::Role::Button;
            harness
                .get_by_role_and_label(gear, sidebar::SETTINGS)
                .click();
        };
        let mut harness = window(shell(ViewState::default()));
        let mut state = engine_state();
        state.soloist = SoloistHealth::Failed {
            message: "Spotify Soloist keeps stopping (exit code 1).".into(),
            log_tail: String::new(),
        };
        link_msg(&mut harness, LinkMsg::Connected(Box::new(state)));
        harness.run();
        harness.state().link.take();
        harness.get_by_label("Spotify Soloist keeps stopping (exit code 1).");
        click_gear(&harness);
        harness.run();
        assert!(
            harness
                .query_by_label("Spotify Soloist keeps stopping (exit code 1).")
                .is_none()
        );
        harness.get_by_label(settings_view::FORGET_KEY);
        harness.get_by_label(settings_view::START_AT_LOGIN).click();
        harness.run();
        assert_eq!(
            harness.state().link.take(),
            [Request::SetSetting(Setting::Autostart(true))]
        );
        let mut settings = engine_state().settings;
        settings.autostart = true;
        event(&mut harness, Event::Settings(settings.clone()));
        assert_eq!(harness.state().shell.settings.as_ref(), Some(&settings));
        fail_last(&mut harness, "The engine is stopping.");
        harness.run();
        harness.get_by_label("Couldn't change Start at login: The engine is stopping.");

        // The gear again closes them; so does the current page's entry, kept as it was.
        click_gear(&harness);
        harness.run();
        harness.get_by_label("Spotify Soloist keeps stopping (exit code 1).");
        click_gear(&harness);
        harness.run();
        harness.get_by_label(settings_view::FORGET_KEY);
        harness.get_by_label("Home").click();
        harness.run();
        assert!(harness.query_by_label(settings_view::FORGET_KEY).is_none());
        assert_eq!(harness.state().shell.view.nav.current(), &PageRequest::Home);
    }

    /// Deferred Task 29 minor: one rule closes the settings, any navigation: a row of the
    /// right panel's queue, Back and Forward, as well as the sidebar and the bar.
    /// Coming back to the page they were opened over doesn't reopen them.
    #[test]
    fn any_navigation_closes_the_settings() {
        let click_gear = |harness: &Harness<'static, Wired>| {
            let gear = egui::accesskit::Role::Button;
            harness
                .get_by_role_and_label(gear, sidebar::SETTINGS)
                .click();
        };
        let view = ViewState {
            panel: Some(PanelTab::Queue),
            ..ViewState::default()
        };
        let mut state = engine_state();
        state.queue = vec![NowPlaying {
            uri: "spotify:track:next".into(),
            title: "Census".into(),
            artists: vec!["Jane Remover".into()],
            artist_uris: vec!["spotify:artist:jane".into()],
            album: None,
            album_uri: None,
            duration_ms: Some(150_000),
            cover_url: None,
        }];
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1400.0, 800.0))
            .allow_missing_glyphs()
            .build_ui_state(
                |ui, w: &mut Wired| w.shell.draw(ui, &w.link, Instant::now()),
                Wired {
                    shell: shell(view),
                    link: FakeLink::default(),
                },
            );
        link_msg(&mut harness, LinkMsg::Connected(Box::new(state)));
        harness.run();
        click_gear(&harness);
        harness.run();
        harness.get_by_label(settings_view::FORGET_KEY);

        // A row of the right panel's queue: its Go to artist.
        harness.get_by_label("Census").click_secondary();
        harness.run();
        harness.get_by_label(track_list::GO_TO_ARTIST).click();
        harness.run();
        let jane = PageRequest::Artist {
            uri: "spotify:artist:jane".into(),
        };
        assert_eq!(harness.state().shell.view.nav.current(), &jane);
        assert!(harness.query_by_label(settings_view::FORGET_KEY).is_none());
        // Back where they were opened: still closed.
        assert!(harness.state_mut().shell.view.nav.back());
        harness.run();
        assert_eq!(harness.state().shell.view.nav.current(), &PageRequest::Home);
        assert!(harness.query_by_label(settings_view::FORGET_KEY).is_none());

        // Opened again, Forward closes them.
        click_gear(&harness);
        harness.run();
        harness.get_by_label(settings_view::FORGET_KEY);
        assert!(harness.state_mut().shell.view.nav.forward());
        harness.run();
        assert!(harness.query_by_label(settings_view::FORGET_KEY).is_none());
    }

    /// The whole window around `shell`. The bar's symbols draw as boxes in egui's own
    /// fonts, which is all these tests have: they check what is wired, not glyphs.
    fn window(shell: Shell) -> Harness<'static, Wired> {
        Harness::builder().allow_missing_glyphs().build_ui_state(
            |ui, w: &mut Wired| w.shell.draw(ui, &w.link, Instant::now()),
            Wired {
                shell,
                link: FakeLink::default(),
            },
        )
    }

    /// The bottom bar draws from the engine's state as the shell keeps it: Soloist's
    /// health, the queue and the player's events reach it, its failures reach the line
    /// above it, and its links open pages.
    #[test]
    fn bottom_bar_is_wired() {
        let nerve = NowPlaying {
            uri: "spotify:track:nerve".into(),
            title: "nerve".into(),
            artists: vec!["2hollis".into()],
            artist_uris: vec!["spotify:artist:2hollis".into()],
            album: Some("boy".into()),
            album_uri: Some("spotify:album:boy".into()),
            duration_ms: Some(150_000),
            cover_url: None,
        };
        let mut state = engine_state();
        state.player.track = Some(nerve.clone());
        state.player.status = PlayStatus::Playing;
        state.player.actions = Actions {
            play: true,
            pause: true,
            ..Actions::default()
        };
        let mut harness = window(shell(ViewState::default()));
        let w = harness.state_mut();
        w.shell
            .on_link(&w.link, LinkMsg::Connected(Box::new(state)), Instant::now());
        harness.run();
        harness.state().link.take();

        harness.get_by_label(bottom_bar::PAUSE).click();
        harness.run();
        let w = harness.state_mut();
        assert_eq!(w.link.take(), vec![Request::Pause]);
        let failed = LinkMsg::Reply {
            id: w.link.last_id.get(),
            result: Err("The player is restarting.".into()),
        };
        assert!(!w.shell.on_link(&w.link, failed, Instant::now()));
        harness.run();
        harness.get_by_label("Couldn't pause: The player is restarting.");
        harness.get_by_label(bottom_bar::PAUSE);
        // Trying again is the owner's whole action: it goes after a while (carry 1).
        assert!(harness.state().shell.notices.errors().is_empty());

        // The engine's events: the player paused, Soloist restarting, the queue.
        let w = harness.state_mut();
        let mut paused = w.shell.player.clone();
        paused.status = PlayStatus::Paused;
        let events = [
            Event::Player(paused),
            Event::Soloist(SoloistHealth::Starting),
            Event::Queue(vec![nerve]),
        ];
        for event in events {
            w.shell
                .on_link(&w.link, LinkMsg::Event(event), Instant::now());
        }
        assert_eq!(w.shell.queue.len(), 1);
        harness.run();
        harness.get_by_label(bottom_bar::RECONNECTING_PLAYER);
        assert!(
            harness
                .get_by_label(bottom_bar::PLAY)
                .accesskit_node()
                .is_disabled()
        );

        harness.get_by_label("nerve").click();
        harness.run();
        let album = PageRequest::Album {
            uri: "spotify:album:boy".into(),
        };
        assert_eq!(harness.state().shell.view.nav.current(), &album);
        harness.get_by_label("2hollis").click();
        harness.run();
        let artist = PageRequest::Artist {
            uri: "spotify:artist:2hollis".into(),
        };
        assert_eq!(harness.state().shell.view.nav.current(), &artist);
    }

    /// The bar's toggle opens the right panel (on the queue, the engine's) and closes it;
    /// the lyrics are asked for, their answer reaching the panel; the toggle reopens it on
    /// the tab it last showed.
    #[test]
    fn right_panel_from_the_bar() {
        let track = |uri: &str, title: &str| NowPlaying {
            uri: uri.into(),
            title: title.into(),
            artists: vec!["Jane Remover".into()],
            artist_uris: vec!["spotify:artist:jane".into()],
            album: None,
            album_uri: None,
            duration_ms: Some(150_000),
            cover_url: None,
        };
        let mut state = engine_state();
        state.player.track = Some(track("spotify:track:nerve", "nerve"));
        state.queue = vec![track("spotify:track:next", "Census")];
        let mut harness = window(shell(ViewState::default()));
        let w = harness.state_mut();
        w.shell
            .on_link(&w.link, LinkMsg::Connected(Box::new(state)), Instant::now());
        harness.run();
        harness.state().link.take();
        assert!(harness.query_by_label(side_panel::QUEUE).is_none());
        let toggle = |harness: &Harness<'_, Wired>| {
            harness
                .get_by_label(bottom_bar::PANEL)
                .accesskit_node()
                .toggled()
        };
        assert_eq!(toggle(&harness), Some(Toggled::False));

        harness.get_by_label(bottom_bar::PANEL).click();
        harness.run();
        assert_eq!(harness.state().shell.view.panel, Some(PanelTab::Queue));
        assert_eq!(toggle(&harness), Some(Toggled::True));
        harness.get_by_label("Census");

        harness.get_by_label(side_panel::LYRICS).click();
        harness.run();
        let w = harness.state_mut();
        assert_eq!(w.shell.view.panel, Some(PanelTab::Lyrics));
        let lyrics = Request::Lyrics {
            track_uri: "spotify:track:nerve".into(),
        };
        assert_eq!(w.link.take().last(), Some(&lyrics));
        let answer = LinkMsg::Reply {
            id: w.link.last_id.get(),
            result: Ok(Reply::Lyrics(None)),
        };
        assert!(!w.shell.on_link(&w.link, answer, Instant::now()));
        harness.run();
        harness.get_by_label(side_panel::NO_LYRICS);

        harness.get_by_label(bottom_bar::PANEL).click();
        harness.run();
        assert_eq!(harness.state().shell.view.panel, None);
        assert!(harness.query_by_label(side_panel::LYRICS).is_none());
        harness.get_by_label(bottom_bar::PANEL).click();
        harness.run();
        assert_eq!(harness.state().shell.view.panel, Some(PanelTab::Lyrics));
        assert_eq!(harness.state().link.take(), vec![], "kept for the session");
    }

    /// A page row's `Show credits` while the Credits tab is open already: the frame that
    /// picks the track asks for the next, which shows its credits. egui also repaints
    /// after any frame with input, so this checks the outcome rather than the window's
    /// own guard alone.
    #[test]
    fn show_credits_while_credits_open() {
        let mut view = ViewState::default();
        view.nav.go(PageRequest::Liked);
        view.panel = Some(PanelTab::Credits);
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .allow_missing_glyphs()
            .build_ui_state(
                |ui, w: &mut Wired| w.shell.draw(ui, &w.link, Instant::now()),
                Wired {
                    shell: shell(view),
                    link: FakeLink::default(),
                },
            );
        let mut state = engine_state();
        state.player.track = Some(NowPlaying {
            uri: "spotify:track:nerve".into(),
            title: "nerve".into(),
            artists: vec![],
            artist_uris: vec![],
            album: None,
            album_uri: None,
            duration_ms: None,
            cover_url: None,
        });
        let w = harness.state_mut();
        w.shell
            .on_link(&w.link, LinkMsg::Connected(Box::new(state)), Instant::now());
        let liked_id = w.link.last_id.get() - 1;
        let alpha = "spotify:track:alpha";
        let liked = Page::Liked(LikedPage {
            total: 1,
            tracks: vec![TrackRow {
                uri: alpha.into(),
                uid: None,
                title: "Alpha".into(),
                artists: vec![],
                album: None,
                duration_ms: 60_000,
                images: vec![],
                added_at: None,
            }],
        });
        let answer = LinkMsg::Reply {
            id: liked_id,
            result: Ok(Reply::Page(reply(liked))),
        };
        w.shell.on_link(&w.link, answer, Instant::now());
        harness.run();
        let credits = |uri: &str| Request::Credits {
            track_uri: uri.into(),
        };
        assert!(
            harness
                .state()
                .link
                .take()
                .contains(&credits("spotify:track:nerve"))
        );

        harness.get_by_label("Alpha").click_secondary();
        harness.run();
        harness.get_by_label(track_list::SHOW_CREDITS).click();
        harness.step();
        assert_eq!(harness.state().shell.views.credits.as_deref(), Some(alpha));
        let delay = harness.output().viewport_output[&egui::ViewportId::ROOT].repaint_delay;
        assert_eq!(delay, Duration::ZERO);
        harness.run();
        assert_eq!(harness.state().link.take(), vec![credits(alpha)]);
    }

    /// The engine's state and events reach the pages: the account's name, the player,
    /// and a heart changed elsewhere.
    #[test]
    fn engine_events_reach_the_pages() {
        let mut shell = shell(ViewState::default());
        let link = FakeLink::default();
        let mut state = engine_state();
        state.username = Some("dannycake".into());
        shell.on_link(&link, LinkMsg::Connected(Box::new(state)), Instant::now());
        assert_eq!(shell.username.as_deref(), Some("dannycake"));

        let mut player = engine_state().player;
        player.context_uri = Some("spotify:album:a".into());
        let event = LinkMsg::Event(Event::Player(player.clone()));
        shell.on_link(&link, event, Instant::now());
        assert_eq!(shell.player, player);

        let liked = LinkMsg::Event(Event::LikedChanged {
            uri: "spotify:track:t".into(),
            liked: true,
        });
        shell.on_link(&link, liked, Instant::now());
        assert_eq!(shell.views.liked_set.get("spotify:track:t"), Some(true));

        // Final review I1: connected before the login, the window hears the name later.
        shell.on_link(
            &link,
            LinkMsg::Connected(Box::new(engine_state())),
            Instant::now(),
        );
        assert_eq!(shell.username, None);
        let name = LinkMsg::Event(Event::Username("dannycake".into()));
        shell.on_link(&link, name, Instant::now());
        assert_eq!(shell.username.as_deref(), Some("dannycake"));
    }

    /// Final review C1, I1 and I4, the window's half of "the session gets ready after
    /// the window's Welcome" (the window started the engine, or the engine restarted
    /// under it): the account's name arrives; the pages that failed, or whose refresh
    /// failed, for want of the session are asked for again once it is ready; the hearts
    /// are asked about again once the engine has read Liked Songs; and a heart clicked
    /// before that likes, never unlikes.
    #[test]
    fn session_ready_after_the_window_connected() {
        let playlist = PageRequest::Playlist {
            uri: "spotify:playlist:p".into(),
        };
        let mut view = ViewState::default();
        view.nav.go(playlist.clone());
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .allow_missing_glyphs()
            .build_ui_state(
                |ui, w: &mut Wired| w.shell.draw(ui, &w.link, Instant::now()),
                Wired {
                    shell: shell(view),
                    link: FakeLink::default(),
                },
            );
        let mut state = engine_state();
        state.session = SessionHealth::Connecting;
        link_msg(&mut harness, LinkMsg::Connected(Box::new(state)));
        assert_eq!(
            harness.state().link.take(),
            [page(playlist.clone()), page(PageRequest::Sidebar)]
        );
        let sidebar_id = harness.state().link.last_id.get();
        // The sidebar fails for want of the session; the playlist is the engine's cached
        // copy, whose refresh fails the same way.
        let result = Err("not logged in".into());
        link_msg(
            &mut harness,
            LinkMsg::Reply {
                id: sidebar_id,
                result,
            },
        );
        let t = "spotify:track:t";
        let cached = |error: Option<&str>| PageReply {
            page: Page::Playlist(PlaylistPage {
                uri: "spotify:playlist:p".into(),
                name: "Main".into(),
                description: String::new(),
                images: vec![],
                total: 1,
                tracks: vec![TrackRow {
                    uri: t.into(),
                    uid: None,
                    title: "Alpha".into(),
                    artists: vec![],
                    album: None,
                    duration_ms: 60_000,
                    images: vec![],
                    added_at: None,
                }],
            }),
            fetched_at_ms: 1,
            stale: true,
            error: error.map(str::to_owned),
        };
        let result = Ok(Reply::Page(cached(None)));
        link_msg(
            &mut harness,
            LinkMsg::Reply {
                id: sidebar_id - 1,
                result,
            },
        );
        event(
            &mut harness,
            Event::PageUpdated {
                req: playlist.clone(),
                reply: cached(Some("not logged in")),
            },
        );
        harness.run();
        let ask = || Request::LikedState {
            uris: vec![t.into()],
        };
        assert_eq!(harness.state().link.take(), [ask()]);
        let id = harness.state().link.last_id.get();
        let result = Ok(Reply::LikedState(vec![None]));
        link_msg(&mut harness, LinkMsg::Reply { id, result });
        harness.run();
        // Not known yet: drawn as not liked, and a click likes.
        harness.get_by_label(track_list::HEART_LABEL).click();
        harness.run();
        assert_eq!(
            harness.state().link.take(),
            [Request::SetLiked {
                uri: t.into(),
                liked: true
            }]
        );

        // The session is ready: its updates come in the supervisor's order.
        event(&mut harness, Event::Login(LoginStep::LoggedIn));
        event(&mut harness, Event::Username("dannycake".into()));
        event(&mut harness, Event::Session(SessionHealth::Ready));
        assert_eq!(harness.state().shell.username.as_deref(), Some("dannycake"));
        assert_eq!(
            harness.state().link.take(),
            [page(playlist), page(PageRequest::Sidebar)],
            "what failed is asked for again"
        );
        event(&mut harness, Event::Session(SessionHealth::Ready));
        assert_eq!(harness.state().link.take(), [], "once");

        // Liked Songs read: the hearts are asked about again.
        event(&mut harness, Event::LikedSetReady);
        harness.run();
        assert_eq!(harness.state().link.take(), [ask()]);
    }

    /// The window as the owner sees it: the sidebar and the current page's state, from
    /// what the link brings; Connected asks for the current page and the playlists.
    #[test]
    fn window_shows_sidebar_and_page_state() {
        let mut view = ViewState::default();
        view.nav.go(PageRequest::Liked);
        let mut harness = window(shell(view));
        harness.run();
        // Before the engine answers: the sidebar, and the page and the playlists loading.
        harness.get_by_label("Home");
        assert_eq!(harness.query_all_by_label(LOADING).count(), 2);
        assert_eq!(harness.state().link.take(), vec![]);

        let w = harness.state_mut();
        let connected = LinkMsg::Connected(Box::new(engine_state()));
        assert!(!w.shell.on_link(&w.link, connected, Instant::now()));
        assert_eq!(
            w.link.take(),
            vec![page(PageRequest::Liked), page(PageRequest::Sidebar)]
        );
        let liked_id = w.link.last_id.get() - 1;
        let liked = Page::Liked(LikedPage {
            total: 0,
            tracks: vec![],
        });
        let reply_msg = LinkMsg::Reply {
            id: liked_id,
            result: Ok(Reply::Page(reply(liked))),
        };
        w.shell.on_link(&w.link, reply_msg, Instant::now());
        let rootlist = Page::Sidebar(vec![RootEntry::Playlist(PlaylistRef {
            uri: "spotify:playlist:a".into(),
            name: "Morning".into(),
        })]);
        let updated = LinkMsg::Event(Event::PageUpdated {
            req: PageRequest::Sidebar,
            reply: reply(rootlist),
        });
        w.shell.on_link(&w.link, updated, Instant::now());
        harness.run();
        assert!(harness.query_by_label(LOADING).is_none());
        // The sidebar's entry, and the page's title.
        assert_eq!(harness.query_all_by_label("Liked Songs").count(), 2);
        harness.get_by_label(pages::NO_SONGS);

        harness.get_by_label("Morning").click();
        harness.run();
        let playlist = PageRequest::Playlist {
            uri: "spotify:playlist:a".into(),
        };
        assert_eq!(harness.state().shell.view.nav.current(), &playlist);
        assert_eq!(harness.state().link.take(), vec![page(playlist)]);
        harness.get_by_label(LOADING);

        // Back to Liked Songs, loaded: asked for once more (the engine refreshes only
        // when asked), its copy on screen meanwhile.
        harness.get_by_label("Liked Songs").click();
        harness.run();
        assert_eq!(
            harness.state().shell.view.nav.current(),
            &PageRequest::Liked
        );
        assert_eq!(harness.state().link.take(), vec![page(PageRequest::Liked)]);
        assert_eq!(harness.query_all_by_label("Liked Songs").count(), 2);
        harness.run();
        assert_eq!(harness.state().link.take(), vec![], "once per landing");

        // The engine closing the window is reported, for the view to be saved.
        let w = harness.state_mut();
        let close = LinkMsg::Event(Event::CloseWindow);
        assert!(w.shell.on_link(&w.link, close, Instant::now()));
    }

    /// Final review I2 (and the Task 27 carry): with the engine gone the music has
    /// stopped, Soloist dying with it. The window shows the player paused where it had
    /// got to, its clock no longer counting, and its controls off until it is back.
    #[test]
    fn engine_gone_shows_the_player_stopped() {
        let mut state = engine_state();
        state.player.track = Some(NowPlaying {
            uri: "spotify:track:nerve".into(),
            title: "nerve".into(),
            artists: vec![],
            artist_uris: vec![],
            album: None,
            album_uri: None,
            duration_ms: Some(150_000),
            cover_url: None,
        });
        state.player.status = PlayStatus::Playing;
        state.player.actions = Actions {
            play: true,
            pause: true,
            seek: true,
            ..Actions::default()
        };
        let at = bottom_bar::now_ms();
        state.player.position = Some(playback::event::Position {
            position_ms: 10_000,
            timestamp_ms: at - 5_000,
            speed: 1.0,
        });
        let mut harness = window(shell(ViewState::default()));
        link_msg(&mut harness, LinkMsg::Connected(Box::new(state)));
        harness.run();
        assert!(
            !harness
                .get_by_label(bottom_bar::PAUSE)
                .accesskit_node()
                .is_disabled()
        );

        link_msg(&mut harness, LinkMsg::Disconnected);
        harness.run();
        let shell = &harness.state().shell;
        assert_eq!(shell.player.status, PlayStatus::Paused);
        assert_eq!(shell.soloist, None, "Soloist's health is unknown");
        let now = Instant::now();
        let (later, playing) = shell.bar.shown_position(&shell.player, now, at + 60_000);
        assert!(!playing);
        let (here, _) = shell.bar.shown_position(&shell.player, now, at);
        assert_eq!(later, here, "the clock stopped");
        assert!(here.is_some_and(|ms| ms >= 15_000), "{here:?}");
        assert!(
            harness
                .get_by_label(bottom_bar::PLAY)
                .accesskit_node()
                .is_disabled()
        );
    }

    /// Review Focus 1, the window's half: the engine goes, the banner shows over the
    /// window; it comes back, the banner goes.
    #[test]
    fn banner_while_reconnecting() {
        let t0 = Instant::now();
        let mut harness = Harness::new_ui_state(
            |ui, connection: &mut Connection| banner(ui, connection, Instant::now()),
            Connection::new(t0),
        );
        harness.state_mut().connected();
        harness.step();
        assert!(harness.query_by_label(RECONNECTING).is_none());

        harness.state_mut().disconnected(Instant::now());
        harness.step();
        harness.get_by_label(RECONNECTING);

        harness.state_mut().connected();
        harness.step();
        assert!(harness.query_by_label(RECONNECTING).is_none());
    }

    /// Task 30: only `LOUDAUTO_DEBUG_REPAINT=1` turns the frame counter on.
    #[test]
    fn debug_repaint_only_on_one() {
        let with = |value: Option<&'static str>| {
            debug_repaint_enabled(move |name| {
                assert_eq!(name, "LOUDAUTO_DEBUG_REPAINT");
                value.map(String::from)
            })
        };
        assert!(with(Some("1")));
        for off in [
            None,
            Some(""),
            Some("0"),
            Some("true"),
            Some("yes"),
            Some(" 1"),
        ] {
            assert!(!with(off), "{off:?}");
        }
    }

    #[test]
    fn frame_counter_counts_until_taken() {
        let counter = FrameCounter::default();
        assert_eq!(counter.take(), 0);
        for _ in 0..3 {
            counter.tick();
        }
        assert_eq!(counter.take(), 3);
        assert_eq!(counter.take(), 0);
        counter.tick();
        assert_eq!(counter.take(), 1);
    }

    /// Task 30: the counter's thread logs the frames of each period, an idle one as 0,
    /// and ends once the window has dropped the counter.
    #[test]
    fn frame_log_says_each_period_and_idle_as_zero() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (frames, thread) = FrameLog::start(Duration::from_millis(50), move |line| {
            let _ = tx.send(line);
        })
        .expect("the frame counter's thread starts");
        frames.frame();
        frames.frame();
        let lines: Vec<String> = rx.iter().take(3).collect();
        // The two frames land in the first period or, on a slow machine, the second.
        let counted: u64 = lines
            .iter()
            .map(|line| {
                line.strip_prefix("frames last 10 s: ")
                    .and_then(|n| n.parse::<u64>().ok())
                    .unwrap_or_else(|| panic!("{line:?}"))
            })
            .sum();
        assert_eq!(counted, 2, "{lines:?}");
        assert_eq!(lines[2], "frames last 10 s: 0", "{lines:?}");
        drop(frames);
        thread.join().expect("the thread ends without a panic");
    }

    /// Off, there is no counter and no thread.
    #[test]
    fn frame_log_off_is_nothing() {
        let frames = FrameLog::off();
        assert!(frames.counter.is_none());
        frames.frame();
    }

    /// Task 30: counting and logging never ask egui for a frame: a pass that counts asks
    /// for the same repaint as one that doesn't, and while the thread logs, nothing asks.
    #[test]
    fn frame_counter_never_asks_for_a_repaint() {
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pass = |frames: &FrameLog| {
            let ctx = egui::Context::default();
            let seen = requests.clone();
            ctx.set_repaint_observer(move |_| {
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            });
            requests.store(0, std::sync::atomic::Ordering::SeqCst);
            let output = ctx.run_ui(egui::RawInput::default(), |ui| {
                frames.frame();
                ui.label("LOUDAUTO");
            });
            let delay = output.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
            output.drop_without_applying_deltas();
            let asked = requests.load(std::sync::atomic::Ordering::SeqCst);
            (ctx, delay, asked)
        };

        let (ctx_off, delay_off, asked_off) = pass(&FrameLog::off());

        let (tx, rx) = std::sync::mpsc::channel();
        let (frames, thread) = FrameLog::start(Duration::from_millis(20), move |line| {
            let _ = tx.send(line);
        })
        .expect("the frame counter's thread starts");
        let (ctx, delay_on, asked_on) = pass(&frames);
        assert_eq!(delay_on, delay_off);
        assert_eq!(asked_on, asked_off);

        // The thread logs twice after the pass; still nothing asks for a frame.
        requests.store(0, std::sync::atomic::Ordering::SeqCst);
        let lines: Vec<String> = rx.iter().take(2).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(ctx.has_requested_repaint(), ctx_off.has_requested_repaint());
        drop(frames);
        thread.join().expect("the thread ends without a panic");
    }

    #[test]
    fn first_connection_is_quiet_at_first() {
        let t0 = Instant::now();
        let mut connection = Connection::new(t0);
        assert_eq!(connection.banner(t0), Banner::Later(CONNECT_GRACE));
        assert_eq!(
            connection.banner(t0 + CONNECT_GRACE / 2),
            Banner::Later(CONNECT_GRACE / 2)
        );
        assert_eq!(
            connection.banner(t0 + CONNECT_GRACE),
            Banner::Show(CONNECTING)
        );
        connection.connected();
        assert_eq!(connection.banner(t0 + CONNECT_GRACE), Banner::None);
        // Once connected, a disconnect says so at once.
        connection.disconnected(t0 + CONNECT_GRACE);
        assert_eq!(
            connection.banner(t0 + CONNECT_GRACE),
            Banner::Show(RECONNECTING)
        );
    }
}
