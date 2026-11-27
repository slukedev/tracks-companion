//! `fixtures_match_wire_types`: every response shape, built from seeded core records with the
//! handlers' own builders, compared to `tests/fixtures/**/*.json`. Tracks' tests decode the same
//! files, so a change here is an API change.
//!
//! `TRACKSIDE_UPDATE_FIXTURES=1 cargo test -p trackside-api --test fixtures` rewrites them.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use trackside_api::wire::auth::{ServerSummary, ServerUrl, UrlKind};
use trackside_api::wire::{self, Time};
use trackside_api::{ServerInfo, build, map};
use trackside_auth::{IssuedDeviceKey, MediaToken, Principal, Scopes, Via};
use trackside_core::control::{
    AcquireFeature, AcquisitionState, AcquisitionView, AdminAcquisitionView, AdminCandidate,
    CandidateFormat, CandidateView, ExternalSearch, InstantStreamUrls, LibraryScanStatus,
    ProviderHealthState, ProviderStats, ProviderStatus, Quality, ScanTrigger, WatchMode,
};
use trackside_core::records::{
    Album, AlbumMbz, Artist, AudioFormat, Codec, Container, Credit, CreditRole, Explicit, ItemRef,
    Library, Lyrics, LyricsKind, LyricsSource, ReplayGain, ScanCounts, ScanRun, ScanRunKind,
    ScanState, Song, SongFile, SongMbz, UserAnnotations,
};
use trackside_core::repo::devices::Device;
use trackside_core::repo::keys::{ApiKeyRow, KeyKind};
use trackside_core::repo::listens::{ListenOutcome, ListenStatus, RejectReason};
use trackside_core::repo::play_queue::PlayQueue;
use trackside_core::repo::playlists::Playlist;
use trackside_core::repo::ratings::Rating;
use trackside_core::repo::stars::Star;
use trackside_core::repo::users::User;
use trackside_core::sync::{
    DeltaDeletes, DeltaPage, DeltaUpserts, PlayStat, PlaylistWithEntries, SnapshotItems,
    SnapshotKind, SnapshotPage, SnapshotPosition, SyncCursor, SyncPage,
};
use trackside_core::testing::TEST_START;
use trackside_core::{
    Access, AlbumId, ArtistId, ArtworkId, AudioDigest, CandidateToken, DeviceKey, ExternalArtId,
    FolderId, KeyId, LibraryId, LibraryKey, LibrarySet, Millis, Permissions, PlaylistId, PublicId,
    Role, Seq, SongId, UserKey,
};
use trackside_media::{
    CachedTranscode, Decision, DecisionReason, HlsJobId, MediaCapabilities, Profile,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn sha2_of(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).into()
}

const SERVER_ID: &str = "5f0c2b9e-8a3d-4c1e-9b7a-2d6e4f8a1c3b";

fn at(offset_s: i64) -> Millis {
    Millis(TEST_START.0 + offset_s * 1000)
}

fn id<T: PublicId>(seed: &str) -> T {
    T::from_hash(&spread(seed))
}

/// Stable ids from a name: the name's bytes spread over 32.
fn spread(seed: &str) -> [u8; 32] {
    let mut out = [0_u8; 32];
    for (i, b) in seed.bytes().enumerate() {
        out[i % 32] = out[i % 32].wrapping_mul(31).wrapping_add(b);
        out[(i * 7 + 3) % 32] ^= b.rotate_left(3);
    }
    out
}

struct Seed {
    library: Library,
    acquired: Library,
    beyonce: Artist,
    queen: Artist,
    album: Album,
    halo: Song,
    ladies: Song,
    dont_stop: Song,
    user: User,
    device: Device,
}

fn library(name: &str, key: i64, root: &str, songs: u64) -> Library {
    Library {
        id: id(&format!("library {name}")),
        key: LibraryKey(key),
        name: name.to_owned(),
        root: PathBuf::from(root),
        enabled: true,
        writable: name == "Acquired",
        watch: true,
        available: true,
        song_count: songs,
        quick_scan_interval: None,
        unavailable_reason: None,
        unavailable_since: None,
        last_scan_at: Some(at(-3600)),
        created_at: at(-86_400 * 60),
    }
}

fn flac(size: u64, bitrate: u32) -> AudioFormat {
    AudioFormat {
        container: Container::Flac,
        codec: Codec::Flac,
        content_type: Container::Flac.content_type(),
        lossless: true,
        bitrate_kbps: Some(bitrate),
        sample_rate: Some(44_100),
        bit_depth: Some(16),
        channels: Some(2),
        size,
    }
}

#[expect(clippy::too_many_arguments, reason = "a test record builder")]
fn song(
    library: &Library,
    title: &str,
    artist: &Artist,
    album: Option<&Album>,
    track: u32,
    duration_ms: u32,
    format: AudioFormat,
    digest: u8,
) -> Song {
    Song {
        id: SongId::derive(&AudioDigest([digest; 32])),
        library: library.id.clone(),
        title: title.to_owned(),
        artist: artist.name.clone(),
        artist_id: Some(artist.id.clone()),
        credits: vec![Credit {
            artist: artist.id.clone(),
            name: artist.name.clone(),
            role: CreditRole::Main,
        }],
        album: album.map(|a| a.name.clone()),
        album_id: album.map(|a| a.id.clone()),
        album_artist: album.map(|a| a.album_artist.clone()),
        track_number: Some(track),
        disc_number: Some(1),
        disc_subtitle: None,
        year: album.and_then(|a| a.year),
        release_date: album.and_then(|a| a.release_date.clone()),
        genres: vec![String::from("Pop")],
        duration_ms,
        format,
        artwork: album.and_then(|a| a.artwork.clone()),
        song_key: tracks_model::song_key(title, &artist.name),
        replay_gain: None,
        mbz: SongMbz::default(),
        isrc: None,
        bpm: None,
        comment: None,
        explicit: None,
        has_lyrics: false,
        folder: FolderId::derive(&library.id, artist.name.as_bytes()),
        added_at: at(-86_400 * 30),
        updated_at: at(-86_400 * 30),
        missing: false,
        content_version: 1,
    }
}

#[expect(clippy::too_many_lines, reason = "one seeded library")]
fn seed() -> Seed {
    let music = library("Music", 1, "/music", 48_211);
    let acquired = library("Acquired", 3, "/acquired", 312);
    let artist = |name: &str, albums, songs| Artist {
        id: ArtistId::derive(&tracks_model::folded(name)),
        name: name.to_owned(),
        sort_name: name.to_owned(),
        identity: tracks_model::folded(name),
        album_count: albums,
        song_count: songs,
        artwork: None,
        mbz_artist: None,
    };
    let mut beyonce = artist("Beyoncé", 7, 112);
    beyonce.mbz_artist = Some(String::from("859d0860-d480-4efd-970c-c05d5f1776b8"));
    let queen = artist("Queen", 15, 180);
    let album_name = "I Am... Sasha Fierce";
    let album = Album {
        id: AlbumId::derive(
            &music.id,
            &format!(
                "{}\u{1f}{}",
                tracks_model::folded(album_name),
                beyonce.identity
            ),
        ),
        library: music.id.clone(),
        name: album_name.to_owned(),
        sort_name: album_name.to_owned(),
        album_artist: beyonce.name.clone(),
        artist_id: Some(beyonce.id.clone()),
        year: Some(2008),
        release_date: Some(String::from("2008-11-12")),
        original_date: None,
        genres: vec![String::from("R&B")],
        is_compilation: false,
        release_type: Some(String::from("album")),
        song_count: 11,
        disc_count: 1,
        duration_ms: 2_486_000,
        size_bytes: 330_000_000,
        artwork: Some(ArtworkId::of_bytes(b"sasha fierce cover")),
        album_key: tracks_model::album_key(album_name, &beyonce.identity),
        mbz: AlbumMbz {
            release: Some(String::from("9c8d0d3e-6d5b-4a2e-8f1a-0b2c3d4e5f60")),
            release_group: None,
        },
        added_at: at(-86_400 * 30),
        updated_at: at(-86_400 * 30),
    };
    let mut halo = song(
        &music,
        "Halo",
        &beyonce,
        Some(&album),
        3,
        261_640,
        flac(33_102_456, 1012),
        1,
    );
    halo.genres = vec![String::from("R&B"), String::from("Pop")];
    halo.replay_gain = Some(ReplayGain {
        track_gain: Some(-8.21),
        track_peak: Some(0.988),
        album_gain: Some(-8.9),
        album_peak: Some(1.0),
    });
    halo.mbz = SongMbz {
        recording: Some(String::from("e6a7c1b2-3d4e-4f5a-8b6c-7d8e9f0a1b2c")),
        track: None,
        release: album.mbz.release.clone(),
    };
    halo.isrc = Some(String::from("USSM10804555"));
    halo.explicit = Some(Explicit::Clean);
    halo.has_lyrics = true;
    let ladies = song(
        &music,
        "Single Ladies (Put a Ring on It)",
        &beyonce,
        Some(&album),
        1,
        193_200,
        flac(24_100_000, 998),
        2,
    );
    let mut dont_stop = song(
        &acquired,
        "Don't Stop Me Now",
        &queen,
        None,
        12,
        209_000,
        AudioFormat {
            container: Container::M4a,
            codec: Codec::Aac,
            content_type: Container::M4a.content_type(),
            lossless: false,
            bitrate_kbps: Some(256),
            sample_rate: Some(44_100),
            bit_depth: None,
            channels: Some(2),
            size: 6_700_000,
        },
        3,
    );
    dont_stop.genres = Vec::new();
    let user = User {
        key: UserKey(1),
        id: id("user luke"),
        username: String::from("luke"),
        display_name: String::from("Luke"),
        role: Role::Member,
        permissions: Permissions::default(),
        sync_epoch: 1,
        created_at: at(-86_400 * 70),
        updated_at: at(-86_400 * 70),
        password_changed_at: at(-86_400 * 70),
        disabled_at: None,
        must_change_password: false,
    };
    let device = Device {
        key: DeviceKey(4),
        id: id("device iphone"),
        user: UserKey(1),
        client_device_id: String::from("8C2E7A1B-4D3F-4E2A-9B8C-1D2E3F4A5B6C"),
        name: String::from("Luke's iPhone"),
        platform: String::from("ios"),
        model: Some(String::from("iPhone18,1")),
        app_name: Some(String::from("Tracks")),
        app_version: Some(String::from("3.2")),
        media_epoch: 1,
        created_at: at(-86_400 * 70),
        last_seen_at: at(-60),
        last_ip: Some(String::from("192.168.1.41")),
        revoked_at: None,
    };
    Seed {
        library: music,
        acquired,
        beyonce,
        queen,
        album,
        halo,
        ladies,
        dont_stop,
        user,
        device,
    }
}

