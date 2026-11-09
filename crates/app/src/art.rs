//! Cover art, the only colour on screen: nothing tints it.
//!
//! The window fetches it itself from Spotify's public image CDN (no auth), into a disk
//! cache shared across runs, `~/.cache/loudauto/art/<sha256(url)[..32]>`. Two worker
//! threads read the cache or fetch, decode to RGBA and send the pixels back; the UI
//! thread turns them into textures as it draws (mipmapped, so a large cover drawn small
//! stays smooth), and keeps those it drew last, up to [`TEXTURE_BYTES`].
//! The UI thread never waits on the network or the disk: it only queues jobs and takes
//! in what has finished.

use std::{
    collections::{HashMap, HashSet},
    io::ErrorKind,
    num::NonZeroUsize,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock, PoisonError, mpsc},
    thread,
    time::{Duration, Instant},
};

use eframe::egui::{self, ColorImage, TextureFilter, TextureHandle, TextureOptions};
use lru::LruCache;
use sha2::{Digest, Sha256};
use spot_core::{
    files,
    pages::{Image, pick_image},
    web::USER_AGENT,
};

/// The bytes of textures kept in memory; the one drawn longest ago goes first. About
/// sixty of Spotify's largest covers (640 px, mipmapped), and many times more of the
/// small ones rows and cards draw.
const TEXTURE_BYTES: usize = 128 * 1024 * 1024;
/// How covers are filtered: linear, and mipmapped (egui_glow generates the mipmaps), so
/// a 640 px cover drawn 56 pt wide is smooth rather than grainy.
const COVER_TEXTURE: TextureOptions = TextureOptions {
    mipmap_mode: Some(TextureFilter::Linear),
    ..TextureOptions::LINEAR
};
/// Decoded covers waiting for their first draw (prefetched, or arrived since the last
/// frame); past this the oldest are dropped, and read from the disk cache again if drawn.
const PENDING: NonZeroUsize = NonZeroUsize::new(64).unwrap();
/// How long a URL that failed is left alone.
const RETRY_AFTER: Duration = Duration::from_secs(10 * 60);
const WORKERS: usize = 2;
/// The longest one fetch may take, so a stalled one can't hold a worker for good.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// What a worker made of one URL.
struct Done {
    url: String,
    image: Result<ColorImage, String>,
}

/// Cover art for the UI thread. Dropping it lets its workers finish their jobs and stop.
pub struct Art {
    ctx: egui::Context,
    /// URLs for the workers.
    jobs: mpsc::Sender<String>,
    done: mpsc::Receiver<Done>,
    textures: LruCache<String, TextureHandle>,
    /// What `textures` hold, in bytes ([`texture_bytes`]), and the most they may.
    texture_bytes: usize,
    budget: usize,
    /// Decoded, not yet made a texture: that waits for the next `get` of it.
    pending: LruCache<String, ColorImage>,
    /// Sent to the workers, not yet back.
    queued: HashSet<String>,
    /// When each URL that failed came back failed.
    failed: HashMap<String, Instant>,
}

impl Art {
    /// Cover art cached in `~/.cache/loudauto/art`.
    pub fn new(ctx: egui::Context) -> Art {
        Art::with_dir(ctx, files::cache_dir().join("art"))
    }

    /// Cover art cached in `dir`. Starts the workers, which ask `ctx` for a repaint as
    /// each job finishes.
    pub fn with_dir(ctx: egui::Context, dir: PathBuf) -> Art {
        let (jobs, job_rx) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let loader = Arc::new(Loader {
            dir,
            client: OnceLock::new(),
        });
        for n in 0..WORKERS {
            let (job_rx, done_tx) = (job_rx.clone(), done_tx.clone());
            let (loader, ctx) = (loader.clone(), ctx.clone());
            let spawned = thread::Builder::new()
                .name(format!("art-{n}"))
                .spawn(move || work(&job_rx, &done_tx, &loader, &ctx));
            if let Err(e) = spawned {
                log::error!("can't start cover art worker {n}: {e}");
            }
        }
        Art::from_channels(ctx, jobs, done, TEXTURE_BYTES)
    }

    /// Cover art that is never fetched nor read from disk: every cover stays the grey
    /// square. For tests, which must reach neither the network nor the owner's cache.
    pub fn offline(ctx: egui::Context) -> Art {
        let (jobs, _) = mpsc::channel();
        let (_, done) = mpsc::channel();
        Art::from_channels(ctx, jobs, done, TEXTURE_BYTES)
    }

