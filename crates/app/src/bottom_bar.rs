//! The bottom bar, always on screen: the cover, the title (a link to its album) over the
//! artists (each a link to its page), the heart, shuffle, previous, play/pause, next and
//! repeat, the seek bar with the elapsed and total time, the volume, and the quality
//! badge. It stays low in brightness for OLED: the surface grey, secondary text and quiet
//! controls but the title; the cover art is the only bright thing in it.
//!
//! The controls follow the engine's `actions`, and are disabled while Soloist isn't
//! ready. A click shows its outcome at once ([`Optimistic`]): the wanted state stands
//! until the engine reports it, the request fails (reverted, and said so), or
//! [`HOLD`] passes. While another device plays, the bar says so and the controls act on
//! that device (the engine routes them).
//!
//! It repaints on its own only while playing, once a second for the clock; and once
//! when a hold runs out or a held-back volume may go.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontId, Id, Painter, Pos2, Rect, Response, Role, Sense,
    Shape, Stroke, StrokeKind, TextStyle, Ui, Vec2, WidgetInfo, pos2, vec2,
};
use engine_ipc::{PlayerState, Reply, Request, SoloistHealth};
use playback::{
    command::Repeat,
    event::{NowPlaying, PlayStatus, Position, StreamQuality},
};
use spot_core::pages::Image;

use crate::{
    art::Art,
    link::{RequestId, Requests},
    notices::failure_text,
    theme,
    track_list::{HEART_LABEL, duration, galley, is_page, paint_link},
};

/// The bar's height, its margins included.
pub const HEIGHT: f32 = 72.0;
/// The cover, in points.
pub const COVER: f32 = 56.0;
/// How long a click's outcome is shown without the engine reporting it.
pub const HOLD: Duration = Duration::from_secs(3);
/// The volume slider sends `SetVolume` at most this often.
pub const VOLUME_EVERY: Duration = Duration::from_millis(100);
/// While playing, the seek bar and the time are redrawn this often.
pub const REPAINT: Duration = Duration::from_secs(1);

pub const PLAY: &str = "Play";
pub const PAUSE: &str = "Pause";
pub const PREVIOUS: &str = "Previous";
pub const NEXT: &str = "Next";
pub const SHUFFLE: &str = "Shuffle";
pub const REPEAT: &str = "Repeat";
pub const SEEK: &str = "Seek";
pub const VOLUME: &str = "Volume";
/// The toggle for the right panel; toggled while it is open.
pub const PANEL: &str = "Queue, lyrics and credits";
/// While Soloist isn't ready.
pub const RECONNECTING_PLAYER: &str = "Reconnecting player…";
/// While another device has playback.
pub const ELSEWHERE: &str = "Playing on another device";

/// The glyphs set in type; shuffle and repeat are drawn (no font here has them in one
/// colour), as is the speaker.
const PLAY_GLYPH: &str = "▶";
const PAUSE_GLYPH: &str = "⏸";
const PREVIOUS_GLYPH: &str = "⏮";
const NEXT_GLYPH: &str = "⏭";
const LIKED_GLYPH: &str = "♥";
const NOT_LIKED_GLYPH: &str = "♡";

/// A control's square, in points: grown with the icons (Look 2), as far as the bar's
/// height leaves room for over the seek bar.
const BUTTON: f32 = 36.0;
/// Between the transport controls.
const SPACING: f32 = 12.0;
/// Between the cover, the names and the heart.
const GAP: f32 = 12.0;
/// A slider's height to grab, and its rail's.
const SLIDER: f32 = 16.0;
const RAIL: f32 = 4.0;
/// The seek bar at its widest.
const SEEK_MAX: f32 = 560.0;
/// The room for each time beside the seek bar.
const TIME: f32 = 44.0;
const VOLUME_WIDTH: f32 = 96.0;
/// Codecs that are lossless: their badge gives the sample rate, the others' the bitrate.
const LOSSLESS: [&str; 5] = ["flac", "alac", "wav", "pcm", "aiff"];

/// What the owner did in the bar that the window carries out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BarAction {
    /// Open this album's page (the title).
    GoAlbum(String),
    /// Open this artist's page (an artist's name).
    GoArtist(String),
    /// Open or close the right panel.
    TogglePanel,
}

/// What a request from the bar was: what to revert, and what to say, should it fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flip {
    Play,
    Pause,
    Next,
    Previous,
    Shuffle(bool),
    Repeat(Repeat),
    Seek,
    Volume,
    Heart,
}

impl Flip {
    /// What it was, for `Couldn't <what>: …`.
    pub fn what(&self) -> &'static str {
        match self {
            Flip::Play => "play",
            Flip::Pause => "pause",
            Flip::Next => "skip to the next track",
            Flip::Previous => "go back to the previous track",
            Flip::Shuffle(true) => "turn shuffle on",
            Flip::Shuffle(false) => "turn shuffle off",
            Flip::Repeat(Repeat::Off) => "turn repeat off",
            Flip::Repeat(Repeat::Context) => "turn repeat on",
            Flip::Repeat(Repeat::Track) => "repeat the track",
            Flip::Seek => "seek",
            Flip::Volume => "change the volume",
            Flip::Heart => "like",
        }
    }
}

/// The state the owner asked for, shown in place of the engine's until the engine
/// reports it, the request fails, or [`HOLD`] passes.
#[derive(Debug, Default)]
pub struct Optimistic {
    status: Option<(PlayStatus, Instant)>,
    shuffle: Option<(bool, Instant)>,
    repeat: Option<(Repeat, Instant)>,
    volume: Option<(u8, Instant)>,
    heart: Option<Heart>,
}

/// The heart the owner clicked: held until the engine answers (a write to Spotify can
/// take a while), then for [`HOLD`] at most.
#[derive(Debug)]
struct Heart {
    /// The track it is for.
    uri: String,
    liked: bool,
    /// The click, then the answer.
    at: Instant,
    answered: bool,
}

/// How much longer a hold from `at` stands at `now`; `None` once it has run out.
fn hold_left(at: Instant, now: Instant) -> Option<Duration> {
    HOLD.checked_sub(now.saturating_duration_since(at))
        .filter(|left| !left.is_zero())
}

/// The wanted value while it holds, else `real`.
fn shown<T: Clone>(want: &Option<(T, Instant)>, real: &T, now: Instant) -> T {
    match want {
        Some((value, at)) if hold_left(*at, now).is_some() => value.clone(),
        _ => real.clone(),
    }
}

/// The want is dropped once the engine reports it.
fn settle<T: PartialEq>(want: &mut Option<(T, Instant)>, real: &T) {
    if want.as_ref().is_some_and(|(value, _)| value == real) {
        *want = None;
    }
}

impl Optimistic {
    pub fn want_status(&mut self, status: PlayStatus, now: Instant) {
        self.status = Some((status, now));
    }

    pub fn want_shuffle(&mut self, on: bool, now: Instant) {
        self.shuffle = Some((on, now));
    }

    pub fn want_repeat(&mut self, mode: Repeat, now: Instant) {
        self.repeat = Some((mode, now));
    }

    pub fn want_volume(&mut self, volume: u8, now: Instant) {
        self.volume = Some((volume, now));
    }

    /// The heart of track `uri` clicked to `liked`.
    pub fn want_heart(&mut self, uri: String, liked: bool, now: Instant) {
        self.heart = Some(Heart {
            uri,
            liked,
            at: now,
            answered: false,
        });
    }

    /// The status to show: the wanted one until the engine reports it, its request
    /// fails, or [`HOLD`] passes; else `real`.
    pub fn displayed_status(&self, real: &PlayStatus, now: Instant) -> PlayStatus {
        shown(&self.status, real, now)
    }

    pub fn displayed_shuffle(&self, real: bool, now: Instant) -> bool {
        shown(&self.shuffle, &real, now)
    }

    pub fn displayed_repeat(&self, real: Repeat, now: Instant) -> Repeat {
        shown(&self.repeat, &real, now)
    }

    pub fn displayed_volume(&self, real: u8, now: Instant) -> u8 {
        shown(&self.volume, &real, now)
    }

    /// Whether the current track shows liked: the click until the engine answers, the
    /// answer for [`HOLD`] at most; else the engine's word.
    pub fn displayed_liked(&self, player: &PlayerState, now: Instant) -> Option<bool> {
        let current = player.track.as_ref().map(|t| t.uri.as_str());
        match &self.heart {
            Some(heart)
                if current == Some(heart.uri.as_str())
                    && (!heart.answered || hold_left(heart.at, now).is_some()) =>
            {
                Some(heart.liked)
            }
            _ => player.liked,
        }
    }

    /// The engine's word: what it now reports is no longer held. A heart for a track no
    /// longer playing is dropped.
    pub fn on_player(&mut self, player: &PlayerState) {
        settle(&mut self.status, &player.status);
        settle(&mut self.shuffle, &player.shuffle);
        settle(&mut self.repeat, &player.repeat);
        settle(&mut self.volume, &player.volume);
        if let Some(heart) = &self.heart {
            let current = player.track.as_ref().map(|t| t.uri.as_str());
            if current != Some(heart.uri.as_str()) || player.liked == Some(heart.liked) {
                self.heart = None;
            }
        }
    }

    /// The heart's answer: `uri` is now `liked`, shown so for [`HOLD`] at most.
    pub fn on_liked(&mut self, uri: String, liked: bool, now: Instant) {
        match &mut self.heart {
            Some(heart) if heart.uri == uri => {
                heart.liked = liked;
                heart.at = now;
                heart.answered = true;
            }
            _ => self.heart = None,
        }
    }

