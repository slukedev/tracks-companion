//! Core records to wire types. Pure functions, shared by the handlers and the fixture test.

use std::collections::HashMap;
use std::hash::BuildHasher;

use trackside_auth::Scopes;
use trackside_core::control::{
    AcquisitionView, AdminAcquisitionView, CandidateView, ProviderHealthState, ProviderStatus,
};
use trackside_core::records::{
    Album, Artist, ItemRef, Library, Lyrics, LyricsKind, ScanRun, Song, UserAnnotations,
};
use trackside_core::repo::devices::Device;
use trackside_core::repo::keys::ApiKeyRow;
use trackside_core::repo::play_queue::PlayQueue;
use trackside_core::repo::playlists::Playlist;
use trackside_core::repo::users::User;
use trackside_core::sync::{PlayStat, PlaylistWithEntries};
use trackside_core::{Permissions, PublicId, Role};

use crate::wire;
use crate::wire::Time;

pub fn song(song: &Song) -> wire::Song {
    let mut mbz = wire::Mbz::new();
    for (key, value) in [
        ("recording", &song.mbz.recording),
        ("track", &song.mbz.track),
        ("release", &song.mbz.release),
    ] {
        if let Some(value) = value {
            mbz.insert(key.to_owned(), value.clone());
        }
    }
    wire::Song {
        id: song.id.to_string(),
        library_id: song.library.to_string(),
        title: song.title.clone(),
        artist: song.artist.clone(),
        artist_id: song.artist_id.as_ref().map(ToString::to_string),
        artists: song
            .credits
            .iter()
            .map(|c| wire::Credit {
                id: c.artist.to_string(),
                name: c.name.clone(),
                role: c.role.as_str().to_owned(),
            })
            .collect(),
        album: song.album.clone(),
        album_id: song.album_id.as_ref().map(ToString::to_string),
        album_artist: song.album_artist.clone(),
        track_number: song.track_number,
        disc_number: song.disc_number,
        disc_subtitle: song.disc_subtitle.clone(),
        year: song.year,
        release_date: song.release_date.clone(),
        genres: song.genres.clone(),
        duration_ms: song.duration_ms,
        format: wire::Format {
            container: song.format.container.as_str().to_owned(),
            codec: song.format.codec.as_str().to_owned(),
            content_type: song.format.content_type.to_owned(),
            lossless: song.format.lossless,
            bitrate_kbps: song.format.bitrate_kbps,
            sample_rate: song.format.sample_rate,
            bit_depth: song.format.bit_depth,
            channels: song.format.channels,
            size: song.format.size,
        },
        artwork_id: song.artwork.as_ref().map(ToString::to_string),
        song_key: song.song_key.clone(),
        replay_gain: song.replay_gain.map(|g| wire::ReplayGain {
            track_gain: g.track_gain,
            track_peak: g.track_peak,
            album_gain: g.album_gain,
            album_peak: g.album_peak,
        }),
        mbz,
        isrc: song.isrc.clone(),
        bpm: song.bpm,
        explicit: song.explicit.map(|e| e.as_str().to_owned()),
        has_lyrics: song.has_lyrics,
        added_at: song.added_at.into(),
        updated_at: song.updated_at.into(),
        missing: song.missing,
        me: None,
    }
}

pub fn me(annotations: &UserAnnotations) -> wire::Me {
    wire::Me {
        starred_at: annotations.starred_at.map(Time),
        rating: annotations.rating,
        play_count: annotations.play_count,
        last_played_at: annotations.last_played_at.map(Time),
    }
}

/// Songs for a browse response, each with the caller's `me` when they have any.
pub fn songs_with_me<S: BuildHasher>(
    songs: &[Song],
    annotations: &HashMap<ItemRef, UserAnnotations, S>,
) -> Vec<wire::Song> {
    songs
        .iter()
        .map(|s| {
            let mut out = song(s);
            out.me = annotations.get(&ItemRef::Song(s.id.clone())).map(me);
            out
        })
        .collect()
}

