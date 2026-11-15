//! A real `/rest` router on an ephemeral port, a small HTTP/1.1 client that sends requests
//! byte for byte as written, and a seeded library.
//!
//! Seeding goes through the real repositories, ingest and auth.

#![allow(dead_code, reason = "each test binary uses a different part")]

use std::error::Error;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use trackside_auth::{Auth, KeyKind, Scopes, SecretKey};
use trackside_core::access::{Permissions, Role};
use trackside_core::control::{ScanTrigger, SongOrigin};
use trackside_core::ingest::{
    self, ScannedFile, ScannedLyrics, ScannedProperties, ScannedTags, SeenSet,
};
use trackside_core::records::{
    Codec, Container, LyricsKind, LyricsSource, ScanCounts, ScanRunKind, ScanState,
};
use trackside_core::repo;
use trackside_core::repo::folders::FolderUpsert;
use trackside_core::repo::libraries::NewLibrary;
use trackside_core::repo::users::NewUser;
use trackside_core::testing::{FakeScanControl, TestDb};
use trackside_core::{
    Access, AlbumId, ArtistId, AudioDigest, Config, LibraryId, LibraryKey, PublicId, SongId,
    UserKey,
};
use trackside_media::Media;
use trackside_media::testing::FakeTranscoder;
use trackside_subsonic::{SubsonicState, router};

pub type TestResult = Result<(), Box<dyn Error>>;

/// An HTTP response as read off the socket.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }

    /// `subsonic-response` of a JSON answer.
    pub fn envelope(&self) -> Result<serde_json::Value, Box<dyn Error>> {
        let json = self.json()?;
        Ok(json
            .get("subsonic-response")
            .cloned()
            .ok_or("no subsonic-response")?)
    }

    /// The Subsonic error code, from JSON or XML.
    pub fn error_code(&self) -> Option<i64> {
        if let Ok(env) = self.envelope() {
            return env["error"]["code"].as_i64();
        }
        let text = self.text();
        let at = text.find("<error code=\"")? + 13;
        text[at..].split('"').next()?.parse().ok()
    }
}

fn decode_chunked(mut body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(line_end) = body.windows(2).position(|w| w == b"\r\n") else {
            return out;
        };
        let size_text = String::from_utf8_lossy(&body[..line_end]);
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("0").trim(), 16)
            .unwrap_or(0);
        body = &body[line_end + 2..];
        if size == 0 || body.len() < size {
            return out;
        }
        out.extend_from_slice(&body[..size]);
        body = body.get(size + 2..).unwrap_or_default();
    }
}

/// Sends one request on a new connection and reads the whole response.
pub async fn send(
    addr: SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<Reply, Box<dyn Error>> {
    let mut stream = TcpStream::connect(addr).await?;
    let mut request =
        format!("{method} {target} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (name, value) in headers {
        let _ = write!(request, "{name}: {value}\r\n");
    }
    if !body.is_empty() || method == "POST" {
        let _ = write!(request, "Content-Length: {}\r\n", body.len());
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await?;
    stream.write_all(body).await?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("no header end")?;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .ok_or("no status line")?
        .parse()?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_owned(), v.trim().to_owned()))
        .collect();
    let mut body = raw[split + 4..].to_vec();
    let chunked = headers.iter().any(|(n, v)| {
        n.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked")
    });
    if chunked {
        body = decode_chunked(&body);
    }
    Ok(Reply {
        status,
        headers,
        body,
    })
}

/// The router with a real database, auth and media, and a fake scanner.
pub struct Server {
    pub addr: SocketAddr,
    pub test: TestDb,
    pub auth: Arc<Auth>,
    pub scan: Arc<FakeScanControl>,
    pub media: Arc<Media>,
    pub transcoder: Arc<FakeTranscoder>,
    pub dir: tempfile::TempDir,
}

impl Server {
    /// A server on a host without ffmpeg.
    pub async fn start() -> Result<Self, Box<dyn Error>> {
        Self::start_with(FakeTranscoder::missing()).await
    }

