//! The background worker: one call at a time, paced, looking up what people play first and
//! what a request is waiting for before anything else.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use futures_util::future::join_all;
use serde_json::{Map, Value};
use tokio::sync::{Notify, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use trackside_core::config::Secret;
use trackside_core::enrich::{
    EnrichControl, EnrichStatus, EnrichTest, ExternalArtStore, QUEUE_MAX, Target,
};
use trackside_core::repo::enrichment::{self as store, ArtistFacts, Kind};
use trackside_core::{BoxFuture, Db, DbError, Millis};

use crate::art::{self, ArtError};
use crate::lastfm::{self, LastFmError, Method};

/// How the worker paces itself and how long answers are kept.
#[derive(Clone, Debug)]
pub struct EnrichOptions {
    pub endpoint: String,
    /// The least time between two calls. Last.fm asks for at most 5 a second.
    pub min_interval: Duration,
    pub ttl: Duration,
    /// How long "Last.fm does not know it" is kept.
    pub unknown_ttl: Duration,
    /// The first wait after a rate limit or an outage; it doubles up to `backoff_max`.
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
    pub request_timeout: Duration,
    /// With nothing due, how often to look again.
    pub idle_recheck: Duration,
    /// Due artists and songs read per pass.
    pub batch: u32,
    /// Similar artists and songs and top songs asked for per call.
    pub limit: u32,
    /// The only origins cover images are fetched from.
    pub image_hosts: Vec<String>,
    /// How long a cover that could not be fetched (an unreachable host) waits before it is
    /// looked up again.
    pub art_retry_ttl: Duration,
}

impl Default for EnrichOptions {
    fn default() -> Self {
        Self {
            endpoint: String::from(lastfm::ENDPOINT),
            min_interval: Duration::from_millis(400),
            ttl: Duration::from_hours(30 * 24),
            unknown_ttl: Duration::from_hours(7 * 24),
            backoff_initial: Duration::from_secs(30),
            backoff_max: Duration::from_secs(3600),
            request_timeout: Duration::from_secs(10),
            idle_recheck: Duration::from_secs(3600),
            batch: 25,
            limit: 50,
            image_hosts: crate::art::IMAGE_HOSTS
                .iter()
                .map(|h| (*h).to_owned())
                .collect(),
            art_retry_ttl: Duration::from_hours(1),
        }
    }
}

/// The artist asked about by the dashboard's test.
const TEST_ARTIST: &str = "Radiohead";

#[derive(Default)]
struct State {
    urgent: VecDeque<Target>,
    background: VecDeque<Target>,
    waiters: HashMap<Target, Vec<oneshot::Sender<()>>>,
    last_error: Option<(Millis, String)>,
    /// Last.fm refused the key; nothing is fetched until it changes.
    rejected: bool,
}

struct Inner {
    db: Db,
    http: reqwest::Client,
    images: reqwest::Client,
    art_store: RwLock<Option<Arc<dyn ExternalArtStore>>>,
    options: EnrichOptions,
    key: RwLock<Option<Secret>>,
    state: Mutex<State>,
    wake: Notify,
    next_call: tokio::sync::Mutex<Instant>,
}

/// Fills the similarity tables from Last.fm with one server-wide key.
pub struct Enricher {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Enricher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Enricher").finish_non_exhaustive()
    }
}

/// Why the worker could not start.
#[derive(Debug, thiserror::Error)]
#[error("cannot build the HTTP client: {0}")]
pub struct StartError(#[from] reqwest::Error);

enum Outcome {
    Done,
    Retry(LastFmError),
    KeyRejected(LastFmError),
    Skipped(String),
}

impl Enricher {
    /// Starts the worker; it stops when `shutdown` is cancelled.
    pub fn start(
        db: Db,
        key: Option<Secret>,
        options: EnrichOptions,
        shutdown: CancellationToken,
    ) -> Result<Arc<Self>, StartError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .user_agent(concat!("Trackside/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        let inner = Arc::new(Inner {
            db,
            http,
            images: art::client()?,
            art_store: RwLock::new(None),
            options,
            key: RwLock::new(key),
            state: Mutex::new(State::default()),
            wake: Notify::new(),
            next_call: tokio::sync::Mutex::new(Instant::now()),
        });
        tokio::spawn(run(Arc::clone(&inner), shutdown));
        Ok(Arc::new(Self { inner }))
    }

    /// Where covers for suggested songs go. Without one, cover lookups are skipped.
    pub fn set_art_store(&self, store: Arc<dyn ExternalArtStore>) {
        *self
            .inner
            .art_store
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(store);
    }
}

impl Inner {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn key(&self) -> Option<Secret> {
        self.key
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn now(&self) -> Millis {
        self.db.clock().now()
    }

    fn ttl_ms(d: Duration) -> i64 {
        i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
    }