    /// The request `flip` failed with `err`: its flip is reverted, and this is what to
    /// tell the owner, `Couldn't <what>: <err>` (or the engine's own `Couldn't …`).
    pub fn on_failure(&mut self, flip: &Flip, err: &str) -> String {
        match flip {
            Flip::Play | Flip::Pause => self.status = None,
            Flip::Shuffle(_) => self.shuffle = None,
            Flip::Repeat(_) => self.repeat = None,
            Flip::Volume => self.volume = None,
            Flip::Heart => self.heart = None,
            Flip::Next | Flip::Previous | Flip::Seek => {}
        }
        failure_text(flip.what(), err)
    }

    /// How long until the next hold runs out, for a repaint then.
    pub fn next_expiry(&self, now: Instant) -> Option<Duration> {
        let heart = self.heart.as_ref().filter(|h| h.answered).map(|h| h.at);
        [
            self.status.as_ref().map(|w| w.1),
            self.shuffle.map(|w| w.1),
            self.repeat.map(|w| w.1),
            self.volume.map(|w| w.1),
            heart,
        ]
        .into_iter()
        .flatten()
        .filter_map(|at| hold_left(at, now))
        .min()
    }
}

/// A position the bar keeps itself while the engine's anchor is stale: the engine
/// reported a change of status (a pause, say) without a fresh position, or the owner
/// sought or clicked play or pause and the engine hasn't answered with one yet.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Held {
    /// The engine's anchor this stands in for: once the engine sends another, it knows
    /// better.
    pub stale: Option<Position>,
    pub position_ms: u64,
    /// Unix milliseconds.
    pub at_ms: i64,
}

/// Where playback is at `now_ms` (Unix milliseconds), `playing` being whether it shows
/// playing: from `held` while the engine's anchor is the one it stands in for, else from
/// `anchor`; moving on only while playing.
pub fn position_at(
    anchor: Option<&Position>,
    held: Option<&Held>,
    playing: bool,
    now_ms: i64,
) -> Option<u64> {
    match held.filter(|h| h.stale.as_ref() == anchor) {
        Some(h) if playing => {
            let since = u64::try_from(now_ms.saturating_sub(h.at_ms)).unwrap_or(0);
            Some(h.position_ms.saturating_add(since))
        }
        Some(h) => Some(h.position_ms),
        None if playing => anchor.map(|a| a.at(now_ms)),
        None => anchor.map(|a| a.position_ms),
    }
}

/// What the bar holds once the engine's state goes from `before` to `after` at `now_ms`,
/// `was` and `is` being whether it showed playing before and shows playing after. A fresh
/// anchor (or another track) drops the hold; a change of status with the same anchor
/// holds where playback had got to, so a pause doesn't jump back to the old anchor.
pub fn held_after(
    before: &PlayerState,
    after: &PlayerState,
    held: Option<Held>,
    was: bool,
    is: bool,
    now_ms: i64,
) -> Option<Held> {
    let uri = |p: &PlayerState| p.track.as_ref().map(|t| t.uri.clone());
    if uri(before) != uri(after) || before.position != after.position {
        return None;
    }
    if was == is {
        return held;
    }
    let position_ms = position_at(before.position.as_ref(), held.as_ref(), was, now_ms)?;
    Some(Held {
        stale: after.position,
        position_ms,
        at_ms: now_ms,
    })
}

/// Lets a value through at most every [`VOLUME_EVERY`]; the latest one held back goes
/// once it may.
#[derive(Debug, Default)]
struct Throttle {
    last: Option<Instant>,
    waiting: Option<u8>,
}

impl Throttle {
    fn open(&self, now: Instant) -> bool {
        self.last
            .is_none_or(|last| now.saturating_duration_since(last) >= VOLUME_EVERY)
    }

    /// `value` was asked for at `now`: what to send now, if anything.
    fn offer(&mut self, value: u8, now: Instant) -> Option<u8> {
        if self.open(now) {
            self.last = Some(now);
            self.waiting = None;
            Some(value)
        } else {
            self.waiting = Some(value);
            None
        }
    }

    /// The value held back, once it may go.
    fn due(&mut self, now: Instant) -> Option<u8> {
        if self.waiting.is_some() && self.open(now) {
            self.last = Some(now);
            self.waiting.take()
        } else {
            None
        }
    }

    /// How long until the value held back may go.
    fn wait(&self, now: Instant) -> Option<Duration> {
        self.waiting?;
        let last = self.last?;
        Some(VOLUME_EVERY.saturating_sub(now.saturating_duration_since(last)))
    }
}

/// What the bar keeps between frames.
#[derive(Debug, Default)]
pub struct BottomBar {
    pub opt: Optimistic,
    held: Option<Held>,
    /// What each request the bar sent was.
    sent: HashMap<RequestId, Flip>,
    /// The last seek sent, whose failure drops the held position.
    seek_id: Option<RequestId>,
    /// Where the seek bar is while it is dragged.
    seeking: Option<u64>,
    /// The volume slider while it is dragged.
    dragging_volume: Option<u8>,
    volume: Throttle,
}

impl BottomBar {
    /// Whether the reply to request `id` is the bar's.
    pub fn wants(&self, id: RequestId) -> bool {
        self.sent.contains_key(&id)
    }

    /// The reply to request `id`: when it failed, what to tell the owner.
    pub fn on_reply(
        &mut self,
        id: RequestId,
        result: Result<Reply, String>,
        now: Instant,
    ) -> Option<String> {
        let flip = self.sent.remove(&id)?;
        match result {
            Ok(Reply::Liked { uri, liked }) => {
                self.opt.on_liked(uri, liked, now);
                None
            }
            Ok(_) => None,
            Err(e) => {
                if self.seek_id == Some(id) {
                    self.held = None;
                }
                Some(self.opt.on_failure(&flip, &e))
            }
        }
    }

    /// The engine's player went from `before` to `after`.
    pub fn on_player(
        &mut self,
        before: &PlayerState,
        after: &PlayerState,
        now: Instant,
        now_ms: i64,
    ) {
        let was = self.playing(before, now);
        self.opt.on_player(after);
        let is = self.playing(after, now);
        self.held = held_after(before, after, self.held, was, is, now_ms);
    }

    /// The link (re)connected: nothing sent before is waited for.
    pub fn on_connected(&mut self) {
        *self = BottomBar::default();
    }

    /// Whether `player` shows playing.
    fn playing(&self, player: &PlayerState, now: Instant) -> bool {
        self.opt.displayed_status(&player.status, now) == PlayStatus::Playing
    }

    /// Where the bar shows playback at `now_ms`, and whether it shows it playing: what
    /// the lyrics follow.
    pub fn shown_position(
        &self,
        player: &PlayerState,
        now: Instant,
        now_ms: i64,
    ) -> (Option<u64>, bool) {
        let playing = self.playing(player, now);
        (self.position(player, playing, now_ms), playing)
    }

    /// Seeks to `position_ms` from outside the bar (a lyric line), as its own seek bar
    /// does: held there until the engine sends a position; a failure is the bar's to say.
    pub fn seek_to(
        &mut self,
        link: &dyn Requests,
        player: &PlayerState,
        position_ms: u64,
        now_ms: i64,
    ) {
        self.held = Some(Held {
            stale: player.position,
            position_ms,
            at_ms: now_ms,
        });
        let id = self.send(link, Request::Seek { position_ms }, Flip::Seek);
        self.seek_id = Some(id);
    }

    /// Where `player` shows it is at `now_ms`.
    fn position(&self, player: &PlayerState, playing: bool, now_ms: i64) -> Option<u64> {
        position_at(
            player.position.as_ref(),
            self.held.as_ref(),
            playing,
            now_ms,
        )
    }

    fn send(&mut self, link: &dyn Requests, req: Request, flip: Flip) -> RequestId {
        let id = link.send(req);
        self.sent.insert(id, flip);
        id
    }

    /// Play or pause, shown at once; the clock stops or starts where it stands.
    fn play_pause(&mut self, cx: &Cx<'_>) {
        let status = self.opt.displayed_status(&cx.player.status, cx.now);
        let (req, flip, want) = if shows_pause(&status) {
            (Request::Pause, Flip::Pause, PlayStatus::Paused)
        } else {
            (Request::Play, Flip::Play, PlayStatus::Playing)
        };
        let (was, is) = (status == PlayStatus::Playing, want == PlayStatus::Playing);
        if was != is {
            let at = self.position(cx.player, was, cx.now_ms);
            self.held = at.map(|position_ms| Held {
                stale: cx.player.position,
                position_ms,
                at_ms: cx.now_ms,
            });
        }
        self.opt.want_status(want, cx.now);
        self.send(cx.link, req, flip);
    }

    /// Seeks to `position_ms`, held there until the engine sends a position.
    fn seek(&mut self, cx: &Cx<'_>, position_ms: u64) {
        self.seek_to(cx.link, cx.player, position_ms, cx.now_ms);
    }

    fn set_volume(&mut self, link: &dyn Requests, volume: u8) {
        self.send(link, Request::SetVolume { volume }, Flip::Volume);
    }
}

/// Whether the play/pause control offers pause: while playing, or about to.
fn shows_pause(status: &PlayStatus) -> bool {
    matches!(status, PlayStatus::Playing | PlayStatus::Buffering)
}

/// Repeat's next mode on a click: off, the list, the track.
fn next_repeat(mode: Repeat) -> Repeat {
    match mode {
        Repeat::Off => Repeat::Context,
        Repeat::Context => Repeat::Track,
        Repeat::Track => Repeat::Off,
    }
}

/// `FLAC 44.1 kHz` for a lossless stream, `VORBIS 160 kbps` for a lossy one.
pub fn quality_badge(q: &StreamQuality) -> String {
    let codec = q.codec.to_uppercase();
    if LOSSLESS.contains(&q.codec.to_lowercase().as_str()) {
        let khz = format!("{:.2}", f64::from(q.sample_rate_hz) / 1000.0);
        let khz = khz.trim_end_matches('0').trim_end_matches('.');
        format!("{codec} {khz} kHz")
    } else {
        format!("{codec} {} kbps", q.bitrate_bps / 1000)
    }
}