fn server_summary(version: bool) -> ServerSummary {
    ServerSummary {
        server_id: SERVER_ID.to_owned(),
        name: String::from("Home"),
        version: version.then(|| String::from("1.0.0")),
        urls: vec![
            ServerUrl {
                url: String::from("http://192.168.1.20:4747"),
                kind: UrlKind::Lan,
                tls: false,
                fingerprint: None,
            },
            ServerUrl {
                url: String::from("http://home.local:4747"),
                kind: UrlKind::Lan,
                tls: false,
                fingerprint: None,
            },
            ServerUrl {
                url: String::from("https://music.example.com"),
                kind: UrlKind::Public,
                tls: true,
                fingerprint: None,
            },
        ],
    }
}

fn principal(seed: &Seed) -> Principal {
    Principal {
        access: Access {
            user: seed.user.key,
            role: Role::Member,
            libraries: LibrarySet::Only([seed.library.key, seed.acquired.key].into()),
            sync_epoch: 1,
        },
        user: seed.user.id.clone(),
        username: seed.user.username.clone(),
        role: Role::Member,
        permissions: seed.user.permissions.clone(),
        scopes: Scopes::ALL,
        via: Via::DeviceKey,
        device: Some((seed.device.key, seed.device.id.clone())),
        key: Some(id("key iphone")),
        session: None,
    }
}

fn token() -> MediaToken {
    MediaToken {
        token: String::from("mt1.eyJ1IjoidThiMSIsImQiOiJkMmM0In0.f3c1a9b2"),
        expires_at: at(7 * 86_400),
    }
}

fn view(seed: &Seed, state: AcquisitionState) -> AcquisitionView {
    AcquisitionView {
        id: id("acquisition halo"),
        state,
        title: seed.halo.title.clone(),
        artist: seed.halo.artist.clone(),
        album: seed.halo.album.clone(),
        progress: (state == AcquisitionState::Downloading)
            .then_some((12_000_000, Some(33_000_000))),
        quality: Some(Quality::Lossless),
        song: (state == AcquisitionState::Done).then(|| seed.halo.id.clone()),
        existing: false,
        failure: None,
        stream: (state == AcquisitionState::Downloading).then(|| InstantStreamUrls {
            hls: String::from("/api/v1/acquisitions/q/stream.m3u8"),
            progressive: String::from("/api/v1/acquisitions/q/stream"),
        }),
        created_at: at(0),
        updated_at: at(7),
    }
}

/// The cover looked up for a suggested song, by Last.fm's file name for it.
fn external_art() -> ExternalArtId {
    ExternalArtId::derive("4128a6eb29f94943c9d206c08e625904")
}

fn candidate() -> CandidateView {
    CandidateView {
        token: CandidateToken::parse("xtka7fegwc8p56dnxvg3w").expect("a candidate token"),
        title: String::from("Halo"),
        artist: String::from("Beyoncé"),
        album: Some(String::from("I Am... Sasha Fierce")),
        duration_ms: Some(261_000),
        quality: Quality::Lossless,
        format: CandidateFormat {
            container: Some(Container::Flac),
            codec: Some(Codec::Flac),
            bitrate_kbps: Some(1010),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
        },
        size: Some(33_000_000),
        instant: false,
        exact: true,
    }
}

fn scan_run(library: &Library) -> ScanRun {
    ScanRun {
        id: id("scan music"),
        library: library.id.clone(),
        kind: ScanRunKind::Full,
        trigger: ScanTrigger::Admin,
        state: ScanState::Done,
        started_at: at(-3600),
        finished_at: Some(at(-2700)),
        counts: ScanCounts {
            folders_seen: 4112,
            files_seen: 48_214,
            added: 212,
            updated: 37,
            moved: 3,
            missing: 1,
            errors: 2,
        },
        message: None,
    }
}

fn provider() -> ProviderStatus {
    ProviderStatus {
        name: String::from("ytdlp"),
        enabled: true,
        configured: true,
        health: ProviderHealthState::Ok,
        detail: Some(String::from("yt-dlp 2026.11.03")),
        cooldown_until: None,
        version: Some(String::from("2026.11.03")),
        latest_known: Some(String::from("2026.11.03")),
        settings: serde_json::json!({"concurrency": 2, "search_rate": "10/1m"}),
        stats: ProviderStats {
            searches_today: 18,
            fetches_today: 2,
            failures_today: 0,
        },
    }
}

/// A developer token in the real shape: a JWT whose header and claims are what the server
/// signs. ECDSA signatures differ every time, so the signature part is a fixed example.
fn developer_token_example() -> wire::auth::DeveloperToken {
    use base64::Engine;
    let b64 = |v: serde_json::Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string())
    };
    let iat = TEST_START.0 / 1000;
    let exp = iat + 86_400;
    let token = format!(
        "{}.{}.{}",
        b64(serde_json::json!({"alg": "ES256", "kid": "ABC123DEFG"})),
        b64(serde_json::json!({"iss": "TEAM456XYZ", "iat": iat, "exp": exp})),
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7_u8; 64]),
    );
    wire::auth::DeveloperToken {
        token,
        expires_at: Time(Millis(exp * 1000)),
        refresh_after: Time(Millis((exp - 3600) * 1000)),
    }
}