    /// One paced call.
    async fn call(
        &self,
        key: &Secret,
        method: Method,
        params: &[(&str, &str)],
    ) -> Result<Map<String, Value>, LastFmError> {
        {
            let mut next = self.next_call.lock().await;
            tokio::time::sleep_until(*next).await;
            *next = Instant::now() + self.options.min_interval;
        }
        let url = lastfm::url(&self.options.endpoint, key.expose(), method, params);
        lastfm::get(&self.http, &url, self.options.request_timeout).await
    }

    fn note_error(&self, message: String) {
        tracing::warn!(error = %message, "Last.fm lookup failed");
        self.state().last_error = Some((self.now(), message));
    }

    fn finish(&self, target: &Target) {
        if let Some(waiters) = self.state().waiters.remove(target) {
            for waiter in waiters {
                let _ = waiter.send(());
            }
        }
    }

    async fn is_fresh(&self, target: &Target) -> Result<bool, DbError> {
        let now = self.now();
        if let Target::Art { key, .. } = target {
            let key = key.clone();
            return self
                .db
                .read(move |c| store::art_is_fresh(c, &key, now))
                .await;
        }
        let (kind, key) = kind_key(target);
        let key = key.to_owned();
        self.db
            .read(move |c| store::is_fresh(c, kind, &key, now))
            .await
    }

