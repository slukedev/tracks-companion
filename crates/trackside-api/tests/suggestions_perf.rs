//! Suggestion timings on a realistic server: 50k songs, 5 listeners and 300k listens, with
//! Last.fm data stored for every artist and the most played songs. Every suggestion endpoint,
//! REST and Subsonic, cold (the first answer after the data it reads changed, or for a seed
//! not asked about yet) and warm (the same request again). Not run by default:
//! `cargo test -p trackside-api --test suggestions_perf -- --ignored --nocapture`.
//!
//! `TRACKSIDE_PERF_SONGS` changes the size, `TRACKSIDE_PERF_ONLY` runs only the endpoints
//! whose names contain it, `TRACKSIDE_PERF_DIR` keeps the seeded database
//! between runs, and `TRACKSIDE_PERF_LASTFM=1` has a configured key whose lookups take as
//! long as a paced Last.fm call would.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio_util::sync::CancellationToken;
use trackside_api::{ApiState, LocalState, NoOps, ServerInfo, wire};
use trackside_auth::{Auth, KeyKind, Scopes, SecretKey};
use trackside_core::access::{Permissions, Role};
use trackside_core::control::{
    AcquireControl, AcquireError, AcquireFeature, AcquireRequest, AcquisitionFilter,
    AcquisitionView, AdminAcquisitionView, ExternalQuery, ExternalSearch, InstantStream,
    ProviderChange, ProviderStatus, ProviderTest,
};
use trackside_core::db::FilesystemCheck;
use trackside_core::enrich::{EnrichControl, EnrichStatus, EnrichTest, Target};
use trackside_core::ids::random_id;
use trackside_core::repo::enrichment::{self as store, ArtistFacts, Kind, Related};
use trackside_core::repo::listens::{self, ListenSource, NewListen};
use trackside_core::testing::{FakeScanControl, TEST_START, scanned};
use trackside_core::{
    Access, AcquisitionId, BoxFuture, Clock, Config, Db, DbOptions, EventBus, Millis, Page, SongId,
    TestClock, UserKey,
};
use trackside_media::{Media, MediaError, TranscodeProcess, TranscodeSpec, Transcoder};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const DAY: i64 = 86_400_000;
const WORDS: [&str; 24] = [
    "love", "night", "heart", "dance", "fire", "blue", "dream", "rain", "city", "light", "home",
    "wild", "gold", "summer", "river", "ghost", "angel", "storm", "shadow", "sweet", "electric",
    "midnight", "paradise", "forever",
];
const GENRES: usize = 30;
const SONGS_PER_ARTIST: usize = 50;
const USERS: usize = 5;
const LISTENS_PER_USER: usize = 60_000;
/// Bump when the seeded data changes, so a kept database is rebuilt.
const FIXTURE_VERSION: &str = "2";

fn word(n: usize) -> &'static str {
    WORDS[n % WORDS.len()]
}

/// A small, repeatable generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    /// Skewed toward 0: most picks land on the first few.
    fn skewed(&mut self, n: usize) -> usize {
        let x = (self.next() % 10_000) as f64 / 10_000.0;
        ((x * x * x) * n as f64) as usize
    }
}

fn artist_name(a: usize) -> String {
    format!("{} {} {a}", word(a * 7 + 1), word(a / 24 + 5))
}

fn title(n: usize) -> String {
    format!("{} {} {n}", word(n), word(n / 3 + 1))
}

#[derive(Debug)]
struct NoTranscoder;

impl Transcoder for NoTranscoder {
    fn version(&self) -> Option<String> {
        None
    }
    fn spawn(&self, _spec: TranscodeSpec) -> Result<Box<dyn TranscodeProcess>, MediaError> {
        Err(MediaError::NoTranscoder)
    }
}

#[derive(Debug)]
struct OpenAcquisition;

fn absent<'a, T: Send + 'a>() -> BoxFuture<'a, Result<T, AcquireError>> {
    Box::pin(async { Err(AcquireError::NotFound) })
}

