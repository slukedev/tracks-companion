//! `star`, `unstar`, `setRating`, `scrobble` and `reportPlayback`.
//!
//! A star is a favourite and nothing else: it never starts an acquisition. Plays count only
//! through `scrobble submission=true` and finished `reportPlayback` sessions, once per play.

use rusqlite::Connection;
use trackside_auth::Scopes;
use trackside_core::now_playing::{NowPlayingReport, PlayState as NowState};
use trackside_core::records::{ItemRef, Song};
use trackside_core::repo::listens::{ListenSource, NewListen};
use trackside_core::{Access, AlbumId, ArtistId, DeviceKey, Millis, PublicId, SongId, repo};

use crate::error::ApiError;
use crate::ids::{self, AnyId};
use crate::response::Body;
use crate::state::{PlayState, Report};
use crate::{Call, Reply};

fn exists(c: &Connection, a: &Access, item: &ItemRef) -> Result<bool, ApiError> {
    Ok(match item {
        ItemRef::Song(id) => repo::songs::get(c, a, id)?.is_some(),
        ItemRef::Album(id) => repo::albums::get(c, a, id)?.is_some(),
        ItemRef::Artist(id) => repo::artists::get(c, a, id)?.is_some(),
    })
}

/// The items named by `id` (any kind), `albumId` and `artistId`.
fn items(call: &Call<'_>) -> Result<Vec<ItemRef>, ApiError> {
    let params = call.params;
    let mut items = Vec::new();
    for raw in params.all("id") {
        let item = AnyId::parse(raw)
            .and_then(|id| id.item())
            .ok_or_else(|| ApiError::not_found("Item"))?;
        items.push(item);
    }
    for raw in params.all("albumId") {
        items.push(ItemRef::Album(ids::parse::<AlbumId>(raw, "Album")?));
    }
    for raw in params.all("artistId") {
        items.push(ItemRef::Artist(ids::parse::<ArtistId>(raw, "Artist")?));
    }
    if items.is_empty() {
        return Err(ApiError::missing("id"));
    }
    Ok(items)
}

pub async fn star(call: &Call<'_>, on: bool) -> Result<Reply, ApiError> {
    call.require(Scopes::WRITE)?;
    let items = items(call)?;
    let user = call.principal.access.user;
    let now = call.now();
    call.write(move |tx, a| {
        for item in &items {
            if !exists(tx.conn(), a, item)? {
                return Err(ApiError::not_found("Item"));
            }
            if on {
                repo::stars::set(tx, user, item, now)?;
            } else {
                repo::stars::clear(tx, user, item)?;
            }
        }
        Ok(())
    })
    .await?;
    Ok(Body::Empty.into())
}

pub async fn set_rating(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::WRITE)?;
    let item = AnyId::parse(call.params.required("id")?)
        .and_then(|id| id.item())
        .ok_or_else(|| ApiError::not_found("Item"))?;
    let rating: u8 = call
        .params
        .int("rating")?
        .ok_or_else(|| ApiError::missing("rating"))?;
    if rating > 5 {
        return Err(ApiError::invalid("rating"));
    }
    let user = call.principal.access.user;
    call.write(move |tx, a| {
        if !exists(tx.conn(), a, &item)? {
            return Err(ApiError::not_found("Item"));
        }
        if rating == 0 {
            repo::ratings::clear(tx, user, &item)?;
        } else {
            repo::ratings::set(tx, user, &item, rating)?;
        }
        Ok(())
    })
    .await?;
    Ok(Body::Empty.into())
}

/// Subsonic's `time` is milliseconds; a few clients send seconds, which would land in 1970.
pub fn scrobble_time(raw: Option<i64>, now: Millis) -> Millis {
    match raw {
        Some(t) if t > 0 && t < 100_000_000_000 => Millis(t * 1000),
        Some(t) if t > 0 => Millis(t),
        _ => now,
    }
}

fn listen(
    song: &Song,
    client_listen_id: String,
    started_at: Millis,
    played_ms: Option<u32>,
    source: ListenSource,
) -> NewListen {
    NewListen {
        client_listen_id,
        song: Some(song.id.clone()),
        title: song.title.clone(),
        artist: song.artist.clone(),
        album: song.album.clone(),
        started_at,
        played_ms,
        duration_ms: Some(song.duration_ms),
        source,
    }
}

fn device_key(call: &Call<'_>) -> Option<DeviceKey> {
    call.principal.device.as_ref().map(|(key, _)| *key)
}

/// Fetches songs in request order; any unknown id is error 70.
async fn songs_in_order(call: &Call<'_>, ids: Vec<SongId>) -> Result<Vec<Song>, ApiError> {
    call.read(move |c, a| {
        let found = repo::songs::get_many(c, a, &ids)?;
        ids.iter()
            .map(|id| {
                found
                    .iter()
                    .find(|s| s.id == *id)
                    .cloned()
                    .ok_or_else(|| ApiError::not_found("Song"))
            })
            .collect()
    })
    .await
}