/// Every fixture: path under `tests/fixtures` and its JSON.
#[expect(clippy::too_many_lines, reason = "one list of every response shape")]
fn fixtures() -> Result<BTreeMap<&'static str, serde_json::Value>, serde_json::Error> {
    let s = seed();
    let mut out = BTreeMap::new();
    let mut put = |name: &'static str, value: &dyn erased::Json| -> Result<(), serde_json::Error> {
        out.insert(name, value.json()?);
        Ok(())
    };
    let base = "/api/v1";
    let me_halo = HashMap::from([(
        ItemRef::Song(s.halo.id.clone()),
        UserAnnotations {
            starred_at: Some(at(-86_400 * 10)),
            rating: Some(4),
            play_count: 17,
            last_played_at: Some(at(-43_200)),
        },
    )]);

    put(
        "ping.json",
        &wire::Ping {
            server_id: SERVER_ID.to_owned(),
            time: Time(TEST_START),
            proof: None,
        },
    )?;
    put(
        "ping-proof.json",
        &wire::Ping {
            server_id: SERVER_ID.to_owned(),
            time: Time(TEST_START),
            proof: Some(trackside_auth::proof::endpoint_proof(
                &sha2_of(b"tsk_example"),
                SERVER_ID,
                "192.168.1.10:4533",
                "AAAAAAAAAAAAAAAAAAAAAA",
            )),
        },
    )?;
    let mut info = ServerInfo::new(uuid_of(SERVER_ID), "Home", TEST_START);
    info.version = "1.0.0";
    let media = MediaCapabilities {
        ffmpeg: Some(String::from("7.1")),
        profiles: Profile::ALL.to_vec(),
        hls: true,
        suggested_cellular_profile: Some(Profile::Aac256),
    };
    put(
        "capabilities.json",
        &build::capabilities_body(
            &info,
            &media,
            String::from("Home"),
            false,
            false,
            Some(cloud::EPOCH),
        ),
    )?;
    put(
        "setup-status.json",
        &wire::auth::SetupStatus {
            setup_required: true,
        },
    )?;
    put(
        "error-not-found.json",
        &wire::ErrorEnvelope {
            error: wire::ErrorBody {
                code: String::from("not_found"),
                message: String::from("Not found."),
                details: None,
                request_id: Some(String::from("0b3f9c2e7d4a4f1e8c6b5a4d3e2f1a0b")),
            },
        },
    )?;
    put(
        "error-https-required.json",
        &wire::ErrorEnvelope {
            error: wire::ErrorBody {
                code: String::from("https_required"),
                message: trackside_auth::AuthError::HttpsRequired.to_string(),
                details: Some(serde_json::json!({"https_urls": [
                    {"url": "https://music.example.com", "kind": "public", "tls": true},
                    {"url": "https://home.tail4a2c.ts.net", "kind": "tailnet", "tls": true},
                ]})),
                request_id: Some(String::from("7a1e4c9b2d8f4a6c9e3b5d7f1a2c4e6b")),
            },
        },
    )?;
    put(
        "error-quota.json",
        &wire::ErrorEnvelope {
            error: wire::ErrorBody {
                code: String::from("quota_exceeded"),
                message: String::from("The request quota is used up."),
                details: Some(serde_json::json!({"resets_at": at(86_400).to_rfc3339()})),
                request_id: Some(String::from("5d2c1b0a9f8e4d7c6b5a4f3e2d1c0b9a")),
            },
        },
    )?;

    let libraries = vec![(s.library.clone(), true), (s.acquired.clone(), true)];
    let feature = Some(AcquireFeature {
        instant_play: true,
        external_play: true,
    });
    put(
        "me.json",
        &build::me_body(
            &s.user,
            &libraries,
            Some(&s.device),
            server_summary(true),
            feature,
            true,
        ),
    )?;
    put(
        "me-without-acquisition.json",
        &build::me_body(
            &s.user,
            &libraries,
            Some(&s.device),
            server_summary(true),
            None,
            false,
        ),
    )?;

    let issued = IssuedDeviceKey {
        secret: String::from("tsk_9pQvL2mN6tY0wB4kD9hJ1sF5gA7cE3uI8oP2zR6x"),
        key: id::<KeyId>("key iphone"),
        device: s.device.id.clone(),
        device_name: s.device.name.clone(),
        principal: principal(&s),
    };
    let pair = build::pair_result_body(&issued, &s.user, &token(), server_summary(false));
    put("pair-result.json", &pair)?;
    put(
        "pair-poll-pending.json",
        &wire::auth::PairPoll {
            state: wire::auth::PairState::Pending,
            result: None,
        },
    )?;
    put(
        "pair-poll-approved.json",
        &wire::auth::PairPoll {
            state: wire::auth::PairState::Approved,
            result: Some(pair.clone()),
        },
    )?;
    put(
        "pair-request.json",
        &wire::auth::PairRequest {
            id: id::<trackside_core::PairingId>("pair request").to_string(),
            code: build::spaced_code("481207"),
            poll_secret: String::from("Zx81kQ4mP7rT2vW9yB3nC6dF"),
            expires_at: at(600).into(),
            poll_interval_ms: 2000,
        },
    )?;
    let pairing_id: trackside_core::PairingId = id("pairing offer");
    let qr = wire::auth::QrPayload {
        v: 1,
        server_id: SERVER_ID.to_owned(),
        name: String::from("Home"),
        urls: server_summary(false).urls,
        offer: pairing_id.to_string(),
        code: String::from("Q7RM4X2K9TBV6WNZ3HCJ8DPF5Y"),
        exp: at(600).0 / 1000,
    };
    put(
        "pairing-code.json",
        &wire::auth::PairingCode {
            id: pairing_id.to_string(),
            code: String::from("Q7RM-4X2K"),
            qr_payload: build::qr_link(&qr),
            qr_svg_url: format!("{base}/me/pairing-codes/{pairing_id}/qr.svg"),
            expires_at: at(600).into(),
        },
    )?;
    put(
        "media-token.json",
        &wire::auth::MediaToken {
            token: token().token,
            expires_at: token().expires_at.into(),
        },
    )?;
    put(
        "apple-music-developer-token.json",
        &developer_token_example(),
    )?;
    put(
        "admin/apple-music.json",
        &build::apple_music_admin_body("ABC123DEFG", "TEAM456XYZ", at(-86_400 * 3)),
    )?;
    put(
        "admin/apple-music-unset.json",
        &wire::admin::AppleMusicKey::default(),
    )?;
    put(
        "approved.json",
        &wire::auth::Approved {
            device: wire::auth::ApprovedDevice {
                name: Some(String::from("Living Room Mac")),
                platform: Some(String::from("macos")),
            },
        },
    )?;
    put(
        "invite-info.json",
        &wire::auth::InviteInfo {
            valid: true,
            role: String::from("member"),
            expires_at: at(7 * 86_400).into(),
            invited_by: Some(String::from("Luke")),
            note: Some(String::from("For Mum")),
        },
    )?;
    put("user.json", &map::user(&s.user))?;
    let mut session_user = map::user(&s.user);
    session_user.recent_auth_until = Some(Time(at(600)));
    put("session-user.json", &session_user)?;
    let days = build::day_buckets(Some("Europe/London"), TEST_START, 30);
    let running: Vec<i64> = vec![days.starts[0]];
    put(
        "admin/stats-listening.json",
        &build::listening_body(
            &days,
            std::slice::from_ref(&s.user),
            &[(s.user.key, days.starts[3], 4)],
            &running,
        ),
    )?;
    put(
        "admin/stats-requests.json",
        &build::requests_body(
            &days,
            &[(String::from("done"), days.starts[2], 1)],
            &running,
        ),
    )?;
    let sample = trackside_core::repo::stats::HourStats {
        hour_start: trackside_core::repo::stats::hour_of(TEST_START),
        streams: 2,
        transcodes: 1,
        failed: 0,
    };
    put(
        "admin/stats-streams.json",
        &build::streams_body(TEST_START, 24, &[sample], None, Some(4)),
    )?;
    let storage_days = build::day_buckets(Some("UTC"), TEST_START, 90);
    put(
        "admin/stats-storage.json",
        &build::storage_body(
            &storage_days,
            &[trackside_core::repo::stats::DayStorage {
                day: storage_days.days[89].clone(),
                data_free_bytes: Some(150_000_000_000),
                database_bytes: Some(700_000_000),
                cache_bytes: Some(2_200_000_000),
                volume_bytes: Some(500_000_000_000),
            }],
        ),
    )?;
    put(
        "devices.json",
        &wire::auth::Devices {
            devices: vec![map::device_record(&s.device, true)],
        },
    )?;
    let key_row = ApiKeyRow {
        id: id("key symfonium"),
        user: s.user.key,
        device: None,
        kind: KeyKind::ApiKey,
        name: String::from("Symfonium"),
        prefix: String::from("tsk_p3XvQ8"),
        scopes: String::from("library stream write"),
        created_at: at(-86_400),
        last_used_at: Some(at(-120)),
        last_used_ip: Some(String::from("192.168.1.41")),
        expires_at: None,
        revoked_at: None,
    };
    put(
        "api-keys.json",
        &wire::auth::Keys {
            keys: vec![map::key_record(&key_row)],
        },
    )?;
    let mut created = map::key_record(&key_row);
    created.secret = Some(String::from(
        "tsk_p3XvQ8rL2mN6tY0wB4kD9hJ1sF5gA7cE3uI8oP2zR6x",
    ));
    put("api-key-created.json", &created)?;

    let song_with_me = map::songs_with_me(std::slice::from_ref(&s.halo), &me_halo).remove(0);
    put("song.json", &song_with_me)?;
    put(
        "songs.json",
        &wire::library::Songs {
            songs: map::songs_with_me(&[s.halo.clone(), s.dont_stop.clone()], &me_halo),
        },
    )?;
    put(
        "album-page.json",
        &wire::library::AlbumPage {
            album: map::album(&s.album),
            songs: map::songs_with_me(&[s.ladies.clone(), s.halo.clone()], &me_halo),
        },
    )?;
    put(
        "albums.json",
        &wire::library::Albums {
            albums: vec![map::album(&s.album)],
            next_cursor: Some(String::from("eyJzIjoibmV3ZXN0IiwiayI6WzE3OTJdfQ")),
        },
    )?;
    put(
        "artists.json",
        &wire::library::Artists {
            artists: vec![map::artist(&s.beyonce), map::artist(&s.queen)],
            next_cursor: None,
        },
    )?;
    put(
        "artist-page.json",
        &wire::library::ArtistPage {
            artist: map::artist(&s.beyonce),
            albums: vec![map::album(&s.album)],
            appears_on: Vec::new(),
            top_songs: map::songs_with_me(&[s.halo.clone(), s.ladies.clone()], &me_halo),
            bio: Some(String::from(
                "Beyoncé Giselle Knowles-Carter is an American singer, songwriter and actress.",
            )),
        },
    )?;
    put(
        "similar-songs.json",
        &wire::library::SuggestedSongs {
            songs: map::songs_with_me(std::slice::from_ref(&s.ladies), &me_halo),
            external: Some(vec![wire::library::Suggestion {
                title: String::from("Irreplaceable"),
                artist: String::from("Beyoncé"),
                artwork_id: Some(external_art().to_string()),
            }]),
        },
    )?;
    put(
        "mix.json",
        &wire::library::Songs {
            songs: map::songs_with_me(std::slice::from_ref(&s.halo), &me_halo),
        },
    )?;
    put(
        "top-songs.json",
        &wire::library::Songs {
            songs: map::songs_with_me(&[s.halo.clone(), s.dont_stop.clone()], &me_halo),
        },
    )?;
    put(
        "get-more.json",
        &wire::library::SuggestedSongs {
            songs: map::songs_with_me(std::slice::from_ref(&s.ladies), &me_halo),
            external: Some(vec![wire::library::Suggestion {
                title: String::from("Irreplaceable"),
                artist: String::from("Beyoncé"),
                artwork_id: Some(external_art().to_string()),
            }]),
        },
    )?;
    put(
        "picks.json",
        &wire::library::SuggestedSongs {
            songs: map::songs_with_me(&[s.ladies.clone(), s.dont_stop.clone()], &me_halo),
            external: Some(vec![
                wire::library::Suggestion {
                    title: String::from("Irreplaceable"),
                    artist: String::from("Beyoncé"),
                    artwork_id: Some(external_art().to_string()),
                },
                wire::library::Suggestion {
                    title: String::from("Run the World (Girls)"),
                    artist: String::from("Beyoncé"),
                    artwork_id: None,
                },
            ]),
        },
    )?;
    put(
        "similar-artists.json",
        &wire::library::SimilarArtists {
            artists: vec![
                wire::library::SimilarArtist {
                    artist: Some(map::artist(&s.queen)),
                    name: s.queen.name.clone(),
                    identity: s.queen.identity.clone(),
                    score: 0.62,
                    in_library: true,
                },
                wire::library::SimilarArtist {
                    artist: None,
                    name: String::from("Rihanna"),
                    identity: tracks_model::folded("Rihanna"),
                    score: 0.58,
                    in_library: false,
                },
            ],
        },
    )?;
    put(
        "genres.json",
        &wire::library::Genres {
            genres: vec![wire::library::Genre {
                name: String::from("Pop"),
                song_count: 3120,
                album_count: 240,
            }],
        },
    )?;
    put(
        "lyrics.json",
        &map::lyrics(&[
            Lyrics {
                kind: LyricsKind::Synced,
                lang: String::from("eng"),
                source: LyricsSource::Sidecar,
                content: String::from(
                    "[00:12.34]Remember those walls I built?\n[00:16.80]Well, baby, they're tumbling down",
                ),
            },
            Lyrics {
                kind: LyricsKind::Plain,
                lang: String::from("eng"),
                source: LyricsSource::Embedded,
                content: String::from(
                    "Remember those walls I built?\nWell, baby, they're tumbling down",
                ),
            },
        ]),
    )?;
    put(
        "libraries.json",
        &wire::Libraries {
            libraries: libraries.iter().map(|(l, a)| map::library(l, *a)).collect(),
        },
    )?;
    put(
        "search.json",
        &wire::library::SearchResults {
            songs: Some(wire::Section::new(
                map::songs_with_me(std::slice::from_ref(&s.halo), &me_halo),
                Some(String::from("eyJvIjoyMH0")),
            )),
            albums: Some(wire::Section::new(vec![map::album(&s.album)], None)),
            artists: Some(wire::Section::new(vec![map::artist(&s.beyonce)], None)),
            external: Some(wire::library::ExternalSection {
                search_id: String::from("v2b5n8m1q4w7e0r3t6y9"),
                complete: true,
                candidates: vec![map::candidate(&candidate(), None)],
            }),
        },
    )?;
    put(
        "search-library-only.json",
        &wire::library::SearchResults {
            songs: Some(wire::Section::new(
                map::songs_with_me(std::slice::from_ref(&s.halo), &me_halo),
                None,
            )),
            albums: Some(wire::Section::new(Vec::new(), None)),
            artists: Some(wire::Section::new(vec![map::artist(&s.beyonce)], None)),
            external: None,
        },
    )?;

    let playlist = Playlist {
        id: id::<PlaylistId>("playlist late drive"),
        owner: s.user.key,
        owner_id: s.user.id.clone(),
        owner_display_name: s.user.display_name.clone(),
        owner_username: s.user.username.clone(),
        name: String::from("Late Drive"),
        comment: None,
        is_public: false,
        song_count: 2,
        duration_ms: 470_640,
        revision: 12,
        created_at: at(-86_400 * 300),
        updated_at: at(-86_400 * 4),
    };
    let queue = PlayQueue {
        entries: vec![s.halo.id.clone(), s.ladies.id.clone()],
        current: Some(1),
        position_ms: 81_000,
        changed_by: String::from("Tracks"),
        device: Some(s.device.id.clone()),
        updated_at: at(-300),
    };
    let available = |_: &LibraryId| true;
    let snapshot = |items, next: SyncCursor, complete| {
        SyncPage::Snapshot(SnapshotPage {
            seq: Seq(918_274),
            libraries: vec![s.library.clone(), s.acquired.clone()],
            items,
            next_cursor: next,
            complete,
        })
    };
    let snap_cursor = |kind, after| SyncCursor {
        epoch: 1,
        seq: Seq(918_274),
        snapshot: Some(SnapshotPosition {
            kind,
            after,
            after_key: None,
        }),
    };
    let delta_cursor = SyncCursor {
        epoch: 1,
        seq: Seq(918_274),
        snapshot: None,
    };
    put(
        "sync-snapshot-songs.json",
        &build::page_body(
            &snapshot(
                SnapshotItems::Songs(vec![s.halo.clone(), s.ladies.clone(), s.dont_stop.clone()]),
                snap_cursor(SnapshotKind::Songs, 3),
                false,
            ),
            available,
        ),
    )?;
    put(
        "sync-snapshot-artists.json",
        &build::page_body(
            &snapshot(
                SnapshotItems::Artists(vec![s.beyonce.clone(), s.queen.clone()]),
                snap_cursor(SnapshotKind::Albums, 0),
                false,
            ),
            available,
        ),
    )?;
    put(
        "sync-snapshot-playlists.json",
        &build::page_body(
            &snapshot(
                SnapshotItems::Playlists(vec![PlaylistWithEntries {
                    playlist: playlist.clone(),
                    entries: queue.entries.clone(),
                }]),
                snap_cursor(SnapshotKind::Stars, 0),
                false,
            ),
            available,
        ),
    )?;
    put(
        "sync-snapshot-complete.json",
        &build::page_body(
            &snapshot(
                SnapshotItems::PlayQueue(Some(queue.clone())),
                delta_cursor,
                true,
            ),
            available,
        ),
    )?;
    let mut renamed = s.album.clone();
    renamed.name = String::from("I Am... Sasha Fierce (Deluxe Edition)");
    let mut halo_renamed = s.halo.clone();
    halo_renamed.album = Some(renamed.name.clone());
    halo_renamed.updated_at = at(-10);
    put(
        "sync-delta.json",
        &build::page_body(
            &SyncPage::Delta(DeltaPage {
                upserts: DeltaUpserts {
                    songs: vec![halo_renamed],
                    albums: vec![renamed],
                    artists: Vec::new(),
                    libraries: Vec::new(),
                    playlists: vec![PlaylistWithEntries {
                        playlist: playlist.clone(),
                        entries: queue.entries.clone(),
                    }],
                    stars: vec![Star {
                        item: ItemRef::Song(s.halo.id.clone()),
                        starred_at: at(-60),
                    }],
                    ratings: vec![Rating {
                        item: ItemRef::Album(s.album.id.clone()),
                        rating: 5,
                    }],
                    play_stats: vec![PlayStat {
                        song: s.halo.id.clone(),
                        play_count: 18,
                        last_played_at: at(-30),
                    }],
                    play_queue: Some(queue.clone()),
                },
                deletes: DeltaDeletes {
                    songs: vec![SongId::derive(&AudioDigest([9; 32]))],
                    stars: vec![ItemRef::Song(s.ladies.id.clone())],
                    ..DeltaDeletes::default()
                },
                next_cursor: SyncCursor {
                    epoch: 1,
                    seq: Seq(918_301),
                    snapshot: None,
                },
                has_more: false,
            }),
            available,
        ),
    )?;

    let file = SongFile {
        song: s.halo.id.clone(),
        library: s.library.key,
        path: PathBuf::from("/music/Beyoncé/I Am... Sasha Fierce/03 Halo.flac"),
        size: s.halo.format.size,
        mtime_ns: 0,
        content_version: 1,
        format: s.halo.format.clone(),
        duration_ms: s.halo.duration_ms,
    };
    let (container, codec) = Profile::Aac256.output();
    let transcode = CachedTranscode {
        path: PathBuf::from("/data/cache/transcodes/x.m4a"),
        size: 8_413_002,
        etag: format!("\"{}.1.aac-256.f3c1a9\"", s.halo.id),
        profile: Profile::Aac256,
        format: AudioFormat {
            container,
            codec,
            content_type: container.content_type(),
            lossless: false,
            bitrate_kbps: Some(256),
            sample_rate: Some(44_100),
            bit_depth: None,
            channels: Some(2),
            size: 8_413_002,
        },
    };
    put(
        "decision-direct.json",
        &build::decision_body(base, &file, &Decision::Direct { reason: None }, None),
    )?;
    put(
        "decision-file.json",
        &build::decision_body(
            base,
            &file,
            &Decision::File {
                transcode,
                reason: DecisionReason::OverBitrate,
            },
            None,
        ),
    )?;
    put(
        "decision-hls.json",
        &build::decision_body(
            base,
            &file,
            &Decision::Hls {
                profile: Profile::Aac256,
                reason: DecisionReason::UnsupportedCodec,
            },
            Some(&HlsJobId("j8k2m4n6p8".into())),
        ),
    )?;
    put(
        "prepared.json",
        &wire::playback::Prepared {
            jobs: vec![
                wire::playback::PreparedJob {
                    song_id: s.halo.id.to_string(),
                    state: String::from("cached"),
                },
                wire::playback::PreparedJob {
                    song_id: s.ladies.id.to_string(),
                    state: String::from("queued"),
                },
            ],
        },
    )?;

    put(
        "listen-results.json",
        &wire::social::ListenResults {
            results: vec![
                build::listen_result(&ListenOutcome {
                    client_listen_id: String::from("6F1D9C2A-3B4E-4F5A-8B7C-1D2E3F4A5B6C"),
                    status: ListenStatus::Accepted,
                }),
                build::listen_result(&ListenOutcome {
                    client_listen_id: String::from("7A2E0D3B-4C5F-4A6B-9C8D-2E3F4A5B6C7D"),
                    status: ListenStatus::Duplicate,
                }),
                build::listen_result(&ListenOutcome {
                    client_listen_id: String::from("8B3F1E4C-5D6A-4B7C-8D9E-3F4A5B6C7D8E"),
                    status: ListenStatus::Rejected(RejectReason::Future),
                }),
            ],
        },
    )?;
    put("playlist.json", &map::playlist(&playlist))?;
    put(
        "playlists.json",
        &wire::social::Playlists {
            playlists: vec![map::playlist(&playlist)],
        },
    )?;
    put(
        "playlist-page.json",
        &wire::social::PlaylistPage {
            playlist: map::playlist(&playlist),
            entries: vec![
                wire::social::PlaylistEntry {
                    position: 0,
                    song: Some(song_with_me.clone()),
                    song_id: s.halo.id.to_string(),
                    added_at: at(-86_400 * 300).into(),
                },
                wire::social::PlaylistEntry {
                    position: 1,
                    song: None,
                    song_id: SongId::derive(&AudioDigest([9; 32])).to_string(),
                    added_at: at(-86_400 * 200).into(),
                },
            ],
        },
    )?;
    put("play-queue.json", &map::play_queue(&queue))?;
    put(
        "starred.json",
        &wire::library::Starred {
            items: vec![wire::library::StarredItem::Song(Box::new(
                song_with_me.clone(),
            ))],
            next_cursor: None,
        },
    )?;
    put(
        "upload.json",
        &wire::social::Upload {
            id: id::<trackside_core::ImportId>("upload").to_string(),
            state: String::from("receiving"),
            received: Some(0),
            chunk_max: Some(build::CHUNK_MAX),
            song_id: None,
            error: None,
        },
    )?;

    let search = ExternalSearch {
        search_id: String::from("v2b5n8m1q4w7e0r3t6y9"),
        complete: true,
        in_library: vec![s.halo.clone()],
        candidates: vec![candidate()],
    };
    let art: HashMap<String, ExternalArtId> =
        [(tracks_model::song_key("Halo", "Beyoncé"), external_art())].into();
    put(
        "acquisition-search.json",
        &build::search_body(&search, &art),
    )?;
    put(
        "external-play.json",
        &build::external_play_body(
            &trackside_core::control::ExternalPlay {
                token: Some(CandidateToken::derive("ytdlp", "{\"id\":\"halo\"}")),
                title: String::from("Halo"),
                artist: String::from("Beyoncé"),
                duration_ms: Some(261_000),
                song: None,
            },
            "",
        ),
    )?;
    let urls = server_summary(false).urls;
    put(
        "server-urls.json",
        &wire::auth::ServerUrls {
            server_id: SERVER_ID.to_owned(),
            version: build::urls_version(&urls),
            urls,
        },
    )?;
    put(
        "server-urls-event.json",
        &wire::events::ServerUrlsEvent {
            version: build::urls_version(&server_summary(false).urls),
        },
    )?;
    put(
        "acquisition-downloading.json",
        &map::acquisition(&view(&s, AcquisitionState::Downloading), ""),
    )?;
    let mut done = view(&s, AcquisitionState::Done);
    done.existing = true;
    put("acquisition-done.json", &map::acquisition(&done, ""))?;
    put(
        "acquisitions.json",
        &wire::acquisition::Acquisitions {
            acquisitions: vec![map::acquisition(&view(&s, AcquisitionState::Queued), "")],
            next_cursor: None,
        },
    )?;

    let targets = wire::admin::Targets {
        uploads_library: Some(s.acquired.id.to_string()),
        acquire_library: Some(s.acquired.id.to_string()),
    };
    let status = LibraryScanStatus {
        library: s.library.id.clone(),
        available: true,
        scanning: None,
        last_run: Some(scan_run(&s.library)),
        watching: WatchMode::Events,
    };
    let mut overview = wire::admin::Overview {
        server: wire::admin::OverviewServer {
            name: String::from("Home"),
            server_id: SERVER_ID.to_owned(),
            version: String::from("1.0.0"),
            uptime_s: 86_400,
            started_at: at(-86_400).to_rfc3339(),
            data_dir: String::from("/data"),
            tls: String::from("off"),
            listen: vec![String::from("[::]:4747")],
        },
        storage: wire::admin::OverviewStorage {
            database_bytes: 812_000_000,
            wal_bytes: 4_100_000,
            cache_bytes: 3_200_000_000,
            data_free_bytes: 120_000_000_000,
            warnings: Vec::new(),
        },
        libraries: vec![build::overview_library(&s.library, Some(&status), true)],
        media: wire::admin::OverviewMedia {
            ffmpeg: wire::admin::Ffmpeg {
                found: true,
                version: Some(String::from("7.1")),
            },
            transcodes: wire::admin::Transcodes {
                running: 1,
                queued: 0,
            },
            transcode_cache_bytes: 2_100_000_000,
        },
        acquisition: Some(wire::admin::OverviewAcquisition {
            enabled: true,
            providers: vec![map::provider(&provider())],
            active: 2,
            today: 14,
            fetched: Some(wire::admin::FetchedUsage {
                songs: 212,
                bytes: 7_400_000_000,
                fetching: 1,
                keep: 500,
                max_bytes: 20 << 30,
            }),
        }),
        users: wire::admin::OverviewUsers {
            count: 4,
            devices: 7,
        },
        backups: wire::admin::OverviewBackups {
            last: Some(wire::admin::BackupSummary {
                name: String::from("trackside-20261114T033000Z.db"),
                at: at(-21_600).into(),
                bytes: 790_000_000,
            }),
            next_at: Some(at(64_800).into()),
            last_error: None,
        },
        problems: build::library_problems_for(std::slice::from_ref(&s.library), &[], |_| false),
    };
    put("admin/overview.json", &overview)?;
    overview.acquisition = None;
    put("admin/overview-no-acquisition.json", &overview)?;
    let mut admin_user = s.user.clone();
    admin_user.role = Role::Admin;
    put(
        "admin/users.json",
        &wire::admin::AdminUsers {
            users: vec![build::admin_user(
                &admin_user,
                vec![s.library.id.to_string(), s.acquired.id.to_string()],
                Some(at(-60)),
                None,
            )],
        },
    )?;
    put(
        "admin/user.json",
        &build::admin_user(
            &s.user,
            vec![s.library.id.to_string()],
            Some(at(-60)),
            Some(wire::admin::UserCounts {
                devices: 2,
                api_keys: 1,
                sessions: 1,
                cloud: Some(wire::cloud::Amount {
                    items: 152_340,
                    bytes: 98_312_448,
                }),
            }),
        ),
    )?;
    put(
        "admin/libraries.json",
        &wire::admin::AdminLibraries {
            libraries: vec![
                build::admin_library(
                    &s.library,
                    Some(&status),
                    true,
                    Some(Duration::from_mins(30)),
                    &targets,
                ),
                build::admin_library(
                    &s.acquired,
                    None,
                    true,
                    Some(Duration::from_mins(30)),
                    &targets,
                ),
            ],
        },
    )?;
    put(
        "admin/scans.json",
        &wire::admin::Scans {
            scans: vec![map::scan_run(&scan_run(&s.library))],
        },
    )?;
    put(
        "admin/scan.json",
        &build::scan_detail_body(
            &scan_run(&s.library),
            &[trackside_core::repo::scan_runs::ScanErrorRow {
                rel_path: b"Various Artists/Now 47/07 Track07.mp3".to_vec(),
                path_display: String::from("Various Artists/Now 47/07 Track07.mp3"),
                error: String::from(
                    "Tags could not be read; the title was taken from the file name.",
                ),
            }],
        ),
    )?;
    put(
        "admin/providers.json",
        &wire::admin::Providers {
            providers: vec![map::provider(&provider())],
        },
    )?;
    let admin_view = AdminAcquisitionView {
        view: view(&s, AcquisitionState::Downloading),
        user: Some(s.user.id.clone()),
        provider: Some(String::from("ytdlp")),
        chosen: Some(AdminCandidate {
            token: candidate().token,
            provider: String::from("ytdlp"),
            provider_ref: serde_json::json!("bnVUHWCynig"),
            title: String::from("Halo"),
            artist: String::from("Beyoncé"),
            score: 0.94,
            quality: Quality::Lossless,
            format: CandidateFormat {
                container: Some(Container::Flac),
                codec: Some(Codec::Flac),
                bitrate_kbps: None,
                sample_rate: None,
                bit_depth: None,
            },
            size: Some(33_000_000),
            tried_at: Some(at(2)),
            failure: None,
        }),
        attempts: 1,
        failure_detail: None,
        candidates: Vec::new(),
    };
    put(
        "admin/acquisitions.json",
        &wire::admin::AdminAcquisitions {
            acquisitions: vec![map::admin_acquisition(
                &admin_view,
                Some(map::user_ref(&s.user)),
                "",
            )],
            next_cursor: None,
        },
    )?;
    put(
        "admin/backups.json",
        &wire::admin::Backups {
            backups: vec![wire::admin::Backup {
                name: String::from("trackside-20261114T033000Z.db"),
                kind: String::from("scheduled"),
                at: at(-21_600).into(),
                bytes: 790_000_000,
                has_key: true,
            }],
            next_at: Some(at(64_800).into()),
        },
    )?;
    let settings: wire::admin::Settings = BTreeMap::from([
        (
            String::from("server.name"),
            wire::admin::Setting {
                value: serde_json::json!("Home"),
                source: String::from("dashboard"),
                editable: true,
            },
        ),
        (
            String::from("discovery.mdns"),
            wire::admin::Setting {
                value: serde_json::json!(true),
                source: String::from("config"),
                editable: false,
            },
        ),
        (
            String::from("enrichment.lastfm_api_key"),
            wire::admin::Setting {
                value: serde_json::json!(true),
                source: String::from("dashboard"),
                editable: true,
            },
        ),
    ]);
    put("admin/settings.json", &settings)?;
    put(
        "admin/enrichment.json",
        &trackside_api::build::enrichment_status_body(&trackside_core::enrich::EnrichStatus {
            configured: true,
            artists: 1840,
            songs: 5210,
            last_fetch: Some(at(-120)),
            last_error: Some((
                at(-3600),
                String::from("Last.fm error 29: Rate Limit Exceeded"),
            )),
        }),
    )?;
    put(
        "admin/enrichment-test.json",
        &wire::enrichment::EnrichmentTestResult {
            ok: false,
            elapsed_ms: 212,
            detail: Some(String::from("Last.fm error 10: Invalid API key")),
        },
    )?;
    drop(put);
    for (name, value) in admin_extras(&s)? {
        out.insert(name, value);
    }
    for (name, value) in cloud::fixtures()? {
        out.insert(name, value);
    }
    Ok(out)
}

