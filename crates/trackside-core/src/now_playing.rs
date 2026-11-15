//! What each user is playing right now, shared by the native `POST /api/v1/now-playing` and
//! Subsonic's `scrobble?submission=false` and `getNowPlaying`. In memory only: it describes
//! the present and is not worth a write.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::ids::{DeviceKey, SongId, UserKey};
use crate::time::Millis;

/// How long an entry outlives the song's expected end.
pub const GRACE: Duration = Duration::from_secs(30 * 60);

/// Whether the player is going.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayState {
    Playing,
    Paused,
}

/// One user's player on one device (or one client without a device).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NowPlaying {
    pub user: UserKey,
    pub username: String,
    pub device: Option<DeviceKey>,
    /// The client's name, such as `Tracks` or a Subsonic `c` parameter.
    pub client: String,
    pub song: SongId,
    pub position_ms: u64,
    pub state: PlayState,
    pub updated_at: Millis,
    pub expires_at: Millis,
}

/// A report from a player.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NowPlayingReport {
    pub user: UserKey,
    pub username: String,
    pub device: Option<DeviceKey>,
    pub client: String,
    pub song: SongId,
    pub position_ms: u64,
    pub state: PlayState,
    /// The song's length, to know when the entry goes stale.
    pub duration_ms: u32,
}

type Key = (UserKey, Option<DeviceKey>, String);

/// The shared registry. Cheap to clone; clones share entries.
#[derive(Clone, Debug, Default)]
pub struct NowPlayingRegistry {
    entries: Arc<Mutex<HashMap<Key, NowPlaying>>>,
}

impl NowPlayingRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Key, NowPlaying>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records what a player is doing. It expires [`GRACE`] after the song would end.
    pub fn report(&self, report: NowPlayingReport, now: Millis) {
        let left = u64::from(report.duration_ms).saturating_sub(report.position_ms);
        let expires_at = now
            .saturating_add(Duration::from_millis(left))
            .saturating_add(GRACE);
        let key = (report.user, report.device, report.client.clone());
        self.lock().insert(
            key,
            NowPlaying {
                user: report.user,
                username: report.username,
                device: report.device,
                client: report.client,
                song: report.song,
                position_ms: report.position_ms,
                state: report.state,
                updated_at: now,
                expires_at,
            },
        );
    }

    /// A player stopped.
    pub fn stop(&self, user: UserKey, device: Option<DeviceKey>, client: &str) {
        self.lock().remove(&(user, device, client.to_owned()));
    }

    /// Forgets every entry of a user, as when the user is deleted.
    pub fn forget_user(&self, user: UserKey) {
        self.lock().retain(|(u, _, _), _| *u != user);
    }

    /// Live entries, most recently updated first. Expired ones are dropped.
    pub fn list(&self, now: Millis) -> Vec<NowPlaying> {
        let mut entries = self.lock();
        entries.retain(|_, entry| entry.expires_at > now);
        let mut out: Vec<NowPlaying> = entries.values().cloned().collect();
        out.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then(a.username.cmp(&b.username))
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::AudioDigest;

    fn report(user: i64, position_ms: u64) -> NowPlayingReport {
        NowPlayingReport {
            user: UserKey(user),
            username: format!("user{user}"),
            device: None,
            client: String::from("Tracks"),
            song: SongId::derive(&AudioDigest([1; 32])),
            position_ms,
            state: PlayState::Playing,
            duration_ms: 60_000,
        }
    }

    #[test]
    fn entries_expire_after_the_song_would_end_plus_grace() {
        let registry = NowPlayingRegistry::new();
        registry.report(report(1, 0), Millis(0));
        let end = Millis(60_000).saturating_add(GRACE);
        assert_eq!(registry.list(Millis(end.0 - 1)).len(), 1);
        assert_eq!(registry.list(end).len(), 0);
    }

    #[test]
    fn a_new_report_replaces_the_same_player_and_stop_removes_it() {
        let registry = NowPlayingRegistry::new();
        registry.report(report(1, 0), Millis(0));
        registry.report(report(1, 10_000), Millis(10));
        registry.report(report(2, 0), Millis(5));
        let list = registry.list(Millis(20));
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].position_ms, 10_000);
        registry.stop(UserKey(1), None, "Tracks");
        assert_eq!(registry.list(Millis(20)).len(), 1);
    }
}
