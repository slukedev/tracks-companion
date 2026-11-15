//! In-memory state of the `/rest` layer: `reportPlayback` sessions (to count finished plays),
//! and continuation points for the legacy empty-`search3` paging. What is playing now lives in
//! core's [`NowPlayingRegistry`], shared with the native API.

use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasher, RandomState};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use trackside_auth::Auth;
use trackside_core::now_playing::NowPlayingRegistry;
use trackside_core::{LibraryKey, Millis, SongId, UserKey};

use crate::SubsonicState;

/// The router's state.
#[derive(Debug)]
pub struct Inner {
    pub state: SubsonicState,
    pub now_playing: NowPlayingRegistry,
    pub sessions: Sessions,
    pub cursors: SongCursors,
}

/// The router's state handle.
#[derive(Clone, Debug)]
pub struct Shared(pub Arc<Inner>);

impl axum::extract::FromRef<Shared> for Arc<Auth> {
    fn from_ref(shared: &Shared) -> Self {
        Arc::clone(&shared.0.state.auth)
    }
}

/// A random seed for `getAlbumList type=random`.
pub fn random_seed() -> u64 {
    RandomState::new().hash_one(std::time::SystemTime::now())
}

/// How long a now-playing entry outlives the song's expected end.
const LINGER: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayState {
    Starting,
    Playing,
    Paused,
    Stopped,
}

impl PlayState {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "starting" => Some(Self::Starting),
            "playing" => Some(Self::Playing),
            "paused" => Some(Self::Paused),
            "stopped" => Some(Self::Stopped),
            _ => None,
        }
    }
}

/// One user's player and what it plays.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub user: UserKey,
    pub username: String,
    pub player: String,
    pub song: SongId,
    pub duration_ms: u32,
    pub state: PlayState,
    pub position_ms: u64,
    pub rate: f64,
    pub started_at: Millis,
    pub updated_at: Millis,
    playing_since: Option<Millis>,
    played_ms: u64,
    /// Sessions from `reportPlayback` count a listen when they stop; `scrobble` sessions do not.
    counts_listen: bool,
}

impl Entry {
    fn expires_at(&self) -> Millis {
        self.started_at
            .saturating_add(Duration::from_millis(u64::from(self.duration_ms)))
            .saturating_add(LINGER)
    }

    fn settle(&mut self, now: Millis) {
        if let Some(since) = self.playing_since.take() {
            let elapsed = now.since(since).as_millis() as f64 * self.rate.max(0.0);
            self.played_ms += elapsed as u64;
        }
    }

    /// Last.fm's rule: longer than 30 s, and played for half its length or 4 minutes.
    fn qualifies(&self) -> bool {
        let duration = u64::from(self.duration_ms);
        duration > 30_000 && self.played_ms >= (duration / 2).min(240_000)
    }
}

/// A finished `reportPlayback` session that should be stored as a listen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finished {
    pub song: SongId,
    pub started_at: Millis,
    pub played_ms: u64,
}

/// A report from a player.
#[derive(Clone, Debug)]
pub struct Report {
    pub user: UserKey,
    pub username: String,
    pub player: String,
    pub song: SongId,
    pub duration_ms: u32,
    pub state: PlayState,
    pub position_ms: u64,
    pub rate: f64,
    pub ignore_scrobble: bool,
}

/// `reportPlayback` sessions per user and player, timing how long each song really played.
#[derive(Debug, Default)]
pub struct Sessions {
    entries: Mutex<Vec<Entry>>,
}

impl Sessions {
    fn entries(&self) -> std::sync::MutexGuard<'_, Vec<Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `reportPlayback`. Returns the session that just ended, when it earned a listen.
    pub fn report(&self, report: &Report, now: Millis) -> Option<Finished> {
        let mut entries = self.entries();
        entries.retain(|e| e.expires_at() > now);
        let existing = entries
            .iter()
            .position(|e| e.user == report.user && e.player == report.player);
        let mut finished = None;
        let same_session = existing
            .is_some_and(|i| entries[i].song == report.song && report.state != PlayState::Starting);
        if let Some(i) = existing
            && !same_session
        {
            let mut old = entries.remove(i);
            old.settle(now);
            if old.counts_listen && old.qualifies() {
                finished = Some(Finished {
                    song: old.song,
                    started_at: old.started_at,
                    played_ms: old.played_ms,
                });
            }
        }
        let index = if same_session {
            existing
        } else {
            entries.push(Entry {
                user: report.user,
                username: report.username.clone(),
                player: report.player.clone(),
                song: report.song.clone(),
                duration_ms: report.duration_ms,
                state: report.state,
                position_ms: report.position_ms,
                rate: report.rate,
                started_at: now,
                updated_at: now,
                playing_since: None,
                played_ms: 0,
                counts_listen: !report.ignore_scrobble,
            });
            Some(entries.len() - 1)
        };
        let Some(i) = index else { return finished };
        let entry = &mut entries[i];
        entry.settle(now);
        entry.state = report.state;
        entry.position_ms = report.position_ms;
        entry.rate = report.rate;
        entry.updated_at = now;
        entry.counts_listen &= !report.ignore_scrobble;
        match report.state {
            PlayState::Playing => entry.playing_since = Some(now),
            PlayState::Starting | PlayState::Paused => {}
            PlayState::Stopped => {
                let ended = entries.remove(i);
                if ended.counts_listen && ended.qualifies() {
                    finished = Some(Finished {
                        song: ended.song,
                        started_at: ended.started_at,
                        played_ms: ended.played_ms,
                    });
                }
            }
        }
        finished
    }