pub fn album(album: &Album) -> wire::Album {
    let mut mbz = wire::Mbz::new();
    if let Some(release) = &album.mbz.release {
        mbz.insert(String::from("release"), release.clone());
    }
    if let Some(group) = &album.mbz.release_group {
        mbz.insert(String::from("release_group"), group.clone());
    }
    wire::Album {
        id: album.id.to_string(),
        library_id: album.library.to_string(),
        name: album.name.clone(),
        album_artist: album.album_artist.clone(),
        artist_id: album.artist_id.as_ref().map(ToString::to_string),
        year: album.year,
        release_date: album.release_date.clone(),
        genres: album.genres.clone(),
        is_compilation: album.is_compilation,
        release_type: album.release_type.clone(),
        song_count: album.song_count,
        disc_count: album.disc_count,
        duration_ms: album.duration_ms,
        artwork_id: album.artwork.as_ref().map(ToString::to_string),
        album_key: album.album_key.clone(),
        mbz,
        added_at: album.added_at.into(),
        updated_at: album.updated_at.into(),
        me: None,
    }
}

pub fn albums_with_me<S: BuildHasher>(
    albums: &[Album],
    annotations: &HashMap<ItemRef, UserAnnotations, S>,
) -> Vec<wire::Album> {
    albums
        .iter()
        .map(|a| {
            let mut out = album(a);
            out.me = annotations.get(&ItemRef::Album(a.id.clone())).map(me);
            out
        })
        .collect()
}

pub fn artist(artist: &Artist) -> wire::Artist {
    let mut mbz = wire::Mbz::new();
    if let Some(id) = &artist.mbz_artist {
        mbz.insert(String::from("artist"), id.clone());
    }
    wire::Artist {
        id: artist.id.to_string(),
        name: artist.name.clone(),
        identity: artist.identity.clone(),
        album_count: artist.album_count,
        song_count: artist.song_count,
        artwork_id: artist.artwork.as_ref().map(ToString::to_string),
        mbz,
        me: None,
    }
}

/// A library, with availability from the scanner's monitor as well as the stored flag.
pub fn library(library: &Library, available_now: bool) -> wire::Library {
    wire::Library {
        id: library.id.to_string(),
        name: library.name.clone(),
        state: library_state(library.available && available_now),
        song_count: library.song_count,
    }
}

pub fn library_state(available: bool) -> wire::LibraryState {
    if available {
        wire::LibraryState::Ok
    } else {
        wire::LibraryState::Unavailable
    }
}

pub fn playlist(playlist: &Playlist) -> wire::Playlist {
    wire::Playlist {
        id: playlist.id.to_string(),
        name: playlist.name.clone(),
        comment: playlist.comment.clone().unwrap_or_default(),
        owner: wire::Owner {
            id: playlist.owner_id.to_string(),
            display_name: playlist.owner_display_name.clone(),
        },
        is_public: playlist.is_public,
        song_count: playlist.song_count,
        duration_ms: playlist.duration_ms,
        revision: playlist.revision,
        created_at: playlist.created_at.into(),
        updated_at: playlist.updated_at.into(),
    }
}

pub fn playlist_with_entries(entry: &PlaylistWithEntries) -> wire::sync::PlaylistWithEntries {
    wire::sync::PlaylistWithEntries {
        playlist: playlist(&entry.playlist),
        entries: entry.entries.iter().map(ToString::to_string).collect(),
    }
}

pub fn item_ref(item: &ItemRef) -> wire::sync::ItemRef {
    wire::sync::ItemRef {
        kind: item.kind().as_str().to_owned(),
        id: item.pid().to_owned(),
    }
}

pub fn play_stat(stat: &PlayStat) -> wire::sync::PlayStat {
    wire::sync::PlayStat {
        id: stat.song.to_string(),
        play_count: stat.play_count,
        last_played_at: stat.last_played_at.into(),
    }
}

pub fn play_queue(queue: &PlayQueue) -> wire::social::PlayQueue {
    wire::social::PlayQueue {
        entries: queue.entries.iter().map(ToString::to_string).collect(),
        current: queue.current,
        position_ms: queue.position_ms,
        changed_by: queue.changed_by.clone(),
        device_id: queue.device.as_ref().map(ToString::to_string),
        updated_at: queue.updated_at.into(),
    }
}

/// What a user may do. Admins have every permission regardless of the stored flags.
pub fn permissions(role: Role, permissions: &Permissions) -> wire::auth::Permissions {
    let admin = role == Role::Admin;
    wire::auth::Permissions {
        can_download: admin || permissions.can_download,
        can_upload: admin || permissions.can_upload,
        can_request: admin || permissions.can_request,
        legacy_subsonic: admin || permissions.legacy_subsonic,
        can_use_speakers: admin || permissions.can_use_speakers,
        can_play_services: admin || permissions.can_play_services,
        max_bitrate_kbps: if admin {
            None
        } else {
            permissions.max_bitrate_kbps
        },
    }
}

