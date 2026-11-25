//! The worker against a fake Last.fm on an ephemeral port, on a real SQLite database.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use tokio_util::sync::CancellationToken;
use trackside_core::ExternalArtId;
use trackside_core::config::Secret;
use trackside_core::enrich::{EnrichControl, ExternalArtStore, Target};
use trackside_core::repo::enrichment as store;
use trackside_core::testing::{TestDb, scanned};
use trackside_core::{Clock, Millis, Role};
use trackside_enrich::{EnrichOptions, Enricher};

#[derive(Clone, Debug)]
struct Call {
    params: HashMap<String, String>,
    user_agent: String,
    at: Instant,
}

impl Call {
    fn method(&self) -> &str {
        self.params.get("method").map_or("", String::as_str)
    }

    fn artist(&self) -> &str {
        self.params.get("artist").map_or("", String::as_str)
    }
}

type Reply = dyn Fn(&Call, usize) -> (u16, String) + Send + Sync;

struct Fake {
    calls: Mutex<Vec<Call>>,
    count: tokio::sync::watch::Sender<usize>,
    reply: Box<Reply>,
    /// Where the answers say images are: this server unless a test points them elsewhere.
    image_base: Mutex<String>,
    /// Image paths asked for.
    images: Mutex<Vec<String>>,
    /// What an image request gets: status, content type and bytes.
    image: Mutex<(u16, &'static str, Vec<u8>)>,
}

/// The start of a PNG: enough for the fake store, which checks the signature as the real one
/// checks the whole image.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n fake image";

async fn image(
    State(fake): State<Arc<Fake>>,
    uri: axum::http::Uri,
) -> (
    axum::http::StatusCode,
    [(&'static str, &'static str); 1],
    Vec<u8>,
) {
    fake.images
        .lock()
        .expect("test setup")
        .push(uri.path().to_owned());
    let (status, kind, bytes) = fake.image.lock().expect("test setup").clone();
    (
        axum::http::StatusCode::from_u16(status).expect("test setup"),
        [("content-type", kind)],
        bytes,
    )
}

/// Keeps images in memory, refusing anything that does not start like a PNG.
#[derive(Debug, Default)]
struct FakeStore {
    kept: Mutex<HashMap<ExternalArtId, Vec<u8>>>,
}

impl ExternalArtStore for FakeStore {
    fn has(&self, id: &ExternalArtId) -> bool {
        self.kept.lock().expect("test setup").contains_key(id)
    }
    fn put(&self, id: &ExternalArtId, bytes: &[u8]) -> Result<(), String> {
        if !bytes.starts_with(b"\x89PNG") {
            return Err(String::from("not an image"));
        }
        self.kept
            .lock()
            .expect("test setup")
            .insert(id.clone(), bytes.to_vec());
        Ok(())
    }
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .expect("fixture")
}

fn answer(call: &Call) -> (u16, String) {
    let name = match call.method() {
        "track.getInfo" if call.artist() == "Blur" => "track.getInfo-placeholder.json",
        "track.getInfo" => "track.getInfo.json",
        "album.getInfo" => "album.getInfo-placeholder.json",
        "artist.getSimilar" => "artist.getSimilar.json",
        "artist.getTopTracks" => "artist.getTopTracks.json",
        "artist.getInfo" => "artist.getInfo.json",
        "track.getSimilar" => "track.getSimilar.json",
        _ => "error-invalid-key.json",
    };
    (200, fixture(name))
}

async fn handle(
    State(fake): State<Arc<Fake>>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> (axum::http::StatusCode, String) {
    let call = Call {
        params,
        user_agent: headers
            .get("user-agent")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned(),
        at: Instant::now(),
    };
    let index = {
        let mut calls = fake.calls.lock().expect("test setup");
        calls.push(call.clone());
        calls.len() - 1
    };
    let (status, body) = (fake.reply)(&call, index);
    let body = body.replace(
        "IMAGE_BASE",
        &fake.image_base.lock().expect("test setup").clone(),
    );
    fake.count.send_replace(index + 1);
    (
        axum::http::StatusCode::from_u16(status).expect("test setup"),
        body,
    )
}

struct Setup {
    test: TestDb,
    fake: Arc<Fake>,
    enricher: Arc<Enricher>,
    options: EnrichOptions,
    shutdown: CancellationToken,
}

impl Setup {
    fn calls(&self) -> Vec<Call> {
        self.fake.calls.lock().expect("test setup").clone()
    }

    async fn wait_for_calls(&self, n: usize) {
        let mut rx = self.fake.count.subscribe();
        tokio::time::timeout(Duration::from_secs(10), rx.wait_for(|c| *c >= n))
            .await
            .unwrap_or_else(|_| panic!("expected {n} calls, got {:?}", self.calls().len()))
            .expect("test setup");
    }

    fn restart(&mut self) {
        self.shutdown.cancel();
        self.shutdown = CancellationToken::new();
        self.enricher = Enricher::start(
            self.test.db.clone(),
            Some(Secret::new("key-1")),
            self.options.clone(),
            self.shutdown.clone(),
        )
        .expect("test setup");
    }
}

fn options(port: u16) -> EnrichOptions {
    EnrichOptions {
        endpoint: format!("http://127.0.0.1:{port}/2.0/"),
        image_hosts: vec![format!("http://127.0.0.1:{port}")],
        min_interval: Duration::from_millis(30),
        backoff_initial: Duration::from_millis(120),
        backoff_max: Duration::from_secs(1),
        request_timeout: Duration::from_secs(5),
        ..EnrichOptions::default()
    }
}

/// A library with Radiohead (played twice) and Zed (never played), and the fake Last.fm.
async fn setup(key: Option<&str>, reply: Box<Reply>) -> Setup {
    let test = TestDb::new().expect("test setup");
    let library = test
        .add_library("Music", Path::new("/music"))
        .expect("test setup");
    test.ingest(
        library.key,
        &[
            scanned("z.flac", "Alphabet", "Zed", Some("Letters")),
            scanned("r.flac", "Creep", "Radiohead", Some("Pablo Honey")),
        ],
    )
    .expect("test setup");
    let user = test.add_user("luke", Role::Admin, &[]).expect("test setup");
    test.db
        .write_blocking(move |tx| {
            tx.execute(
                "INSERT INTO play_stats (user_id, song_pid, play_count, last_played_at)
                 SELECT ?1, pid, 2, 0 FROM songs WHERE title = 'Creep'",
                [user.key],
            )?;
            Ok(())
        })
        .expect("test setup");
    let fake = Arc::new(Fake {
        calls: Mutex::new(Vec::new()),
        count: tokio::sync::watch::channel(0).0,
        reply,
        image_base: Mutex::new(String::new()),
        images: Mutex::new(Vec::new()),
        image: Mutex::new((200, "image/png", PNG.to_vec())),
    });
    let app = axum::Router::new()
        .route("/2.0/", axum::routing::get(handle))
        .route("/i/u/{size}/{name}", axum::routing::get(image))
        .with_state(Arc::clone(&fake));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test setup");
    let port = listener.local_addr().expect("test setup").port();
    *fake.image_base.lock().expect("test setup") = format!("http://127.0.0.1:{port}");
    tokio::spawn(async move { axum::serve(listener, app).await });
    let shutdown = CancellationToken::new();
    let options = options(port);
    let enricher = Enricher::start(
        test.db.clone(),
        key.map(Secret::new),
        options.clone(),
        shutdown.clone(),
    )
    .expect("test setup");
    Setup {
        test,
        fake,
        enricher,
        options,
        shutdown,
    }
}

fn normal() -> Box<Reply> {
    Box::new(|call, _| answer(call))
}

fn fetched(setup: &Setup, kind: store::Kind, key: &str) -> Option<bool> {
    let key = key.to_owned();
    setup
        .test
        .db
        .read_blocking(move |c| {
            Ok(c.query_row(
                "SELECT found FROM enrichment_fetches WHERE kind = ?1 AND key = ?2",
                [kind.as_str(), key.as_str()],
                |row| row.get::<_, bool>(0),
            )
            .ok())
        })
        .expect("test setup")
}

#[tokio::test]
async fn played_artists_go_first_and_fill_the_similar_tables() {
    let setup = setup(Some("key-1"), normal()).await;
    // Radiohead (3 calls), Creep (1), Zed (3).
    setup.wait_for_calls(7).await;
    let calls = setup.calls();
    assert_eq!(calls[0].artist(), "Radiohead");
    assert_eq!(calls[0].method(), "artist.getSimilar");
    assert_eq!(calls[3].method(), "track.getSimilar");
    assert_eq!(calls[4].artist(), "Zed");
    let (similar, top, bio) = setup
        .test
        .db
        .read_blocking(|c| {
            Ok((
                c.query_row(
                    "SELECT count(*) FROM similar_artists WHERE artist_identity = 'radiohead' AND source = 'lastfm'",
                    [],
                    |r| r.get::<_, i64>(0),
                )?,
                c.query_row(
                    "SELECT title FROM enrichment_top_songs WHERE artist_identity = 'radiohead' AND rank = 0",
                    [],
                    |r| r.get::<_, String>(0),
                )?,
                store::bio(c, "radiohead")?,
            ))
        })
        .expect("test setup");
    assert_eq!(similar, 3);
    assert_eq!(top, "Creep");
    assert!(bio.unwrap_or_default().starts_with("Radiohead are"));
    let creep = tracks_model_key("Creep", "Radiohead");
    assert_eq!(fetched(&setup, store::Kind::Song, &creep), Some(true));
}

fn tracks_model_key(title: &str, artist: &str) -> String {
    match Target::song(title, artist) {
        Target::Song { key, .. } => key,
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn calls_are_paced_and_send_only_names_and_the_key() {
    let setup = setup(Some("key-1"), normal()).await;
    setup.wait_for_calls(7).await;
    let calls = setup.calls();
    for pair in calls.windows(2) {
        let gap = pair[1].at - pair[0].at;
        assert!(gap >= Duration::from_millis(25), "calls {gap:?} apart");
    }
    let allowed: BTreeSet<&str> = [
        "method",
        "artist",
        "track",
        "limit",
        "autocorrect",
        "api_key",
        "format",
    ]
    .into();
    for call in &calls {
        for name in call.params.keys() {
            assert!(allowed.contains(name.as_str()), "sent {name}");
        }
        assert_eq!(
            call.params.get("api_key").map(String::as_str),
            Some("key-1")
        );
        assert!(
            call.user_agent.starts_with("Trackside/"),
            "{}",
            call.user_agent
        );
        assert!(!call.method().starts_with("auth."));
    }
}

#[tokio::test]
async fn a_rate_limit_backs_off_and_then_retries() {
    let setup = setup(
        Some("key-1"),
        Box::new(|call, index| {
            if index == 0 {
                (200, fixture("error-rate-limit.json"))
            } else if index == 1 {
                (503, String::new())
            } else {
                answer(call)
            }
        }),
    )
    .await;
    setup.wait_for_calls(2).await;
    let status = setup.enricher.status().await;
    assert!(status.last_error.is_some(), "{status:?}");
    setup.wait_for_calls(5).await;
    let calls = setup.calls();
    let first = calls[1].at - calls[0].at;
    let second = calls[2].at - calls[1].at;
    assert!(
        first >= Duration::from_millis(110),
        "retried after {first:?}"
    );
    assert!(
        second >= Duration::from_millis(230),
        "backoff did not grow: {second:?}"
    );
    assert_eq!(calls[2].artist(), "Radiohead");
    assert_eq!(calls[2].method(), "artist.getSimilar");
    setup.wait_for_calls(9).await;
    assert_eq!(setup.enricher.status().await.last_error, None);
}

#[tokio::test]
async fn answers_are_kept_until_they_expire_and_across_restarts() {
    let mut setup = setup(Some("key-1"), normal()).await;
    setup.wait_for_calls(7).await;
    setup
        .enricher
        .ensure(vec![Target::artist("Zed")], Duration::from_secs(5))
        .await;
    assert_eq!(fetched(&setup, store::Kind::Artist, "zed"), Some(true));
    setup.restart();
    let started = Instant::now();
    setup
        .enricher
        .ensure(vec![Target::artist("Radiohead")], Duration::from_secs(5))
        .await;
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(setup.calls().len(), 7);
    let now = Clock::now(&*setup.test.clock);
    let due = setup
        .test
        .db
        .read_blocking(move |c| store::due_artists(c, now, 10))
        .expect("test setup");
    assert!(due.is_empty(), "{due:?}");

    setup
        .test
        .clock
        .advance(setup.options.ttl + Duration::from_secs(1));
    setup
        .enricher
        .ensure(vec![Target::artist("Radiohead")], Duration::from_secs(5))
        .await;
    let calls = setup.calls();
    assert!(calls.len() >= 10, "{}", calls.len());
    assert_eq!(calls[7].artist(), "Radiohead");
}

#[tokio::test]
async fn unknown_artists_are_not_asked_again_until_the_negative_ttl() {
    let setup = setup(
        Some("key-1"),
        Box::new(|call, _| {
            if call.artist() == "Zed" {
                (400, fixture("error-not-found.json"))
            } else {
                answer(call)
            }
        }),
    )
    .await;
    // Radiohead (3), Creep (1), Zed (1: unknown after the first call).
    setup.wait_for_calls(5).await;
    let zed = || Target::artist("Zed");
    setup
        .enricher
        .ensure(vec![zed()], Duration::from_secs(5))
        .await;
    assert_eq!(fetched(&setup, store::Kind::Artist, "zed"), Some(false));
    assert_eq!(setup.calls().len(), 5);
    assert_eq!(setup.calls()[4].artist(), "Zed");

    setup
        .test
        .clock
        .advance(setup.options.unknown_ttl + Duration::from_secs(1));
    setup
        .enricher
        .ensure(vec![zed()], Duration::from_secs(5))
        .await;
    assert!(setup.calls().iter().filter(|c| c.artist() == "Zed").count() >= 2);
}

#[tokio::test]
async fn a_rejected_key_stops_everything_until_a_new_one() {
    let setup = setup(
        Some("bad"),
        Box::new(|call, _| {
            if call.params.get("api_key").map(String::as_str) == Some("bad") {
                (403, fixture("error-invalid-key.json"))
            } else {
                answer(call)
            }
        }),
    )
    .await;
    setup.wait_for_calls(1).await;
    let started = Instant::now();
    setup
        .enricher
        .ensure(vec![Target::artist("Radiohead")], Duration::from_secs(5))
        .await;
    assert!(started.elapsed() < Duration::from_secs(1));
    let status = setup.enricher.status().await;
    assert!(status.configured);
    assert!(
        status
            .last_error
            .is_some_and(|(_, m)| m.contains("Invalid API key"))
    );
    assert_eq!(setup.calls().len(), 1);

    setup.enricher.set_key(Some(Secret::new("good")));
    setup.wait_for_calls(4).await;
    assert_eq!(
        setup.calls()[1].params.get("api_key").map(String::as_str),
        Some("good")
    );
}

#[tokio::test]
async fn without_a_key_nothing_is_called() {
    let setup = setup(None, normal()).await;
    let started = Instant::now();
    setup
        .enricher
        .ensure(vec![Target::artist("Radiohead")], Duration::from_secs(5))
        .await;
    assert!(started.elapsed() < Duration::from_secs(1));
    let status = setup.enricher.status().await;
    assert!(!status.configured);
    assert_eq!(status.artists, 0);
    let test = setup.enricher.test(None).await;
    assert!(!test.ok);
    assert!(setup.calls().is_empty());
}

#[tokio::test]
async fn the_test_call_reports_ok_or_the_error() {
    let setup = setup(
        None,
        Box::new(|call, _| {
            if call.params.get("api_key").map(String::as_str) == Some("bad") {
                (403, fixture("error-invalid-key.json"))
            } else {
                answer(call)
            }
        }),
    )
    .await;
    let ok = setup.enricher.test(Some(Secret::new("good"))).await;
    assert!(ok.ok, "{ok:?}");
    let bad = setup.enricher.test(Some(Secret::new("bad"))).await;
    assert!(!bad.ok);
    assert!(!format!("{bad:?}").contains("api_key=bad"));
    assert!(bad.detail.unwrap_or_default().contains("Invalid API key"));
    let calls = setup.calls();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|c| c.method() == "artist.getInfo"));
}

#[tokio::test]
async fn status_counts_what_last_fm_knew() {
    let setup = setup(Some("key-1"), normal()).await;
    setup.wait_for_calls(7).await;
    setup
        .enricher
        .ensure(vec![Target::artist("Zed")], Duration::from_secs(5))
        .await;
    let status = setup.enricher.status().await;
    assert_eq!((status.artists, status.songs), (2, 1));
    assert!(status.last_fetch.is_some_and(|at| at >= Millis(0)));
}

/// What the worker stored about a suggested song's cover.
#[derive(Debug, PartialEq, Eq)]
enum Stored {
    Nothing,
    NoCover,
    Cover(String),
}

fn stored_art(setup: &Setup, title: &str, artist: &str) -> Stored {
    let Target::Art { key, .. } = Target::art(title, artist) else {
        unreachable!()
    };
    let found = setup
        .test
        .db
        .read_blocking(move |c| {
            Ok(c.query_row(
                "SELECT art_id FROM external_art WHERE song_key = ?1",
                [key],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok())
        })
        .expect("test setup");
    match found {
        None => Stored::Nothing,
        Some(None) => Stored::NoCover,
        Some(Some(id)) => Stored::Cover(id),
    }
}

async fn art_setup() -> (Setup, Arc<FakeStore>) {
    let setup = setup(Some("key-1"), normal()).await;
    // Let the background pass (Radiohead, Creep, Zed) finish first.
    setup.wait_for_calls(7).await;
    let store = Arc::new(FakeStore::default());
    setup.enricher.set_art_store(store.clone());
    (setup, store)
}

#[tokio::test]
async fn a_suggested_songs_cover_is_fetched_once_and_kept() {
    let (setup, store) = art_setup().await;
    let started = Instant::now();
    setup
        .enricher
        .queue(vec![Target::art("Glory Box", "Portishead")]);
    assert!(
        started.elapsed() < Duration::from_millis(50),
        "queue never waits"
    );
    setup
        .enricher
        .ensure(
            vec![Target::art("Glory Box", "Portishead")],
            Duration::from_secs(5),
        )
        .await;
    let id = ExternalArtId::derive("4128a6eb29f94943c9d206c08e625904");
    assert_eq!(
        stored_art(&setup, "Glory Box", "Portishead"),
        Stored::Cover(id.to_string())
    );
    assert!(store.has(&id));
    let images = setup.fake.images.lock().expect("test setup").clone();
    assert_eq!(
        images,
        ["/i/u/300x300/4128a6eb29f94943c9d206c08e625904.png"],
        "the largest"
    );
    let info = setup
        .calls()
        .into_iter()
        .filter(|c| c.method() == "track.getInfo")
        .count();
    assert_eq!(info, 1);
    // The same album's cover for another song is not downloaded again.
    setup
        .enricher
        .ensure(
            vec![Target::art("Roads", "Portishead")],
            Duration::from_secs(5),
        )
        .await;
    assert_eq!(
        stored_art(&setup, "Roads", "Portishead"),
        Stored::Cover(id.to_string())
    );
    assert_eq!(setup.fake.images.lock().expect("test setup").len(), 1);
}

#[tokio::test]
async fn last_fms_placeholder_star_is_no_cover() {
    let (setup, _store) = art_setup().await;
    setup
        .enricher
        .ensure(vec![Target::art("Song 2", "Blur")], Duration::from_secs(5))
        .await;
    assert_eq!(stored_art(&setup, "Song 2", "Blur"), Stored::NoCover);
    let methods: Vec<String> = setup
        .calls()
        .into_iter()
        .skip(7)
        .map(|c| c.method().to_owned())
        .collect();
    assert_eq!(methods, ["track.getInfo", "album.getInfo"]);
    assert!(setup.fake.images.lock().expect("test setup").is_empty());
}

#[tokio::test]
async fn images_on_other_hosts_are_never_fetched() {
    let (setup, store) = art_setup().await;
    for base in [
        "https://evil.example",
        "http://169.254.169.254",
        "https://lastfm.freetls.fastly.net.evil.example",
    ] {
        *setup.fake.image_base.lock().expect("test setup") = base.to_owned();
        let title = format!("Glory Box {base}");
        setup
            .enricher
            .ensure(
                vec![Target::art(&title, "Portishead")],
                Duration::from_secs(5),
            )
            .await;
        assert_eq!(
            stored_art(&setup, &title, "Portishead"),
            Stored::NoCover,
            "{base}"
        );
    }
    assert!(setup.fake.images.lock().expect("test setup").is_empty());
    assert!(store.kept.lock().expect("test setup").is_empty());
}

#[tokio::test]
async fn bytes_that_are_not_an_image_are_refused() {
    let (setup, store) = art_setup().await;
    *setup.fake.image.lock().expect("test setup") =
        (200, "image/png", b"<html>not a picture</html>".to_vec());
    setup
        .enricher
        .ensure(
            vec![Target::art("Glory Box", "Portishead")],
            Duration::from_secs(5),
        )
        .await;
    assert_eq!(
        stored_art(&setup, "Glory Box", "Portishead"),
        Stored::NoCover
    );
    *setup.fake.image.lock().expect("test setup") = (200, "text/html", PNG.to_vec());
    setup
        .enricher
        .ensure(
            vec![Target::art("Teardrop", "Portishead")],
            Duration::from_secs(5),
        )
        .await;
    assert_eq!(
        stored_art(&setup, "Teardrop", "Portishead"),
        Stored::NoCover
    );
    *setup.fake.image.lock().expect("test setup") =
        (200, "image/png", [PNG, &vec![0; 3 * 1024 * 1024]].concat());
    setup
        .enricher
        .ensure(
            vec![Target::art("Numb", "Portishead")],
            Duration::from_secs(5),
        )
        .await;
    assert_eq!(stored_art(&setup, "Numb", "Portishead"), Stored::NoCover);
    assert!(store.kept.lock().expect("test setup").is_empty());
}