pub async fn scrobble(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::WRITE)?;
    let params = call.params;
    let ids: Vec<SongId> = params
        .all("id")
        .into_iter()
        .map(|raw| ids::parse(raw, "Song"))
        .collect::<Result<_, _>>()?;
    if ids.is_empty() {
        return Err(ApiError::missing("id"));
    }
    let times: Vec<i64> = params.ints("time")?;
    let submission = params.bool("submission")?.unwrap_or(true);
    let songs = songs_in_order(call, ids).await?;
    let now = call.now();
    let principal = &call.principal;
    if !submission {
        if let Some(song) = songs.last() {
            call.inner.now_playing.report(
                NowPlayingReport {
                    user: principal.access.user,
                    username: principal.username.clone(),
                    device: device_key(call),
                    client: call.client(),
                    song: song.id.clone(),
                    position_ms: 0,
                    state: NowState::Playing,
                    duration_ms: song.duration_ms,
                },
                now,
            );
        }
        return Ok(Body::Empty.into());
    }
    let listens: Vec<NewListen> = songs
        .iter()
        .enumerate()
        .map(|(i, song)| {
            let at = scrobble_time(times.get(i).copied(), now);
            let id = format!("sub:{}:{}:{}", principal.user, song.id.as_str(), at.0);
            listen(song, id, at, None, ListenSource::Subsonic)
        })
        .collect();
    let user = principal.access.user;
    let device = device_key(call);
    call.write(move |tx, _| Ok(repo::listens::submit(tx, user, device, &listens)?))
        .await?;
    Ok(Body::Empty.into())
}

pub async fn report_playback(call: &Call<'_>) -> Result<Reply, ApiError> {
    call.require(Scopes::WRITE)?;
    let params = call.params;
    let media_type = params.required("mediaType")?;
    if media_type != "song" {
        return Err(ApiError::not_supported("Podcast playback reports"));
    }
    let id: SongId = ids::parse(params.required("mediaId")?, "Song")?;
    let position_ms: u64 = params
        .int("positionMs")?
        .ok_or_else(|| ApiError::missing("positionMs"))?;
    let state =
        PlayState::parse(params.required("state")?).ok_or_else(|| ApiError::invalid("state"))?;
    let rate: f64 = params.int("playbackRate")?.unwrap_or(1.0);
    let ignore_scrobble = params.bool("ignoreScrobble")?.unwrap_or(false);
    let songs = songs_in_order(call, vec![id]).await?;
    let Some(song) = songs.into_iter().next() else {
        return Err(ApiError::not_found("Song"));
    };
    let principal = &call.principal;
    let report = Report {
        user: principal.access.user,
        username: principal.username.clone(),
        player: call.client(),
        song: song.id.clone(),
        duration_ms: song.duration_ms,
        state,
        position_ms,
        rate,
        ignore_scrobble,
    };
    let now = call.now();
    let shown = match state {
        PlayState::Starting | PlayState::Playing => Some(NowState::Playing),
        PlayState::Paused => Some(NowState::Paused),
        PlayState::Stopped => None,
    };
    match shown {
        Some(shown) => call.inner.now_playing.report(
            NowPlayingReport {
                user: principal.access.user,
                username: principal.username.clone(),
                device: device_key(call),
                client: report.player.clone(),
                song: song.id.clone(),
                position_ms,
                state: shown,
                duration_ms: song.duration_ms,
            },
            now,
        ),
        None => {
            call.inner
                .now_playing
                .stop(principal.access.user, device_key(call), &report.player);
        }
    }
    if let Some(finished) = call.inner.sessions.report(&report, now) {
        let id = format!(
            "rep:{}:{}:{}",
            principal.user,
            finished.song.as_str(),
            finished.started_at.0
        );
        let played = u32::try_from(finished.played_ms).ok();
        let item = listen(&song, id, finished.started_at, played, ListenSource::Report);
        let user = principal.access.user;
        let device = device_key(call);
        call.write(move |tx, _| Ok(repo::listens::submit(tx, user, device, &[item])?))
            .await?;
    }
    Ok(Body::Empty.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrobble_times_accept_milliseconds_and_seconds() {
        let now = Millis(1_794_648_600_000);
        assert_eq!(
            scrobble_time(Some(1_794_600_000_123), now),
            Millis(1_794_600_000_123)
        );
        assert_eq!(
            scrobble_time(Some(1_794_600_000), now),
            Millis(1_794_600_000_000)
        );
        assert_eq!(scrobble_time(None, now), now);
        assert_eq!(scrobble_time(Some(-5), now), now);
    }
}