pub fn user(user: &User) -> wire::auth::User {
    wire::auth::User {
        id: user.id.to_string(),
        username: user.username.clone(),
        display_name: user.display_name.clone(),
        role: user.role.as_str().to_owned(),
        permissions: permissions(user.role, &user.permissions),
        recent_auth_until: None,
    }
}

pub fn user_ref(user: &User) -> wire::admin::UserRef {
    wire::admin::UserRef {
        id: user.id.to_string(),
        username: user.username.clone(),
        display_name: user.display_name.clone(),
    }
}

pub fn device_record(device: &Device, current: bool) -> wire::auth::DeviceRecord {
    wire::auth::DeviceRecord {
        id: device.id.to_string(),
        name: device.name.clone(),
        platform: device.platform.clone(),
        model: device.model.clone(),
        app_name: device.app_name.clone(),
        app_version: device.app_version.clone(),
        created_at: device.created_at.into(),
        last_seen_at: device.last_seen_at.into(),
        last_ip: device.last_ip.clone(),
        current,
    }
}

pub fn scope_names(stored: &str) -> Vec<String> {
    Scopes::parse(stored)
        .unwrap_or(Scopes::NONE)
        .names()
        .into_iter()
        .map(str::to_owned)
        .collect()
}

pub fn key_record(key: &ApiKeyRow) -> wire::auth::KeyRecord {
    wire::auth::KeyRecord {
        id: key.id.to_string(),
        kind: key.kind.as_str().to_owned(),
        name: key.name.clone(),
        prefix: key.prefix.clone(),
        scopes: scope_names(&key.scopes),
        created_at: key.created_at.into(),
        last_used_at: key.last_used_at.map(Time),
        last_used_ip: key.last_used_ip.clone(),
        secret: None,
        username: None,
    }
}

/// Lyrics in api.md's shape: LRC text split into timed lines for synced lyrics.
pub fn lyrics(all: &[Lyrics]) -> wire::library::Lyrics {
    wire::library::Lyrics {
        lyrics: all
            .iter()
            .map(|l| match l.kind {
                LyricsKind::Synced => {
                    let (offset, lines) = parse_lrc(&l.content);
                    wire::library::LyricsEntry {
                        kind: l.kind.as_str().to_owned(),
                        lang: l.lang.clone(),
                        source: l.source.as_str().to_owned(),
                        offset_ms: Some(offset),
                        lines,
                    }
                }
                LyricsKind::Plain => wire::library::LyricsEntry {
                    kind: l.kind.as_str().to_owned(),
                    lang: l.lang.clone(),
                    source: l.source.as_str().to_owned(),
                    offset_ms: None,
                    lines: l
                        .content
                        .lines()
                        .map(|text| wire::library::LyricsLine {
                            start_ms: None,
                            text: text.to_owned(),
                        })
                        .collect(),
                },
            })
            .collect(),
    }
}

/// `[mm:ss.xx]text` lines, several stamps per line allowed, `[offset:+/-ms]` honoured. Other
/// tags and untimed lines are skipped.
pub fn parse_lrc(text: &str) -> (i64, Vec<wire::library::LyricsLine>) {
    let mut offset = 0_i64;
    let mut lines = Vec::new();
    for raw in text.lines() {
        let mut rest = raw.trim();
        let mut stamps = Vec::new();
        while let Some(stripped) = rest.strip_prefix('[') {
            let Some(end) = stripped.find(']') else { break };
            let tag = &stripped[..end];
            if let Some(value) = tag.strip_prefix("offset:") {
                offset = value.trim().parse().unwrap_or(0);
            } else if let Some(ms) = lrc_stamp(tag) {
                stamps.push(ms);
            }
            rest = &stripped[end + 1..];
        }
        for start in stamps {
            lines.push(wire::library::LyricsLine {
                start_ms: Some(start),
                text: rest.trim().to_owned(),
            });
        }
    }
    lines.sort_by_key(|l| l.start_ms);
    (offset, lines)
}

