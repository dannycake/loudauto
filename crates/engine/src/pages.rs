//! Pages for the window, answered from a disk cache at once and refreshed in the
//! background (stale-while-revalidate), so a page is never blank and still opens
//! offline.
//!
//! - A cached copy is the reply at once, marked stale. A refresh then fetches the page
//!   on the Background lane (Prefetch, for a prefetch) and announces the result as
//!   `Event::PageUpdated`: the new copy, or the old one again, still stale, with the
//!   error. A refresh the Prefetch lane refuses announces nothing. A copy younger than
//!   [`FRESH_FOR_MS`] is the reply as it is, not stale, and isn't refreshed.
//! - An uncached page is fetched on the User lane (Prefetch, for a prefetch), and the
//!   caller waits for it at most [`UNCACHED_LIMIT`]. A fetch that outlives the wait
//!   carries on: if it lands, it is cached and announced.
//! - No fetch runs longer than [`FLIGHT_LIMIT`]; one that does fails, so a hung request
//!   never wedges its page.
//! - A fetch waits for the session to be ready, at most [`SESSION_WAIT`]: a page asked for
//!   as the engine starts doesn't fail "not logged in" a second before the session is up.
//! - One fetch per page at a time: whoever asks for a page that is being fetched shares
//!   that fetch, and a refresh while one runs is that one.
//! - Each page is one file, `<dir>/<sha256(json(req))[..32]>.json`, holding
//!   `{"fetched_at_ms":…,"page":…}`. Search results are never written. A file that
//!   can't be read or parsed counts as absent.