    /// The UI thread's half, fed by whatever reads `jobs` and answers on `done`, keeping
    /// up to `budget` bytes of textures.
    fn from_channels(
        ctx: egui::Context,
        jobs: mpsc::Sender<String>,
        done: mpsc::Receiver<Done>,
        budget: usize,
    ) -> Art {
        Art {
            ctx,
            jobs,
            done,
            textures: LruCache::unbounded(),
            texture_bytes: 0,
            budget,
            pending: LruCache::new(PENDING),
            queued: HashSet::new(),
            failed: HashMap::new(),
        }
    }

    /// The cover to draw `points` wide, if it has arrived; else it is fetched (once) and
    /// the caller draws a LINE-grey square meanwhile. A repaint follows its arrival.
    pub fn get(
        &mut self,
        images: &[Image],
        points: f32,
        ctx: &egui::Context,
    ) -> Option<TextureHandle> {
        self.get_at(images, points, ctx, Instant::now())
    }

    /// [`Art::get`] at `now`.
    fn get_at(
        &mut self,
        images: &[Image],
        points: f32,
        ctx: &egui::Context,
        now: Instant,
    ) -> Option<TextureHandle> {
        self.take_done(now);
        let url = &pick_image(images, pixels(points, ctx.pixels_per_point()))?.url;
        if let Some(texture) = self.textures.get(url) {
            return Some(texture.clone());
        }
        if let Some(image) = self.pending.pop(url) {
            let texture = ctx.load_texture(url, image, COVER_TEXTURE);
            self.keep(url.clone(), texture.clone());
            return Some(texture);
        }
        self.queue(url, now);
        None
    }

    /// Keeps `texture` for `url`, pushing out those drawn longest ago until the textures
    /// fit the budget again (the one just drawn always stays).
    fn keep(&mut self, url: String, texture: TextureHandle) {
        self.texture_bytes += texture_bytes(texture.size());
        if let Some(old) = self.textures.put(url, texture) {
            self.texture_bytes -= texture_bytes(old.size());
        }
        while self.texture_bytes > self.budget && self.textures.len() > 1 {
            let Some((_, out)) = self.textures.pop_lru() else {
                break;
            };
            self.texture_bytes -= texture_bytes(out.size());
        }
    }

    /// Fetches the cover to draw `points` wide ahead of its first draw.
    pub fn prefetch(&mut self, images: &[Image], points: f32) {
        self.prefetch_at(images, points, Instant::now());
    }

    /// [`Art::prefetch`] at `now`.
    fn prefetch_at(&mut self, images: &[Image], points: f32, now: Instant) {
        self.take_done(now);
        let Some(image) = pick_image(images, pixels(points, self.ctx.pixels_per_point())) else {
            return;
        };
        if !self.textures.contains(&image.url) && !self.pending.contains(&image.url) {
            self.queue(&image.url, now);
        }
    }

    /// Sends `url` to the workers unless it is on its way or failed lately.
    fn queue(&mut self, url: &str, now: Instant) {
        if self.queued.contains(url) {
            return;
        }
        if let Some(&failed_at) = self.failed.get(url) {
            if !retry_due(failed_at, now) {
                return;
            }
            self.failed.remove(url);
        }
        if self.jobs.send(url.to_owned()).is_ok() {
            self.queued.insert(url.to_owned());
        }
    }

    /// Takes in what the workers have finished, without waiting for more.
    fn take_done(&mut self, now: Instant) {
        while let Ok(Done { url, image }) = self.done.try_recv() {
            self.queued.remove(&url);
            match image {
                Ok(image) => {
                    self.pending.put(url, image);
                }
                Err(_) => {
                    self.failed.insert(url, now);
                }
            }
        }
    }
}

/// What a texture `size` pixels big takes: RGBA, and a third more for its mipmaps.
fn texture_bytes(size: [usize; 2]) -> usize {
    let base = size[0] * size[1] * 4;
    base + base / 3
}

/// The pixels a square `points` wide covers.
fn pixels(points: f32, pixels_per_point: f32) -> u32 {
    (points * pixels_per_point) as u32
}

/// Whether a URL that failed at `failed_at` may be tried again at `now`.
fn retry_due(failed_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(failed_at) >= RETRY_AFTER
}

/// A URL's file in the disk cache: the first 32 hex digits of its SHA-256.
fn cache_name(url: &str) -> String {
    let mut hash = format!("{:x}", Sha256::digest(url.as_bytes()));
    hash.truncate(32);
    hash
}