impl AcquireControl for OpenAcquisition {
    fn feature_for(&self, a: &Access, p: &Permissions, request: bool) -> Option<AcquireFeature> {
        (request && (a.is_admin() || p.can_request)).then_some(AcquireFeature {
            instant_play: false,
            external_play: false,
        })
    }
    fn search<'a>(
        &'a self,
        _: &'a Access,
        _: ExternalQuery,
    ) -> BoxFuture<'a, Result<ExternalSearch, AcquireError>> {
        absent()
    }
    fn search_more<'a>(
        &'a self,
        _: &'a Access,
        _: &'a str,
    ) -> BoxFuture<'a, Result<ExternalSearch, AcquireError>> {
        absent()
    }
    fn request<'a>(
        &'a self,
        _: &'a Access,
        _: AcquireRequest,
    ) -> BoxFuture<'a, Result<AcquisitionView, AcquireError>> {
        absent()
    }
    fn get<'a>(
        &'a self,
        _: &'a Access,
        _: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<AcquisitionView, AcquireError>> {
        absent()
    }
    fn list<'a>(
        &'a self,
        _: &'a Access,
        _: AcquisitionFilter,
    ) -> BoxFuture<'a, Result<Page<AcquisitionView>, AcquireError>> {
        absent()
    }
    fn cancel<'a>(
        &'a self,
        _: &'a Access,
        _: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<(), AcquireError>> {
        absent()
    }
    fn instant_stream<'a>(
        &'a self,
        _: &'a Access,
        _: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<InstantStream, AcquireError>> {
        Box::pin(async { Err(AcquireError::NotFound) })
    }
    fn providers(&self) -> BoxFuture<'_, Vec<ProviderStatus>> {
        Box::pin(async { Vec::new() })
    }
    fn configure(&self, _: ProviderChange) -> BoxFuture<'_, Result<(), AcquireError>> {
        Box::pin(async { Ok(()) })
    }
    fn test_provider<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, Result<ProviderTest, AcquireError>> {
        absent()
    }
    fn update_ytdlp(&self) -> BoxFuture<'_, Result<(), AcquireError>> {
        Box::pin(async { Ok(()) })
    }
    fn retry<'a>(&'a self, _: &'a AcquisitionId) -> BoxFuture<'a, Result<(), AcquireError>> {
        absent()
    }
    fn admin_list(
        &self,
        _: AcquisitionFilter,
    ) -> BoxFuture<'_, Result<Page<AdminAcquisitionView>, AcquireError>> {
        Box::pin(async { Ok(Page::default()) })
    }
}

/// A configured key with a slow Last.fm: a lookup a request waits for takes as long as the
/// worker's paced calls would (one for a song, three for an artist, 400 ms apart), and stores
/// nothing. Counts the lookups requests start.
#[derive(Debug)]
struct PacedLastFm {
    db: Db,
    on: bool,
    asked: AtomicUsize,
    /// Of those, put in line without waiting.
    queued: AtomicUsize,
}

impl EnrichControl for PacedLastFm {
    fn ensure(&self, targets: Vec<Target>, wait: Duration) -> BoxFuture<'_, ()> {
        self.asked.fetch_add(targets.len(), Ordering::Relaxed);
        Box::pin(async move {
            if !self.on {
                return;
            }
            let now = self.db.clock().now();
            let mut cost = Duration::ZERO;
            for target in targets {
                let (kind, key, calls) = match &target {
                    Target::Artist { identity, .. } => (Kind::Artist, identity.clone(), 3),
                    Target::Song { key, .. } => (Kind::Song, key.clone(), 1),
                    Target::Art { .. } => continue,
                };
                let fresh = self
                    .db
                    .read(move |c| store::is_fresh(c, kind, &key, now))
                    .await
                    .unwrap_or(true);
                if !fresh {
                    cost += Duration::from_millis(400) * calls;
                }
            }
            tokio::time::sleep(cost.min(wait)).await;
        })
    }
    fn queue(&self, targets: Vec<Target>) {
        self.asked.fetch_add(targets.len(), Ordering::Relaxed);
        self.queued.fetch_add(targets.len(), Ordering::Relaxed);
    }
    fn set_key(&self, _: Option<trackside_core::config::Secret>) {}
    fn status(&self) -> BoxFuture<'_, EnrichStatus> {
        Box::pin(async { EnrichStatus::default() })
    }
    fn test(&self, _: Option<trackside_core::config::Secret>) -> BoxFuture<'_, EnrichTest> {
        Box::pin(async {
            EnrichTest {
                ok: true,
                elapsed: Duration::ZERO,
                detail: None,
            }
        })
    }
}

