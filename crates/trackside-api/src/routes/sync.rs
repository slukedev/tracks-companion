//! `GET /api/v1/sync`: snapshot pages, then deltas (api.md section 10).

use axum::Router;
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use http::StatusCode;
use serde::Deserialize;
use trackside_auth::Scopes;
use trackside_auth::extract::Authed;
use trackside_core::LibraryId;
use trackside_core::sync::{self, DeltaPage, SnapshotItems, SnapshotPage, SyncCursor, SyncPage};

use crate::error::{ApiError, ApiResult};
use crate::extract::{Query, json_with};
use crate::wire::sync::{Deletes, Rating, Records, SnapshotInfo, Star, SyncMode, SyncPage as Wire};
use crate::{ApiState, map};

const LIBRARY: u8 = Scopes::LIBRARY.bits();

pub(super) fn routes() -> Router<ApiState> {
    Router::new().route("/sync", get(sync_page))
}

#[derive(Debug, Deserialize)]
struct SyncQuery {
    cursor: Option<String>,
    limit: Option<u32>,
}

const DEFAULT_LIMIT: u32 = 2000;

async fn sync_page(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Query(query): Query<SyncQuery>,
) -> ApiResult<Response> {
    let cursor = query
        .cursor
        .as_deref()
        .filter(|c| !c.is_empty())
        .map(SyncCursor::decode)
        .transpose()
        .map_err(ApiError::from)?;
    let max = state.config.sync.max_page.max(1);
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, max);
    let access = principal.access.clone();
    let page = state
        .db
        .read(move |c| Ok(sync::page(c, &access, cursor.as_ref(), limit)))
        .await
        .map_err(ApiError::from)?
        .map_err(ApiError::from)?;
    let body = page_body(&page, |library| super::available(&state, library));
    Ok(json_with(StatusCode::OK, &body))
}

/// The wire page for a core page. Pure, for the fixture test; `available` is the scanner's
/// view of each library.
pub fn page_body(page: &SyncPage, available: impl Fn(&LibraryId) -> bool) -> Wire {
    match page {
        SyncPage::Snapshot(snapshot) => snapshot_body(snapshot, &available),
        SyncPage::Delta(delta) => delta_body(delta, &available),
    }
}

fn snapshot_body(snapshot: &SnapshotPage, available: &impl Fn(&LibraryId) -> bool) -> Wire {
    let mut records = Records {
        libraries: snapshot
            .libraries
            .iter()
            .map(|l| map::library(l, available(&l.id)))
            .collect(),
        ..Records::default()
    };
    let kind = match &snapshot.items {
        SnapshotItems::Libraries(items) => {
            records.libraries = items
                .iter()
                .map(|l| map::library(l, available(&l.id)))
                .collect();
            "libraries"
        }
        SnapshotItems::Artists(items) => {
            records.artists = items.iter().map(map::artist).collect();
            "artists"
        }
        SnapshotItems::Albums(items) => {
            records.albums = items.iter().map(map::album).collect();
            "albums"
        }
        SnapshotItems::Songs(items) => {
            records.songs = items.iter().map(map::song).collect();
            "songs"
        }
        SnapshotItems::Playlists(items) => {
            records.playlists = items.iter().map(map::playlist_with_entries).collect();
            "playlists"
        }
        SnapshotItems::Stars(items) => {
            records.stars = items.iter().map(star).collect();
            "stars"
        }
        SnapshotItems::Ratings(items) => {
            records.ratings = items.iter().map(rating).collect();
            "ratings"
        }
        SnapshotItems::PlayStats(items) => {
            records.play_stats = items.iter().map(map::play_stat).collect();
            "play_stats"
        }
        SnapshotItems::PlayQueue(queue) => {
            records.play_queue = queue.as_ref().map(map::play_queue);
            "play_queue"
        }
    };
    let next = snapshot.next_cursor.encode();
    Wire {
        mode: SyncMode::Snapshot,
        snapshot: Some(SnapshotInfo {
            seq: snapshot.seq.0,
            kind: kind.to_owned(),
            next: (!snapshot.complete).then(|| next.clone()),
        }),
        complete: snapshot.complete.then_some(true),
        records,
        upserts: None,
        deletes: None,
        next_cursor: next,
        has_more: None,
    }
}

fn delta_body(delta: &DeltaPage, available: &impl Fn(&LibraryId) -> bool) -> Wire {
    let up = &delta.upserts;
    let upserts = Records {
        songs: up.songs.iter().map(map::song).collect(),
        albums: up.albums.iter().map(map::album).collect(),
        artists: up.artists.iter().map(map::artist).collect(),
        libraries: up
            .libraries
            .iter()
            .map(|l| map::library(l, available(&l.id)))
            .collect(),
        playlists: up
            .playlists
            .iter()
            .map(map::playlist_with_entries)
            .collect(),
        stars: up.stars.iter().map(star).collect(),
        ratings: up.ratings.iter().map(rating).collect(),
        play_stats: up.play_stats.iter().map(map::play_stat).collect(),
        play_queue: up.play_queue.as_ref().map(map::play_queue),
    };
    let gone = &delta.deletes;
    let deletes = Deletes {
        songs: gone.songs.iter().map(ToString::to_string).collect(),
        albums: gone.albums.iter().map(ToString::to_string).collect(),
        artists: gone.artists.iter().map(ToString::to_string).collect(),
        libraries: gone.libraries.iter().map(ToString::to_string).collect(),
        playlists: gone.playlists.iter().map(ToString::to_string).collect(),
        stars: gone.stars.iter().map(map::item_ref).collect(),
        ratings: gone.ratings.iter().map(map::item_ref).collect(),
    };
    Wire {
        mode: SyncMode::Delta,
        snapshot: None,
        complete: None,
        records: Records::default(),
        upserts: Some(upserts),
        deletes: Some(deletes),
        next_cursor: delta.next_cursor.encode(),
        has_more: Some(delta.has_more),
    }
}

fn star(star: &trackside_core::repo::stars::Star) -> Star {
    Star {
        kind: star.item.kind().as_str().to_owned(),
        id: star.item.pid().to_owned(),
        starred_at: star.starred_at.into(),
    }
}

fn rating(rating: &trackside_core::repo::ratings::Rating) -> Rating {
    Rating {
        kind: rating.item.kind().as_str().to_owned(),
        id: rating.item.pid().to_owned(),
        rating: rating.rating,
    }
}