/// One worker: takes jobs until the `Art` that sends them is dropped.
fn work(
    jobs: &Mutex<mpsc::Receiver<String>>,
    done: &mpsc::Sender<Done>,
    loader: &Loader,
    ctx: &egui::Context,
) {
    loop {
        // The lock is held only while waiting for a job: the other worker takes the next.
        let job = jobs.lock().unwrap_or_else(PoisonError::into_inner).recv();
        let Ok(url) = job else {
            return;
        };
        let image = loader.load(&url);
        if let Err(e) = &image {
            log::warn!("no cover art from {url}: {e}");
        }
        if done.send(Done { url, image }).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

/// What the workers share: the disk cache, and one HTTP client, made by the first fetch
/// (it reads the system's certificates) so the UI thread never waits on it.
struct Loader {
    dir: PathBuf,
    client: OnceLock<Result<reqwest::blocking::Client, String>>,
}

impl Loader {
    /// `url`'s cover from the disk cache, else from the network (then cached), decoded.
    fn load(&self, url: &str) -> Result<ColorImage, String> {
        let path = self.dir.join(cache_name(url));
        match std::fs::read(&path) {
            Ok(bytes) => match decode(&bytes) {
                Ok(image) => return Ok(image),
                Err(e) => {
                    log::warn!("dropping unreadable cached cover {}: {e}", path.display());
                    let _ = std::fs::remove_file(&path);
                }
            },
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => log::warn!("can't read cached cover {}: {e}", path.display()),
        }
        let bytes = self.fetch(url)?;
        let image = decode(&bytes)?;
        // Only bytes that decoded are cached.
        if let Err(e) = files::write_atomic_blocking(&path, &bytes) {
            log::warn!("can't cache cover {}: {e}", path.display());
        }
        Ok(image)
    }

    fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        let client = self
            .client
            .get_or_init(|| {
                reqwest::blocking::Client::builder()
                    .user_agent(USER_AGENT)
                    .timeout(FETCH_TIMEOUT)
                    .build()
                    .map_err(|e| format!("no HTTP client: {e}"))
            })
            .as_ref()
            .map_err(String::clone)?;
        let response = client.get(url).send().map_err(|e| e.to_string())?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("HTTP {status}"));
        }
        let bytes = response.bytes().map_err(|e| e.to_string())?;
        Ok(bytes.to_vec())
    }
}