struct Health;

impl trackside_api::HealthSource for Health {
    fn overview(&self) -> BoxFuture<'_, wire::admin::Overview> {
        Box::pin(async { wire::admin::Overview::default() })
    }
    fn ready(&self) -> wire::Ready {
        wire::Ready {
            status: wire::ReadyStatus::Ready,
            checks: std::collections::BTreeMap::default(),
            notes: Vec::new(),
        }
    }
}

/// When one person listened: about 82 songs a day for two years, in three sessions a day
/// (morning, midday, evening), one song every three and a half minutes or so.
fn listening_times(rng: &mut Rng) -> Vec<i64> {
    const DAYS: usize = 730;
    let mut out = Vec::with_capacity(LISTENS_PER_USER);
    for day in 0..DAYS {
        let today = LISTENS_PER_USER * (day + 1) / DAYS - LISTENS_PER_USER * day / DAYS;
        let midnight = TEST_START.0 - DAY - day as i64 * DAY;
        let mut left = today;
        for (session, hour) in [8_i64, 13, 19].into_iter().enumerate() {
            let n = if session == 2 { left } else { today / 3 };
            left -= n;
            let mut at = midnight + hour * DAY / 24 + rng.below(3_600_000) as i64;
            for _ in 0..n {
                out.push(at);
                at += 180_000 + rng.below(80_000) as i64;
            }
        }
    }
    out
}

fn open(path: &Path, clock: Arc<TestClock>) -> TestResult<Db> {
    let mut options = DbOptions::new(path);
    options.filesystem = FilesystemCheck::AssumeLocal;
    Ok(Db::open(&options, EventBus::default(), clock)?)
}