    pub async fn start_with(transcoder: FakeTranscoder) -> Result<Self, Box<dyn Error>> {
        let transcoder = Arc::new(transcoder);
        let dir = tempfile::tempdir()?;
        let test = TestDb::new()?;
        let config = Config::defaults(dir.path());
        let auth = Arc::new(Auth::new(
            test.db.clone(),
            &config.auth,
            SecretKey::from_bytes([7; 32]),
            Vec::new(),
            test.clock.clone(),
        ));
        let media = Arc::new(Media::new(
            &config.media,
            &dir.path().join("cache"),
            test.db.clone(),
            transcoder.clone(),
            test.clock.clone(),
        )?);
        let scan = Arc::new(FakeScanControl::new());
        let state = SubsonicState {
            db: test.db.clone(),
            auth: Arc::clone(&auth),
            media: Arc::clone(&media),
            scan: scan.clone(),
            enrich: Arc::new(trackside_core::NoEnrich),
            config: Arc::new(config),
            server_id: uuid::Uuid::nil(),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self {
            addr,
            test,
            auth,
            scan,
            media,
            transcoder,
            dir,
        })
    }

    pub async fn get(&self, target: &str) -> Result<Reply, Box<dyn Error>> {
        send(self.addr, "GET", target, &[], &[]).await
    }

    pub async fn post_form(&self, path: &str, form: &str) -> Result<Reply, Box<dyn Error>> {
        send(
            self.addr,
            "POST",
            path,
            &[("Content-Type", "application/x-www-form-urlencoded")],
            form.as_bytes(),
        )
        .await
    }
}