/// JPEG or PNG bytes as an image for a texture.
fn decode(bytes: &[u8]) -> Result<ColorImage, String> {
    let rgba = image::load_from_memory(bytes)
        .map_err(|e| e.to_string())?
        .into_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Cursor, Read, Write},
        net::TcpListener,
        thread,
    };

    use egui::Color32;

    use super::*;

    const A: &str = "http://127.0.0.1:9/a.jpg";
    const B: &str = "http://127.0.0.1:9/b.jpg";
    const C: &str = "http://127.0.0.1:9/c.jpg";

    /// One URL with no width: what the bottom bar and the queue rows have (ruling R2).
    fn cover(url: &str) -> Vec<Image> {
        vec![Image {
            url: url.into(),
            width: None,
        }]
    }

    /// An `Art` whose workers are the test: it reads the jobs and sends the results. It
    /// keeps up to `budget` bytes of textures.
    fn fake(budget: usize) -> (Art, mpsc::Receiver<String>, mpsc::Sender<Done>) {
        let (jobs, job_rx) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let art = Art::from_channels(egui::Context::default(), jobs, done, budget);
        (art, job_rx, done_tx)
    }

    fn loaded(url: &str) -> Done {
        Done {
            url: url.into(),
            image: Ok(ColorImage::filled([2, 2], Color32::RED)),
        }
    }

    /// A `w`×`h` PNG.
    fn png(w: u32, h: u32) -> Vec<u8> {
        let pixels = image::RgbaImage::from_pixel(w, h, image::Rgba([200, 30, 40, 255]));
        let mut bytes = Cursor::new(Vec::new());
        pixels
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    /// Asks for `images` until the workers have drawn them.
    fn wait_for(art: &mut Art, images: &[Image], ctx: &egui::Context) -> TextureHandle {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(texture) = art.get(images, 8.0, ctx) {
                return texture;
            }
            assert!(Instant::now() < until, "no texture after 10 s");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Serves `body` to one request on a local port; the request's head comes back on
    /// the channel.
    fn serve_once(body: Vec<u8>) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/cover.png", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut buf = [0; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                head.extend_from_slice(&buf[..n]);
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: image/png\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
            tx.send(String::from_utf8_lossy(&head).into_owned())
                .unwrap();
        });
        (url, rx)
    }

    /// Asked for twice (and prefetched) before it arrives, a cover is fetched once; once
    /// it arrives the next ask draws it, with no new job.
    #[test]
    fn queues_once() {
        let (mut art, jobs, done) = fake(TEXTURE_BYTES);
        let ctx = egui::Context::default();
        assert!(art.get(&cover(A), 56.0, &ctx).is_none());
        assert!(art.get(&cover(A), 56.0, &ctx).is_none());
        art.prefetch(&cover(A), 56.0);
        assert_eq!(jobs.try_iter().collect::<Vec<_>>(), [A]);

        done.send(loaded(A)).unwrap();
        let texture = art
            .get(&cover(A), 56.0, &ctx)
            .expect("drawn once it arrives");
        assert_eq!(texture.size(), [2, 2]);
        assert!(art.get(&cover(A), 56.0, &ctx).is_some());
        assert_eq!(jobs.try_iter().count(), 0);
    }

    /// The size fetched is the smallest at least as wide as the square is drawn.
    #[test]
    fn picks_the_size_drawn() {
        let (mut art, jobs, _done) = fake(TEXTURE_BYTES);
        let ctx = egui::Context::default();
        let sizes: Vec<Image> = [640, 64, 300]
            .into_iter()
            .map(|w| Image {
                url: format!("http://127.0.0.1:9/{w}.jpg"),
                width: Some(w),
            })
            .collect();
        art.get(&sizes, 150.0, &ctx);
        art.prefetch(&sizes, 40.0);
        assert_eq!(
            jobs.try_iter().collect::<Vec<_>>(),
            ["http://127.0.0.1:9/300.jpg", "http://127.0.0.1:9/64.jpg"]
        );
    }

    /// A cover already in the disk cache is read from it: its URL is unreachable.
    #[test]
    fn disk_cache_hit_skips_network() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(cache_name(A)), png(3, 2)).unwrap();
        let ctx = egui::Context::default();
        let mut art = Art::with_dir(ctx.clone(), dir.path().to_owned());
        let texture = wait_for(&mut art, &cover(A), &ctx);
        assert_eq!(texture.size(), [3, 2]);
        assert!(art.failed.is_empty());
    }

    /// A cover not in the cache is fetched with LOUDAUTO's user-agent, drawn, and written
    /// to the cache, where the next run finds it with the server gone.
    #[test]
    fn fetched_cover_is_cached() {
        let dir = tempfile::tempdir().unwrap();
        let body = png(4, 4);
        let (url, heads) = serve_once(body.clone());
        let ctx = egui::Context::default();
        let mut art = Art::with_dir(ctx.clone(), dir.path().to_owned());
        assert_eq!(wait_for(&mut art, &cover(&url), &ctx).size(), [4, 4]);
        let head = heads.recv_timeout(Duration::from_secs(5)).unwrap();
        let user_agent = format!("user-agent: {}\r\n", spot_core::web::USER_AGENT);
        assert!(
            head.to_lowercase().contains(&user_agent.to_lowercase()),
            "{head}"
        );
        assert_eq!(
            std::fs::read(dir.path().join(cache_name(&url))).unwrap(),
            body
        );
        drop(art);

        let mut next_run = Art::with_dir(ctx.clone(), dir.path().to_owned());
        assert_eq!(wait_for(&mut next_run, &cover(&url), &ctx).size(), [4, 4]);
    }

    /// A cover that can't be fetched is noted as failed, and nothing is cached for it.
    #[test]
    fn unreachable_url_fails() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let mut art = Art::with_dir(ctx.clone(), dir.path().to_owned());
        let until = Instant::now() + Duration::from_secs(10);
        while !art.failed.contains_key(A) {
            assert!(art.get(&cover(A), 8.0, &ctx).is_none());
            assert!(Instant::now() < until, "no failure after 10 s");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!dir.path().join(cache_name(A)).exists());
    }

    /// With room for two textures, a third pushes out the one drawn longest ago, which
    /// is fetched again when next drawn.
    #[test]
    fn lru_evicts_oldest() {
        let (mut art, jobs, done) = fake(2 * texture_bytes([2, 2]));
        let ctx = egui::Context::default();
        for url in [A, B, C] {
            assert!(art.get(&cover(url), 56.0, &ctx).is_none());
            done.send(loaded(url)).unwrap();
            assert!(art.get(&cover(url), 56.0, &ctx).is_some());
        }
        assert!(!art.textures.contains(A));
        assert!(art.textures.contains(B) && art.textures.contains(C));
        assert_eq!(jobs.try_iter().collect::<Vec<_>>(), [A, B, C]);

        assert!(art.get(&cover(A), 56.0, &ctx).is_none());
        assert_eq!(jobs.try_iter().collect::<Vec<_>>(), [A]);
    }

    /// Deferred Task 24 minor: covers are mipmapped, so the 640 px cover in the 56 pt
    /// bar and the queue's thumbnails are smooth rather than grainy.
    #[test]
    fn covers_are_mipmapped() {
        let (mut art, _jobs, done) = fake(TEXTURE_BYTES);
        let ctx = egui::Context::default();
        assert!(art.get(&cover(A), 56.0, &ctx).is_none());
        done.send(loaded(A)).unwrap();
        let texture = art.get(&cover(A), 56.0, &ctx).unwrap();
        let options = ctx.tex_manager().read().meta(texture.id()).unwrap().options;
        assert_eq!(options.mipmap_mode, Some(egui::TextureFilter::Linear));
        assert_eq!(options.minification, egui::TextureFilter::Linear);
    }

    /// Deferred Task 24 minor: the textures kept are bounded by their bytes, not their
    /// count: a large cover is pushed out to make room for small ones, and the budget
    /// holds what a window draws at once many times over.
    #[test]
    fn textures_are_bounded_by_bytes() {
        let big = |url: &str| Done {
            url: url.into(),
            image: Ok(ColorImage::filled([640, 640], Color32::RED)),
        };
        let budget = texture_bytes([640, 640]) + 3 * texture_bytes([2, 2]);
        let (mut art, _jobs, done) = fake(budget);
        let ctx = egui::Context::default();
        assert!(art.get(&cover(A), 56.0, &ctx).is_none());
        done.send(big(A)).unwrap();
        assert!(art.get(&cover(A), 56.0, &ctx).is_some());
        let small = [
            "http://127.0.0.1:9/1.jpg",
            "http://127.0.0.1:9/2.jpg",
            "http://127.0.0.1:9/3.jpg",
        ];
        for url in small {
            assert!(art.get(&cover(url), 56.0, &ctx).is_none());
            done.send(loaded(url)).unwrap();
            assert!(art.get(&cover(url), 56.0, &ctx).is_some());
        }
        assert!(art.textures.contains(A), "within the budget");
        assert!(art.get(&cover(B), 56.0, &ctx).is_none());
        done.send(loaded(B)).unwrap();
        assert!(art.get(&cover(B), 56.0, &ctx).is_some());
        assert!(
            !art.textures.contains(A),
            "the large one drawn longest ago went"
        );
        assert!(small.iter().all(|url| art.textures.contains(*url)));
        // Room for fifty and more of the largest covers Spotify serves.
        assert!(TEXTURE_BYTES / texture_bytes([640, 640]) >= 50);
    }

    /// A URL that failed is asked for again only after ten minutes.
    #[test]
    fn failed_url_backoff() {
        let (mut art, jobs, done) = fake(TEXTURE_BYTES);
        let ctx = egui::Context::default();
        let t0 = Instant::now();
        assert!(art.get_at(&cover(A), 56.0, &ctx, t0).is_none());
        assert_eq!(jobs.try_iter().count(), 1);
        done.send(Done {
            url: A.into(),
            image: Err("connection refused".into()),
        })
        .unwrap();

        let almost = t0 + RETRY_AFTER - Duration::from_secs(1);
        for now in [t0, almost] {
            assert!(art.get_at(&cover(A), 56.0, &ctx, now).is_none());
            art.prefetch_at(&cover(A), 56.0, now);
        }
        assert_eq!(jobs.try_iter().count(), 0);

        assert!(
            art.get_at(&cover(A), 56.0, &ctx, t0 + RETRY_AFTER)
                .is_none()
        );
        assert_eq!(jobs.try_iter().collect::<Vec<_>>(), [A]);
    }

    #[test]
    fn retry_due_after_ten_minutes() {
        let t0 = Instant::now();
        assert!(!retry_due(t0, t0));
        assert!(!retry_due(t0, t0 + Duration::from_secs(599)));
        assert!(retry_due(t0, t0 + Duration::from_secs(600)));
        // A clock that went backwards is not a reason to retry.
        assert!(!retry_due(t0 + Duration::from_secs(5), t0));
    }

    #[test]
    fn cache_name_is_32_hex_of_sha256() {
        // sha256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        assert_eq!(cache_name("abc"), "ba7816bf8f01cfea414140de5dae2223");
    }
}