/// Songs by 1000 artists (50 each, albums of 12, one genre and decade per artist, every 17th
/// song featuring the next artist), 5 members with their own favourite artists, 60k listens
/// each over two years (more lately, 3% sent by name only and 2% of songs not on the server),
/// and Last.fm data for every artist and the 3000 most played songs.
#[expect(clippy::too_many_lines, reason = "one seeding script")]
fn seed(db: &Db, root: &Path, total: usize) -> TestResult {
    let started = Instant::now();
    let library = db.write_blocking({
        let root = root.to_path_buf();
        move |tx| {
            trackside_core::repo::libraries::create(
                tx,
                &trackside_core::repo::libraries::NewLibrary {
                    name: String::from("Music"),
                    root,
                    writable: false,
                    watch: false,
                    grant_all_users: false,
                    granted_by: None,
                },
            )
        }
    })?;
    let artists = total.div_ceil(SONGS_PER_ARTIST);
    let mut ids: Vec<SongId> = Vec::with_capacity(total);
    for chunk in 0..total.div_ceil(1000) {
        let files: Vec<_> = (chunk * 1000..((chunk + 1) * 1000).min(total))
            .map(|n| {
                let a = n / SONGS_PER_ARTIST;
                let mut artist = artist_name(a);
                if n % 17 == 0 {
                    artist = format!("{artist} feat. {}", artist_name((a + 1) % artists));
                }
                let album = format!("{} {}", word(n / 12 + 5), n / 12);
                let mut file = scanned(
                    &format!("{a}/{}/{n}.flac", n / 12),
                    &title(n),
                    &artist,
                    Some(&album),
                );
                file.tags.album_artist = Some(artist_name(a));
                file.tags.track = Some((n % 12) as u32 + 1);
                file.tags.year = Some(1960 + (a % 60) as i32);
                file.tags.genres = vec![format!("{} {}", word(a % GENRES), a % GENRES)];
                file
            })
            .collect();
        let found = db.write_blocking(move |tx| {
            let run = random_id();
            trackside_core::ingest::apply_batch(
                tx,
                library.key,
                &run,
                trackside_core::control::SongOrigin::Scan,
                &files,
            )
        })?;
        ids.extend(found.iter().map(|r| r.song().clone()));
    }
    println!(
        "seeded {total} songs in {:.1} s",
        started.elapsed().as_secs_f64()
    );

    let mut users = Vec::new();
    for u in 0..USERS {
        let user = db.write_blocking(move |tx| {
            trackside_core::repo::users::create(
                tx,
                &trackside_core::repo::users::NewUser {
                    username: format!("listener{u}"),
                    display_name: format!("Listener {u}"),
                    password_hash: String::from("$argon2id$test"),
                    role: if u == 0 { Role::Admin } else { Role::Member },
                    permissions: Permissions::default(),
                    libraries: vec![library.key],
                    service: None,
                },
            )
        })?;
        users.push(user.key);
    }

    let started = Instant::now();
    let mut played: Vec<u32> = vec![0; total];
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for (u, user) in users.iter().copied().enumerate() {
        // Each listener's 40 favourite artists sit in their own stretch of the catalogue.
        let favourites: Vec<usize> = (0..40)
            .map(|i| (u * artists / USERS + i * 3 + rng.below(3)) % artists)
            .collect();
        let mut plays: Vec<(usize, i64)> = listening_times(&mut rng)
            .into_iter()
            .map(|at| {
                let song = if rng.below(10) < 7 {
                    let a = favourites[rng.skewed(favourites.len())];
                    (a * SONGS_PER_ARTIST + rng.skewed(SONGS_PER_ARTIST)).min(total - 1)
                } else {
                    rng.below(total)
                };
                (song, at)
            })
            .collect();
        plays.sort_by_key(|p| p.1);
        for (batch_no, batch) in plays.chunks(2000).enumerate() {
            let items: Vec<NewListen> = batch
                .iter()
                .enumerate()
                .map(|(i, &(n, at))| {
                    let roll = (n * 31 + i) % 100;
                    let missing = roll < 2;
                    played[n] += 1;
                    NewListen {
                        client_listen_id: format!("{u}-{batch_no}-{i}"),
                        song: (roll >= 5).then(|| ids[n].clone()),
                        title: if missing {
                            format!("Not Here {n}")
                        } else {
                            title(n)
                        },
                        artist: artist_name(n / SONGS_PER_ARTIST),
                        album: None,
                        started_at: Millis(at),
                        played_ms: Some(180_000),
                        duration_ms: Some(180_000),
                        source: ListenSource::Native,
                    }
                })
                .collect();
            db.write_blocking(move |tx| listens::submit(tx, user, None, &items))?;
        }
    }
    println!(
        "seeded {} listens in {:.1} s",
        USERS * LISTENS_PER_USER,
        started.elapsed().as_secs_f64()
    );

    let started = Instant::now();
    let mut ranked: Vec<usize> = (0..total).collect();
    ranked.sort_by_key(|n| std::cmp::Reverse(played[*n]));
    for chunk in (0..artists).collect::<Vec<_>>().chunks(100) {
        let chunk = chunk.to_vec();
        db.write_blocking(move |tx| {
            for a in chunk {
                let name = artist_name(a);
                let facts = ArtistFacts {
                    similar: (1..=30)
                        .map(|i| Related {
                            title: String::new(),
                            artist: if i % 2 == 0 {
                                artist_name((a + i * 7) % artists)
                            } else {
                                format!("Elsewhere {a} {i}")
                            },
                            score: 1.0 - i as f64 / 40.0,
                        })
                        .collect(),
                    top: (0..20)
                        .map(|i| {
                            if i % 2 == 0 {
                                title(a * SONGS_PER_ARTIST + i)
                            } else {
                                format!("Hit {a} {i}")
                            }
                        })
                        .collect(),
                    bio: Some(format!("{name} is a band.")),
                };
                store::store_artist(tx, &tracks_model::folded(&name), &name, &facts, 30 * DAY)?;
            }
            Ok(())
        })?;
    }
    let top: Vec<usize> = ranked.iter().copied().take(3000).collect();
    for chunk in top.chunks(200) {
        let chunk = chunk.to_vec();
        db.write_blocking(move |tx| {
            for n in chunk {
                let key = tracks_model::song_key(&title(n), &artist_name(n / SONGS_PER_ARTIST));
                let similar: Vec<Related> = (1..=50)
                    .map(|i| {
                        let other = (n * 13 + i * 101) % total;
                        if i % 5 < 3 {
                            Related {
                                title: title(other),
                                artist: artist_name(other / SONGS_PER_ARTIST),
                                score: 1.0 - i as f64 / 60.0,
                            }
                        } else {
                            Related {
                                title: format!("Missing {other}"),
                                artist: artist_name(other / SONGS_PER_ARTIST),
                                score: 1.0 - i as f64 / 60.0,
                            }
                        }
                    })
                    .collect();
                store::store_song(tx, &key, &similar, 30 * DAY)?;
            }
            Ok(())
        })?;
    }
    db.checkpoint_and_optimize()?;
    println!(
        "seeded Last.fm data in {:.1} s",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

struct Server {
    base: String,
    http: reqwest::Client,
    db: Db,
    clock: Arc<TestClock>,
    auth: Arc<Auth>,
    keys: Vec<(UserKey, String)>,
    lastfm: Arc<PacedLastFm>,
    stop: CancellationToken,
    _dir: Option<tempfile::TempDir>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one server, wired as trackside-server does"
)]
async fn start(total: usize) -> TestResult<Server> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (dir, keep) = match std::env::var_os("TRACKSIDE_PERF_DIR") {
        Some(path) => (PathBuf::from(path), None),
        None => {
            let temp = tempfile::tempdir()?;
            (temp.path().to_path_buf(), Some(temp))
        }
    };
    std::fs::create_dir_all(&dir)?;
    let stamp = format!("{FIXTURE_VERSION}-{total}");
    let db_path = dir.join(format!("perf-{stamp}.db"));
    let clock = Arc::new(TestClock::new(TEST_START));
    if !db_path.exists() {
        let partial = dir.join(format!("perf-{stamp}.partial.db"));
        let _ = std::fs::remove_file(&partial);
        {
            let db = open(&partial, clock.clone())?;
            seed(&db, &dir.join("music"), total)?;
        }
        std::fs::rename(&partial, &db_path)?;
    }
    let db = open(&db_path, clock.clone())?;
    let data = dir.join("data");
    let mut config = Config::defaults(&data);
    config.auth.argon2_memory = 1 << 16;
    config.auth.argon2_iterations = 1;
    config.media.cache_min_free = 0;
    let auth = Arc::new(Auth::new(
        db.clone(),
        &config.auth,
        SecretKey::from_bytes([7; 32]),
        Vec::new(),
        clock.clone(),
    ));
    let media = Arc::new(Media::new(
        &config.media,
        &data.join("cache"),
        db.clone(),
        Arc::new(NoTranscoder),
        clock.clone(),
    )?);
    let lastfm = Arc::new(PacedLastFm {
        db: db.clone(),
        on: std::env::var("TRACKSIDE_PERF_LASTFM").as_deref() == Ok("1"),
        asked: AtomicUsize::new(0),
        queued: AtomicUsize::new(0),
    });
    let config = Arc::new(config);
    let stop = CancellationToken::new();
    let state = ApiState {
        db: db.clone(),
        auth: Arc::clone(&auth),
        media: Arc::clone(&media),
        scan: Arc::new(FakeScanControl::new()),
        acquire: Arc::new(OpenAcquisition),
        enrich: lastfm.clone(),
        assistant: std::sync::Arc::new(trackside_core::NoAssistant),
        events: db.events().clone(),
        config: Arc::clone(&config),
        server: ServerInfo::new(uuid::Uuid::new_v4(), "Home", clock.now()),
        health: Arc::new(Health),
        ops: Arc::new(NoOps),
        local: Arc::new(LocalState::new(stop.clone())),
    };
    let subsonic = trackside_subsonic::SubsonicState {
        db: db.clone(),
        auth: Arc::clone(&auth),
        media,
        scan: Arc::new(FakeScanControl::new()),
        enrich: lastfm.clone(),
        config,
        server_id: uuid::Uuid::nil(),
    };
    let app = trackside_api::router(state).merge(trackside_subsonic::router(subsonic));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address: SocketAddr = listener.local_addr()?;
    let shutdown = stop.clone();
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await;
    });
    let users: Vec<UserKey> = db.read_blocking(|c| {
        Ok(c.prepare("SELECT id FROM users ORDER BY id")?
            .query_map([], |row| Ok(UserKey(row.get(0)?)))?
            .collect::<Result<_, _>>()?)
    })?;
    let mut keys = Vec::new();
    for user in users {
        let key = auth
            .create_key(user, KeyKind::ApiKey, "Perf", Scopes::ALL)
            .await?
            .secret;
        keys.push((user, key));
    }
    Ok(Server {
        base: format!("http://{address}"),
        http: reqwest::Client::new(),
        db,
        clock,
        auth,
        keys,
        lastfm,
        stop,
        _dir: keep,
    })
}