    #[cfg(test)]
    fn active(&self, now: Millis) -> usize {
        let mut entries = self.entries();
        entries.retain(|e| e.expires_at() > now);
        entries.len()
    }
}

/// Where a song listing continues: the caller, their sync epoch, the library filter and the
/// offset reached.
type CursorKey = (UserKey, i64, Option<LibraryKey>, usize);

/// Continuation points for `search3` with an empty query. Clients page it by offset, the
/// repository by internal id; remembering where each page ended keeps a full walk linear.
#[derive(Debug, Default)]
pub struct SongCursors {
    inner: Mutex<(HashMap<CursorKey, i64>, VecDeque<CursorKey>)>,
}

const MAX_CURSORS: usize = 1024;

impl SongCursors {
    pub fn get(&self, key: CursorKey) -> Option<i64> {
        let guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        guard.0.get(&key).copied()
    }

    pub fn put(&self, key: CursorKey, after: i64) {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let (map, order) = &mut *guard;
        if map.insert(key, after).is_none() {
            order.push_back(key);
        }
        while order.len() > MAX_CURSORS {
            if let Some(old) = order.pop_front() {
                map.remove(&old);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trackside_core::AudioDigest;

    const T0: Millis = Millis(1_794_648_600_000);

    fn report(state: PlayState, song: &SongId, position_ms: u64) -> Report {
        Report {
            user: UserKey(1),
            username: String::from("luke"),
            player: String::from("Feishin"),
            song: song.clone(),
            duration_ms: 200_000,
            state,
            position_ms,
            rate: 1.0,
            ignore_scrobble: false,
        }
    }

    fn at(secs: u64) -> Millis {
        T0.saturating_add(Duration::from_secs(secs))
    }

    #[test]
    fn a_session_played_long_enough_counts_once_when_stopped() {
        let np = Sessions::default();
        let song = SongId::derive(&AudioDigest([5; 32]));
        assert_eq!(
            np.report(&report(PlayState::Starting, &song, 0), at(0)),
            None
        );
        assert_eq!(
            np.report(&report(PlayState::Playing, &song, 0), at(2)),
            None
        );
        assert_eq!(np.active(at(3)), 1);
        assert_eq!(
            np.report(&report(PlayState::Paused, &song, 60_000), at(62)),
            None
        );
        assert_eq!(
            np.report(&report(PlayState::Playing, &song, 60_000), at(500)),
            None
        );
        let finished = np.report(&report(PlayState::Stopped, &song, 110_000), at(550));
        assert_eq!(
            finished,
            Some(Finished {
                song: song.clone(),
                started_at: at(0),
                played_ms: 110_000
            })
        );
        assert_eq!(np.active(at(551)), 0);
        assert_eq!(
            np.report(&report(PlayState::Stopped, &song, 0), at(552)),
            None
        );
    }

    #[test]
    fn a_skipped_song_does_not_count() {
        let np = Sessions::default();
        let song = SongId::derive(&AudioDigest([5; 32]));
        let next = SongId::derive(&AudioDigest([6; 32]));
        np.report(&report(PlayState::Playing, &song, 0), at(0));
        assert_eq!(
            np.report(&report(PlayState::Starting, &next, 0), at(20)),
            None
        );
        assert_eq!(np.active(at(21)), 1);
    }

    #[test]
    fn ignore_scrobble_only_updates_now_playing() {
        let np = Sessions::default();
        let song = SongId::derive(&AudioDigest([5; 32]));
        let mut quiet = report(PlayState::Playing, &song, 0);
        quiet.ignore_scrobble = true;
        np.report(&quiet, at(0));
        quiet.state = PlayState::Stopped;
        assert_eq!(np.report(&quiet, at(300)), None);
    }

    #[test]
    fn sessions_expire_thirty_minutes_after_the_expected_end() {
        let np = Sessions::default();
        let song = SongId::derive(&AudioDigest([5; 32]));
        np.report(&report(PlayState::Playing, &song, 0), at(0));
        assert_eq!(np.active(at(200 + 1799)), 1);
        assert_eq!(np.active(at(200 + 1801)), 0);
    }

    #[test]
    fn cursors_remember_where_a_page_ended_and_stay_bounded() {
        let cursors = SongCursors::default();
        cursors.put((UserKey(1), 0, None, 500), 812);
        assert_eq!(cursors.get((UserKey(1), 0, None, 500)), Some(812));
        assert_eq!(cursors.get((UserKey(2), 0, None, 500)), None);
        for offset in 0..MAX_CURSORS + 10 {
            cursors.put((UserKey(3), 0, None, offset), 1);
        }
        assert_eq!(cursors.get((UserKey(1), 0, None, 500)), None);
    }
}