/// The cover of `track` as Art takes it (ruling R2: one URL).
fn cover(track: &NowPlaying) -> Option<[Image; 1]> {
    track.cover_url.as_ref().map(|url| {
        [Image {
            url: url.clone(),
            width: None,
        }]
    })
}

/// The cover of the first track queued, to fetch before it plays.
pub fn next_cover(queue: &[NowPlaying]) -> Option<Image> {
    let [image] = cover(queue.first()?)?;
    Some(image)
}

/// Fetches the first queued track's cover ahead of its playing: call it whenever the
/// queue changes.
pub fn prefetch_next(art: &mut Art, queue: &[NowPlaying]) {
    if let Some(image) = next_cover(queue) {
        art.prefetch(&[image], COVER);
    }
}

/// Now, in Unix milliseconds: what the engine's position anchors count in.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// What the bar draws from and sends with, for a frame.
pub struct BarCx<'a> {
    pub link: &'a dyn Requests,
    pub art: &'a mut Art,
    pub player: &'a PlayerState,
    /// Soloist's health as the engine last said; `None` before it has.
    pub soloist: Option<&'a SoloistHealth>,
    pub now: Instant,
    /// Unix milliseconds, for the position.
    pub now_ms: i64,
    /// Whether the right panel is open: its toggle shows it.
    pub panel: bool,
}

/// A frame's [`BarCx`], less the art.
struct Cx<'a> {
    link: &'a dyn Requests,
    player: &'a PlayerState,
    /// Whether the player takes commands: Soloist ready.
    ready: bool,
    now: Instant,
    now_ms: i64,
    id: Id,
}

/// The bar, as a panel along the bottom of `ui`: add it before the side and central
/// panels, so it spans the window.
pub fn show(ui: &mut Ui, bar: &mut BottomBar, cx: BarCx<'_>) -> Option<BarAction> {
    egui::Panel::bottom("bottom-bar")
        .resizable(false)
        .exact_size(HEIGHT)
        .frame(
            egui::Frame::new()
                .fill(theme::SURFACE)
                .inner_margin(egui::Margin::symmetric(12, 8)),
        )
        .show(ui, |ui| draw(ui, bar, cx))
        .inner
}

/// The bar inside its panel: the track on the left third, the transport and the seek
/// bar in the middle, the volume and the badge (or what the player is up to) on the
/// right.
fn draw(ui: &mut Ui, bar: &mut BottomBar, cx: BarCx<'_>) -> Option<BarAction> {
    let BarCx {
        link,
        art,
        player,
        soloist,
        now,
        now_ms,
        panel,
    } = cx;
    let cx = Cx {
        link,
        player,
        ready: soloist == Some(&SoloistHealth::Ready),
        now,
        now_ms,
        id: ui.make_persistent_id("bottom-bar"),
    };
    let rect = ui.max_rect();
    let side = (rect.width() * 0.3).floor();
    let left = Rect::from_x_y_ranges(rect.left()..=rect.left() + side, rect.y_range());
    let right = Rect::from_x_y_ranges(rect.right() - side..=rect.right(), rect.y_range());
    let centre = Rect::from_x_y_ranges(left.right()..=right.left(), rect.y_range());

    let mut action = now_playing(ui, left, bar, &cx, art);
    transport(ui, centre, bar, &cx);
    let speaker = volume_and_badge(ui, right, bar, &cx, status_line(player, soloist));
    // The panel's toggle, left of the volume.
    let toggle = Rect::from_center_size(
        pos2(speaker.left() - SPACING - BUTTON / 2.0, speaker.center().y),
        Vec2::splat(BUTTON),
    );
    let c = Control {
        icon: Icon::Panel,
        label: PANEL,
        enabled: true,
        on: Some(panel),
    };
    if button(ui, toggle, cx.id.with("panel"), c).clicked() {
        action = Some(BarAction::TogglePanel);
    }

    let ctx = ui.ctx();
    if bar.playing(player, now) && player.track.is_some() {
        ctx.request_repaint_after(REPAINT);
    }
    for wait in [bar.opt.next_expiry(now), bar.volume.wait(now)]
        .into_iter()
        .flatten()
    {
        ctx.request_repaint_after(wait);
    }
    action
}

/// What the bar says in place of the badge: Soloist on its way back, or another device
/// playing. Soloist needing the owner (not paired, missing, expired, failed) is the main
/// area's screen to say: the bar says nothing then, its controls off.
fn status_line(player: &PlayerState, soloist: Option<&SoloistHealth>) -> Option<&'static str> {
    use SoloistHealth as H;
    match soloist {
        Some(H::Starting | H::Reconnecting { .. }) => return Some(RECONNECTING_PLAYER),
        Some(H::NotPaired | H::Failed { .. } | H::Expired { .. } | H::BinaryMissing { .. }) => {
            return None;
        }
        Some(H::Ready) | None => {}
    }
    if !player.active_here && matches!(player.status, PlayStatus::Playing | PlayStatus::Paused) {
        Some(ELSEWHERE)
    } else {
        None
    }
}

/// The cover, the title over the artists, and the heart, in `area`.
fn now_playing(
    ui: &mut Ui,
    area: Rect,
    bar: &mut BottomBar,
    cx: &Cx<'_>,
    art: &mut Art,
) -> Option<BarAction> {
    let track = cx.player.track.as_ref()?;
    let cover_rect = Rect::from_min_size(
        pos2(area.left(), area.center().y - COVER / 2.0),
        Vec2::splat(COVER),
    );
    if ui.is_rect_visible(cover_rect) {
        match cover(track).and_then(|images| art.get(&images, COVER, ui.ctx())) {
            Some(texture) => {
                let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                ui.painter()
                    .image(texture.id(), cover_rect, uv, theme::UNTINTED);
            }
            None => {
                ui.painter().rect_filled(cover_rect, 0.0, theme::LINES);
            }
        }
    }
    let heart = Rect::from_center_size(
        pos2(area.right() - BUTTON / 2.0, area.center().y),
        Vec2::splat(BUTTON),
    );
    let mut action = None;
    let (left, right) = (cover_rect.right() + GAP, heart.left() - GAP);
    if right > left {
        let line = ui.text_style_height(&TextStyle::Body);
        let title_top = area.center().y - line - 1.0;
        let artists_top = area.center().y + 1.0;
        // The names' cell: they are cut short, and clipped, at its right.
        let cell = Rect::from_x_y_ranges(left..=right, area.y_range());
        let album = track
            .album_uri
            .as_deref()
            .filter(|uri| is_page(uri, "album"));
        let title = name(
            ui,
            cx.id.with("title"),
            &track.title,
            pos2(left, title_top),
            cell,
            (theme::TEXT, album.is_some()),
        );
        if let (true, Some(uri)) = (title.response.clicked(), album) {
            action = Some(BarAction::GoAlbum(uri.to_owned()));
        }
        let mut x = left;
        let mut first = true;
        for (i, artist) in track.artists.iter().enumerate() {
            if artist.is_empty() {
                continue;
            }
            if !first {
                x += plain(ui, ", ", pos2(x, artists_top), right - x, theme::SECONDARY);
            }
            first = false;
            if right - x <= 0.0 {
                break;
            }
            let page = track
                .artist_uris
                .get(i)
                .map(String::as_str)
                .filter(|uri| is_page(uri, "artist"));
            let shown = name(
                ui,
                cx.id.with(("artist", i)),
                artist,
                pos2(x, artists_top),
                cell,
                (theme::SECONDARY, page.is_some()),
            );
            if let (true, Some(uri)) = (shown.response.clicked(), page) {
                action = Some(BarAction::GoArtist(uri.to_owned()));
            }
            x += shown.width;
            if shown.elided {
                break;
            }
        }
    }
    let liked = bar.opt.displayed_liked(cx.player, cx.now) == Some(true);
    if heart_button(ui, heart, cx.id.with("heart"), liked).clicked() {
        bar.opt.want_heart(track.uri.clone(), !liked, cx.now);
        // The state wanted, for the track shown: never a toggle of whatever plays when
        // the engine gets to it.
        let req = Request::SetLiked {
            uri: track.uri.clone(),
            liked: !liked,
        };
        bar.send(cx.link, req, Flip::Heart);
    }
    action
}

/// Shuffle, previous, play/pause, next and repeat over the seek bar, in `area`.
fn transport(ui: &mut Ui, area: Rect, bar: &mut BottomBar, cx: &Cx<'_>) {
    let (player, now, ready) = (cx.player, cx.now, cx.ready);
    let actions = player.actions;
    let status = bar.opt.displayed_status(&player.status, now);
    let shuffle = bar.opt.displayed_shuffle(player.shuffle, now);
    let repeat = bar.opt.displayed_repeat(player.repeat, now);
    let gap = 4.0;
    let top = area.center().y - (BUTTON + gap + SLIDER) / 2.0;
    let width = 5.0 * BUTTON + 4.0 * SPACING;
    let mut x = area.center().x - width / 2.0;
    let mut slot = || {
        let rect = Rect::from_min_size(pos2(x, top), Vec2::splat(BUTTON));
        x += BUTTON + SPACING;
        rect
    };
    let control = |icon, label, enabled, on| Control {
        icon,
        label,
        enabled: ready && enabled,
        on,
    };

    let c = control(Icon::Shuffle, SHUFFLE, actions.shuffle, Some(shuffle));
    if button(ui, slot(), cx.id.with("shuffle"), c).clicked() {
        bar.opt.want_shuffle(!shuffle, now);
        let req = Request::SetShuffle { on: !shuffle };
        bar.send(cx.link, req, Flip::Shuffle(!shuffle));
    }
    let c = control(
        Icon::Glyph(PREVIOUS_GLYPH),
        PREVIOUS,
        actions.skip_prev,
        None,
    );
    if button(ui, slot(), cx.id.with("previous"), c).clicked() {
        bar.send(cx.link, Request::Prev, Flip::Previous);
    }
    let (glyph, label) = match shows_pause(&status) {
        true => (PAUSE_GLYPH, PAUSE),
        false => (PLAY_GLYPH, PLAY),
    };
    let c = control(
        Icon::Main(glyph),
        label,
        actions.play || actions.pause,
        None,
    );
    if button(ui, slot(), cx.id.with("play"), c).clicked() {
        bar.play_pause(cx);
    }
    let c = control(Icon::Glyph(NEXT_GLYPH), NEXT, actions.skip_next, None);
    if button(ui, slot(), cx.id.with("next"), c).clicked() {
        bar.send(cx.link, Request::Next, Flip::Next);
    }
    let c = control(
        Icon::Repeat(repeat),
        REPEAT,
        actions.set_repeat,
        Some(repeat != Repeat::Off),
    );
    if button(ui, slot(), cx.id.with("repeat"), c).clicked() {
        let mode = next_repeat(repeat);
        bar.opt.want_repeat(mode, now);
        bar.send(cx.link, Request::SetRepeat { mode }, Flip::Repeat(mode));
    }

    let row = Rect::from_x_y_ranges(
        area.x_range(),
        top + BUTTON + gap..=top + BUTTON + gap + SLIDER,
    );
    seek_bar(ui, row, bar, cx, status == PlayStatus::Playing);
}