use std::{
    collections::HashMap,
    io::ErrorKind,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use async_trait::async_trait;
use engine_ipc::proto::{Event, Page, PageReply, PageRequest, Reply};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spot_core::{
    SpotError, files,
    library::Library,
    pages::LikedPage,
    web::{LIKED_PAGE_SIZE, Web, lanes::Lane},
};
use tokio::sync::{broadcast, watch};

/// A cached copy younger than this, in milliseconds, is the answer as it is: fresh, with
/// no refresh behind it (Task 14/23 ruling, final pass).
pub const FRESH_FOR_MS: i64 = 60_000;
/// How long a caller waits for a page that isn't cached.
pub const UNCACHED_LIMIT: Duration = Duration::from_secs(15);
/// The error for an uncached page that took longer than [`UNCACHED_LIMIT`].
pub const TIMEOUT_MESSAGE: &str = "Spotify didn't answer in 15 s.";
/// The most one fetch may take, however many requests it makes: long enough to walk
/// all of a large Liked Songs on the Background lane. A fetch that hangs (librespot's
/// rootlist request has no timeout of its own) then ends, so the next request for the
/// page fetches afresh instead of joining it.
pub const FLIGHT_LIMIT: Duration = Duration::from_secs(3 * 60);
/// How long a fetch waits for the session to be ready before it goes ahead anyway (and
/// fails, if the session still isn't): well within [`UNCACHED_LIMIT`].
pub const SESSION_WAIT: Duration = Duration::from_secs(5);
/// The error for a fetch that took longer than [`FLIGHT_LIMIT`].
pub const FLIGHT_TIMEOUT_MESSAGE: &str = "Spotify didn't answer in 3 min.";
/// The error when a fetch ended without an outcome (its task panicked).
const STOPPED_MESSAGE: &str = "Fetching the page stopped unexpectedly.";

/// Where pages come from: Spotify, or a fake in tests.
#[async_trait]
pub trait PageSource: Send + Sync {
    /// Fetches `req` on `lane`. A Prefetch the lane refuses is `SpotError::Dropped`.
    async fn fetch(&self, lane: Lane, req: &PageRequest) -> spot_core::Result<Page>;
}

/// [`PageSource`] for the live session: pathfinder pages through `Web`, the sidebar
/// through `Library`.
pub struct WebPageSource {
    web: Arc<Web>,
    library: Arc<Library>,
}

impl WebPageSource {
    pub fn new(web: Arc<Web>, library: Arc<Library>) -> Self {
        Self { web, library }
    }

    /// All of Liked Songs in one page: the first on `lane`, then one every
    /// [`LIKED_PAGE_SIZE`] up to `total` on Background.
    async fn liked(&self, lane: Lane) -> spot_core::Result<LikedPage> {
        let mut liked = self.web.liked_page(lane, 0).await?;
        for offset in (LIKED_PAGE_SIZE..liked.total).step_by(LIKED_PAGE_SIZE as usize) {
            let page = self.web.liked_page(Lane::Background, offset).await?;
            liked.tracks.extend(page.tracks);
        }
        Ok(liked)
    }
}

#[async_trait]
impl PageSource for WebPageSource {
    async fn fetch(&self, lane: Lane, req: &PageRequest) -> spot_core::Result<Page> {
        let web = &self.web;
        Ok(match req {
            PageRequest::Home => Page::Home(web.home(lane).await?),
            PageRequest::Search { query } => Page::Search(web.search(lane, query).await?),
            PageRequest::Artist { uri } => Page::Artist(web.artist(lane, uri).await?),
            PageRequest::Album { uri } => Page::Album(web.album(lane, uri).await?),
            PageRequest::Playlist { uri } => Page::Playlist(web.playlist(lane, uri).await?),
            PageRequest::Liked => Page::Liked(self.liked(lane).await?),
            PageRequest::Recents => Page::Recents(web.recents(lane).await?),
            PageRequest::Sidebar => Page::Sidebar(self.library.rootlist(lane).await?),
        })
    }
}

/// Answers page requests; see the module docs. Clones share the fetches under way.
#[derive(Clone)]
pub struct PageService {
    source: Arc<dyn PageSource>,
    dir: PathBuf,
    events: broadcast::Sender<Event>,
    flights: Arc<Mutex<Flights>>,
    /// Whether the session is ready, when the service waits for it.
    session: Option<watch::Receiver<bool>>,
}

/// A page as fetched at a moment: what a cache file holds.
#[derive(Clone, Serialize, Deserialize)]
struct Cached {
    fetched_at_ms: i64,
    page: Page,
}

/// How a fetch ended.
#[derive(Clone)]
enum Outcome {
    Fetched(Cached),
    /// The Prefetch lane refused it.
    Dropped,
    Failed(String),
}

#[derive(Default)]
struct Flights {
    next_id: u64,
    by_req: HashMap<PageRequest, Flight>,
}

/// A fetch under way, shared by everyone who asks for its page meanwhile.
struct Flight {
    id: u64,
    /// The lane it is fetching on.
    lane: Lane,
    /// `None` until it ends.
    done: watch::Receiver<Option<Outcome>>,
    /// Where to fetch again if the Prefetch lane refuses it: someone who mustn't be
    /// dropped joined it.
    retry_on: Option<Lane>,
    /// Announce the outcome as `Event::PageUpdated`: a refresh joined it, or a caller
    /// stopped waiting for it.
    announce: bool,
    /// The cached copy a refresh replaces, announced again with the error on failure.
    /// Boxed: a page is large, and most flights have none.
    old: Option<Box<Cached>>,
}

/// What a flight does once a fetch ends.
enum Landing {
    /// Fetch again on this lane.
    Again(Lane),
    /// It is over: the flight, out of the map.
    Over(Option<Flight>),
}

impl PageService {
    pub fn new(
        source: Arc<dyn PageSource>,
        dir: PathBuf,
        events: broadcast::Sender<Event>,
    ) -> Self {
        Self {
            source,
            dir,
            events,
            flights: Arc::default(),
            session: None,
        }
    }

    /// The service, its fetches waiting (at most [`SESSION_WAIT`]) for `ready`, whether
    /// the session is ready.
    pub fn waiting_for_session(self, ready: watch::Receiver<bool>) -> Self {
        Self {
            session: Some(ready),
            ..self
        }
    }

    /// `$XDG_CACHE_HOME/loudauto/pages`, else under `~/.cache`.
    pub fn default_dir() -> PathBuf {
        files::cache_dir().join("pages")
    }

    /// Answers `req`: the cached copy at once, refreshed afterwards unless it is younger
    /// than [`FRESH_FOR_MS`], else a fetch the caller waits for at most
    /// [`UNCACHED_LIMIT`]. A `prefetch` uses the Prefetch lane and may be answered
    /// `Reply::Dropped`. `now_ms` stamps what this call fetches; `Err` is the message the
    /// window shows.
    pub async fn get(
        &self,
        req: PageRequest,
        prefetch: bool,
        now_ms: i64,
    ) -> Result<Reply, String> {
        if let Some(old) = self.read_cache(&req).await {
            let age = now_ms - old.fetched_at_ms;
            if (0..FRESH_FOR_MS).contains(&age) {
                return Ok(Reply::Page(PageReply {
                    page: old.page,
                    fetched_at_ms: old.fetched_at_ms,
                    stale: false,
                    error: None,
                }));
            }
            let reply = PageReply {
                page: old.page.clone(),
                fetched_at_ms: old.fetched_at_ms,
                stale: true,
                error: None,
            };
            let lane = if prefetch {
                Lane::Prefetch
            } else {
                Lane::Background
            };
            self.join(&req, lane, now_ms, Some(old));
            return Ok(Reply::Page(reply));
        }
        let lane = if prefetch { Lane::Prefetch } else { Lane::User };
        let (id, mut done) = self.join(&req, lane, now_ms, None);
        let waited = tokio::time::timeout(UNCACHED_LIMIT, async {
            // An error means the flight ended without sending: its task panicked.
            done.wait_for(Option::is_some)
                .await
                .ok()
                .and_then(|o| o.clone())
        })
        .await;
        match waited {
            Err(_elapsed) => {
                self.give_up(&req, id);
                Err(TIMEOUT_MESSAGE.into())
            }
            Ok(Some(Outcome::Fetched(new))) => Ok(Reply::Page(PageReply {
                page: new.page,
                fetched_at_ms: new.fetched_at_ms,
                stale: false,
                error: None,
            })),
            Ok(Some(Outcome::Dropped)) => Ok(Reply::Dropped),
            Ok(Some(Outcome::Failed(error))) => Err(error),
            Ok(None) => Err(STOPPED_MESSAGE.into()),
        }
    }

    /// Joins the fetch of `req` under way, or starts one on `lane`; returns the flight's
    /// id and where its outcome will appear. `old` makes this a refresh: the outcome is
    /// announced, and a failure announces `old` again.
    fn join(
        &self,
        req: &PageRequest,
        lane: Lane,
        now_ms: i64,
        old: Option<Cached>,
    ) -> (u64, watch::Receiver<Option<Outcome>>) {
        let mut flights = self.flights();
        if let Some(flight) = flights.by_req.get_mut(req) {
            if flight.lane == Lane::Prefetch && lane != Lane::Prefetch {
                let user = lane == Lane::User || flight.retry_on == Some(Lane::User);
                flight.retry_on = Some(if user { Lane::User } else { Lane::Background });
            }
            if let Some(old) = old {
                flight.announce = true;
                flight.old.get_or_insert_with(|| Box::new(old));
            }
            return (flight.id, flight.done.clone());
        }
        let id = flights.next_id;
        flights.next_id += 1;
        let (tx, done) = watch::channel(None);
        flights.by_req.insert(
            req.clone(),
            Flight {
                id,
                lane,
                done: done.clone(),
                retry_on: None,
                announce: old.is_some(),
                old: old.map(Box::new),
            },
        );
        drop(flights);
        tokio::spawn(self.clone().fly(req.clone(), lane, now_ms, id, tx));
        (id, done)
    }

    /// Runs flight `id` to its end: fetches, caches what it got, hands the outcome to
    /// everyone waiting and announces it if asked to.
    async fn fly(
        self,
        req: PageRequest,
        mut lane: Lane,
        now_ms: i64,
        id: u64,
        tx: watch::Sender<Option<Outcome>>,
    ) {
        let _cleanup = Cleanup {
            flights: &self.flights,
            req: &req,
            id,
        };
        if let Some(mut ready) = self.session.clone()
            && !*ready.borrow_and_update()
        {
            // Not ready by then: the fetch says why itself.
            let _ = tokio::time::timeout(SESSION_WAIT, ready.wait_for(|ready| *ready)).await;
        }
        loop {
            let fetched = tokio::time::timeout(FLIGHT_LIMIT, self.source.fetch(lane, &req)).await;
            let outcome = match fetched {
                Ok(Ok(page)) => Outcome::Fetched(Cached {
                    fetched_at_ms: now_ms,
                    page,
                }),
                Ok(Err(SpotError::Dropped)) => Outcome::Dropped,
                Ok(Err(e)) => Outcome::Failed(e.to_string()),
                Err(_elapsed) => Outcome::Failed(FLIGHT_TIMEOUT_MESSAGE.into()),
            };
            if let Outcome::Fetched(new) = &outcome {
                self.write_cache(&req, new).await;
            }
            match self.land(&req, id, &outcome, &tx) {
                Landing::Again(retry) => lane = retry,
                Landing::Over(flight) => {
                    if let Some(flight) = flight {
                        self.announce(req.clone(), flight, outcome);
                    }
                    return;
                }
            }
        }
    }

    /// Ends flight `id` with `outcome`, unless the Prefetch lane refused it and someone
    /// who mustn't be dropped joined: then it fetches again on their lane. Taking the
    /// flight out of the map and sending the outcome happen under one lock, so a
    /// request either joins this flight and gets its outcome or starts a new one.
    fn land(
        &self,
        req: &PageRequest,
        id: u64,
        outcome: &Outcome,
        tx: &watch::Sender<Option<Outcome>>,
    ) -> Landing {
        let mut flights = self.flights();
        let mine = flights.by_req.get_mut(req).filter(|f| f.id == id);
        if let Some(flight) = mine
            && matches!(outcome, Outcome::Dropped)
            && let Some(retry) = flight.retry_on.take()
        {
            flight.lane = retry;
            return Landing::Again(retry);
        }
        let flight = match flights.by_req.get(req) {
            Some(f) if f.id == id => flights.by_req.remove(req),
            _ => None,
        };
        tx.send_replace(Some(outcome.clone()));
        Landing::Over(flight)
    }

    /// Announces a flight's outcome as `Event::PageUpdated`, if it was asked to.
    fn announce(&self, req: PageRequest, flight: Flight, outcome: Outcome) {
        if !flight.announce {
            return;
        }
        let reply = match (outcome, flight.old) {
            (Outcome::Fetched(new), _) => PageReply {
                page: new.page,
                fetched_at_ms: new.fetched_at_ms,
                stale: false,
                error: None,
            },
            (Outcome::Failed(error), Some(old)) => {
                let Cached {
                    fetched_at_ms,
                    page,
                } = *old;
                PageReply {
                    page,
                    fetched_at_ms,
                    stale: true,
                    error: Some(error),
                }
            }
            // Refused, or failed with nothing on screen to mark: the caller has its answer.
            _ => return,
        };
        // With no window connected nobody listens; the cache holds the page for the next.
        let _ = self.events.send(Event::PageUpdated { req, reply });
    }

    /// A caller stopped waiting for flight `id`: if it lands, announce it.
    fn give_up(&self, req: &PageRequest, id: u64) {
        if let Some(flight) = self.flights().by_req.get_mut(req).filter(|f| f.id == id) {
            flight.announce = true;
        }
    }

    /// `<dir>/<sha256(json(req))[..32]>.json`; `None` for Search, which is never cached.
    fn cache_path(&self, req: &PageRequest) -> Option<PathBuf> {
        if matches!(req, PageRequest::Search { .. }) {
            return None;
        }
        let json = serde_json::to_string(req).ok()?;
        let hash = format!("{:x}", Sha256::digest(json.as_bytes()));
        Some(self.dir.join(format!("{}.json", &hash[..32])))
    }

    async fn read_cache(&self, req: &PageRequest) -> Option<Cached> {
        let path = self.cache_path(req)?;
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == ErrorKind::NotFound => return None,
            Err(e) => {
                log::warn!("ignoring unreadable page cache {}: {e}", path.display());
                return None;
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(cached) => Some(cached),
            Err(e) => {
                log::warn!("ignoring malformed page cache {}: {e}", path.display());
                None
            }
        }
    }

    /// Saves `cached` for `req`. A failed save is logged: the page was still fetched.
    async fn write_cache(&self, req: &PageRequest, cached: &Cached) {
        let Some(path) = self.cache_path(req) else {
            return;
        };
        let written = match serde_json::to_vec(cached) {
            Ok(bytes) => files::write_atomic(&path, &bytes).await,
            Err(e) => Err(e.into()),
        };
        if let Err(e) = written {
            log::warn!("couldn't cache page {}: {e}", path.display());
        }
    }

    fn flights(&self) -> MutexGuard<'_, Flights> {
        lock(&self.flights)
    }
}