/// Percent-encodes a query value as Tracks does: `URLComponents`, then `+` as `%2B`.
pub fn encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// A query string from pairs, encoded.
pub fn query(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(n, v)| format!("{n}={}", encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// One seeded song.
#[derive(Clone, Debug)]
pub struct Seeded {
    pub id: SongId,
    pub album: Option<AlbumId>,
    pub artist: Option<ArtistId>,
    pub path: PathBuf,
    pub bytes: Vec<u8>,
}

/// A library of two libraries: A, granted to the member, and B, visible to the admin only.
pub struct Fixture {
    pub server: Server,
    pub admin: UserKey,
    pub member: UserKey,
    pub admin_key: String,
    pub member_key: String,
    pub member_name: String,
    pub member_password: String,
    pub library_a: LibraryId,
    pub library_b: LibraryId,
    pub halo: Seeded,
    pub boy: Seeded,
    pub dog_days: Seeded,
    pub secret: Seeded,
}

#[derive(Clone, Copy)]
struct SongSpec {
    rel: &'static str,
    title: &'static str,
    artist: &'static str,
    album: &'static str,
    track: u32,
    container: Container,
    codec: Codec,
    genre: &'static str,
    seed: u8,
}

const SPECS_A: [SongSpec; 3] = [
    SongSpec {
        rel: "Beyonce/I Am/03 Halo.flac",
        title: "Halo",
        artist: "Beyoncé",
        album: "I Am... Sasha Fierce",
        track: 3,
        container: Container::Flac,
        codec: Codec::Flac,
        genre: "R&B",
        seed: 1,
    },
    SongSpec {
        rel: "Beyonce/I Am/01 If I Were a Boy.flac",
        title: "If I Were a Boy",
        artist: "Beyoncé",
        album: "I Am... Sasha Fierce",
        track: 1,
        container: Container::Flac,
        codec: Codec::Flac,
        genre: "R&B",
        seed: 2,
    },
    SongSpec {
        rel: "Florence/Lungs/04 Dog Days Are Over.mp3",
        title: "Dog Days Are Over",
        artist: "Florence + the Machine",
        album: "Lungs",
        track: 4,
        container: Container::Mp3,
        codec: Codec::Mp3,
        genre: "Indie",
        seed: 3,
    },
];

const SPEC_B: SongSpec = SongSpec {
    rel: "Hidden/Secret Song.flac",
    title: "Secret Song",
    artist: "Hidden Artist",
    album: "Hidden Album",
    track: 1,
    container: Container::Flac,
    codec: Codec::Flac,
    genre: "Ambient",
    seed: 9,
};

fn mtime_ns(path: &Path) -> Result<i64, Box<dyn Error>> {
    let modified = std::fs::metadata(path)?.modified()?;
    Ok(modified
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos()
        .try_into()?)
}

fn write_file(root: &Path, spec: &SongSpec) -> Result<(PathBuf, Vec<u8>), Box<dyn Error>> {
    let path = root.join(spec.rel);
    std::fs::create_dir_all(path.parent().ok_or("no parent")?)?;
    let bytes: Vec<u8> = (0..4096u32)
        .map(|i| (i as u8).wrapping_mul(spec.seed.wrapping_add(13)))
        .collect();
    std::fs::write(&path, &bytes)?;
    Ok((path, bytes))
}

async fn ingest_library(
    server: &Server,
    key: LibraryKey,
    root: &Path,
    specs: &[&SongSpec],
) -> Result<Vec<(SongId, PathBuf, Vec<u8>)>, Box<dyn Error>> {
    let mut files: Vec<(SongSpec, i64, u64)> = Vec::new();
    let mut written = Vec::new();
    for spec in specs {
        let (path, bytes) = write_file(root, spec)?;
        files.push((**spec, mtime_ns(&path)?, bytes.len() as u64));
        written.push((path, bytes));
    }
    let ids = server
        .test
        .db
        .write(move |tx| {
            let run = repo::scan_runs::start(tx, key, ScanRunKind::Full, ScanTrigger::Cli)?;
            let mut scanned = Vec::new();
            let mut seen = SeenSet::new();
            for (spec, mtime, size) in &files {
                let parent = Path::new(spec.rel)
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let folder = repo::folders::upsert(
                    tx,
                    key,
                    &FolderUpsert {
                        rel_path: parent.as_bytes().to_vec(),
                        name: parent.rsplit('/').next().unwrap_or("").to_owned(),
                        mtime_ns: None,
                        listing_hash: None,
                    },
                )?;
                seen.insert(spec.rel.as_bytes().to_vec());
                scanned.push(ScannedFile {
                    rel_path: spec.rel.as_bytes().to_vec(),
                    rel_path_display: spec.rel.to_owned(),
                    folder,
                    size: *size,
                    mtime_ns: *mtime,
                    dev: None,
                    ino: None,
                    digest: AudioDigest([spec.seed; 32]),
                    tags: ScannedTags {
                        title: spec.title.to_owned(),
                        artist: spec.artist.to_owned(),
                        album: Some(spec.album.to_owned()),
                        album_artist: Some(spec.artist.to_owned()),
                        track: Some(spec.track),
                        disc: Some(1),
                        year: Some(2008),
                        genres: vec![spec.genre.to_owned()],
                        ..ScannedTags::default()
                    },
                    properties: ScannedProperties {
                        duration_ms: 200_000 + u32::from(spec.seed) * 1000,
                        bitrate_kbps: Some(if spec.codec == Codec::Mp3 { 256 } else { 1000 }),
                        sample_rate: Some(44_100),
                        bit_depth: (spec.codec == Codec::Flac).then_some(16),
                        channels: Some(2),
                        codec: spec.codec,
                        container: spec.container,
                    },
                    artwork: None,
                    lyrics: if spec.seed == 1 {
                        vec![ScannedLyrics {
                            kind: LyricsKind::Synced,
                            lang: String::from("eng"),
                            source: LyricsSource::Sidecar,
                            content: String::from(
                                "[offset:+100]\n[00:01.00]First line\n[00:05.50]Second line",
                            ),
                        }]
                    } else {
                        Vec::new()
                    },
                    tag_problem: None,
                });
            }
            let resolutions = ingest::apply_batch(tx, key, &run.id, SongOrigin::Scan, &scanned)?;
            ingest::finish_run(tx, key, &run.id, &seen, &[])?;
            repo::scan_runs::finish(tx, &run.id, ScanState::Done, &ScanCounts::default(), None)?;
            Ok(resolutions
                .iter()
                .map(|r| r.song().clone())
                .collect::<Vec<_>>())
        })
        .await?;
    Ok(ids
        .into_iter()
        .zip(written)
        .map(|(id, (path, bytes))| (id, path, bytes))
        .collect())
}

impl Fixture {
    /// The seeded library, or `None` (with a message) while the packages it needs are stubs.
    pub async fn start() -> Result<Self, Box<dyn Error>> {
        Self::start_with(FakeTranscoder::missing()).await
    }

    /// The seeded library on a host with a (fake) ffmpeg.
    pub async fn start_with(transcoder: FakeTranscoder) -> Result<Self, Box<dyn Error>> {
        Self::seed(transcoder).await
    }

    #[expect(clippy::too_many_lines, reason = "one linear seeding script")]
    async fn seed(transcoder: FakeTranscoder) -> Result<Self, Box<dyn Error>> {
        let server = Server::start_with(transcoder).await?;
        let root_a = server.dir.path().join("a");
        let root_b = server.dir.path().join("b");
        std::fs::create_dir_all(&root_a)?;
        std::fs::create_dir_all(&root_b)?;
        let (ra, rb) = (root_a.clone(), root_b.clone());
        let (library_a, library_b, admin, member) = server
            .test
            .db
            .write(move |tx| {
                let a = repo::libraries::create(
                    tx,
                    &NewLibrary {
                        name: String::from("Music"),
                        root: ra,
                        writable: false,
                        watch: false,
                        grant_all_users: false,
                        granted_by: None,
                    },
                )?;
                let b = repo::libraries::create(
                    tx,
                    &NewLibrary {
                        name: String::from("Private"),
                        root: rb,
                        writable: false,
                        watch: false,
                        grant_all_users: false,
                        granted_by: None,
                    },
                )?;
                let hash = String::from("$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$aGFzaGhhc2g");
                let admin = repo::users::create(
                    tx,
                    &NewUser {
                        username: String::from("admin"),
                        display_name: String::from("Admin"),
                        password_hash: hash.clone(),
                        role: Role::Admin,
                        permissions: Permissions::default(),
                        libraries: Vec::new(),
                    },
                )?;
                let member = repo::users::create(
                    tx,
                    &NewUser {
                        username: String::from("luke+music@example.com"),
                        display_name: String::from("Luke"),
                        password_hash: hash,
                        role: Role::Member,
                        permissions: Permissions {
                            legacy_subsonic: true,
                            ..Permissions::default()
                        },
                        libraries: vec![a.key],
                    },
                )?;
                Ok((a, b, admin.key, member.key))
            })
            .await?;
        let seeded_a = ingest_library(
            &server,
            library_a.key,
            &root_a,
            &SPECS_A.iter().collect::<Vec<_>>(),
        )
        .await?;
        let seeded_b = ingest_library(&server, library_b.key, &root_b, &[&SPEC_B]).await?;
        let admin_key = server
            .auth
            .create_key(admin, KeyKind::ApiKey, "Tests", Scopes::ALL)
            .await?
            .secret;
        let member_key = server
            .auth
            .create_key(member, KeyKind::ApiKey, "Feishin", Scopes::ALL)
            .await?
            .secret;
        let app_password = server
            .auth
            .create_key(member, KeyKind::AppPassword, "Tracks", Scopes::ALL)
            .await?;
        let ids: Vec<SongId> = seeded_a
            .iter()
            .chain(seeded_b.iter())
            .map(|(id, _, _)| id.clone())
            .collect();
        let songs = server
            .test
            .db
            .read(move |c| repo::songs::get_many(c, &Access::system(), &ids))
            .await?;
        let seeded =
            |(id, path, bytes): &(SongId, PathBuf, Vec<u8>)| -> Result<Seeded, Box<dyn Error>> {
                let song = songs
                    .iter()
                    .find(|s| s.id == *id)
                    .ok_or("seeded song missing")?;
                Ok(Seeded {
                    id: id.clone(),
                    album: song.album_id.clone(),
                    artist: song.artist_id.clone(),
                    path: path.clone(),
                    bytes: bytes.clone(),
                })
            };
        Ok(Self {
            admin,
            member,
            admin_key,
            member_key,
            member_name: app_password.username.clone(),
            member_password: app_password.secret.clone(),
            library_a: library_a.id,
            library_b: library_b.id,
            halo: seeded(&seeded_a[0])?,
            boy: seeded(&seeded_a[1])?,
            dog_days: seeded(&seeded_a[2])?,
            secret: seeded(&seeded_b[0])?,
            server,
        })
    }

    /// Tracks' signing: `u`, `t = md5(password + salt)`, `s`, `v`, `c`, then the endpoint's
    /// parameters sorted by name.
    pub fn tracks_url(&self, endpoint: &str, params: &[(&str, &str)], json: bool) -> String {
        use md5::Digest as _;
        let salt = "k3m9x2q7w1z8";
        let token: String = md5::Md5::digest(format!("{}{salt}", self.member_password).as_bytes())
            .iter()
            .fold(String::new(), |mut hex, b| {
                let _ = write!(hex, "{b:02x}");
                hex
            });
        let mut pairs: Vec<(&str, &str)> = vec![
            ("u", &self.member_name),
            ("t", &token),
            ("s", salt),
            ("v", "1.16.1"),
            ("c", "Tracks"),
        ];
        if json {
            pairs.push(("f", "json"));
        }
        let mut sorted = params.to_vec();
        sorted.sort_by_key(|(k, _)| *k);
        pairs.extend(sorted);
        format!("/rest/{endpoint}?{}", query(&pairs))
    }

    /// An `apiKey` request as `OpenSubsonic` clients send it.
    pub fn key_url(&self, key: &str, endpoint: &str, params: &[(&str, &str)]) -> String {
        let _ = self;
        let mut pairs: Vec<(&str, &str)> = vec![
            ("apiKey", key),
            ("v", "1.16.1"),
            ("c", "Feishin"),
            ("f", "json"),
        ];
        pairs.extend_from_slice(params);
        format!("/rest/{endpoint}?{}", query(&pairs))
    }

    pub async fn get(&self, target: &str) -> Result<Reply, Box<dyn Error>> {
        self.server.get(target).await
    }

    /// The response of a JSON call that must succeed.
    pub async fn ok(&self, target: &str) -> Result<serde_json::Value, Box<dyn Error>> {
        let reply = self.get(target).await?;
        let env = reply.envelope()?;
        if env["status"] != "ok" {
            return Err(format!("{target} failed: {env}").into());
        }
        Ok(env)
    }

    pub async fn count(&self, sql: &'static str) -> Result<i64, Box<dyn Error>> {
        Ok(self
            .server
            .test
            .db
            .read(move |c| Ok(c.query_row(sql, [], |row| row.get::<_, i64>(0))?))
            .await?)
    }
}

pub fn id_of(value: &serde_json::Value) -> &str {
    value["id"].as_str().unwrap_or_default()
}

pub fn ids_in(list: &serde_json::Value) -> Vec<String> {
    list.as_array()
        .map(|items| items.iter().map(|i| id_of(i).to_owned()).collect())
        .unwrap_or_default()
}

/// The song fields `SubsonicClient.swift` decodes, typed as it decodes them.
pub fn assert_tracks_song(song: &serde_json::Value) {
    assert!(song["id"].is_string(), "id is a string: {song}");
    assert!(song["title"].is_string(), "title is a string: {song}");
    for key in [
        "artist",
        "album",
        "albumId",
        "artistId",
        "suffix",
        "contentType",
        "created",
    ] {
        assert!(song[key].is_string(), "{key} is a string: {song}");
    }
    for key in [
        "duration",
        "bitRate",
        "size",
        "track",
        "discNumber",
        "year",
        "samplingRate",
    ] {
        assert!(song[key].is_i64(), "{key} is an integer: {song}");
    }
    assert!(song.get("isExternal").is_none());
    let created = song["created"].as_str().unwrap_or_default();
    assert!(
        trackside_core::Millis::parse_rfc3339(created).is_some() && created.ends_with('Z'),
        "created is RFC 3339 with Z: {created}"
    );
    assert_eq!(created.len(), 24, "created has milliseconds: {created}");
}

pub fn pid<T: PublicId>(id: &T) -> &str {
    id.as_str()
}