impl Server {
    /// New keys for everyone: the per-key request rate refills by the test clock, which
    /// stands still.
    async fn fresh_keys(&mut self) -> TestResult {
        for (user, key) in &mut self.keys {
            *key = self
                .auth
                .create_key(*user, KeyKind::ApiKey, "Perf", Scopes::ALL)
                .await?
                .secret;
        }
        Ok(())
    }

    async fn get(&self, key: &str, path: &str) -> TestResult<(Duration, Value)> {
        let url = if path.starts_with("/rest/") {
            let sep = if path.contains('?') { '&' } else { '?' };
            format!(
                "{}{path}{sep}apiKey={key}&f=json&v=1.16.1&c=perf",
                self.base
            )
        } else {
            format!("{}/api/v1{path}", self.base)
        };
        let started = Instant::now();
        let response = self.http.get(&url).bearer_auth(key).send().await?;
        let status = response.status();
        let body: Value = response.json().await?;
        let took = started.elapsed();
        if status != 200 {
            return Err(format!("{path}: {status} {body}").into());
        }
        Ok((took, body))
    }

    /// One more listen by `user`, so what reads their listens has changed.
    async fn listen(&self, user: UserKey, n: usize) -> TestResult {
        let at = Millis(self.clock.now().0 - 60_000);
        let song: SongId = self
            .db
            .read(move |c| {
                Ok(c.query_row(
                    "SELECT pid FROM songs ORDER BY id LIMIT 1 OFFSET ?1",
                    [n as i64 % 1000],
                    |row| row.get(0),
                )?)
            })
            .await?;
        let item = NewListen {
            client_listen_id: format!("perf-{n}-{}", uuid::Uuid::new_v4()),
            song: Some(song),
            title: String::from("x"),
            artist: String::from("y"),
            album: None,
            started_at: at,
            played_ms: None,
            duration_ms: None,
            source: ListenSource::Native,
        };
        self.db
            .write(move |tx| listens::submit(tx, user, None, &[item]))
            .await?;
        Ok(())
    }
}