    fn art_store(&self) -> Option<Arc<dyn ExternalArtStore>> {
        self.art_store
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// A paced image download from an allowed host.
    async fn download(&self, url: reqwest::Url) -> Result<Vec<u8>, ArtError> {
        {
            let mut next = self.next_call.lock().await;
            tokio::time::sleep_until(*next).await;
            *next = Instant::now() + self.options.min_interval;
        }
        art::download(&self.images, url, self.options.request_timeout).await
    }

    /// The cover of a suggested song: `track.getInfo`, then `album.getInfo` when the track's
    /// album has no image, then the image itself, checked and kept in the artwork cache.
    async fn fetch_art(&self, key: &Secret, song_key: &str, title: &str, artist: &str) -> Outcome {
        let Some(store) = self.art_store() else {
            return Outcome::Done;
        };
        let found = async {
            let info = self
                .call(
                    key,
                    Method::TrackInfo,
                    &[("artist", artist), ("track", title)],
                )
                .await?;
            let cover = lastfm::track_cover(&info);
            if cover.image.is_some() {
                return Ok(cover.image);
            }
            let Some(album) = cover.album else {
                return Ok(None);
            };
            let by = cover.album_artist.unwrap_or_else(|| artist.to_owned());
            let info = self
                .call(
                    key,
                    Method::AlbumInfo,
                    &[("artist", &by), ("album", &album)],
                )
                .await?;
            Ok::<_, LastFmError>(lastfm::album_cover(&info))
        }
        .await;
        let image = match found {
            Ok(image) => image,
            Err(e) if e.is_unknown() => None,
            Err(e) if e.is_key_problem() => return Outcome::KeyRejected(e),
            Err(e) if e.is_retryable() => return Outcome::Retry(e),
            Err(e) => {
                self.note_error(e.to_string());
                None
            }
        };
        let mut ttl = self.options.unknown_ttl;
        let mut stored = None;
        if let Some(url) = image {
            match self.keep_image(&store, &url).await {
                Ok(id) => {
                    stored = Some(id);
                    ttl = self.options.ttl;
                }
                Err(e) => {
                    if e.is_transient() {
                        ttl = self.options.art_retry_ttl;
                    }
                    tracing::debug!(error = %e, "cover not kept");
                }
            }
        }
        let (song_key, ttl) = (song_key.to_owned(), Self::ttl_ms(ttl));
        match self
            .db
            .write(move |tx| store::store_art(tx, &song_key, stored.as_ref(), ttl))
            .await
        {
            Ok(()) => Outcome::Done,
            Err(e) => Outcome::Skipped(e.to_string()),
        }
    }

    /// Fetches and stores the image at `url` unless it is already held. Never fetches from
    /// a host outside the allowed list.
    async fn keep_image(
        &self,
        store: &Arc<dyn ExternalArtStore>,
        url: &str,
    ) -> Result<trackside_core::ExternalArtId, ArtError> {
        let url = art::allowed(url, &self.options.image_hosts).ok_or(ArtError::Host)?;
        let id = art::image_id(&url);
        if store.has(&id) {
            return Ok(id);
        }
        let bytes = self.download(url).await?;
        let (store, kept) = (Arc::clone(store), id.clone());
        tokio::task::spawn_blocking(move || store.put(&kept, &bytes))
            .await
            .map_err(|e| ArtError::NotImage(e.to_string()))?
            .map_err(ArtError::NotImage)?;
        Ok(id)
    }

    async fn fetch(&self, key: &Secret, target: &Target) -> Outcome {
        let limit = self.options.limit.to_string();
        let result = match target {
            Target::Art {
                key: song_key,
                title,
                artist,
            } => return self.fetch_art(key, song_key, title, artist).await,
            Target::Artist { name, .. } => self.fetch_artist(key, name, &limit).await,
            Target::Song { title, artist, .. } => self
                .call(
                    key,
                    Method::TrackSimilar,
                    &[("artist", artist), ("track", title), ("limit", &limit)],
                )
                .await
                .map(|json| Fetched::Song(lastfm::similar_tracks(&json))),
        };
        let fetched = match result {
            Ok(fetched) => Some(fetched),
            Err(e) if e.is_unknown() => None,
            Err(e) if e.is_key_problem() => return Outcome::KeyRejected(e),
            Err(e) if e.is_retryable() => return Outcome::Retry(e),
            Err(e) => {
                // A malformed answer or a bare 4xx: keep it as unknown rather than ask again.
                self.note_error(e.to_string());
                None
            }
        };
        match self.save(target, fetched).await {
            Ok(()) => Outcome::Done,
            Err(e) => Outcome::Skipped(e.to_string()),
        }
    }

    async fn fetch_artist(
        &self,
        key: &Secret,
        name: &str,
        limit: &str,
    ) -> Result<Fetched, LastFmError> {
        let similar = self
            .call(
                key,
                Method::ArtistSimilar,
                &[("artist", name), ("limit", limit)],
            )
            .await?;
        let top = self
            .call(
                key,
                Method::ArtistTopTracks,
                &[("artist", name), ("limit", limit)],
            )
            .await?;
        let info = self
            .call(key, Method::ArtistInfo, &[("artist", name)])
            .await?;
        Ok(Fetched::Artist(ArtistFacts {
            similar: lastfm::similar_artists(&similar),
            top: lastfm::top_tracks(&top),
            bio: lastfm::artist_bio(&info),
        }))
    }

    async fn save(&self, target: &Target, fetched: Option<Fetched>) -> Result<(), DbError> {
        if matches!(target, Target::Art { .. }) {
            return Ok(());
        }
        let ttl = Self::ttl_ms(self.options.ttl);
        let unknown_ttl = Self::ttl_ms(self.options.unknown_ttl);
        let target = target.clone();
        self.db
            .write(move |tx| match (&target, fetched) {
                (Target::Artist { identity, name }, Some(Fetched::Artist(facts))) => {
                    store::store_artist(tx, identity, name, &facts, ttl)
                }
                (Target::Song { key, .. }, Some(Fetched::Song(similar))) => {
                    store::store_song(tx, key, &similar, ttl)
                }
                (target, _) => {
                    let (kind, key) = kind_key(target);
                    store::store_unknown(tx, kind, key, unknown_ttl)
                }
            })
            .await
    }

    /// The next target: one a request waits for, else the next due one.
    async fn next(&self) -> Result<Option<Target>, DbError> {
        if let Some(target) = self.state().urgent.pop_front() {
            return Ok(Some(target));
        }
        if let Some(target) = self.state().background.pop_front() {
            return Ok(Some(target));
        }
        let now = self.now();
        let batch = self.options.batch;
        let (artists, songs) = self
            .db
            .read(move |c| {
                Ok((
                    store::due_artists(c, now, batch)?,
                    store::due_songs(c, now, batch)?,
                ))
            })
            .await?;
        if artists.is_empty() && songs.is_empty() {
            self.db.write(|tx| store::prune_names(tx)).await?;
            return Ok(None);
        }
        let mut artists = artists
            .into_iter()
            .map(|(identity, name)| Target::Artist { identity, name });
        let mut songs =
            songs
                .into_iter()
                .map(|(key, title, artist)| Target::Song { key, title, artist });
        let mut state = self.state();
        loop {
            let (a, s) = (artists.next(), songs.next());
            if a.is_none() && s.is_none() {
                break;
            }
            state.background.extend(a.into_iter().chain(s));
        }
        Ok(state.background.pop_front())
    }
}

enum Fetched {
    Artist(ArtistFacts),
    Song(Vec<store::Related>),
}

fn kind_key(target: &Target) -> (Kind, &str) {
    match target {
        Target::Artist { identity, .. } => (Kind::Artist, identity),
        Target::Song { key, .. } | Target::Art { key, .. } => (Kind::Song, key),
    }
}

async fn idle(inner: &Inner, shutdown: &CancellationToken, wait: Option<Duration>) -> bool {
    let sleep = async {
        match wait {
            Some(d) => tokio::time::sleep(d).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        () = shutdown.cancelled() => false,
        () = inner.wake.notified() => true,
        () = sleep => true,
    }
}

async fn run(inner: Arc<Inner>, shutdown: CancellationToken) {
    let mut backoff = inner.options.backoff_initial;
    while !shutdown.is_cancelled() {
        let key = inner.key().filter(|_| !inner.state().rejected);
        let Some(key) = key else {
            if !idle(&inner, &shutdown, None).await {
                return;
            }
            continue;
        };
        let target = match inner.next().await {
            Ok(Some(target)) => target,
            Ok(None) => {
                if !idle(&inner, &shutdown, Some(inner.options.idle_recheck)).await {
                    return;
                }
                continue;
            }
            Err(e) => {
                inner.note_error(e.to_string());
                if !idle(&inner, &shutdown, Some(backoff)).await {
                    return;
                }
                continue;
            }
        };
        if inner.is_fresh(&target).await.unwrap_or(false) {
            inner.finish(&target);
            continue;
        }
        let outcome = tokio::select! {
            () = shutdown.cancelled() => return,
            outcome = inner.fetch(&key, &target) => outcome,
        };
        match outcome {
            Outcome::Done => {
                backoff = inner.options.backoff_initial;
                inner.state().last_error = None;
            }
            Outcome::Retry(e) => {
                inner.note_error(e.to_string());
                inner.state().urgent.push_front(target.clone());
                inner.finish(&target);
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(inner.options.backoff_max);
                continue;
            }
            Outcome::KeyRejected(e) => {
                inner.note_error(e.to_string());
                let mut state = inner.state();
                state.rejected = true;
                state.urgent.push_front(target.clone());
                // Nothing runs until the key changes, so nobody should wait.
                state.waiters.clear();
            }
            Outcome::Skipped(e) => inner.note_error(e),
        }
        inner.finish(&target);
    }
}

impl EnrichControl for Enricher {
    fn ensure(&self, targets: Vec<Target>, wait: Duration) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let inner = &self.inner;
            if inner.key().is_none() || inner.state().rejected {
                return;
            }
            let mut pending = Vec::new();
            for target in targets {
                if !inner.is_fresh(&target).await.unwrap_or(true) {
                    pending.push(target);
                }
            }
            if pending.is_empty() {
                return;
            }
            let receivers: Vec<_> = {
                let mut state = inner.state();
                if state.rejected {
                    return;
                }
                pending
                    .into_iter()
                    .rev()
                    .map(|target| {
                        let (tx, rx) = oneshot::channel();
                        if !state.urgent.contains(&target) {
                            state.urgent.push_front(target.clone());
                        }
                        state.waiters.entry(target).or_default().push(tx);
                        rx
                    })
                    .collect()
            };
            inner.wake.notify_one();
            let _ = tokio::time::timeout(wait, join_all(receivers)).await;
        })
    }

