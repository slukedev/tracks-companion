//! The kinds of record Trackside Cloud keeps, and which merge rule each follows.

/// A kind of synced record. The name is its path segment under `/api/v1/cloud` and the
/// `entity` of the `cloud` event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Entity {
    Plays,
    Sessions,
    Stations,
    ForgottenSongs,
    BlockedArtists,
    ExcludedStations,
    Playlists,
    PlaylistEntries,
    Likes,
    Settings,
}

/// How two copies of a record are reconciled (ADR 0012).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// Core fields immutable, enrichments first non-null, `artworkURL` newest.
    Play,
    /// Only the owning device or a successor writes; otherwise the last write wins.
    Owned,
    /// Upserted by `stationKey(name)`: first seen takes the minimum, last seen the maximum,
    /// everything else the first non-null.
    Station,
    /// Grows only; the latest date wins per key.
    GrowOnly,
    /// One item, newest `client_modified_at` wins, deletes included.
    LastWriter,
    /// Newest `client_modified_at` wins per field.
    PerField,
    /// Settings, decided by [`crate::settings`].
    Setting,
}

impl Entity {
    /// Every entity, settings last.
    pub const ALL: [Self; 10] = [
        Self::Plays,
        Self::Sessions,
        Self::Stations,
        Self::ForgottenSongs,
        Self::BlockedArtists,
        Self::ExcludedStations,
        Self::Playlists,
        Self::PlaylistEntries,
        Self::Likes,
        Self::Settings,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Plays => "plays",
            Self::Sessions => "sessions",
            Self::Stations => "stations",
            Self::ForgottenSongs => "forgotten-songs",
            Self::BlockedArtists => "blocked-artists",
            Self::ExcludedStations => "excluded-stations",
            Self::Playlists => "playlists",
            Self::PlaylistEntries => "playlist-entries",
            Self::Likes => "likes",
            Self::Settings => "settings",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.as_str() == name)
    }

    pub const fn rule(self) -> Rule {
        match self {
            Self::Plays => Rule::Play,
            Self::Sessions => Rule::Owned,
            Self::Stations => Rule::Station,
            Self::ForgottenSongs => Rule::GrowOnly,
            Self::BlockedArtists | Self::ExcludedStations | Self::Likes => Rule::LastWriter,
            Self::Playlists | Self::PlaylistEntries => Rule::PerField,
            Self::Settings => Rule::Setting,
        }
    }

    /// Whether tombstones are kept forever rather than for [`crate::limits::TOMBSTONE_DAYS`].
    pub const fn keeps_tombstones(self) -> bool {
        matches!(self, Self::Plays)
    }

    /// How the record's id is formed.
    pub const fn key(self) -> Key {
        match self {
            Self::Stations | Self::ExcludedStations | Self::BlockedArtists => Key::FoldedName,
            Self::ForgottenSongs | Self::Likes => Key::SongIdentity,
            Self::Settings => Key::SettingName,
            Self::Plays
            | Self::Sessions
            | Self::Playlists
            | Self::PlaylistEntries => Key::ClientId,
        }
    }
}

/// How a record's id is formed and checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// Chosen by the client (a UUID, or `lastfm:<played_at_s>:<hash>` for imported plays).
    ClientId,
    /// `tracks_model::folded(fields.name)`, what Tracks' `stationKey(name)` computes. The
    /// server derives it when the client leaves `id` empty.
    FoldedName,
    /// `tracks_model::song_key(title, artist)`, Tracks' `SongIdentity.key`.
    SongIdentity,
    /// A settings key.
    SettingName,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for entity in Entity::ALL {
            assert_eq!(Entity::parse(entity.as_str()), Some(entity));
        }
        assert_eq!(Entity::parse("stats"), None);
    }
}