/// Takes a flight out of the map however its task ends, a panic included, so the next
/// request for the page starts afresh instead of joining a fetch that will never end.
struct Cleanup<'a> {
    flights: &'a Mutex<Flights>,
    req: &'a PageRequest,
    id: u64,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let mut flights = lock(self.flights);
        if flights
            .by_req
            .get(self.req)
            .is_some_and(|f| f.id == self.id)
        {
            flights.by_req.remove(self.req);
        }
    }
}

// Nothing panics while holding the lock, so a poisoned one is still consistent.
fn lock(flights: &Mutex<Flights>) -> MutexGuard<'_, Flights> {
    flights.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        path::Path,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use spot_core::pages::{LikedPage, SearchResults};
    use tokio::{sync::broadcast::error::TryRecvError, time::Instant};

    use super::*;

    const T0: i64 = 1_000;
    /// Ten minutes on: a copy from `T0` is past the freshness window.
    const T1: i64 = T0 + 10 * 60_000;

    /// What one `fetch` of the fake answers.
    enum Step {
        Page(Page),
        Fail(&'static str),
        Dropped,
        Panic,
    }

    /// A source that answers from a script, one step per call, each after `delay`.
    struct FakeSource {
        script: Mutex<VecDeque<Step>>,
        delay: Duration,
        calls: AtomicUsize,
        lanes: Mutex<Vec<Lane>>,
    }

    impl FakeSource {
        fn new(delay: Duration, script: impl IntoIterator<Item = Step>) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(script.into_iter().collect()),
                delay,
                calls: AtomicUsize::new(0),
                lanes: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn lanes(&self) -> Vec<Lane> {
            self.lanes.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl PageSource for FakeSource {
        async fn fetch(&self, lane: Lane, _req: &PageRequest) -> spot_core::Result<Page> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.lanes.lock().unwrap().push(lane);
            let step = self.script.lock().unwrap().pop_front();
            tokio::time::sleep(self.delay).await;
            match step.expect("a fetch the test didn't script") {
                Step::Page(page) => Ok(page),
                Step::Fail(msg) => Err(SpotError::Net(msg.into())),
                Step::Dropped => Err(SpotError::Dropped),
                Step::Panic => panic!("the source panicked"),
            }
        }
    }

    /// Liked Songs, told apart by `total`.
    fn liked(total: u32) -> Page {
        Page::Liked(LikedPage {
            total,
            tracks: Vec::new(),
        })
    }

    fn search_results() -> Page {
        Page::Search(SearchResults {
            tracks: Vec::new(),
            artists: Vec::new(),
            albums: Vec::new(),
            playlists: Vec::new(),
        })
    }

    fn reply(page: Page, fetched_at_ms: i64, stale: bool) -> PageReply {
        PageReply {
            page,
            fetched_at_ms,
            stale,
            error: None,
        }
    }

    fn service(source: Arc<FakeSource>, dir: &Path) -> (PageService, broadcast::Receiver<Event>) {
        let (events, rx) = broadcast::channel(16);
        (PageService::new(source, dir.to_path_buf(), events), rx)
    }

    fn page_of(reply: Result<Reply, String>) -> PageReply {
        match reply {
            Ok(Reply::Page(page)) => page,
            other => panic!("expected a page, got {other:?}"),
        }
    }

    /// The next event, which must be `PageUpdated` for `req`.
    async fn updated(events: &mut broadcast::Receiver<Event>, req: &PageRequest) -> PageReply {
        match events.recv().await.unwrap() {
            Event::PageUpdated { req: got, reply } => {
                assert_eq!(&got, req);
                reply
            }
            other => panic!("expected PageUpdated, got {other:?}"),
        }
    }

    fn seed(pages: &PageService, req: &PageRequest, fetched_at_ms: i64, page: Page) {
        let path = pages.cache_path(req).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = Cached {
            fetched_at_ms,
            page,
        };
        std::fs::write(path, serde_json::to_vec(&file).unwrap()).unwrap();
    }

    fn cached(pages: &PageService, req: &PageRequest) -> Option<(i64, Page)> {
        let bytes = std::fs::read(pages.cache_path(req)?).ok()?;
        let file: Cached = serde_json::from_slice(&bytes).unwrap();
        Some((file.fetched_at_ms, file.page))
    }

    fn files(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn cached_reply_is_immediate_then_updates() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeSource::new(Duration::from_secs(10), [Step::Page(liked(2))]);
        let (pages, mut events) = service(source.clone(), dir.path());
        seed(&pages, &PageRequest::Liked, T0, liked(1));

        let started = Instant::now();
        let got = page_of(pages.get(PageRequest::Liked, false, T1).await);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "waited on the fetch"
        );
        assert_eq!(got, reply(liked(1), T0, true));

        let update = updated(&mut events, &PageRequest::Liked).await;
        assert_eq!(update, reply(liked(2), T1, false));
        assert_eq!(source.lanes(), [Lane::Background]);
        assert_eq!(cached(&pages, &PageRequest::Liked), Some((T1, liked(2))));
    }

    #[tokio::test]
    async fn uncached_fetch_writes_cache() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeSource::new(Duration::ZERO, [Step::Page(liked(1))]);
        let (pages, _events) = service(source.clone(), dir.path());

        let got = page_of(pages.get(PageRequest::Liked, false, T1).await);
        assert_eq!(got, reply(liked(1), T1, false));
        assert_eq!(source.lanes(), [Lane::User]);

        let names = files(dir.path());
        assert_eq!(names.len(), 1, "{names:?}");
        let (stem, ext) = names[0].split_once('.').unwrap();
        assert_eq!(ext, "json");
        assert_eq!(stem.len(), 32);
        assert!(stem.bytes().all(|b| b.is_ascii_hexdigit()), "{stem}");
        assert_eq!(cached(&pages, &PageRequest::Liked), Some((T1, liked(1))));

        // A later engine answers from the file, even offline.
        let offline = FakeSource::new(Duration::ZERO, [Step::Fail("offline")]);
        let (again, _events) = service(offline, dir.path());
        let got = page_of(again.get(PageRequest::Liked, false, T1 + 2 * 60_000).await);
        assert_eq!(got, reply(liked(1), T1, true));
    }

    #[tokio::test]
    async fn cache_names_differ_per_request() {
        let dir = tempfile::tempdir().unwrap();
        let (pages, _events) = service(FakeSource::new(Duration::ZERO, []), dir.path());
        let album = |uri: &str| PageRequest::Album { uri: uri.into() };
        assert_eq!(
            pages.cache_path(&album("spotify:album:a")),
            pages.cache_path(&album("spotify:album:a"))
        );
        assert_ne!(
            pages.cache_path(&album("spotify:album:a")),
            pages.cache_path(&album("spotify:album:b"))
        );
        assert_ne!(
            pages.cache_path(&PageRequest::Home),
            pages.cache_path(&PageRequest::Recents)
        );
    }

    /// Task 14/23 ruling (final pass): a cached copy under a minute old is the answer as
    /// it is, fresh, with no refresh behind it (each landing on Liked Songs would
    /// otherwise walk all its pages again); an older one refreshes as before.
    #[tokio::test(start_paused = true)]
    async fn a_copy_under_a_minute_old_is_not_refreshed() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeSource::new(Duration::ZERO, [Step::Page(liked(2))]);
        let (pages, mut events) = service(source.clone(), dir.path());
        seed(&pages, &PageRequest::Liked, T0, liked(1));

        let just_now = T0 + FRESH_FOR_MS - 1;
        let got = page_of(pages.get(PageRequest::Liked, false, just_now).await);
        assert_eq!(got, reply(liked(1), T0, false));
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(source.calls(), 0, "refreshed a fresh copy");
        assert_eq!(events.try_recv().unwrap_err(), TryRecvError::Empty);

        let later = T0 + FRESH_FOR_MS;
        let got = page_of(pages.get(PageRequest::Liked, false, later).await);
        assert_eq!(got, reply(liked(1), T0, true));
        let update = updated(&mut events, &PageRequest::Liked).await;
        assert_eq!(update, reply(liked(2), later, false));
    }

    #[tokio::test]
    async fn network_down_serves_stale_with_error() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeSource::new(Duration::ZERO, [Step::Fail("connection refused")]);
        let (pages, mut events) = service(source, dir.path());
        seed(&pages, &PageRequest::Liked, T0, liked(1));

        let got = page_of(pages.get(PageRequest::Liked, false, T1).await);
        assert_eq!(got, reply(liked(1), T0, true));

        let update = updated(&mut events, &PageRequest::Liked).await;
        assert_eq!(update.page, liked(1));
        assert_eq!(update.fetched_at_ms, T0);
        assert!(update.stale);
        let error = update.error.expect("the refresh's error");
        assert!(error.contains("connection refused"), "{error}");
        assert_eq!(cached(&pages, &PageRequest::Liked), Some((T0, liked(1))));
    }

    /// Final review I4: a fetch waits for the session to be ready, at most
    /// `SESSION_WAIT`, so a page asked for as the engine starts (the window starts it)
    /// doesn't fail "not logged in" a second before the session is up.
    #[tokio::test(start_paused = true)]
    async fn a_fetch_waits_for_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeSource::new(Duration::ZERO, [Step::Page(liked(1)), Step::Page(liked(2))]);
        let (pages, _events) = service(source.clone(), dir.path());
        let (ready_tx, ready) = watch::channel(false);
        let pages = pages.waiting_for_session(ready);

        let started = Instant::now();
        let get = tokio::spawn({
            let pages = pages.clone();
            async move { pages.get(PageRequest::Home, false, T1).await }
        });
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(source.calls(), 0, "fetched before the session was ready");
        ready_tx.send_replace(true);
        assert_eq!(page_of(get.await.unwrap()).page, liked(1));
        assert_eq!(started.elapsed(), Duration::from_secs(2));

        // A session that doesn't come: the fetch goes ahead after SESSION_WAIT.
        ready_tx.send_replace(false);
        let started = Instant::now();
        let got = page_of(pages.get(PageRequest::Recents, false, T1).await);
        assert_eq!(got.page, liked(2));
        assert_eq!(started.elapsed(), SESSION_WAIT);
        assert!(
            SESSION_WAIT < UNCACHED_LIMIT,
            "the caller still gets its answer"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn uncached_timeout_fails_in_15s() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeSource::new(Duration::from_secs(60), [Step::Page(liked(1))]);
        let (pages, _events) = service(source, dir.path());

        let started = Instant::now();
        let err = pages.get(PageRequest::Liked, false, T1).await.unwrap_err();
        let waited = started.elapsed();
        assert_eq!(err, "Spotify didn't answer in 15 s.");
        assert!(
            waited >= Duration::from_secs(15) && waited < Duration::from_secs(16),
            "{waited:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn timed_out_fetch_still_lands() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeSource::new(Duration::from_secs(20), [Step::Page(liked(1))]);
        let (pages, mut events) = service(source, dir.path());

        let err = pages.get(PageRequest::Liked, false, T1).await.unwrap_err();
        assert_eq!(err, TIMEOUT_MESSAGE);

        let update = updated(&mut events, &PageRequest::Liked).await;
        assert_eq!(update, reply(liked(1), T1, false));
        assert_eq!(cached(&pages, &PageRequest::Liked), Some((T1, liked(1))));
    }

    #[tokio::test]
    async fn search_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeSource::new(
            Duration::ZERO,
            [Step::Page(search_results()), Step::Page(search_results())],
        );
        let (pages, _events) = service(source.clone(), dir.path());
        let search = PageRequest::Search {
            query: "jane".into(),
        };

        for _ in 0..2 {
            let got = page_of(pages.get(search.clone(), false, T1).await);
            assert_eq!(got, reply(search_results(), T1, false));
        }
        assert_eq!(source.calls(), 2);
        assert_eq!(pages.cache_path(&search), None);
        assert!(files(dir.path()).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_requests_share_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeSource::new(Duration::from_secs(1), [Step::Page(liked(1))]);
        let (pages, _events) = service(source.clone(), dir.path());

        let (a, b) = tokio::join!(
            pages.get(PageRequest::Liked, false, T1),
            pages.get(PageRequest::Liked, false, T1)
        );
        assert_eq!(page_of(a), reply(liked(1), T1, false));
        assert_eq!(page_of(b), reply(liked(1), T1, false));
        assert_eq!(source.calls(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_runs_once_while_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeSource::new(Duration::from_secs(1), [Step::Page(liked(2))]);
        let (pages, mut events) = service(source.clone(), dir.path());
        seed(&pages, &PageRequest::Liked, T0, liked(1));

        for _ in 0..2 {
            let got = page_of(pages.get(PageRequest::Liked, false, T1).await);
            assert_eq!(got, reply(liked(1), T0, true));
        }
        let update = updated(&mut events, &PageRequest::Liked).await;
        assert_eq!(update, reply(liked(2), T1, false));
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(source.calls(), 1);
        assert_eq!(events.try_recv().unwrap_err(), TryRecvError::Empty);
    }

    #[tokio::test]
    async fn prefetch_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeSource::new(Duration::ZERO, [Step::Dropped]);
        let (pages, _events) = service(source.clone(), dir.path());

        let got = pages.get(PageRequest::Liked, true, T1).await;
        assert_eq!(got, Ok(Reply::Dropped));
        assert_eq!(source.lanes(), [Lane::Prefetch]);
        assert!(files(dir.path()).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn user_request_outlives_a_refused_prefetch() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeSource::new(
            Duration::from_secs(1),
            [Step::Dropped, Step::Page(liked(1))],
        );
        let (pages, _events) = service(source.clone(), dir.path());

        let prefetch = tokio::spawn({
            let pages = pages.clone();
            async move { pages.get(PageRequest::Liked, true, T1).await }
        });
        // The prefetch's fetch is under way when the user asks for the same page.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(source.calls(), 1);
        let got = page_of(pages.get(PageRequest::Liked, false, T1).await);
        assert_eq!(got, reply(liked(1), T1, false));
        assert_eq!(source.lanes(), [Lane::Prefetch, Lane::User]);
        assert_eq!(page_of(prefetch.await.unwrap()), reply(liked(1), T1, false));
    }

    #[tokio::test]
    async fn garbage_cache_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeSource::new(Duration::ZERO, [Step::Page(liked(1))]);
        let (pages, _events) = service(source.clone(), dir.path());
        std::fs::write(pages.cache_path(&PageRequest::Liked).unwrap(), b"not json").unwrap();

        let got = page_of(pages.get(PageRequest::Liked, false, T1).await);
        assert_eq!(got, reply(liked(1), T1, false));
        assert_eq!(source.lanes(), [Lane::User]);
        assert_eq!(cached(&pages, &PageRequest::Liked), Some((T1, liked(1))));
    }

    #[tokio::test(start_paused = true)]
    async fn cached_prefetch_refreshes_on_the_prefetch_lane() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeSource::new(Duration::ZERO, [Step::Dropped]);
        let (pages, mut events) = service(source.clone(), dir.path());
        seed(&pages, &PageRequest::Liked, T0, liked(1));

        let got = page_of(pages.get(PageRequest::Liked, true, T1).await);
        assert_eq!(got, reply(liked(1), T0, true));
        tokio::time::sleep(Duration::from_secs(5)).await;
        // Hovering doesn't queue Background work, and a refused refresh says nothing.
        assert_eq!(source.lanes(), [Lane::Prefetch]);
        assert_eq!(events.try_recv().unwrap_err(), TryRecvError::Empty);
        assert_eq!(cached(&pages, &PageRequest::Liked), Some((T0, liked(1))));
    }

    #[tokio::test]
    async fn panicked_fetch_does_not_wedge_the_page() {
        let dir = tempfile::tempdir().unwrap();
        let source = FakeSource::new(Duration::ZERO, [Step::Panic, Step::Page(liked(1))]);
        let (pages, _events) = service(source.clone(), dir.path());

        let err = pages.get(PageRequest::Liked, false, T1).await.unwrap_err();
        assert_eq!(err, STOPPED_MESSAGE);
        let got = page_of(pages.get(PageRequest::Liked, false, T1).await);
        assert_eq!(got, reply(liked(1), T1, false));
        assert_eq!(source.calls(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn hung_fetch_ends_at_the_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let hour = Duration::from_secs(60 * 60);
        let source = FakeSource::new(hour, [Step::Page(liked(1)), Step::Page(liked(2))]);
        let (pages, _events) = service(source.clone(), dir.path());

        let err = pages.get(PageRequest::Liked, false, T1).await.unwrap_err();
        assert_eq!(err, TIMEOUT_MESSAGE);
        tokio::time::sleep(FLIGHT_LIMIT).await;
        // The hung fetch has ended, so this one starts afresh instead of joining it.
        let err = pages.get(PageRequest::Liked, false, T1).await.unwrap_err();
        assert_eq!(err, TIMEOUT_MESSAGE);
        assert_eq!(source.calls(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn hung_refresh_reports_at_the_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let hour = Duration::from_secs(60 * 60);
        let source = FakeSource::new(hour, [Step::Page(liked(2))]);
        let (pages, mut events) = service(source, dir.path());
        // Sidebar: the request whose librespot call has no timeout of its own. The
        // service doesn't look inside pages, so a Liked page stands in for the rootlist.
        seed(&pages, &PageRequest::Sidebar, T0, liked(1));

        let started = Instant::now();
        let got = page_of(pages.get(PageRequest::Sidebar, false, T1).await);
        assert_eq!(got, reply(liked(1), T0, true));
        let update = updated(&mut events, &PageRequest::Sidebar).await;
        let waited = started.elapsed();
        assert_eq!(
            update,
            PageReply {
                page: liked(1),
                fetched_at_ms: T0,
                stale: true,
                error: Some(FLIGHT_TIMEOUT_MESSAGE.into()),
            }
        );
        assert!(
            waited >= FLIGHT_LIMIT && waited < FLIGHT_LIMIT + Duration::from_secs(1),
            "{waited:?}"
        );
    }
}