fn percentile(samples: &mut [Duration], p: f64) -> f64 {
    samples.sort_unstable();
    let rank = ((p * samples.len() as f64).ceil() as usize).clamp(1, samples.len());
    samples[rank - 1].as_secs_f64() * 1000.0
}

fn report(name: &str, cold: &mut [Duration], warm: &mut [Duration]) {
    println!(
        "{name:<34} cold p50 {:>8.1} p95 {:>8.1} ms | warm p50 {:>8.1} p95 {:>8.1} ms",
        percentile(cold, 0.5),
        percentile(cold, 0.95),
        percentile(warm, 0.5),
        percentile(warm, 0.95),
    );
}

const COLD: usize = 15;
const WARM: usize = 20;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "seeds 50k songs and 300k listens; run with --ignored --nocapture"]
#[expect(clippy::too_many_lines, reason = "one measurement after another")]
async fn suggestion_endpoints_are_fast() -> TestResult {
    let total: usize = std::env::var("TRACKSIDE_PERF_SONGS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50_000);
    let mut server = start(total).await?;
    let (songs, artists): (Vec<String>, Vec<String>) = server.db.read_blocking(|c| {
        let songs = c
            .prepare("SELECT pid FROM songs WHERE id % 97 = 1 ORDER BY id LIMIT 200")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        let artists = c
            .prepare(
                "SELECT pid FROM artists WHERE id IN (SELECT artist_id FROM songs)
                 ORDER BY id LIMIT 200",
            )?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        Ok((songs, artists))
    })?;
    println!(
        "--- {total} songs, {USERS} users, {} listens; Last.fm waits {}",
        USERS * LISTENS_PER_USER,
        if server.lastfm.on { "on" } else { "off" }
    );

    // Per-user shelves: cold is the first answer after one more listen by the caller.
    let shelves = [
        ("/me/picks", "GET /me/picks"),
        ("/me/get-more", "GET /me/get-more"),
        ("/me/mixes/onRepeat", "GET /me/mixes/onRepeat"),
        ("/me/mixes/rediscover", "GET /me/mixes/rediscover"),
        ("/songs/top", "GET /songs/top"),
    ];
    let mut n = 0;
    let only = std::env::var("TRACKSIDE_PERF_ONLY").unwrap_or_default();
    for (path, name) in shelves {
        if !name.contains(&only) {
            continue;
        }
        server.fresh_keys().await?;
        let (mut cold, mut warm) = (Vec::new(), Vec::new());
        for i in 0..COLD {
            let (user, key) = &server.keys[i % server.keys.len()];
            n += 1;
            server.listen(*user, n).await?;
            cold.push(server.get(key, path).await?.0);
        }
        let (_, key) = &server.keys[1];
        server.get(key, path).await?;
        for _ in 0..WARM {
            warm.push(server.get(key, path).await?.0);
        }
        report(name, &mut cold, &mut warm);
    }

    // Seeded endpoints: cold is a seed not asked about yet, warm the same seed again.
    let seeded: [(&str, &str, bool); 6] = [
        ("GET /songs/{id}/similar", "/songs/{}/similar", true),
        ("GET /artists/{id}/similar", "/artists/{}/similar", false),
        (
            "GET /artists/{id}/top-songs",
            "/artists/{}/top-songs",
            false,
        ),
        (
            "getSimilarSongs2 (song)",
            "/rest/getSimilarSongs2?id={}",
            true,
        ),
        ("getTopSongs", "/rest/getTopSongs?id={}", false),
        ("getArtistInfo2", "/rest/getArtistInfo2?id={}", false),
    ];
    for (round, (name, template, by_song)) in seeded.into_iter().enumerate() {
        if !name.contains(&only) {
            continue;
        }
        server.fresh_keys().await?;
        let ids = if by_song { &songs } else { &artists };
        let (mut cold, mut warm) = (Vec::new(), Vec::new());
        let (_, key) = &server.keys[1];
        for i in 0..COLD {
            let id = &ids[(round * COLD + i) % ids.len()];
            cold.push(server.get(key, &template.replace("{}", id)).await?.0);
        }
        let id = &ids[(round * COLD + COLD + 1) % ids.len()];
        let path = template.replace("{}", id);
        server.get(key, &path).await?;
        for _ in 0..WARM {
            warm.push(server.get(key, &path).await?.0);
        }
        report(name, &mut cold, &mut warm);
    }
    // getSimilarSongs2 by artist, as Subsonic clients start artist radio.
    if !"getSimilarSongs2 (artist)".contains(&only) {
        return Ok(());
    }
    server.fresh_keys().await?;
    let (_, key) = &server.keys[1];
    let (mut cold, mut warm) = (Vec::new(), Vec::new());
    for i in 0..COLD {
        let id = &artists[(100 + i) % artists.len()];
        cold.push(
            server
                .get(key, &format!("/rest/getSimilarSongs2?id={id}"))
                .await?
                .0,
        );
    }
    let path = format!("/rest/getSimilarSongs2?id={}", artists[150]);
    server.get(key, &path).await?;
    for _ in 0..WARM {
        warm.push(server.get(key, &path).await?.0);
    }
    report("getSimilarSongs2 (artist)", &mut cold, &mut warm);
    println!(
        "Last.fm lookups requests started: {}, of which queued without waiting: {}",
        server.lastfm.asked.load(Ordering::Relaxed),
        server.lastfm.queued.load(Ordering::Relaxed)
    );
    Ok(())
}