/// The admin shapes api.md leaves open, as the dashboard's fixtures propose them.
#[expect(clippy::too_many_lines, reason = "one list of the admin shapes")]
fn admin_extras(s: &Seed) -> Result<Vec<(&'static str, serde_json::Value)>, serde_json::Error> {
    use wire::admin as a;
    let who = map::user_ref(&s.user);
    let key = ApiKeyRow {
        id: id("key symfonium"),
        user: s.user.key,
        device: None,
        kind: KeyKind::AppPassword,
        name: String::from("Symfonium"),
        prefix: String::from("TRK7"),
        scopes: String::from("library stream write"),
        created_at: at(-86_400),
        last_used_at: Some(at(-120)),
        last_used_ip: Some(String::from("100.90.41.5")),
        expires_at: None,
        revoked_at: None,
    };
    let record = map::key_record(&key);
    let mut app_password = record.clone();
    app_password.secret = Some(String::from("K7MQ-4XD9-P2TA-HN8R-W3CF-6YJB"));
    app_password.username = Some(s.user.username.clone());
    let targets = a::Targets {
        uploads_library: Some(s.acquired.id.to_string()),
        acquire_library: Some(s.acquired.id.to_string()),
    };
    let invite = a::AdminInvite {
        id: id::<trackside_core::InviteId>("invite nana").to_string(),
        role: String::from("member"),
        permissions: map::permissions(Role::Member, &Permissions::default()),
        library_ids: vec![s.library.id.to_string()],
        note: String::from("For Nana"),
        created_by: Some(s.user.username.clone()),
        created_at: at(-900).into(),
        expires_at: at(7 * 86_400).into(),
        state: String::from("open"),
        used_by: None,
        url: None,
        qr_svg_url: None,
    };
    let mut created_invite = invite.clone();
    created_invite.url = Some(String::from(
        "http://192.168.1.20:4747/invite/7KQ2M9XDp3XvQ8rL2mN6tY0w",
    ));
    created_invite.qr_svg_url = Some(format!("/api/v1/admin/invites/{}/qr.svg", invite.id));
    let mut running = scan_run(&s.library);
    running.state = ScanState::Running;
    running.finished_at = None;
    running.counts = ScanCounts::default();
    let mut unavailable = s.library.clone();
    unavailable.available = false;
    unavailable.unavailable_reason = Some(String::from("The folder is empty or not mounted."));
    unavailable.unavailable_since = Some(at(-5400));
    let rows: Vec<(&'static str, serde_json::Value)> = vec![
        (
            "admin/devices.json",
            serde_json::to_value(a::AdminDevices {
                devices: vec![a::AdminDevice {
                    id: s.device.id.to_string(),
                    name: s.device.name.clone(),
                    platform: s.device.platform.clone(),
                    model: s.device.model.clone(),
                    app_name: s.device.app_name.clone(),
                    app_version: s.device.app_version.clone(),
                    user: who.clone(),
                    created_at: s.device.created_at.into(),
                    last_seen_at: s.device.last_seen_at.into(),
                    last_ip: s.device.last_ip.clone(),
                }],
            })?,
        ),
        (
            "admin/api-keys.json",
            serde_json::to_value(a::AdminKeys {
                keys: vec![a::AdminKey {
                    id: record.id.clone(),
                    kind: record.kind.clone(),
                    name: record.name.clone(),
                    prefix: record.prefix.clone(),
                    scopes: record.scopes.clone(),
                    user: who.clone(),
                    created_at: record.created_at,
                    last_used_at: record.last_used_at,
                    last_used_ip: record.last_used_ip.clone(),
                }],
            })?,
        ),
        (
            "app-password-created.json",
            serde_json::to_value(&app_password)?,
        ),
        (
            "admin/sessions.json",
            serde_json::to_value(a::AdminSessions {
                sessions: vec![a::AdminSession {
                    id: String::from("12"),
                    user: who.clone(),
                    user_agent: Some(String::from("Safari on macOS")),
                    ip: Some(String::from("192.168.1.41")),
                    created_at: at(-86_400).into(),
                    last_seen_at: at(-30).into(),
                    current: true,
                }],
            })?,
        ),
        (
            "admin/invites.json",
            serde_json::to_value(a::AdminInvites {
                invites: vec![invite],
            })?,
        ),
        (
            "admin/invite-created.json",
            serde_json::to_value(&created_invite)?,
        ),
        (
            "admin/audit.json",
            serde_json::to_value(a::Audit {
                entries: vec![a::AuditEntry {
                    at: at(-60).into(),
                    actor: s.user.username.clone(),
                    ip: String::from("192.168.1.41"),
                    action: String::from("invite.create"),
                    target: String::from("member"),
                    detail: Some(serde_json::json!("For Nana")),
                    summary: build::audit_summary("invite.create", Some("member")),
                }],
                next_cursor: None,
            })?,
        ),
        (
            "admin/imports.json",
            serde_json::to_value(a::AdminImports {
                imports: vec![a::AdminImport {
                    id: id::<trackside_core::ImportId>("import halo").to_string(),
                    file_name: String::from("01 Halo.flac"),
                    source: String::from("upload"),
                    user: Some(who.clone()),
                    state: String::from("imported"),
                    song_id: Some(s.halo.id.to_string()),
                    error: None,
                    created_at: at(-120).into(),
                }],
                next_cursor: None,
            })?,
        ),
        (
            "admin/duplicates.json",
            serde_json::to_value(a::Duplicates {
                groups: vec![a::DuplicateGroup {
                    songs: vec![
                        a::DuplicateSong {
                            id: s.halo.id.to_string(),
                            title: s.halo.title.clone(),
                            artist: s.halo.artist.clone(),
                            library_id: s.library.id.to_string(),
                            path: String::from("Beyoncé/I Am... Sasha Fierce/03 Halo.flac"),
                        },
                        a::DuplicateSong {
                            id: SongId::derive(&AudioDigest([8; 32])).to_string(),
                            title: s.halo.title.clone(),
                            artist: s.halo.artist.clone(),
                            library_id: s.acquired.id.to_string(),
                            path: String::from("Beyoncé/I Am... Sasha Fierce/Halo.flac"),
                        },
                    ],
                }],
                next_cursor: None,
            })?,
        ),
        (
            "admin/library-problems.json",
            serde_json::to_value(a::LibraryProblems {
                tag_problems: vec![a::TagProblem {
                    song_id: s.halo.id.to_string(),
                    path: String::from("Various Artists/Now 47/07 Track07.mp3"),
                    problem: String::from(
                        "Tags could not be read; the title was taken from the file name.",
                    ),
                }],
                quarantined: vec![a::Quarantined {
                    path: String::from("Aphex Twin/Drukqs/broken.flac"),
                    error: String::from("Reading this file took longer than 30 seconds."),
                    at: at(-3000).into(),
                }],
                unreadable_folders: vec![a::UnreadableFolder {
                    path: String::from("Private"),
                    since: Some(at(-3000).into()),
                }],
            })?,
        ),
        (
            "admin/server-urls.json",
            serde_json::to_value(a::ServerUrls {
                urls: vec![a::ServerUrlStatus {
                    url: String::from("http://192.168.1.20:4747"),
                    kind: UrlKind::Lan,
                    tls: false,
                    source: String::from("detected"),
                    reachable: true,
                    checked_at: at(0).into(),
                }],
            })?,
        ),
        ("admin/targets.json", serde_json::to_value(&targets)?),
        (
            "admin/password-reset.json",
            serde_json::to_value(a::ResetLink {
                url: String::from("http://192.168.1.20:4747/reset/4XQ9p3XvQ8rL2mN6"),
                expires_at: at(86_400).into(),
            })?,
        ),
        (
            "admin/provider-test.json",
            serde_json::to_value(a::ProviderTestResult {
                ok: true,
                elapsed_ms: 1840,
                candidates: 12,
                detail: Some(String::from("Found 12 results.")),
            })?,
        ),
        (
            "admin/scan-started.json",
            serde_json::to_value(map::scan_run(&running))?,
        ),
        (
            "admin/library-created.json",
            serde_json::to_value(build::admin_library(
                &s.library,
                None,
                true,
                Some(Duration::from_mins(30)),
                &targets,
            ))?,
        ),
        (
            "admin/library-unavailable.json",
            serde_json::to_value(build::admin_library(
                &unavailable,
                None,
                false,
                Some(Duration::from_mins(30)),
                &targets,
            ))?,
        ),
        (
            "admin/user-created.json",
            serde_json::to_value(build::admin_user(
                &s.user,
                vec![s.library.id.to_string()],
                None,
                None,
            ))?,
        ),
    ];
    Ok(rows)
}

