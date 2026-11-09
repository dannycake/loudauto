//! `loudauto-cli`: exercises spot-core against the live account.

use std::{
    io::{self, BufRead, IsTerminal, Write},
    path::Path,
    process::ExitCode,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use clap::{Parser, Subcommand};
use engine::{desktop::KwalletKey, heart::heart_message};
use engine_ipc::{EngineClient, IpcError, Reply, Request, Role};
use playback::{
    SoloistError,
    backend::{LoginGate, Playback, SoloistBackend, connect_running},
    command::{Command, Repeat, set_volume},
    config::SoloistConfig,
    event::{NowPlaying, PlayStatus, PlaybackEvent},
    supervisor::{exit_error, running_soloist},
};
use spot_core::{
    SpotError,
    creds::{CredentialStore, SecretServiceStore},
    library::Library,
    model::{LibraryEntry, RootEntry, TrackInfo},
    pages::{Card, Named, TrackRow},
    session::{SessionCell, connect_stored, pair},
    web::{
        SessionTokens, Tokens, Web,
        lanes::{BACKGROUND_GAP, Lane, Lanes},
        ops::Op,
        registry::HashRegistry,
    },
};

#[derive(Parser)]
#[command(
    name = "loudauto-cli",
    about = "Read your Spotify library through spot-core"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Log in with a code at spotify.com/pair and store the session
    Login,
    /// Forget the stored session; with --soloist, also clear Soloist's own login
    Logout {
        /// Also clear Soloist's login (it then needs pairing again from the desktop app)
        #[arg(long)]
        soloist: bool,
    },
    /// Like or unlike the playing track, through the running engine
    Heart,
    /// Save your Soloist API key in the desktop keyring (pasted, or piped on stdin)
    SetKey,
    /// Forget the saved Soloist API key (save a new one with set-key)
    ForgetKey,
    /// List the playlist sidebar
    Playlists,
    /// Show a playlist's tracks
    Ls {
        uri: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Show Liked Songs
    Liked {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Search tracks
    Search {
        #[arg(required = true, num_args = 1..)]
        words: Vec<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Show Spotify's Home
    Home,
    /// Show an album and its tracks
    Album { uri: String },
    /// Show an artist's page
    Artist { uri: String },
    /// Show every row of a playlist, from the web player
    PlaylistRows { uri: String },
    /// Show one page of Liked Songs, from the web player
    LikedRows {
        #[arg(long, default_value_t = 0)]
        offset: u32,
    },
    /// Show Recently played
    Recents,
    /// Show a track's lyrics
    Lyrics { uri: String },
    /// Show a track's credits
    Credits { uri: String },
    /// Show the radio playlist for a track or an artist
    Radio { seed: String },
    /// Show where each pathfinder hash comes from
    Hashes {
        /// Scrape the public web player for new hashes now, even if not due
        #[arg(long)]
        refresh: bool,
    },
    /// Run Spotify Soloist and print its events until Ctrl-C
    Player {
        /// PipeWire node to play into (e.g. easyeffects_sink)
        #[arg(long)]
        device: Option<String>,
        /// Start playing this Spotify URI on this device
        #[arg(long)]
        play: Option<String>,
    },
    /// Control a running `player`
    Ctl {
        #[command(subcommand)]
        action: Ctl,
    },
}

#[derive(Subcommand)]
enum Ctl {
    /// Resume, or play a URI on this device
    Play {
        uri: Option<String>,
    },
    Pause,
    Next,
    Prev,
    Seek {
        position_ms: u64,
    },
    Volume {
        level: u8,
    },
    /// Print what is playing
    Now,
}

/// What `heart` says when there is no engine to ask.
const ENGINE_NOT_RUNNING: &str = "LOUDAUTO engine isn't running";
/// Why `player`, `ctl` and `logout --soloist` refuse while the engine runs.
const ENGINE_OWNS_SOLOIST: &str = "the LOUDAUTO engine is running and owns Spotify Soloist; \
                                   stop it first: systemctl --user stop loudauto.service";
/// The warning before `logout --soloist` clears Soloist's login.
const SOLOIST_PAIRING_WARNING: &str =
    "Soloist will need pairing again from the Spotify desktop app on this PC.";

#[tokio::main]
async fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let cmd = Cli::parse().cmd;
    match cmd {
        Cmd::Heart => return heart().await,
        Cmd::SetKey => return set_key().await,
        Cmd::Logout { soloist: true } => return logout_soloist().await,
        _ => {}
    }
    if matches!(cmd, Cmd::Player { .. } | Cmd::Ctl { .. }) {
        // `ctl` would otherwise drive the engine's Soloist behind its back.
        if engine_running().await {
            eprintln!("error: {ENGINE_OWNS_SOLOIST}");
            return ExitCode::from(1);
        }
        return match run_playback(cmd).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e @ (SoloistError::BinaryMissing(_) | SoloistError::Expired)) => {
                eprintln!("{e}");
                ExitCode::from(3)
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(1)
            }
        };
    }
    match run(cmd).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(
            SpotError::NotLoggedIn
            | SpotError::StoredCredentialsRejected
            | SpotError::CorruptCredentials,
        ) => {
            eprintln!("Not logged in: run 'loudauto-cli login'");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

async fn run(cmd: Cmd) -> spot_core::Result<()> {
    let store = SecretServiceStore::SPOTIFY_SESSION;
    match cmd {
        Cmd::Login => {
            let session = pair(&store, |p| {
                println!("Open {} and enter code {}", p.url, p.code)
            })
            .await?;
            println!("Logged in as {}", session.username());
        }
        Cmd::Logout { soloist: false } => {
            store.clear().await?;
            println!("Logged out");
        }
        Cmd::Logout { soloist: true } | Cmd::Heart | Cmd::SetKey => {
            unreachable!("handled in main")
        }
        Cmd::ForgetKey => {
            SecretServiceStore::SOLOIST_KEY.clear().await?;
            println!("Forgot the saved Soloist API key");
        }
        Cmd::Playlists => {
            let lib = library(&store).await?;
            for entry in lib.rootlist(Lane::User).await? {
                match entry {
                    RootEntry::Playlist(p) => println!("{}\t{}", p.uri, p.name),
                    RootEntry::FolderStart { name, .. } => println!("▸ {name}"),
                    RootEntry::FolderEnd { .. } => println!("◂"),
                    RootEntry::Other { uri } => println!("? {uri}"),
                }
            }
        }
        Cmd::Ls { uri, limit } => {
            let lib = library(&store).await?;
            let detail = lib.playlist(Lane::User, &uri).await?;
            println!("{} ({} items)", detail.name, detail.items.len());
            for item in detail.items.iter().take(limit) {
                println!("{}", describe(&lib, item).await?);
            }
        }
        Cmd::Liked { limit } => {
            let lib = library(&store).await?;
            let liked = lib.liked(Lane::User).await?;
            println!("{} liked songs", liked.len());
            for LibraryEntry { uri, added_at_ms } in liked.iter().take(limit) {
                let date = added_at_ms
                    .map(ymd_utc)
                    .unwrap_or_else(|| "----------".into());
                println!("{date}  {}", describe(&lib, uri).await?);
            }
        }
        Cmd::Home => {
            let home = web(&store).await?.home(Lane::User).await?;
            println!("{}", home.greeting);
            for section in &home.sections {
                println!("▸ {}", section.title);
                for card in &section.cards {
                    println!("  {}", card_line(card));
                }
            }
        }
        Cmd::Album { uri } => {
            let album = web(&store).await?.album(Lane::User, &uri).await?;
            println!(
                "{} — {} ({}, {} tracks)",
                album.name,
                names(&album.artists),
                album.release_date,
                album.total
            );
            print_rows(&album.tracks);
        }
        Cmd::Artist { uri } => {
            let artist = web(&store).await?.artist(Lane::User, &uri).await?;
            println!("{}", artist.name);
            println!("▸ Popular");
            for row in &artist.top_tracks {
                println!("  {}", row_line(row));
            }
            for (title, cards) in [
                ("Albums", &artist.albums),
                ("Singles", &artist.singles),
                ("Fans also like", &artist.related),
            ] {
                println!("▸ {title}");
                for card in cards {
                    println!("  {}", card_line(card));
                }
            }
        }
        Cmd::PlaylistRows { uri } => {
            let playlist = web(&store).await?.playlist(Lane::User, &uri).await?;
            println!("{} ({} rows)", playlist.name, playlist.total);
            print_rows(&playlist.tracks);
        }
        Cmd::LikedRows { offset } => {
            let page = web(&store).await?.liked_page(Lane::User, offset).await?;
            println!("{} liked songs, from {offset}", page.total);
            print_rows(&page.tracks);
        }
        Cmd::Recents => {
            for card in web(&store).await?.recents(Lane::User).await? {
                println!("{}", card_line(&card));
            }
        }
        Cmd::Lyrics { uri } => match web(&store).await?.lyrics(Lane::User, &uri).await? {
            None => println!("No lyrics"),
            Some(lyrics) => {
                let sync = if lyrics.synced { "synced" } else { "unsynced" };
                println!(
                    "{} lines, {sync}, from {}",
                    lyrics.lines.len(),
                    lyrics.provider
                );
                for line in &lyrics.lines {
                    if lyrics.synced {
                        println!("{:>6}  {}", fmt_duration(line.start_ms), line.words);
                    } else {
                        println!("{}", line.words);
                    }
                }
            }
        },
        Cmd::Credits { uri } => {
            let roles = web(&store).await?.credits(Lane::User, &uri).await?;
            if roles.is_empty() {
                println!("No credits");
            }
            for role in roles {
                println!("{}: {}", role.role, role.names.join(", "));
            }
        }
        Cmd::Radio { seed } => {
            println!("{}", web(&store).await?.radio_for(Lane::User, &seed).await?);
        }
        Cmd::Hashes { refresh } => {
            // The scrape reads only public pages, so no session is opened.
            let web = web_with(Arc::new(NoSession)).await;
            let found = if refresh {
                Some(web.refresh_hashes(now_ms()).await?)
            } else {
                None
            };
            let registry = web.registry();
            for op in Op::ALL {
                println!(
                    "{}",
                    hash_line(op, &registry.hash(op), registry.is_cached(op))
                );
            }
            if let Some(found) = found {
                println!("scrape found {found} of {} ops", Op::ALL.len());
            }
        }
        Cmd::Player { .. } | Cmd::Ctl { .. } => unreachable!("handled by run_playback"),
        Cmd::Search { words, limit } => {
            let lib = library(&store).await?;
            for hit in lib
                .search(Lane::User, &words.join(" "))
                .await?
                .iter()
                .take(limit)
            {
                println!("{}", describe(&lib, &hit.uri).await?);
            }
        }
    }
    Ok(())
}

/// Whether an engine answers on its socket (of any protocol version).
async fn engine_running() -> bool {
    matches!(
        EngineClient::connect(Role::Cli).await,
        Ok(_) | Err(IpcError::VersionMismatch { .. })
    )
}

/// `loudauto-cli heart`: the engine likes or unlikes the playing track and shows a
/// notification; this prints its answer. Exit 1 when it couldn't.
async fn heart() -> ExitCode {
    let (client, state, _events) = match EngineClient::connect(Role::Cli).await {
        Ok(connected) => connected,
        Err(IpcError::NotRunning) => {
            eprintln!("{ENGINE_NOT_RUNNING}");
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let answer = client.request(Request::ToggleHeart { uri: None }).await;
    match heart_line(answer, state.player.track.as_ref()) {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(line) => {
            eprintln!("{line}");
            ExitCode::from(1)
        }
    }
}

/// What `heart` prints for the engine's answer: `Ok` for a like or unlike. The reply
/// names the track by uri; the title and artists come from the state the engine sent
/// on connect, as the engine words its own message.
fn heart_line(
    answer: Result<Reply, IpcError>,
    playing: Option<&NowPlaying>,
) -> Result<String, String> {
    match answer {
        Ok(Reply::Liked { uri, liked }) => Ok(match playing.filter(|t| t.uri == uri) {
            Some(track) => heart_message(&track.title, &track.artists, liked),
            None => heart_message(&uri, &[], liked),
        }),
        Ok(other) => Err(format!("error: the engine answered {other:?}")),
        // `Couldn't like: …`, as the engine words it.
        Err(IpcError::Engine(message)) => Err(message),
        Err(e) => Err(format!("error: {e}")),
    }
}

/// `loudauto-cli logout --soloist`: after the warning and a typed `yes`, forgets the
/// stored session and clears Soloist's login. Refuses while anything plays through
/// Soloist's data dir.
async fn logout_soloist() -> ExitCode {
    let config = SoloistConfig::from_env();
    if engine_running().await {
        eprintln!("error: {ENGINE_OWNS_SOLOIST}");
        return ExitCode::from(1);
    }
    if let Some(pid) = running_soloist(&config.data_dir) {
        eprintln!("error: {}", SoloistError::AlreadyRunning(pid));
        return ExitCode::from(1);
    }
    println!("{SOLOIST_PAIRING_WARNING}");
    print!("Type yes to log out and clear Soloist's login: ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if let Err(e) = io::stdin().lock().read_line(&mut line) {
        eprintln!("error: {e}");
        return ExitCode::from(1);
    }
    if !confirmed(&line) {
        println!("Nothing changed");
        return ExitCode::from(1);
    }
    if let Err(e) = SecretServiceStore::SPOTIFY_SESSION.clear().await {
        eprintln!("error: {e}");
        return ExitCode::from(1);
    }
    println!("Logged out");
    match clear_soloist_login(&config.data_dir) {
        Ok(true) => println!("Cleared Soloist's login"),
        Ok(false) => println!("Soloist had no login to clear"),
        Err(e) => {
            eprintln!("error: clearing {}: {e}", config.data_dir.display());
            return ExitCode::from(1);
        }
    }
    ExitCode::SUCCESS
}

/// Reads the Soloist API key (not echoed when typed at a terminal) and keeps it in the
/// keyring, where loudautod finds it.
async fn set_key() -> ExitCode {
    let typed = io::stdin().is_terminal();
    if typed {
        eprint!("Paste your Soloist API key (it won't be shown), then Enter: ");
        let _ = io::stderr().flush();
        echo(false);
    }
    let mut line = String::new();
    let read = io::stdin().lock().read_line(&mut line);
    if typed {
        echo(true);
        eprintln!();
    }
    if let Err(e) = read {
        eprintln!("error: couldn't read the key: {e}");
        return ExitCode::from(1);
    }
    let Some(key) = key_from_input(&line) else {
        eprintln!("No key given; nothing saved.");
        return ExitCode::from(2);
    };
    match SecretServiceStore::SOLOIST_KEY.save(key.as_bytes()).await {
        Ok(()) => {
            println!("Saved the Soloist API key in the keyring.");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: couldn't save the key in the keyring: {e}");
            ExitCode::from(1)
        }
    }
}

/// A pasted key, trimmed; `None` when blank.
fn key_from_input(line: &str) -> Option<String> {
    Some(line.trim().to_string()).filter(|k| !k.is_empty())
}

/// Turns the terminal's echo off or back on (best effort, through `stty`).
fn echo(on: bool) {
    let _ = std::process::Command::new("stty")
        .arg(if on { "echo" } else { "-echo" })
        .stdin(std::process::Stdio::inherit())
        .status();
}

/// Whether the owner typed `yes`.
fn confirmed(line: &str) -> bool {
    line.trim() == "yes"
}

/// Clears Soloist's login by emptying its data dir, where Soloist keeps it (with its
/// device id and prefs, which it makes again; the engine rewrites the quality prefs at
/// the next first session). The dir itself stays. `false` when there was nothing.
fn clear_soloist_login(data_dir: &Path) -> io::Result<bool> {
    let entries = match std::fs::read_dir(data_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let mut cleared = false;
    for entry in entries {
        let entry = entry?;
        // `file_type` doesn't follow links: a link is removed, not what it points at.
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
        cleared = true;
    }
    Ok(cleared)
}

async fn run_playback(cmd: Cmd) -> playback::Result<()> {
    let mut config = SoloistConfig::from_env();
    match cmd {
        Cmd::Player { device, play } => {
            if device.is_some() {
                config.pipewire_device = device;
            }
            let backend = SoloistBackend::launch(config, &KwalletKey).await?;
            let mut events = backend.subscribe();
            // `get_state` answers before login too, so this always yields an `Auth` event.
            backend.refresh().await?;
            let mut gate = LoginGate::default();
            let mut play = play;
            let mut last = String::new();
            let mut ended = None;
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    event = events.recv() => match event {
                        Ok(event) => {
                            if gate.opens(&event) {
                                backend.refresh().await?;
                                if let Some(uri) = play.take() {
                                    backend.play(Some(&uri)).await?;
                                }
                            }
                            if gate.hides(&event) {
                                continue;
                            }
                            if let Some(line) = event_line(&event).filter(|l| *l != last) {
                                println!("{line}");
                                last = line;
                            }
                            if let Some(err) = player_end(&event) {
                                ended = Some(err);
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(_) => break,
                    },
                }
            }
            backend.shutdown().await?;
            // Soloist ending on its own is a failure: say how, with its last output.
            if let Some(err) = ended {
                return Err(err);
            }
        }
        Cmd::Ctl { action } => {
            let (tx, mut events) = tokio::sync::broadcast::channel(64);
            let client = connect_running(&config, tx).await?;
            match action {
                Ctl::Play { uri: Some(uri) } => {
                    client.send(Command::Activate).await?;
                    client.send(Command::Play { uri: Some(uri) }).await?;
                }
                Ctl::Play { uri: None } => client.send(Command::Play { uri: None }).await?,
                Ctl::Pause => client.send(Command::Pause).await?,
                Ctl::Next => client.send(Command::SkipNext).await?,
                Ctl::Prev => client.send(Command::SkipPrev).await?,
                Ctl::Seek { position_ms } => client.send(Command::Seek { position_ms }).await?,
                Ctl::Volume { level } => client.send(set_volume(level)).await?,
                Ctl::Now => {
                    client.send(Command::GetState).await?;
                    let (mut track, mut status) = (None, None);
                    let deadline = tokio::time::sleep(std::time::Duration::from_secs(2));
                    tokio::pin!(deadline);
                    while track.is_none() || status.is_none() {
                        tokio::select! {
                            _ = &mut deadline => break,
                            Ok(event) = events.recv() => match event {
                                PlaybackEvent::Track(_) if track.is_none() => track = event_line(&event),
                                PlaybackEvent::Status(_) if status.is_none() => status = event_line(&event),
                                _ => {}
                            },
                        }
                    }
                    for line in [track, status].into_iter().flatten() {
                        println!("{line}");
                    }
                }
            }
            // Give the writer a moment to flush the command before the process exits.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        _ => unreachable!("only player commands reach run_playback"),
    }
    Ok(())
}

// Every CLI command is the user asking, so every request goes on the User lane.

async fn library(store: &SecretServiceStore) -> spot_core::Result<Library> {
    Ok(Library::new(
        session_cell(store).await?,
        Arc::new(Lanes::new(BACKGROUND_GAP)),
    ))
}

/// The web-player client on the stored session.
async fn web(store: &SecretServiceStore) -> spot_core::Result<Web> {
    Ok(web_with(Arc::new(SessionTokens(session_cell(store).await?))).await)
}

/// A web-player client on `tokens` and the app's own hash cache, as the engine has it.
async fn web_with(tokens: Arc<dyn Tokens>) -> Web {
    Web::new(
        tokens,
        Arc::new(Lanes::new(BACKGROUND_GAP)),
        Arc::new(HashRegistry::load(HashRegistry::default_path()).await),
    )
}

async fn session_cell(store: &SecretServiceStore) -> spot_core::Result<Arc<SessionCell>> {
    Ok(Arc::new(SessionCell::new(connect_stored(store).await?)))
}

/// Tokens for a client that only fetches public pages: asking for one is an error.
struct NoSession;

#[async_trait::async_trait]
impl Tokens for NoSession {
    async fn bearer(&self) -> spot_core::Result<String> {
        Err(SpotError::NotLoggedIn)
    }
    async fn client_token(&self) -> spot_core::Result<String> {
        Err(SpotError::NotLoggedIn)
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

async fn describe(lib: &Library, uri: &str) -> spot_core::Result<String> {
    Ok(match lib.track(Lane::User, uri).await? {
        Some(t) => track_line(&t),
        None => format!("{uri} (not a track)"),
    })
}

/// `m:ss`
fn fmt_duration(ms: u64) -> String {
    let secs = ms / 1000;
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// UTC calendar date `YYYY-MM-DD` for Unix milliseconds.
fn ymd_utc(ms: i64) -> String {
    // Howard Hinnant's days-to-civil algorithm (proleptic Gregorian, UTC).
    let z = ms.div_euclid(86_400_000) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// `<title> — <artists> [<album>] <m:ss>`
fn track_line(t: &TrackInfo) -> String {
    format!(
        "{} — {} [{}] {}",
        t.title,
        t.artists.join(", "),
        t.album,
        fmt_duration(t.duration_ms.into())
    )
}

fn names(named: &[Named]) -> String {
    named
        .iter()
        .map(|n| n.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// `<title> — <artists> [<uri>]`
fn row_line(row: &TrackRow) -> String {
    format!("{} — {} [{}]", row.title, names(&row.artists), row.uri)
}

fn print_rows(rows: &[TrackRow]) {
    for row in rows {
        println!("{}", row_line(row));
    }
}

/// `<title> — <subtitle> [<uri>]`, or `<title> [<uri>]` without a subtitle.
fn card_line(card: &Card) -> String {
    if card.subtitle.is_empty() {
        format!("{} [{}]", card.title, card.uri)
    } else {
        format!("{} — {} [{}]", card.title, card.subtitle, card.uri)
    }
}

/// `<op> <first 8 of hash> <cache|fallback>`, the op name padded to a column.
fn hash_line(op: Op, hash: &str, cached: bool) -> String {
    let prefix: String = hash.chars().take(8).collect();
    let source = if cached { "cache" } else { "fallback" };
    format!("{:<20} {prefix} {source}", op.name())
}

/// Soloist's end, as the error `player` exits with: its code and log tail (code 10 is an
/// expired build).
fn player_end(event: &PlaybackEvent) -> Option<SoloistError> {
    match event {
        PlaybackEvent::Exited { code, tail } => Some(exit_error(*code, tail.clone())),
        _ => None,
    }
}

/// One printable line per player event; `None` for events not worth printing.
fn event_line(event: &PlaybackEvent) -> Option<String> {
    let yes_no = |b: bool| if b { "yes" } else { "no" };
    Some(match event {
        PlaybackEvent::Track(t) => format!("track: {} — {}", t.title, t.artists.join(", ")),
        PlaybackEvent::Status(s) => format!(
            "status: {}",
            match s {
                PlayStatus::Playing => "playing",
                PlayStatus::Paused => "paused",
                PlayStatus::Buffering => "buffering",
                PlayStatus::Stopped => "stopped",
                PlayStatus::Other(other) => other,
            }
        ),
        PlaybackEvent::Quality(q) => format!(
            "quality: {} {} kbps {} kHz",
            q.codec,
            q.bitrate_bps / 1000,
            f64::from(q.sample_rate_hz) / 1000.0
        ),
        PlaybackEvent::Active(b) => format!("active: {}", yes_no(*b)),
        PlaybackEvent::Volume(v) => format!("volume: {v}"),
        PlaybackEvent::Error(m) => format!("error: {m}"),
        PlaybackEvent::Auth { logged_in } => format!("logged in: {}", yes_no(*logged_in)),
        PlaybackEvent::Upcoming(items) => format!("upcoming: {} tracks", items.len()),
        PlaybackEvent::Context(uri) => format!("context: {}", uri.as_deref().unwrap_or("none")),
        PlaybackEvent::Options { shuffle, repeat } => format!(
            "shuffle: {}, repeat: {}",
            yes_no(*shuffle),
            match repeat {
                Repeat::Off => "off",
                Repeat::Context => "context",
                Repeat::Track => "track",
            }
        ),
        PlaybackEvent::ExpiresInDays(days) => {
            format!(
                "expires in: {days} day{}",
                if *days == 1 { "" } else { "s" }
            )
        }
        PlaybackEvent::Position(_) | PlaybackEvent::Actions(_) | PlaybackEvent::Exited { .. } => {
            return None;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_key_is_trimmed_and_a_blank_one_refused() {
        assert_eq!(key_from_input(" abc123\n").as_deref(), Some("abc123"));
        assert_eq!(key_from_input("  \n"), None);
        assert_eq!(key_from_input(""), None);
    }
    use playback::event::{Actions, Position, StreamQuality};

    fn nerve() -> NowPlaying {
        NowPlaying {
            uri: "spotify:track:1XD4K4CGAKTIBmFpvuaFru".into(),
            title: "nerve".into(),
            artists: vec!["2hollis".into()],
            artist_uris: vec![],
            album: None,
            album_uri: None,
            duration_ms: None,
            cover_url: None,
        }
    }

    #[test]
    fn heart_lines() {
        let liked = |liked| {
            Ok(Reply::Liked {
                uri: nerve().uri,
                liked,
            })
        };
        assert_eq!(
            heart_line(liked(true), Some(&nerve())),
            Ok("Liked: nerve — 2hollis".into())
        );
        assert_eq!(
            heart_line(liked(false), Some(&nerve())),
            Ok("Removed from Liked Songs: nerve — 2hollis".into())
        );
        // The track changed meanwhile: named by its uri, as the engine does.
        assert_eq!(
            heart_line(liked(true), None),
            Ok(format!("Liked: {}", nerve().uri))
        );
        assert_eq!(
            heart_line(
                Err(IpcError::Engine("Couldn't like: nothing is playing".into())),
                None
            ),
            Err("Couldn't like: nothing is playing".into())
        );
        assert_eq!(
            heart_line(Err(IpcError::Timeout), Some(&nerve())),
            Err("error: the engine didn't answer in time".into())
        );
        assert!(heart_line(Ok(Reply::Ok), Some(&nerve())).is_err());
    }

    #[test]
    fn only_yes_confirms() {
        assert!(confirmed("yes\n"));
        assert!(confirmed("  yes  "));
        for no in ["", "\n", "y", "Yes", "yes please", "no"] {
            assert!(!confirmed(no), "{no:?}");
        }
    }

    #[test]
    fn soloist_login_cleared_dir_kept() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("soloist-data");
        // Nothing there yet.
        assert!(!clear_soloist_login(&data).unwrap());
        let user = data.join("settings/Users/owner-user");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(user.join("prefs"), "audio.normalize_v2=false\n").unwrap();
        std::fs::write(data.join(".device_id"), "dev-1\n").unwrap();
        // A link inside is removed, not followed.
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("keep"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, data.join("link")).unwrap();

        assert!(clear_soloist_login(&data).unwrap());
        assert!(data.is_dir());
        assert_eq!(std::fs::read_dir(&data).unwrap().count(), 0);
        assert!(outside.join("keep").exists());
        // Already empty.
        assert!(!clear_soloist_login(&data).unwrap());
    }

    #[test]
    fn event_lines() {
        let track = PlaybackEvent::Track(NowPlaying {
            uri: "spotify:track:x".into(),
            title: "ALL THE TIME".into(),
            artists: vec!["femtanyl".into(), "x".into()],
            artist_uris: vec!["spotify:artist:f".into(), "spotify:artist:x".into()],
            album: None,
            album_uri: None,
            duration_ms: Some(158_000),
            cover_url: None,
        });
        assert_eq!(
            event_line(&track).as_deref(),
            Some("track: ALL THE TIME — femtanyl, x")
        );
        assert_eq!(
            event_line(&PlaybackEvent::Status(PlayStatus::Playing)).as_deref(),
            Some("status: playing")
        );
        assert_eq!(
            event_line(&PlaybackEvent::Status(PlayStatus::Other("odd".into()))).as_deref(),
            Some("status: odd")
        );
        let q = PlaybackEvent::Quality(StreamQuality {
            codec: "flac".into(),
            bitrate_bps: 1_400_000,
            sample_rate_hz: 44_100,
        });
        assert_eq!(
            event_line(&q).as_deref(),
            Some("quality: flac 1400 kbps 44.1 kHz")
        );
        assert_eq!(
            event_line(&PlaybackEvent::Active(true)).as_deref(),
            Some("active: yes")
        );
        assert_eq!(
            event_line(&PlaybackEvent::Active(false)).as_deref(),
            Some("active: no")
        );
        assert_eq!(
            event_line(&PlaybackEvent::Volume(60)).as_deref(),
            Some("volume: 60")
        );
        assert_eq!(
            event_line(&PlaybackEvent::Error("m".into())).as_deref(),
            Some("error: m")
        );
        assert_eq!(
            event_line(&PlaybackEvent::Auth { logged_in: true }).as_deref(),
            Some("logged in: yes")
        );
        assert_eq!(
            event_line(&PlaybackEvent::Upcoming(vec![])).as_deref(),
            Some("upcoming: 0 tracks")
        );
        let pos = PlaybackEvent::Position(Position {
            position_ms: 1,
            timestamp_ms: 1,
            speed: 1.0,
        });
        assert_eq!(event_line(&pos), None);
        assert_eq!(
            event_line(&PlaybackEvent::Context(Some("spotify:album:a".into()))).as_deref(),
            Some("context: spotify:album:a")
        );
        assert_eq!(
            event_line(&PlaybackEvent::Context(None)).as_deref(),
            Some("context: none")
        );
        let options = PlaybackEvent::Options {
            shuffle: true,
            repeat: Repeat::Context,
        };
        assert_eq!(
            event_line(&options).as_deref(),
            Some("shuffle: yes, repeat: context")
        );
        assert_eq!(
            event_line(&PlaybackEvent::Actions(Actions::default())),
            None
        );
        assert_eq!(
            event_line(&PlaybackEvent::ExpiresInDays(89)).as_deref(),
            Some("expires in: 89 days")
        );
        assert_eq!(
            event_line(&PlaybackEvent::ExpiresInDays(1)).as_deref(),
            Some("expires in: 1 day")
        );
        // The end of Soloist ends `player` with an error instead (`player_end`).
        let exited = PlaybackEvent::Exited {
            code: Some(3),
            tail: "boom".into(),
        };
        assert_eq!(event_line(&exited), None);
    }

    #[test]
    fn soloist_exit_ends_the_player_with_its_code_and_tail() {
        let crash = PlaybackEvent::Exited {
            code: Some(3),
            tail: "segfault\nboom".into(),
        };
        let err = player_end(&crash).expect("an exit ends the player");
        assert!(
            matches!(&err, SoloistError::Exited { code: Some(3), tail } if tail.contains("boom")),
            "{err}"
        );
        let text = err.to_string();
        assert!(text.contains('3') && text.contains("boom"), "{text}");
        let expired = PlaybackEvent::Exited {
            code: Some(10),
            tail: String::new(),
        };
        assert!(matches!(player_end(&expired), Some(SoloistError::Expired)));
        assert!(player_end(&PlaybackEvent::Error("Soloist connection closed".into())).is_none());
    }

    #[test]
    fn durations() {
        assert_eq!(fmt_duration(0), "0:00");
        assert_eq!(fmt_duration(105_000), "1:45");
        assert_eq!(fmt_duration(242_639), "4:02");
        assert_eq!(fmt_duration(3_600_000), "60:00");
    }

    #[test]
    fn utc_dates() {
        assert_eq!(ymd_utc(1_793_681_473_000), "2026-11-03");
        assert_eq!(ymd_utc(951_782_400_000), "2000-02-29");
        assert_eq!(ymd_utc(0), "1970-01-01");
    }

    #[test]
    fn row_and_card_lines() {
        let named = |name: &str| Named {
            uri: format!("spotify:artist:{name}"),
            name: name.into(),
        };
        let row = TrackRow {
            uri: "spotify:track:x".into(),
            uid: None,
            title: "Dancing with your eyes closed".into(),
            artists: vec![named("Jane Remover"), named("x")],
            album: None,
            duration_ms: 230_526,
            images: vec![],
            added_at: None,
        };
        assert_eq!(
            row_line(&row),
            "Dancing with your eyes closed — Jane Remover, x [spotify:track:x]"
        );
        let mut card = Card {
            uri: "spotify:album:a".into(),
            kind: spot_core::pages::CardKind::Album,
            title: "Revengeseekerz".into(),
            subtitle: "Jane Remover".into(),
            images: vec![],
        };
        assert_eq!(
            card_line(&card),
            "Revengeseekerz — Jane Remover [spotify:album:a]"
        );
        card.subtitle.clear();
        assert_eq!(card_line(&card), "Revengeseekerz [spotify:album:a]");
    }

    #[test]
    fn hash_lines() {
        assert_eq!(
            hash_line(Op::Home, &"ab".repeat(32), true),
            "home                 abababab cache"
        );
        assert_eq!(
            hash_line(Op::InLibrary, Op::InLibrary.fallback_hash(), false),
            format!(
                "areEntitiesInLibrary {} fallback",
                &Op::InLibrary.fallback_hash()[..8]
            )
        );
        assert_eq!(
            hash_line(Op::Recents, "abc", true),
            "recents              abc cache"
        );
    }

    #[test]
    fn track_line_format() {
        let t = TrackInfo {
            uri: "spotify:track:x".into(),
            title: "SPEED DEMON".into(),
            artists: vec!["Lucy Bedroque".into(), "Egobreak".into()],
            album: "SPEED DEMON / WHITEHORSEMANNEQUIN".into(),
            duration_ms: 145_000,
            cover_url: None,
        };
        assert_eq!(
            track_line(&t),
            "SPEED DEMON — Lucy Bedroque, Egobreak [SPEED DEMON / WHITEHORSEMANNEQUIN] 2:25"
        );
    }
}