fn lrc_stamp(tag: &str) -> Option<u64> {
    let (minutes, seconds) = tag.split_once(':')?;
    let minutes: u64 = minutes.trim().parse().ok()?;
    let (whole, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    let whole: u64 = whole.trim().parse().ok()?;
    if whole >= 60 {
        return None;
    }
    let fraction_ms = match fraction.len() {
        0 => 0,
        1 => fraction.parse::<u64>().ok()? * 100,
        2 => fraction.parse::<u64>().ok()? * 10,
        _ => fraction.get(..3)?.parse::<u64>().ok()?,
    };
    minutes
        .checked_mul(60_000)?
        .checked_add(whole * 1000 + fraction_ms)
}

/// A candidate, with the cover looked up for a suggested song of the same `song_key`.
pub fn candidate(
    candidate: &CandidateView,
    artwork: Option<&trackside_core::ExternalArtId>,
) -> wire::acquisition::Candidate {
    wire::acquisition::Candidate {
        token: candidate.token.to_string(),
        title: candidate.title.clone(),
        artist: candidate.artist.clone(),
        album: candidate.album.clone(),
        duration_ms: candidate.duration_ms,
        quality: candidate.quality.as_str().to_owned(),
        format: candidate_format(&candidate.format),
        size: candidate.size,
        instant: candidate.instant,
        r#match: if candidate.exact { "exact" } else { "close" }.to_owned(),
        artwork_id: artwork.map(ToString::to_string),
    }
}

/// An acquisition. Instant-play URLs get the base path in front.
pub fn acquisition(view: &AcquisitionView, base_path: &str) -> wire::acquisition::Acquisition {
    wire::acquisition::Acquisition {
        id: view.id.to_string(),
        state: view.state.as_str().to_owned(),
        title: view.title.clone(),
        artist: view.artist.clone(),
        album: view.album.clone(),
        progress: view
            .progress
            .map(|(bytes, total)| wire::acquisition::Progress { bytes, total }),
        quality: view.quality.map(|q| q.as_str().to_owned()),
        song_id: view.song.as_ref().map(ToString::to_string),
        existing: view.existing,
        failure: view.failure.map(|f| f.as_str().to_owned()),
        stream: view.stream.as_ref().map(|s| wire::acquisition::Stream {
            hls: prefixed(base_path, &s.hls),
            progressive: prefixed(base_path, &s.progressive),
        }),
        created_at: view.created_at.into(),
        updated_at: view.updated_at.into(),
    }
}

/// `path` with the base path in front, unless it already has it.
pub fn prefixed(base_path: &str, path: &str) -> String {
    if base_path.is_empty() || path.starts_with(&format!("{base_path}/")) {
        path.to_owned()
    } else {
        format!("{base_path}{path}")
    }
}

pub fn provider_health(state: ProviderHealthState) -> &'static str {
    match state {
        ProviderHealthState::Ok => "ok",
        ProviderHealthState::Degraded => "degraded",
        ProviderHealthState::Cooldown => "cooldown",
        ProviderHealthState::Outdated => "outdated",
        ProviderHealthState::Unavailable => "unavailable",
        ProviderHealthState::NotSetUp => "setup",
        ProviderHealthState::Disabled => "disabled",
    }
}

pub fn fetched_usage(usage: trackside_core::control::FetchedUsage) -> wire::admin::FetchedUsage {
    wire::admin::FetchedUsage {
        songs: usage.songs,
        bytes: usage.bytes,
        fetching: usage.fetching,
        keep: usage.keep,
        max_bytes: usage.max_bytes,
    }
}

pub fn provider(status: &ProviderStatus) -> wire::admin::ProviderStatus {
    wire::admin::ProviderStatus {
        name: status.name.clone(),
        enabled: status.enabled,
        configured: status.configured,
        health: provider_health(status.health).to_owned(),
        detail: status.detail.clone(),
        cooldown_until: status.cooldown_until.map(Time),
        version: status.version.clone(),
        latest_known: status.latest_known.clone(),
        settings: status.settings.clone(),
        stats: wire::admin::ProviderStats {
            searches_today: status.stats.searches_today,
            fetches_today: status.stats.fetches_today,
            failures_today: status.stats.failures_today,
        },
    }
}

fn source_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn candidate_format(
    format: &trackside_core::control::CandidateFormat,
) -> wire::acquisition::CandidateFormat {
    wire::acquisition::CandidateFormat {
        container: format.container.map(|c| c.as_str().to_owned()),
        codec: format.codec.map(|c| c.as_str().to_owned()),
        bitrate_kbps: format.bitrate_kbps,
        sample_rate: format.sample_rate,
        bit_depth: format.bit_depth,
    }
}