fn uuid_of(text: &str) -> uuid::Uuid {
    uuid::Uuid::parse_str(text).unwrap_or_default()
}

#[path = "support/cloud_fixtures.rs"]
mod cloud;

mod erased {
    pub trait Json {
        fn json(&self) -> Result<serde_json::Value, serde_json::Error>;
    }
    impl<T: serde::Serialize> Json for T {
        fn json(&self) -> Result<serde_json::Value, serde_json::Error> {
            serde_json::to_value(self)
        }
    }
}

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn pretty(value: &serde_json::Value) -> Result<String, serde_json::Error> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    Ok(text)
}

fn has_null(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::Array(items) => items.iter().any(has_null),
        serde_json::Value::Object(map) => map.values().any(has_null),
        _ => false,
    }
}

fn existing(dir: &Path, prefix: &str, out: &mut Vec<String>) -> std::io::Result<()> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
        if entry.file_type()?.is_dir() {
            existing(&entry.path(), &format!("{name}/"), out)?;
        } else if Path::new(&name)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("json"))
        {
            out.push(name);
        }
    }
    Ok(())
}

#[test]
fn fixtures_match_wire_types() -> TestResult {
    let built = fixtures()?;
    let root = dir();
    let update = std::env::var_os("TRACKSIDE_UPDATE_FIXTURES").is_some();
    let mut stale = Vec::new();
    existing(&root, "", &mut stale)?;
    stale.retain(|name| !built.contains_key(name.as_str()));
    if update {
        for name in &stale {
            std::fs::remove_file(root.join(name))?;
        }
    } else {
        assert!(stale.is_empty(), "fixtures no handler builds: {stale:?}");
    }
    for (name, value) in &built {
        // The stats series are the one place null is sent: "not recorded", as opposed to 0.
        // Cloud settings are the other: a cleared setting is an explicit null.
        assert!(
            name.starts_with("admin/stats-")
                || name.starts_with("cloud-setting")
                || !has_null(value),
            "{name} sends null; absent fields are left out"
        );
        let path = root.join(name);
        let text = pretty(value)?;
        if update {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, &text)?;
            continue;
        }
        let stored = std::fs::read_to_string(&path)
            .map_err(|e| format!("{name}: {e}; run with TRACKSIDE_UPDATE_FIXTURES=1"))?;
        assert_eq!(stored, text, "{name} differs from what the handlers build");
    }
    Ok(())
}