/// The seek bar, the elapsed time left of it and the total right of it, in `row`. Dragged,
/// it shows where it is (the clock stopped) and sends `Seek` on release.
fn seek_bar(ui: &mut Ui, row: Rect, bar: &mut BottomBar, cx: &Cx<'_>, playing: bool) {
    let player = cx.player;
    let total = player
        .track
        .as_ref()
        .and_then(|t| t.duration_ms)
        .filter(|&ms| ms > 0);
    let enabled = cx.ready && player.actions.seek && total.is_some();
    let rail_width = (row.width() - 2.0 * (TIME + 8.0)).clamp(0.0, SEEK_MAX);
    let rect = Rect::from_center_size(row.center(), vec2(rail_width, row.height()));
    let response = ui.interact(rect, cx.id.with("seek"), drag_sense(enabled));
    match total {
        Some(total) if enabled => {
            let pressed = response.is_pointer_button_down_on()
                || response.clicked()
                || response.drag_stopped();
            if let (true, Some(pos)) = (pressed, response.interact_pointer_pos()) {
                let ms = (f64::from(fraction(rect, pos)) * total as f64).round() as u64;
                bar.seeking = Some(ms);
            }
            if response.clicked() || response.drag_stopped() {
                if let Some(ms) = bar.seeking.take() {
                    bar.seek(cx, ms);
                }
            } else if !response.is_pointer_button_down_on() {
                bar.seeking = None;
            }
        }
        _ => bar.seeking = None,
    }
    let at = bar
        .seeking
        .or_else(|| bar.position(player, playing, cx.now_ms))
        .map(|ms| total.map_or(ms, |total| ms.min(total)));
    let done = match (at, total) {
        (Some(at), Some(total)) => (at as f64 / total as f64) as f32,
        _ => 0.0,
    };
    let value = at.unwrap_or(0) as f64;
    response.widget_info(|| WidgetInfo::slider(enabled, value, SEEK));
    paint_slider(ui, &response, rect, done, enabled);
    if let Some(total) = total {
        let y = row.center().y;
        let elapsed = duration(at.unwrap_or(0));
        let left = pos2(rect.left() - 8.0, y);
        label_at(
            ui,
            cx.id.with("elapsed"),
            &elapsed,
            (Align2::RIGHT_CENTER, left),
        );
        let right = pos2(rect.right() + 8.0, y);
        label_at(
            ui,
            cx.id.with("total"),
            &duration(total),
            (Align2::LEFT_CENTER, right),
        );
    }
}

/// The volume over the badge, or what the player is up to, at the right of `area`; the
/// speaker's rect.
fn volume_and_badge(
    ui: &mut Ui,
    area: Rect,
    bar: &mut BottomBar,
    cx: &Cx<'_>,
    status: Option<&str>,
) -> Rect {
    let player = cx.player;
    // The volume's row is a control's square from the top, as the panel's toggle beside
    // it; the line under it starts below that square (and the badge's outline with it), so
    // the toggle and its dot never reach the status, however long it is.
    let upper = area.top() + BUTTON / 2.0;
    let line_top = area.top() + BUTTON + 2.0;
    let rect = Rect::from_min_max(
        pos2(area.right() - VOLUME_WIDTH, upper - SLIDER / 2.0),
        pos2(area.right(), upper + SLIDER / 2.0),
    );
    let speaker = Rect::from_center_size(
        pos2(rect.left() - 6.0 - theme::ICON_DRAWN / 2.0, upper),
        Vec2::splat(theme::ICON_DRAWN),
    );
    paint_speaker(ui.painter(), speaker, theme::QUIET);
    let enabled = cx.ready;
    let response = ui.interact(rect, cx.id.with("volume"), drag_sense(enabled));
    if enabled {
        let pressed =
            response.is_pointer_button_down_on() || response.clicked() || response.drag_stopped();
        if let (true, Some(pos)) = (pressed, response.interact_pointer_pos()) {
            let volume = (fraction(rect, pos) * 100.0).round() as u8;
            if bar.dragging_volume != Some(volume) {
                bar.dragging_volume = Some(volume);
                bar.opt.want_volume(volume, cx.now);
                if let Some(volume) = bar.volume.offer(volume, cx.now) {
                    bar.set_volume(cx.link, volume);
                }
            }
        }
    }
    if !response.is_pointer_button_down_on() {
        bar.dragging_volume = None;
    }
    if let Some(volume) = bar.volume.due(cx.now) {
        bar.set_volume(cx.link, volume);
    }
    let volume = bar
        .dragging_volume
        .unwrap_or_else(|| bar.opt.displayed_volume(player.volume, cx.now));
    response.widget_info(|| WidgetInfo::slider(enabled, f64::from(volume), VOLUME));
    paint_slider(ui, &response, rect, f32::from(volume) / 100.0, enabled);

    // One point down, room for the badge's outline above its words.
    let at = (Align2::RIGHT_TOP, pos2(area.right(), line_top + 1.0));
    match (status, &player.quality) {
        (Some(text), _) => {
            label_at(ui, cx.id.with("status"), text, at);
        }
        // Carry 2: only this device's own stream.
        (None, Some(quality)) if player.active_here => {
            let badge = quality_badge(quality);
            let at = (Align2::RIGHT_TOP, pos2(area.right() - 4.0, line_top + 1.0));
            let rect = label_at(ui, cx.id.with("badge"), &badge, at);
            let outline = rect.expand2(vec2(4.0, 1.0));
            let stroke = Stroke::new(1.0, theme::LINES);
            ui.painter()
                .rect_stroke(outline, theme::CORNER, stroke, StrokeKind::Inside);
        }
        _ => {}
    }
    speaker
}

/// How a control looks: an icon, its label, whether it can be used, and for a toggle
/// whether it is on.
struct Control {
    icon: Icon,
    label: &'static str,
    enabled: bool,
    on: Option<bool>,
}