pub fn admin_acquisition(
    view: &AdminAcquisitionView,
    user: Option<wire::admin::UserRef>,
    base_path: &str,
) -> wire::admin::AdminAcquisition {
    wire::admin::AdminAcquisition {
        acquisition: acquisition(&view.view, base_path),
        user,
        provider: view.provider.clone(),
        candidate: view.chosen.as_ref().map(|c| wire::admin::AdminCandidate {
            token: c.token.to_string(),
            provider: c.provider.clone(),
            source: source_text(&c.provider_ref),
            title: c.title.clone(),
            artist: c.artist.clone(),
            score: c.score,
            quality: Some(c.quality.as_str().to_owned()),
            format: Some(candidate_format(&c.format)),
            size: c.size,
        }),
        attempts: view
            .candidates
            .iter()
            .filter(|c| c.tried_at.is_some())
            .map(|c| wire::admin::Attempt {
                provider: c.provider.clone(),
                source: source_text(&c.provider_ref),
                at: c.tried_at.map(Time),
                error: c.failure.clone(),
            })
            .collect(),
        attempt_count: view.attempts,
        failure_detail: view.failure_detail.clone(),
    }
}

pub fn scan_run(run: &ScanRun) -> wire::admin::ScanRun {
    wire::admin::ScanRun {
        id: run.id.to_string(),
        library_id: run.library.to_string(),
        kind: run.kind.as_str().to_owned(),
        trigger: run.trigger.as_str().to_owned(),
        state: run.state.as_str().to_owned(),
        started_at: run.started_at.into(),
        finished_at: run.finished_at.map(Time),
        folders_seen: run.counts.folders_seen,
        files_seen: run.counts.files_seen,
        added: run.counts.added,
        updated: run.counts.updated,
        moved: run.counts.moved,
        missing: run.counts.missing,
        errors: run.counts.errors,
        message: run.message.clone(),
    }
}

/// The item a path names, for stars and ratings.
pub fn parse_item(kind: &str, id: &str) -> Option<ItemRef> {
    match kind {
        "song" => trackside_core::SongId::parse(id).ok().map(ItemRef::Song),
        "album" => trackside_core::AlbumId::parse(id).ok().map(ItemRef::Album),
        "artist" => trackside_core::ArtistId::parse(id)
            .ok()
            .map(ItemRef::Artist),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lrc_lines_are_timed_and_sorted() {
        let (offset, lines) = parse_lrc(
            "[ar:Beyoncé]\n[offset:-250]\n[00:12.34]Remember\n[01:02][00:05.5]Twice\nplain",
        );
        assert_eq!(offset, -250);
        let starts: Vec<_> = lines.iter().map(|l| l.start_ms).collect();
        assert_eq!(starts, vec![Some(5_500), Some(12_340), Some(62_000)]);
        assert_eq!(lines[1].text, "Remember");
    }

    #[test]
    fn bad_lrc_stamps_are_skipped() {
        assert_eq!(lrc_stamp("ab:cd"), None);
        assert_eq!(lrc_stamp("00:75"), None);
        assert_eq!(lrc_stamp("18446744073709551615:00"), None);
        assert_eq!(lrc_stamp("02:03.456"), Some(123_456));
    }

    #[test]
    fn prefixed_adds_the_base_path_once() {
        assert_eq!(prefixed("", "/api/v1/x"), "/api/v1/x");
        assert_eq!(prefixed("/music", "/api/v1/x"), "/music/api/v1/x");
        assert_eq!(prefixed("/music", "/music/api/v1/x"), "/music/api/v1/x");
    }

    #[test]
    fn admins_have_every_permission() {
        let none = Permissions {
            can_download: false,
            can_upload: false,
            can_request: false,
            legacy_subsonic: false,
            max_bitrate_kbps: Some(128),
            can_use_speakers: false,
            can_play_services: false,
        };
        let admin = permissions(Role::Admin, &none);
        assert!(admin.can_download && admin.can_upload && admin.can_request);
        assert_eq!(admin.max_bitrate_kbps, None);
        let member = permissions(Role::Member, &none);
        assert!(!member.can_download);
        assert_eq!(member.max_bitrate_kbps, Some(128));
    }

    #[test]
    fn items_parse_by_kind() {
        let song = trackside_core::SongId::derive(&trackside_core::AudioDigest([3; 32]));
        assert_eq!(
            parse_item("song", song.as_str()),
            Some(ItemRef::Song(song.clone()))
        );
        assert_eq!(parse_item("album", song.as_str()), None);
        assert_eq!(parse_item("playlist", song.as_str()), None);
    }
}