#[test]
fn fixtures_carry_the_song_keys_tracks_computes() -> TestResult {
    for (name, value) in fixtures()? {
        let mut songs = Vec::new();
        collect_songs(&value, &mut songs);
        for song in songs {
            let (Some(title), Some(artist), Some(key)) = (
                song["title"].as_str(),
                song["artist"].as_str(),
                song["song_key"].as_str(),
            ) else {
                continue;
            };
            assert_eq!(
                key,
                tracks_model::song_key(title, artist),
                "{name}: {title}"
            );
        }
    }
    let halo = seed().halo;
    assert_eq!(halo.song_key, "halo\u{1f}beyonce");
    Ok(())
}

fn collect_songs<'v>(value: &'v serde_json::Value, out: &mut Vec<&'v serde_json::Value>) {
    match value {
        serde_json::Value::Object(map) => {
            if map.contains_key("song_key") {
                out.push(value);
            }
            map.values().for_each(|v| collect_songs(v, out));
        }
        serde_json::Value::Array(items) => items.iter().for_each(|v| collect_songs(v, out)),
        _ => {}
    }
}

/// Each fixture reads back into its wire type, so the shapes are what clients decode.
type Check = fn(&serde_json::Value) -> Result<serde_json::Value, serde_json::Error>;

#[test]
fn fixtures_deserialize_into_their_wire_types() -> TestResult {
    fn back<T: serde::de::DeserializeOwned + Serialize>(
        value: &serde_json::Value,
    ) -> Result<serde_json::Value, serde_json::Error> {
        let typed: T = serde_json::from_value(value.clone())?;
        serde_json::to_value(typed)
    }
    let built = fixtures()?;
    let checks: &[(&str, Check)] = &[
        ("song.json", back::<wire::Song>),
        ("me.json", back::<wire::auth::MeResponse>),
        (
            "me-without-acquisition.json",
            back::<wire::auth::MeResponse>,
        ),
        (
            "apple-music-developer-token.json",
            back::<wire::auth::DeveloperToken>,
        ),
        ("admin/apple-music.json", back::<wire::admin::AppleMusicKey>),
        (
            "admin/apple-music-unset.json",
            back::<wire::admin::AppleMusicKey>,
        ),
        ("pair-result.json", back::<wire::auth::PairResult>),
        ("sync-delta.json", back::<wire::sync::SyncPage>),
        ("sync-snapshot-songs.json", back::<wire::sync::SyncPage>),
        ("search.json", back::<wire::library::SearchResults>),
        ("decision-hls.json", back::<wire::playback::Decision>),
        ("acquisition-search.json", back::<wire::acquisition::Search>),
        ("admin/overview.json", back::<wire::admin::Overview>),
        ("server-urls.json", back::<wire::auth::ServerUrls>),
        (
            "server-urls-event.json",
            back::<wire::events::ServerUrlsEvent>,
        ),
        ("admin/user.json", back::<wire::admin::AdminUser>),
        (
            "admin/acquisitions.json",
            back::<wire::admin::AdminAcquisitions>,
        ),
        ("starred.json", back::<wire::library::Starred>),
        ("mix.json", back::<wire::library::Songs>),
        ("get-more.json", back::<wire::library::SuggestedSongs>),
        ("top-songs.json", back::<wire::library::Songs>),
        ("picks.json", back::<wire::library::SuggestedSongs>),
        ("capabilities.json", back::<wire::Capabilities>),
        ("cloud-push-plays.json", back::<wire::cloud::PushBatch>),
        ("cloud-push-results.json", back::<wire::cloud::PushResults>),
        ("cloud-plays-page.json", back::<wire::cloud::Page>),
        (
            "cloud-playlist-entries-page.json",
            back::<wire::cloud::Page>,
        ),
        ("cloud-settings.json", back::<wire::cloud::Settings>),
        (
            "cloud-settings-patch.json",
            back::<wire::cloud::SettingsPatch>,
        ),
        (
            "cloud-setting-results.json",
            back::<wire::cloud::SettingResults>,
        ),
        ("cloud-devices.json", back::<wire::cloud::CloudDevices>),
        (
            "cloud-device-registration.json",
            back::<wire::cloud::DeviceRegistration>,
        ),
        ("cloud-usage.json", back::<wire::cloud::Usage>),
        ("cloud-event.json", back::<wire::cloud::CloudEvent>),
    ];
    for (name, check) in checks {
        let value = built.get(name).ok_or(*name)?;
        assert_eq!(&check(value)?, value, "{name} does not round-trip");
    }
    Ok(())
}