    fn queue(&self, targets: Vec<Target>) {
        let inner = &self.inner;
        if inner.key().is_none() {
            return;
        }
        {
            let mut state = inner.state();
            if state.rejected {
                return;
            }
            let (art, lookups): (Vec<Target>, Vec<Target>) = targets
                .into_iter()
                .partition(|t| matches!(t, Target::Art { .. }));
            for target in lookups.into_iter().rev() {
                if state.urgent.len() < QUEUE_MAX && !state.urgent.contains(&target) {
                    state.urgent.push_front(target);
                }
            }
            for target in art {
                if state.urgent.len() < QUEUE_MAX && !state.urgent.contains(&target) {
                    state.urgent.push_back(target);
                }
            }
        }
        inner.wake.notify_one();
    }

    fn set_key(&self, key: Option<Secret>) {
        *self
            .inner
            .key
            .write()
            .unwrap_or_else(PoisonError::into_inner) = key;
        {
            let mut state = self.inner.state();
            state.rejected = false;
            state.last_error = None;
        }
        self.inner.wake.notify_one();
    }

    fn status(&self) -> BoxFuture<'_, EnrichStatus> {
        Box::pin(async move {
            let counts = self.inner.db.read(store::counts).await.unwrap_or_default();
            EnrichStatus {
                configured: self.inner.key().is_some(),
                artists: counts.artists,
                songs: counts.songs,
                last_fetch: counts.last_fetch,
                last_error: self.inner.state().last_error.clone(),
            }
        })
    }

    fn test(&self, key: Option<Secret>) -> BoxFuture<'_, EnrichTest> {
        Box::pin(async move {
            let Some(key) = key.or_else(|| self.inner.key()) else {
                return EnrichTest {
                    ok: false,
                    elapsed: Duration::ZERO,
                    detail: Some(String::from("No API key is set.")),
                };
            };
            let started = Instant::now();
            let result = self
                .inner
                .call(&key, Method::ArtistInfo, &[("artist", TEST_ARTIST)])
                .await;
            EnrichTest {
                ok: result.is_ok(),
                elapsed: started.elapsed(),
                detail: result.err().map(|e| e.to_string()),
            }
        })
    }
}