enum Icon {
    /// A glyph, at the icons' size.
    Glyph(&'static str),
    /// Play or pause: a larger glyph, quiet like the rest.
    Main(&'static str),
    Shuffle,
    Repeat(Repeat),
    /// A window with a panel down its right side.
    Panel,
}

/// A transport control in `rect`: quiet, white under the pointer, in the line grey when
/// disabled; a toggle that is on is in the accent with a dot under it, under the pointer
/// too, so hovering never hides what it is.
fn button(ui: &mut Ui, rect: Rect, id: Id, control: Control) -> Response {
    let Control {
        icon,
        label,
        enabled,
        on,
    } = control;
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let response = ui.interact(rect, id, sense);
    response.widget_info(|| match on {
        Some(on) => WidgetInfo::selected(Role::Button, enabled, on, label),
        None => WidgetInfo::labeled(Role::Button, enabled, label),
    });
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let colour = if !enabled {
        theme::LINES
    } else if on == Some(true) {
        theme::ACCENT
    } else if response.hovered() {
        theme::TEXT
    } else {
        theme::QUIET
    };
    let painter = ui.painter();
    let square = Rect::from_center_size(rect.center(), Vec2::splat(theme::ICON_DRAWN));
    match icon {
        Icon::Glyph(glyph) => glyph_at(painter, rect, glyph, theme::ICON, colour),
        Icon::Main(glyph) => glyph_at(painter, rect, glyph, theme::ICON_LARGE, colour),
        Icon::Shuffle => paint_shuffle(painter, square, colour),
        Icon::Repeat(mode) => paint_repeat(painter, square, colour, mode == Repeat::Track),
        Icon::Panel => paint_panel(painter, square, colour),
    }
    if on == Some(true) {
        painter.circle_filled(pos2(rect.center().x, rect.bottom() - 2.0), 2.0, colour);
    }
    response
}

/// The heart: a filled heart in the accent when liked, else an outline in the line grey
/// (white under the pointer), as on a track row.
fn heart_button(ui: &mut Ui, rect: Rect, id: Id, liked: bool) -> Response {
    let response = ui.interact(rect, id, Sense::click());
    response.widget_info(|| WidgetInfo::selected(Role::Button, true, liked, HEART_LABEL));
    let (glyph, colour) = match liked {
        true => (LIKED_GLYPH, theme::ACCENT),
        false if response.hovered() => (NOT_LIKED_GLYPH, theme::TEXT),
        false => (NOT_LIKED_GLYPH, theme::LINES),
    };
    glyph_at(ui.painter(), rect, glyph, theme::ICON, colour);
    response
}

fn glyph_at(painter: &Painter, rect: Rect, glyph: &str, size: f32, colour: Color32) {
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        glyph,
        FontId::proportional(size),
        colour,
    );
}

/// Two arrows crossing, both pointing right.
fn paint_shuffle(painter: &Painter, r: Rect, colour: Color32) {
    let p = |x: f32, y: f32| pos2(r.left() + r.width() * x, r.top() + r.height() * y);
    let stroke = Stroke::new(1.5, colour);
    for (from, to) in [(0.8, 0.2), (0.2, 0.8)] {
        let points = vec![p(0.0, from), p(0.3, from), p(0.7, to), p(1.0, to)];
        painter.add(Shape::line(points, stroke));
    }
    for y in [0.2, 0.8] {
        painter.add(Shape::line(
            vec![p(0.8, y - 0.17), p(1.0, y), p(0.8, y + 0.17)],
            stroke,
        ));
    }
}

/// A loop of two arrows; with a 1 inside when it repeats the track.
fn paint_repeat(painter: &Painter, r: Rect, colour: Color32, one: bool) {
    let p = |x: f32, y: f32| pos2(r.left() + r.width() * x, r.top() + r.height() * y);
    let stroke = Stroke::new(1.5, colour);
    painter.add(Shape::line(
        vec![p(0.0, 0.6), p(0.0, 0.2), p(0.95, 0.2)],
        stroke,
    ));
    painter.add(Shape::line(
        vec![p(0.8, 0.05), p(0.95, 0.2), p(0.8, 0.35)],
        stroke,
    ));
    painter.add(Shape::line(
        vec![p(1.0, 0.4), p(1.0, 0.8), p(0.05, 0.8)],
        stroke,
    ));
    painter.add(Shape::line(
        vec![p(0.2, 0.65), p(0.05, 0.8), p(0.2, 0.95)],
        stroke,
    ));
    if one {
        let font = FontId::proportional(r.height() * 0.5);
        painter.text(r.center(), Align2::CENTER_CENTER, "1", font, colour);
    }
}

/// An outlined window with a panel down its right side, for the right panel's toggle.
fn paint_panel(painter: &Painter, r: Rect, colour: Color32) {
    let stroke = Stroke::new(1.5, colour);
    let frame = r.shrink2(vec2(0.0, 1.5));
    painter.rect_stroke(frame, 1.5, stroke, StrokeKind::Inside);
    let x = frame.left() + frame.width() * 0.62;
    painter.vline(x, frame.y_range(), stroke);
}

/// A small speaker, for the volume.
fn paint_speaker(painter: &Painter, r: Rect, colour: Color32) {
    let p = |x: f32, y: f32| pos2(r.left() + r.width() * x, r.top() + r.height() * y);
    painter.rect_filled(Rect::from_min_max(p(0.0, 0.35), p(0.3, 0.65)), 0.0, colour);
    let cone = vec![p(0.3, 0.35), p(0.6, 0.1), p(0.6, 0.9), p(0.3, 0.65)];
    painter.add(Shape::convex_polygon(cone, colour, Stroke::NONE));
    let wave = vec![p(0.75, 0.3), p(0.88, 0.5), p(0.75, 0.7)];
    painter.add(Shape::line(wave, Stroke::new(1.5, colour)));
}

/// What a slider senses: a click or a drag, or the pointer only when disabled.
fn drag_sense(enabled: bool) -> Sense {
    if enabled {
        Sense::click_and_drag()
    } else {
        Sense::hover()
    }
}

/// How far along `rect` `pos` is, from 0 to 1.
fn fraction(rect: Rect, pos: Pos2) -> f32 {
    if rect.width() <= 0.0 {
        return 0.0;
    }
    ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0)
}

/// A slider in `rect`: a thin rail in the line grey, quiet up to `done` (white under the
/// pointer, with a handle).
fn paint_slider(ui: &Ui, response: &Response, rect: Rect, done: f32, enabled: bool) {
    if !ui.is_rect_visible(rect) {
        return;
    }
    let painter = ui.painter();
    let rail = Rect::from_center_size(rect.center(), vec2(rect.width(), RAIL));
    painter.rect_filled(rail, RAIL / 2.0, theme::LINES);
    let x = rail.left() + rail.width() * done.clamp(0.0, 1.0);
    let active = enabled && (response.hovered() || response.is_pointer_button_down_on());
    let colour = if active { theme::TEXT } else { theme::QUIET };
    if x > rail.left() {
        let filled = Rect::from_min_max(rail.min, pos2(x, rail.bottom()));
        painter.rect_filled(filled, RAIL / 2.0, colour);
    }
    if active {
        painter.circle_filled(pos2(x, rail.center().y), 5.0, theme::TEXT);
    }
}

/// `text` in the secondary grey, placed by its `anchor` at a point, with a label for
/// screen readers; its rect.
fn label_at(ui: &mut Ui, id: Id, text: &str, (anchor, pos): (Align2, Pos2)) -> Rect {
    let galley = galley(ui, text, f32::INFINITY);
    let rect = anchor.anchor_size(pos, galley.size());
    let response = ui.interact(rect, id, Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(Role::Label, true, text));
    ui.painter().galley(rect.min, galley, theme::SECONDARY);
    rect
}

/// `text` at `pos`, at most `width` wide, with nothing to it; its width.
fn plain(ui: &Ui, text: &str, pos: Pos2, width: f32, colour: Color32) -> f32 {
    if width <= 0.0 {
        return 0.0;
    }
    let galley = galley(ui, text, width);
    let w = galley.size().x;
    ui.painter().galley(pos, galley, colour);
    w
}

/// A name drawn by [`name`].
struct Name {
    response: Response,
    width: f32,
    /// Cut short: nothing more fits after it.
    elided: bool,
}