#[test]
fn provider_names_appear_only_in_admin_fixtures() -> TestResult {
    for (name, value) in fixtures()? {
        if name.starts_with("admin/") {
            continue;
        }
        let text = value.to_string().to_ascii_lowercase();
        for word in ["ytdlp", "yt-dlp", "youtube"] {
            assert!(!text.contains(word), "{name} names {word}");
        }
    }
    Ok(())
}

#[test]
fn dashboard_labels_every_provider_health() -> TestResult {
    let page = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dashboard/pages/acquisition.js");
    if !page.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(page)?;
    let states = [
        ProviderHealthState::Ok,
        ProviderHealthState::Degraded,
        ProviderHealthState::Cooldown,
        ProviderHealthState::Outdated,
        ProviderHealthState::Unavailable,
        ProviderHealthState::NotSetUp,
        ProviderHealthState::Disabled,
    ];
    for state in states {
        let wire = map::provider_health(state);
        assert!(
            text.contains(&format!("\n  {wire}: [")),
            "the dashboard's HEALTH map has no label for {wire}"
        );
    }
    Ok(())
}

/// The dashboard's preview fixtures (WP7) against what the server builds for the same state:
/// every key the server sends is one the dashboard knows, and every key the dashboard reads
/// is one the server sends, apart from fields that are optional or only the server adds.
#[test]
#[expect(clippy::too_many_lines, reason = "one table of fixture pairs")]
fn dashboard_fixtures_match_server_shapes() -> TestResult {
    let dash = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dashboard/dev/fixtures");
    if !dash.exists() {
        return Ok(());
    }
    let built = fixtures()?;
    let pairs: &[(&str, &str, &[&str])] = &[
        (
            "admin/overview.json",
            "admin-overview.json",
            &[
                "acquisition.fetched",
                "acquisition.providers[].cooldown_until",
                "acquisition.providers[].latest_known",
                "acquisition.providers[].settings.",
                "libraries[].last_scan.message",
            ],
        ),
        (
            "admin/users.json",
            "admin-users.json",
            &["users[].permissions.max_bitrate_kbps"],
        ),
        ("admin/user.json", "admin-user.json", &["cloud"]),
        (
            "admin/user-created.json",
            "admin-user-created.json",
            &["permissions.max_bitrate_kbps"],
        ),
        (
            "admin/libraries.json",
            "admin-libraries.json",
            &[
                "libraries[].unavailable_reason",
                "libraries[].unavailable_since",
            ],
        ),
        (
            "admin/library-created.json",
            "admin-library-created.json",
            &[],
        ),
        (
            "admin/scans.json",
            "admin-library-scans.json",
            &["scans[].message", "scans[].finished_at"],
        ),
        ("admin/scan.json", "admin-scan.json", &[]),
        ("admin/scan-started.json", "admin-scan-started.json", &[]),
        (
            "admin/providers.json",
            "admin-acquire-providers.json",
            &[
                "providers[].cooldown_until",
                "providers[].latest_known",
                "providers[].settings.",
            ],
        ),
        (
            "admin/acquisitions.json",
            "admin-acquisitions.json",
            &[
                "acquisitions[].attempts[].",
                "acquisitions[].candidate.format",
                "acquisitions[].candidate.size",
                "acquisitions[].failure",
                "acquisitions[].song_id",
                "acquisitions[].stream",
                "acquisitions[].attempt_count",
                "acquisitions[].candidate.artist",
                "acquisitions[].candidate.score",
                "acquisitions[].candidate.title",
                "acquisitions[].candidate.token",
            ],
        ),
        ("admin/backups.json", "admin-backups.json", &[]),
        ("admin/devices.json", "admin-devices.json", &[]),
        ("admin/api-keys.json", "admin-api-keys.json", &[]),
        ("admin/sessions.json", "admin-sessions.json", &[]),
        (
            "admin/invites.json",
            "admin-invites.json",
            &["invites[].used_by"],
        ),
        (
            "admin/invite-created.json",
            "admin-invite-created.json",
            &[],
        ),
        (
            "admin/audit.json",
            "admin-audit.json",
            &["next_cursor", "entries[].summary"],
        ),
        (
            "admin/imports.json",
            "admin-imports.json",
            &[
                "imports[].user",
                "imports[].song_id",
                "imports[].error",
                "next_cursor",
            ],
        ),
        (
            "admin/duplicates.json",
            "admin-duplicates.json",
            &["next_cursor"],
        ),
        (
            "admin/library-problems.json",
            "admin-library-problems.json",
            &["unreadable_folders[]."],
        ),
        ("admin/server-urls.json", "admin-server-urls.json", &[]),
        ("admin/targets.json", "admin-targets.json", &[]),
        (
            "admin/password-reset.json",
            "admin-password-reset.json",
            &[],
        ),
        ("admin/provider-test.json", "admin-provider-test.json", &[]),
        ("admin/enrichment.json", "admin-enrichment.json", &[]),
        (
            "admin/enrichment-test.json",
            "admin-enrichment-test.json",
            &[],
        ),
        ("admin/apple-music.json", "admin-apple-music.json", &[]),
        (
            "admin/apple-music-unset.json",
            "admin-apple-music-unset.json",
            &[],
        ),
        ("approved.json", "admin-pairing-approved.json", &[]),
        ("approved.json", "pair-approved.json", &[]),
        (
            "me.json",
            "me.json",
            &[
                "device",
                "features.acquisition",
                "user.permissions.max_bitrate_kbps",
                "libraries[].",
                "server.urls[].",
            ],
        ),
        (
            "user.json",
            "auth-session.json",
            &["permissions.max_bitrate_kbps"],
        ),
        ("user.json", "user-created.json", &[]),
        (
            "session-user.json",
            "auth-session.json",
            &["permissions.max_bitrate_kbps", "recent_auth_until"],
        ),
        (
            "admin/stats-listening.json",
            "admin-stats-listening.json",
            &[],
        ),
        (
            "admin/stats-requests.json",
            "admin-stats-requests.json",
            &[],
        ),
        ("admin/stats-streams.json", "admin-stats-streams.json", &[]),
        ("admin/stats-storage.json", "admin-stats-storage.json", &[]),
        ("devices.json", "me-devices.json", &[]),
        ("api-keys.json", "me-api-keys.json", &[]),
        (
            "api-key-created.json",
            "me-api-key-created.json",
            &["last_used_at", "last_used_ip"],
        ),
        (
            "app-password-created.json",
            "me-app-password-created.json",
            &["last_used_at", "last_used_ip"],
        ),
        ("pairing-code.json", "me-pairing-code.json", &[]),
        ("invite-info.json", "invite-info.json", &[]),
        ("setup-status.json", "setup-status.json", &[]),
        (
            "capabilities.json",
            "capabilities.json",
            &["features.cloud", "cloud"],
        ),
    ];
    let mut problems = Vec::new();
    for (ours, theirs, optional) in pairs {
        let server = built.get(ours).ok_or(*ours)?;
        let text = std::fs::read_to_string(dash.join(theirs))?;
        let dashboard: serde_json::Value = serde_json::from_str(&text)?;
        let (mut a, mut b) = (
            std::collections::BTreeSet::new(),
            std::collections::BTreeSet::new(),
        );
        key_paths(server, "", &mut a);
        key_paths(&dashboard, "", &mut b);
        let allowed = |k: &String| {
            optional.iter().any(|o| {
                k == o || (o.ends_with('.') && k.starts_with(o)) || k.starts_with(&format!("{o}."))
            })
        };
        let only_server: Vec<_> = a.difference(&b).filter(|k| !allowed(k)).cloned().collect();
        let only_dashboard: Vec<_> = b.difference(&a).filter(|k| !allowed(k)).cloned().collect();
        if !only_server.is_empty() || !only_dashboard.is_empty() {
            problems.push(format!(
                "{ours} vs {theirs}: server only {only_server:?}, dashboard only {only_dashboard:?}"
            ));
        }
    }
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dash.join("admin-settings.json"))?)?;
    for (name, setting) in settings.as_object().ok_or("settings is not an object")? {
        for field in ["value", "source", "editable"] {
            if setting.get(field).is_none() {
                problems.push(format!("admin-settings.json {name} lacks {field}"));
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
    Ok(())
}

fn key_paths(
    value: &serde_json::Value,
    prefix: &str,
    out: &mut std::collections::BTreeSet<String>,
) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                out.insert(path.clone());
                key_paths(v, &path, out);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                key_paths(item, &format!("{prefix}[]"), out);
            }
        }
        _ => {}
    }
}

/// The stats charts need more than matching keys: every series is as long as its buckets,
/// series hold only numbers and nulls, and buckets are the format the dashboard parses.
#[test]
fn stats_fixtures_are_aligned_like_the_dashboards() -> TestResult {
    let dash = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dashboard/dev/fixtures");
    let built = fixtures()?;
    let check = |name: &str, value: &serde_json::Value| -> Vec<String> {
        let mut problems = Vec::new();
        let buckets = value
            .get("days")
            .or_else(|| value.get("hours"))
            .and_then(serde_json::Value::as_array);
        let Some(buckets) = buckets else {
            return vec![format!("{name}: no buckets")];
        };
        for b in buckets {
            let text = b.as_str().unwrap_or_default();
            let ok = if value.get("days").is_some() {
                text.len() == 10 && text.as_bytes()[4] == b'-'
            } else {
                text.ends_with(":00:00.000Z")
            };
            if !ok {
                problems.push(format!("{name}: bucket {b}"));
            }
        }
        let mut series: Vec<&serde_json::Value> = Vec::new();
        if let Some(users) = value.get("users").and_then(serde_json::Value::as_array) {
            series.extend(users.iter().filter_map(|u| u.get("plays")));
        }
        for (key, v) in value.as_object().into_iter().flatten() {
            if v.is_array() && key != "days" && key != "hours" && key != "users" {
                series.push(v);
            }
        }
        for s in series {
            let items = s.as_array().cloned().unwrap_or_default();
            if items.len() != buckets.len() {
                problems.push(format!(
                    "{name}: series of {} for {} buckets",
                    items.len(),
                    buckets.len()
                ));
            }
            if items.iter().any(|x| !(x.is_null() || x.is_u64())) {
                problems.push(format!("{name}: a series holds something besides counts"));
            }
        }
        problems
    };
    let mut problems = Vec::new();
    for kind in ["listening", "requests", "streams", "storage"] {
        let ours = built
            .get(format!("admin/stats-{kind}.json").as_str())
            .ok_or(kind)?;
        problems.extend(check(&format!("server {kind}"), ours));
        let path = dash.join(format!("admin-stats-{kind}.json"));
        if path.exists() {
            let theirs: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
            problems.extend(check(&format!("dashboard {kind}"), &theirs));
            for (k, v) in theirs.as_object().into_iter().flatten() {
                let same_kind = ours
                    .get(k)
                    .is_some_and(|o| std::mem::discriminant(o) == std::mem::discriminant(v));
                if !same_kind && !v.is_null() {
                    problems.push(format!("{kind}.{k} differs in type"));
                }
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
    Ok(())
}