/// A name at `pos`, cut short at the right of its `cell`, in `colour`; when it links
/// (`.1`), white and underlined under the pointer, which becomes a hand, as a track row's
/// links are.
fn name(
    ui: &mut Ui,
    id: Id,
    text: &str,
    pos: Pos2,
    cell: Rect,
    (colour, links): (Color32, bool),
) -> Name {
    let galley = galley(ui, text, cell.right() - pos.x);
    let (size, elided) = (galley.size(), galley.elided);
    let rect = Rect::from_min_size(pos, size);
    let sense = if links {
        Sense::click()
    } else {
        Sense::hover()
    };
    let response = ui.interact(rect, id, sense);
    let role = if links { Role::Link } else { Role::Label };
    response.widget_info(|| WidgetInfo::labeled(role, true, text));
    let hovered = links && response.hovered();
    if hovered {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    paint_link(ui.painter(), cell, pos, galley, (colour, hovered));
    Name {
        response,
        width: size.x,
        elided,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use eframe::egui::{
        Pos2, Rect, Shape, Stroke, ViewportId,
        accesskit::{Role, Toggled},
        pos2, vec2,
    };
    use egui_kittest::{
        Harness,
        kittest::{NodeT, Queryable},
    };
    use playback::event::Actions;

    use super::*;
    use crate::{
        test_support::{self, FakeLink, drawn_within, fills, marks_within, size_of, texts},
        theme,
        track_list::HEART_LABEL,
    };

    /// The engine's clock at the start of a test, in Unix milliseconds.
    const T0_MS: i64 = 1_800_000_000_000;
    const MS: Duration = Duration::from_millis(1);
    const NERVE: &str = "spotify:track:nerve";
    const BOY: &str = "spotify:album:boy";
    const JANE: &str = "spotify:artist:jane";

    fn track() -> NowPlaying {
        NowPlaying {
            uri: NERVE.into(),
            title: "nerve".into(),
            artists: vec!["2hollis".into(), "Jane Remover".into()],
            artist_uris: vec!["spotify:artist:2hollis".into(), JANE.into()],
            album: Some("boy".into()),
            album_uri: Some(BOY.into()),
            duration_ms: Some(150_000),
            cover_url: Some("https://i.scdn.co/image/nerve".into()),
        }
    }

    fn flac() -> StreamQuality {
        StreamQuality {
            codec: "flac".into(),
            bitrate_bps: 1_400_000,
            sample_rate_hz: 44_100,
        }
    }

    fn all_actions() -> Actions {
        Actions {
            play: true,
            pause: true,
            skip_next: true,
            skip_prev: true,
            seek: true,
            shuffle: true,
            set_repeat: true,
            add_to_queue: true,
        }
    }

    /// Playing here 10 s into a 2:30 track, as of `T0_MS`.
    fn playing() -> PlayerState {
        PlayerState {
            track: Some(track()),
            status: PlayStatus::Playing,
            active_here: true,
            context_uri: Some("spotify:album:boy".into()),
            position: Some(Position {
                position_ms: 10_000,
                timestamp_ms: T0_MS,
                speed: 1.0,
            }),
            volume: 40,
            shuffle: false,
            repeat: Repeat::Off,
            actions: all_actions(),
            liked: Some(false),
            quality: Some(flac()),
        }
    }

    fn with_status(status: PlayStatus) -> PlayerState {
        PlayerState {
            status,
            ..playing()
        }
    }

    /// The bar and what it draws from, as the window holds them.
    struct Shown {
        bar: BottomBar,
        player: PlayerState,
        soloist: Option<SoloistHealth>,
        link: FakeLink,
        art: Option<Art>,
        now: Instant,
        now_ms: i64,
        actions: Vec<BarAction>,
        /// Whether the right panel is open.
        panel: bool,
    }

    impl Shown {
        fn new(player: PlayerState) -> Shown {
            Shown {
                bar: BottomBar::default(),
                player,
                soloist: Some(SoloistHealth::Ready),
                link: FakeLink::default(),
                art: None,
                now: Instant::now(),
                now_ms: T0_MS,
                actions: vec![],
                panel: false,
            }
        }

        /// Time passes, on both clocks.
        fn advance(&mut self, by: Duration) {
            self.now += by;
            self.now_ms += i64::try_from(by.as_millis()).unwrap();
        }

        /// The engine reports `player`.
        fn engine(&mut self, player: PlayerState) {
            self.bar
                .on_player(&self.player, &player, self.now, self.now_ms);
            self.player = player;
        }

        /// The reply to the last request sent.
        fn reply(&mut self, result: Result<Reply, String>) -> Option<String> {
            let id = self.link.last_id();
            self.bar.on_reply(id, result, self.now)
        }
    }

    fn harness(shown: Shown) -> Harness<'static, Shown> {
        let (builder, _) = test_support::builder(vec2(1200.0, 200.0), false);
        let mut harness = builder.build_ui_state(
            |ui, s: &mut Shown| {
                let art = s.art.get_or_insert_with(|| Art::offline(ui.ctx().clone()));
                let cx = BarCx {
                    link: &s.link,
                    art,
                    player: &s.player,
                    soloist: s.soloist.as_ref(),
                    now: s.now,
                    now_ms: s.now_ms,
                    panel: s.panel,
                };
                if let Some(action) = show(ui, &mut s.bar, cx) {
                    s.actions.push(action);
                }
            },
            shown,
        );
        harness.run();
        harness
    }

    fn disabled(harness: &Harness<'_, Shown>, label: &str) -> bool {
        harness.get_by_label(label).accesskit_node().is_disabled()
    }

    fn toggled(harness: &Harness<'_, Shown>, label: &str) -> bool {
        harness.get_by_label(label).accesskit_node().toggled() == Some(Toggled::True)
    }

    /// The colour `text` was drawn in last frame.
    fn colour(harness: &Harness<'_, Shown>, text: &str) -> Option<eframe::egui::Color32> {
        texts(harness)
            .into_iter()
            .find(|(t, _, _)| t == text)
            .map(|(_, c, _)| c)
    }

    fn drawn(harness: &Harness<'_, Shown>, text: &str) -> bool {
        colour(harness, text).is_some()
    }

    /// The 1 px white lines drawn last frame: the underline of a hovered link.
    fn underlines(harness: &Harness<'_, Shown>) -> Vec<[Pos2; 2]> {
        fn walk(shape: &Shape, out: &mut Vec<[Pos2; 2]>) {
            match shape {
                Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out)),
                Shape::LineSegment { points, stroke }
                    if *stroke == Stroke::new(1.0, theme::TEXT) =>
                {
                    out.push(*points);
                }
                _ => {}
            }
        }
        let mut out = vec![];
        for clipped in &harness.output().shapes {
            walk(&clipped.shape, &mut out);
        }
        out
    }

    /// When the last frame asked to be painted again: `Duration::MAX` for never.
    fn repaint_delay(harness: &Harness<'_, Shown>) -> Duration {
        harness.output().viewport_output[&ViewportId::ROOT].repaint_delay
    }

    /// A slider's point at `fraction` of its width.
    fn along(rect: Rect, fraction: f32) -> Pos2 {
        pos2(rect.left() + rect.width() * fraction, rect.center().y)
    }

    #[test]
    fn optimistic_flip_then_reconcile() {
        let t0 = Instant::now();
        let mut opt = Optimistic::default();
        // Pause clicked at t0: shown paused at once.
        opt.want_status(PlayStatus::Paused, t0);
        assert_eq!(
            opt.displayed_status(&PlayStatus::Playing, t0),
            PlayStatus::Paused
        );
        // A second on, the engine still says playing: still shown paused.
        opt.on_player(&with_status(PlayStatus::Playing));
        let shown = opt.displayed_status(&PlayStatus::Playing, t0 + Duration::from_secs(1));
        assert_eq!(shown, PlayStatus::Paused);
        assert_eq!(
            opt.next_expiry(t0 + Duration::from_secs(1)),
            Some(Duration::from_secs(2))
        );
        // Past the hold, the engine is believed again.
        let late = t0 + Duration::from_millis(3_100);
        assert_eq!(
            opt.displayed_status(&PlayStatus::Playing, late),
            PlayStatus::Playing
        );
        assert_eq!(opt.next_expiry(late), None);

        // Reconciled: once the engine reports the pause, its next word stands at once
        // (the phone resumed, say).
        opt.want_status(PlayStatus::Paused, t0);
        opt.on_player(&with_status(PlayStatus::Paused));
        assert_eq!(
            opt.displayed_status(&PlayStatus::Playing, t0 + MS),
            PlayStatus::Playing
        );
        assert_eq!(opt.next_expiry(t0 + MS), None);

        // Shuffle, repeat and volume flip the same way.
        opt.want_shuffle(true, t0);
        opt.want_repeat(Repeat::Track, t0);
        opt.want_volume(70, t0);
        assert!(opt.displayed_shuffle(false, t0 + MS));
        assert_eq!(opt.displayed_repeat(Repeat::Off, t0 + MS), Repeat::Track);
        assert_eq!(opt.displayed_volume(40, t0 + MS), 70);
        let mut engine = playing();
        engine.shuffle = true;
        engine.repeat = Repeat::Track;
        engine.volume = 70;
        opt.on_player(&engine);
        assert!(!opt.displayed_shuffle(false, t0 + MS));
        assert_eq!(
            opt.displayed_repeat(Repeat::Context, t0 + MS),
            Repeat::Context
        );
        assert_eq!(opt.displayed_volume(30, t0 + MS), 30);
    }

    #[test]
    fn failure_reverts_with_message() {
        let t0 = Instant::now();
        let mut opt = Optimistic::default();
        opt.want_status(PlayStatus::Paused, t0);
        assert_eq!(
            opt.on_failure(&Flip::Pause, "The player is restarting."),
            "Couldn't pause: The player is restarting."
        );
        assert_eq!(
            opt.displayed_status(&PlayStatus::Playing, t0),
            PlayStatus::Playing
        );
        // The engine's own "Couldn't …" is kept whole.
        opt.want_heart(NERVE.into(), true, t0);
        assert_eq!(
            opt.on_failure(&Flip::Heart, "Couldn't like: Spotify answered 503"),
            "Couldn't like: Spotify answered 503"
        );
        assert_eq!(opt.displayed_liked(&playing(), t0), Some(false));
        for (flip, what) in [
            (Flip::Play, "play"),
            (Flip::Next, "skip to the next track"),
            (Flip::Previous, "go back to the previous track"),
            (Flip::Shuffle(true), "turn shuffle on"),
            (Flip::Shuffle(false), "turn shuffle off"),
            (Flip::Repeat(Repeat::Off), "turn repeat off"),
            (Flip::Repeat(Repeat::Context), "turn repeat on"),
            (Flip::Repeat(Repeat::Track), "repeat the track"),
            (Flip::Seek, "seek"),
            (Flip::Volume, "change the volume"),
        ] {
            assert_eq!(flip.what(), what);
        }

        // Through the bar: the click flips the button at once, the failure flips it
        // back and says why.
        let mut harness = harness(Shown::new(playing()));
        harness.get_by_label(PAUSE).click();
        harness.run();
        assert_eq!(harness.state().link.take(), vec![Request::Pause]);
        harness.get_by_label(PLAY);
        let text = harness
            .state_mut()
            .reply(Err("The player is restarting.".into()));
        assert_eq!(
            text.as_deref(),
            Some("Couldn't pause: The player is restarting.")
        );
        harness.run();
        harness.get_by_label(PAUSE);
        // A success says nothing.
        harness.get_by_label(NEXT).click();
        harness.run();
        assert_eq!(harness.state().link.take(), vec![Request::Next]);
        assert_eq!(harness.state_mut().reply(Ok(Reply::Ok)), None);
        assert!(!harness.state().bar.wants(harness.state().link.last_id()));
    }

    #[test]
    fn badge_text() {
        assert_eq!(quality_badge(&flac()), "FLAC 44.1 kHz");
        let at = |codec: &str, bitrate_bps, sample_rate_hz| {
            quality_badge(&StreamQuality {
                codec: codec.into(),
                bitrate_bps,
                sample_rate_hz,
            })
        };
        assert_eq!(at("flac", 2_100_000, 48_000), "FLAC 48 kHz");
        assert_eq!(at("flac", 4_600_000, 96_000), "FLAC 96 kHz");
        assert_eq!(at("flac", 2_800_000, 88_200), "FLAC 88.2 kHz");
        assert_eq!(at("vorbis", 160_000, 44_100), "VORBIS 160 kbps");
        assert_eq!(at("aac", 256_000, 44_100), "AAC 256 kbps");
    }

    /// Another device plays: the bar says so, dim, shows no quality of its own, and its
    /// controls act on that device.
    #[test]
    fn remote_label() {
        let mut player = playing();
        player.active_here = false;
        let mut harness = harness(Shown::new(player));
        harness.get_by_label(ELSEWHERE);
        assert_eq!(colour(&harness, ELSEWHERE), Some(theme::SECONDARY));
        assert!(!drawn(&harness, "FLAC 44.1 kHz"), "the last local codec");
        assert!(!disabled(&harness, PAUSE));
        harness.get_by_label(PAUSE).click();
        harness.run();
        assert_eq!(harness.state().link.take(), vec![Request::Pause]);
        // Paused there: still another device's.
        harness.state_mut().player.status = PlayStatus::Paused;
        harness.run();
        harness.get_by_label(ELSEWHERE);
        // Nothing playing anywhere: nothing to say.
        harness.state_mut().player.status = PlayStatus::Stopped;
        harness.run();
        assert!(harness.query_by_label(ELSEWHERE).is_none());

        // Playing here: the badge, and no label.
        harness.state_mut().player = playing();
        harness.run();
        assert!(harness.query_by_label(ELSEWHERE).is_none());
        assert_eq!(colour(&harness, "FLAC 44.1 kHz"), Some(theme::SECONDARY));
    }

    #[test]
    fn controls_disabled_while_reconnecting() {
        let controls = [SHUFFLE, PREVIOUS, PAUSE, NEXT, REPEAT, SEEK, VOLUME];
        let mut shown = Shown::new(playing());
        shown.soloist = Some(SoloistHealth::Reconnecting {
            attempt: 1,
            retry_in_ms: 500,
        });
        let mut harness = harness(shown);
        harness.get_by_label(RECONNECTING_PLAYER);
        for label in controls {
            assert!(disabled(&harness, label), "{label}");
        }
        harness.get_by_label(PAUSE).click();
        harness.run();
        assert_eq!(harness.state().link.take(), vec![]);

        harness.state_mut().soloist = Some(SoloistHealth::Ready);
        harness.run();
        assert!(harness.query_by_label(RECONNECTING_PLAYER).is_none());
        for label in controls {
            assert!(!disabled(&harness, label), "{label}");
        }
        // Ready, the controls follow what the player allows.
        let actions = &mut harness.state_mut().player.actions;
        actions.skip_next = false;
        actions.seek = false;
        harness.run();
        assert!(disabled(&harness, NEXT));
        assert!(disabled(&harness, SEEK));
        assert!(!disabled(&harness, PREVIOUS));
        // Before the engine has said anything: disabled, and nothing said.
        harness.state_mut().soloist = None;
        harness.run();
        assert!(disabled(&harness, PAUSE));
        assert!(harness.query_by_label(RECONNECTING_PLAYER).is_none());
    }

    /// Task 29 carry 4: while the main area's screen says what Soloist needs of the
    /// owner, the bar doesn't claim it is reconnecting; its controls stay off.
    #[test]
    fn no_reconnecting_while_soloist_needs_the_owner() {
        let path = "/home/u/.local/share/loudauto/soloist/soloist".to_string();
        let mut harness = harness(Shown::new(playing()));
        for health in [
            SoloistHealth::NotPaired,
            SoloistHealth::BinaryMissing { path: path.clone() },
            SoloistHealth::Expired { path },
            SoloistHealth::Failed {
                message: "Spotify Soloist keeps stopping (exit code 1).".into(),
                log_tail: String::new(),
            },
        ] {
            harness.state_mut().soloist = Some(health.clone());
            harness.run();
            assert!(
                harness.query_by_label(RECONNECTING_PLAYER).is_none(),
                "{health:?}"
            );
            assert!(disabled(&harness, PAUSE), "{health:?}");
        }
        harness.state_mut().soloist = Some(SoloistHealth::Starting);
        harness.run();
        harness.get_by_label(RECONNECTING_PLAYER);
    }

    /// Paused, nothing is scheduled; playing, one repaint a second.
    #[test]
    fn repaint_only_while_playing() {
        let mut harness = harness(Shown::new(with_status(PlayStatus::Paused)));
        harness.step();
        assert_eq!(repaint_delay(&harness), Duration::MAX);
        harness.state_mut().player = PlayerState {
            track: None,
            position: None,
            ..with_status(PlayStatus::Stopped)
        };
        harness.step();
        assert_eq!(repaint_delay(&harness), Duration::MAX);

        harness.state_mut().player = playing();
        harness.step();
        assert_eq!(repaint_delay(&harness), REPAINT);
        // The clock moves with it.
        assert!(drawn(&harness, "0:10"));
        harness.state_mut().advance(Duration::from_secs(5));
        harness.step();
        assert!(drawn(&harness, "0:15"));
        assert!(drawn(&harness, "2:30"));
    }

    /// Carry 4: paused with the anchor it had while playing, the bar holds where
    /// playback had got to rather than jump back to the old anchor.
    #[test]
    fn pause_holds_extrapolated_position() {
        let before = playing();
        let after = with_status(PlayStatus::Paused);
        let at = |p: &PlayerState| p.position;
        // Playing: extrapolated from the anchor.
        assert_eq!(
            position_at(at(&before).as_ref(), None, true, T0_MS + 2_500),
            Some(12_500)
        );
        // Paused 4 s on, with the same anchor: held at 14 s, however long it stays.
        let held = held_after(&before, &after, None, true, false, T0_MS + 4_000);
        assert_eq!(
            held,
            Some(Held {
                stale: at(&before),
                position_ms: 14_000,
                at_ms: T0_MS + 4_000
            })
        );
        assert_eq!(
            position_at(at(&after).as_ref(), held.as_ref(), false, T0_MS + 60_000),
            Some(14_000)
        );
        // Without the hold, it would jump back.
        assert_eq!(
            position_at(at(&after).as_ref(), None, false, T0_MS + 60_000),
            Some(10_000)
        );
        // Resumed with the same anchor still: on from 14 s, from the resume.
        let resumed = playing();
        let again = held_after(&after, &resumed, held, false, true, T0_MS + 60_000);
        assert_eq!(
            position_at(at(&resumed).as_ref(), again.as_ref(), true, T0_MS + 61_000),
            Some(15_000)
        );
        // A fresh anchor: the engine knows better.
        let mut fresh = after.clone();
        fresh.position = Some(Position {
            position_ms: 14_100,
            timestamp_ms: T0_MS + 4_050,
            speed: 0.0,
        });
        assert_eq!(
            held_after(&after, &fresh, held, false, false, T0_MS + 5_000),
            None
        );
        // Another track: the same.
        let mut other = after.clone();
        other.track.as_mut().unwrap().uri = "spotify:track:other".into();
        assert_eq!(
            held_after(&after, &other, held, false, false, T0_MS + 5_000),
            None
        );
        // No change of status: the hold stays as it is.
        assert_eq!(
            held_after(&after, &after, held, false, false, T0_MS + 9_000),
            held
        );
        assert_eq!(position_at(None, None, true, T0_MS), None);

        // Through the bar: the engine pauses without a fresh position.
        let mut harness = harness(Shown::new(playing()));
        harness.state_mut().advance(Duration::from_secs(4));
        harness.state_mut().engine(after.clone());
        harness.state_mut().advance(Duration::from_secs(60));
        harness.run();
        assert!(drawn(&harness, "0:14"));
    }

    /// Dragging the seek bar stops the clock and sends nothing; the release sends one
    /// `Seek`, and the bar holds there until the engine reports a position.
    #[test]
    fn seek_on_release() {
        let mut harness = harness(Shown::new(playing()));
        let seek = harness.get_by_label(SEEK).rect();
        harness.drag_at(along(seek, 0.2));
        harness.run();
        harness.hover_at(along(seek, 0.5));
        harness.run();
        assert_eq!(harness.state().link.take(), vec![]);
        assert!(drawn(&harness, "1:15"));
        harness.state_mut().advance(Duration::from_secs(30));
        harness.run();
        assert!(drawn(&harness, "1:15"), "no extrapolation while dragged");
        harness.drop_at(along(seek, 0.5));
        harness.run();
        let sent = harness.state().link.take();
        let [Request::Seek { position_ms }] = sent[..] else {
            panic!("{sent:?}");
        };
        assert!((74_000..=76_000).contains(&position_ms), "{position_ms}");
        // Held there, and moving on, until the engine says.
        harness.state_mut().advance(Duration::from_secs(2));
        harness.run();
        assert!(drawn(
            &harness,
            &crate::track_list::duration(position_ms + 2_000)
        ));
        // The engine's fresh position wins.
        let mut moved = playing();
        moved.position = Some(Position {
            position_ms: 100_000,
            timestamp_ms: harness.state().now_ms,
            speed: 1.0,
        });
        harness.state_mut().engine(moved);
        harness.run();
        assert!(drawn(&harness, "1:40"));
        // A failed seek goes back to the engine's position, and says so.
        let seek = harness.get_by_label(SEEK).rect();
        harness.get_by_label(SEEK).hover();
        harness.drag_at(along(seek, 0.1));
        harness.drop_at(along(seek, 0.1));
        harness.run();
        assert_eq!(harness.state().link.take().len(), 1);
        let text = harness
            .state_mut()
            .reply(Err("The player is restarting.".into()));
        assert_eq!(
            text.as_deref(),
            Some("Couldn't seek: The player is restarting.")
        );
        harness.run();
        assert!(drawn(&harness, "1:40"));
    }

    /// Carry 7: `SetVolume` at most every 100 ms; the last value always goes.
    #[test]
    fn volume_at_most_every_100_ms() {
        let t0 = Instant::now();
        let mut t = Throttle::default();
        assert_eq!(t.offer(10, t0), Some(10));
        assert_eq!(t.offer(20, t0 + 30 * MS), None);
        assert_eq!(t.offer(30, t0 + 60 * MS), None);
        assert_eq!(t.wait(t0 + 60 * MS), Some(40 * MS));
        assert_eq!(t.due(t0 + 99 * MS), None);
        assert_eq!(t.due(t0 + 100 * MS), Some(30));
        assert_eq!(t.due(t0 + 300 * MS), None);
        assert_eq!(t.wait(t0 + 300 * MS), None);
        assert_eq!(t.offer(40, t0 + 150 * MS), None);
        assert_eq!(t.offer(50, t0 + 200 * MS), Some(50));
        assert_eq!(t.due(t0 + 400 * MS), None);

        // Through the slider: a drag within one moment sends once, then the last value
        // 100 ms on.
        let mut harness = harness(Shown::new(playing()));
        let volume = harness.get_by_label(VOLUME).rect();
        harness.drag_at(along(volume, 0.2));
        harness.run();
        harness.hover_at(along(volume, 0.5));
        harness.run();
        harness.hover_at(along(volume, 0.8));
        harness.run();
        harness.drop_at(along(volume, 0.8));
        harness.run();
        assert_eq!(
            harness.state().link.take(),
            vec![Request::SetVolume { volume: 20 }]
        );
        assert!(repaint_delay(&harness) <= VOLUME_EVERY);
        harness.state_mut().advance(VOLUME_EVERY);
        harness.run();
        assert_eq!(
            harness.state().link.take(),
            vec![Request::SetVolume { volume: 80 }]
        );
        harness.state_mut().advance(VOLUME_EVERY);
        harness.run();
        assert_eq!(harness.state().link.take(), vec![]);
    }

    /// Carry 10: the title opens its album, each artist their page; under the pointer
    /// they turn white and underline, as a track row's links do.
    #[test]
    fn title_and_artists_are_links() {
        let mut harness = harness(Shown::new(playing()));
        assert_eq!(colour(&harness, "nerve"), Some(theme::TEXT));
        assert_eq!(colour(&harness, "Jane Remover"), Some(theme::SECONDARY));
        assert!(underlines(&harness).is_empty());
        let links = harness.query_all_by_role(Role::Link).count();
        assert_eq!(links, 3);

        harness.get_by_label("nerve").hover();
        harness.run();
        assert_eq!(colour(&harness, "nerve"), Some(theme::TEXT));
        let under = underlines(&harness);
        assert_eq!(under.len(), 1);
        let title = harness.get_by_label("nerve").rect();
        assert!((title.left()..=title.right()).contains(&under[0][0].x));
        harness.get_by_label("nerve").click();
        harness.run();
        harness.get_by_label("Jane Remover").hover();
        harness.run();
        assert_eq!(colour(&harness, "Jane Remover"), Some(theme::TEXT));
        assert_eq!(colour(&harness, "nerve"), Some(theme::TEXT));
        harness.get_by_label("Jane Remover").click();
        harness.run();
        assert_eq!(
            harness.state().actions,
            vec![
                BarAction::GoAlbum(BOY.into()),
                BarAction::GoArtist(JANE.into())
            ]
        );
    }

    /// Carry 9: the heart flips at once; it holds until the engine answers, takes the
    /// answer, and reverts on a failure, saying so. Final review m1/C1: it asks for the
    /// state it flipped to, for the track it shows, so a track change in between, or an
    /// engine that doesn't know the heart yet, can't make it write anything else.
    #[test]
    fn heart_flips_and_takes_the_answer() {
        let mut harness = harness(Shown::new(playing()));
        assert!(!toggled(&harness, HEART_LABEL));
        assert_eq!(colour(&harness, "♡"), Some(theme::LINES));
        harness.get_by_label(HEART_LABEL).click();
        harness.run();
        assert_eq!(
            harness.state().link.take(),
            vec![Request::SetLiked {
                uri: NERVE.into(),
                liked: true
            }]
        );
        assert!(toggled(&harness, HEART_LABEL));
        assert_eq!(colour(&harness, "♥"), Some(theme::ACCENT));
        // A slow write: still liked 5 s on.
        harness.state_mut().advance(Duration::from_secs(5));
        harness.run();
        assert!(toggled(&harness, HEART_LABEL));
        let answer = Reply::Liked {
            uri: NERVE.into(),
            liked: true,
        };
        assert_eq!(harness.state_mut().reply(Ok(answer)), None);
        let mut liked = playing();
        liked.liked = Some(true);
        harness.state_mut().engine(liked);
        harness.run();
        assert!(toggled(&harness, HEART_LABEL));

        // Unliking fails: back to liked, and why.
        harness.get_by_label(HEART_LABEL).click();
        harness.run();
        assert_eq!(
            harness.state().link.take(),
            vec![Request::SetLiked {
                uri: NERVE.into(),
                liked: false
            }]
        );
        assert!(!toggled(&harness, HEART_LABEL));
        let text = harness
            .state_mut()
            .reply(Err("Couldn't like: Spotify answered 503".into()));
        assert_eq!(text.as_deref(), Some("Couldn't like: Spotify answered 503"));
        harness.run();
        assert!(toggled(&harness, HEART_LABEL));
    }

    /// Shuffle toggles, repeat cycles off → the list → the track; each flips at once.
    #[test]
    fn shuffle_and_repeat_flip() {
        let mut harness = harness(Shown::new(playing()));
        assert!(!toggled(&harness, SHUFFLE));
        harness.get_by_label(SHUFFLE).click();
        harness.run();
        assert_eq!(
            harness.state().link.take(),
            vec![Request::SetShuffle { on: true }]
        );
        assert!(toggled(&harness, SHUFFLE));
        let mut modes = vec![];
        for _ in 0..3 {
            harness.get_by_label(REPEAT).click();
            harness.run();
            modes.extend(harness.state().link.take());
        }
        assert_eq!(
            modes,
            [Repeat::Context, Repeat::Track, Repeat::Off].map(|mode| Request::SetRepeat { mode })
        );
        assert!(!toggled(&harness, REPEAT));
    }

    /// Carry 1: the first queued track's cover, as one URL.
    #[test]
    fn next_cover_is_the_first_queued() {
        assert_eq!(next_cover(&[]), None);
        let mut second = track();
        second.cover_url = Some("https://i.scdn.co/image/second".into());
        assert_eq!(
            next_cover(&[track(), second.clone()]),
            Some(Image {
                url: "https://i.scdn.co/image/nerve".into(),
                width: None
            })
        );
        let mut bare = track();
        bare.cover_url = None;
        assert_eq!(next_cover(&[bare, second]), None);
    }

    /// Low brightness: the surface grey, secondary text, quiet controls, and nothing
    /// white but the title.
    #[test]
    fn bar_is_dim() {
        let harness = harness(Shown::new(playing()));
        let bar = fills(&harness)
            .into_iter()
            .find(|(r, c)| *c == theme::SURFACE && r.height() >= HEIGHT - 1.0);
        assert!(bar.is_some(), "no surface-grey bar");
        for text in ["2hollis", "Jane Remover", "0:10", "2:30", "FLAC 44.1 kHz"] {
            assert_eq!(colour(&harness, text), Some(theme::SECONDARY), "{text}");
        }
        for glyph in ["⏮", "⏸", "⏭"] {
            assert_eq!(colour(&harness, glyph), Some(theme::QUIET), "{glyph}");
        }
        let bright: Vec<String> = texts(&harness)
            .into_iter()
            .filter(|(_, c, _)| *c == theme::TEXT || *c == theme::ACCENT)
            .map(|(t, _, _)| t)
            .collect();
        assert_eq!(bright, ["nerve"]);
    }

    /// Look 2 fix 1: the panel's toggle (36 square, its dot) and the line under the volume
    /// (the status, or the badge in its outline) never meet, however long the status.
    #[test]
    fn panel_toggle_clears_the_status_line() {
        let mut elsewhere = playing();
        elsewhere.active_here = false;
        for (player, line) in [(elsewhere, ELSEWHERE), (playing(), "FLAC 44.1 kHz")] {
            let mut shown = Shown::new(player);
            shown.panel = true;
            let harness = harness(shown);
            let toggle = harness.get_by_label(PANEL).rect();
            let dots: Vec<Rect> = test_support::circles(&harness)
                .into_iter()
                .filter(|(c, _, _)| toggle.contains(*c))
                .map(|(c, r, _)| Rect::from_center_size(c, Vec2::splat(2.0 * r)))
                .collect();
            assert_eq!(dots.len(), 1, "the toggle's on-dot");
            let words = harness.get_by_label(line).rect();
            let mut taken = vec![words];
            taken.extend(
                test_support::outlines(&harness)
                    .into_iter()
                    .filter(|(r, _)| r.contains_rect(words))
                    .map(|(r, _)| r),
            );
            let bar = fills(&harness)
                .into_iter()
                .find(|(r, c)| *c == theme::SURFACE && r.height() >= HEIGHT - 1.0)
                .map(|(r, _)| r)
                .expect("the bar");
            for rect in &taken {
                assert!(
                    !toggle.intersects(*rect),
                    "{line}: {toggle:?} meets {rect:?}"
                );
                assert!(!dots[0].intersects(*rect), "{line}: dot meets {rect:?}");
                assert!(bar.contains_rect(*rect), "{line}: {rect:?} leaves {bar:?}");
            }
        }
    }

    /// Look 2: the controls' icons are a third bigger, their squares to click with them;
    /// a toggle that is on is in the accent, under the pointer too.
    #[test]
    fn icons_are_a_third_bigger() {
        let mut harness = harness(Shown::new(playing()));
        assert_eq!(size_of(&harness, "⏮"), Some(theme::ICON));
        assert_eq!(size_of(&harness, "⏭"), Some(theme::ICON));
        assert_eq!(size_of(&harness, "♡"), Some(theme::ICON));
        assert_eq!(size_of(&harness, "⏸"), Some(theme::ICON_LARGE));
        for label in [SHUFFLE, PREVIOUS, PAUSE, NEXT, REPEAT, PANEL, HEART_LABEL] {
            let rect = harness.get_by_label(label).rect();
            assert_eq!(rect.size(), Vec2::splat(BUTTON), "{label}");
        }
        for label in [SHUFFLE, REPEAT, PANEL] {
            let rect = harness.get_by_label(label).rect();
            let icon = drawn_within(&harness, rect);
            assert!(
                (icon.width() - theme::ICON_DRAWN).abs() <= 2.0,
                "{label}: {icon:?}"
            );
            assert!(
                marks_within(&harness, rect)
                    .iter()
                    .all(|c| *c == theme::QUIET),
                "{label} at rest"
            );
        }

        harness.get_by_label(SHUFFLE).click();
        harness.run();
        assert!(toggled(&harness, SHUFFLE));
        let shuffle = harness.get_by_label(SHUFFLE).rect();
        let marks = marks_within(&harness, shuffle);
        assert!(!marks.is_empty());
        assert!(marks.iter().all(|c| *c == theme::ACCENT), "{marks:?}");
    }
}
