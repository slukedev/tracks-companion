-- Last.fm enrichment (architecture section 5, Suggestions and enrichment): what was fetched
-- and when, so a restart does not fetch again, and what the similarity tables have no room
-- for. Similar artists and songs themselves go to similar_artists and similar_songs with
-- source 'lastfm'.

-- One row per artist identity or song key looked up. found = 0 caches "Last.fm does not know
-- it" until expires_at.
CREATE TABLE enrichment_fetches (
    kind        TEXT NOT NULL CHECK (kind IN ('artist', 'song')),
    key         TEXT NOT NULL,
    found       INTEGER NOT NULL CHECK (found IN (0, 1)),
    fetched_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,
    PRIMARY KEY (kind, key)
) STRICT, WITHOUT ROWID;

-- The plain-text biography summary of an artist.
CREATE TABLE enrichment_artists (
    identity  TEXT PRIMARY KEY,
    bio       TEXT NOT NULL
) STRICT, WITHOUT ROWID;

-- An artist's most played songs on Last.fm, rank 0 first.
CREATE TABLE enrichment_top_songs (
    artist_identity  TEXT NOT NULL,
    rank             INTEGER NOT NULL,
    song_key         TEXT NOT NULL,
    title            TEXT NOT NULL,
    PRIMARY KEY (artist_identity, rank)
) STRICT, WITHOUT ROWID;
CREATE INDEX enrichment_top_songs_key ON enrichment_top_songs(artist_identity, song_key);

-- Title and artist of every song key a Last.fm similar_songs row points at, for suggesting
-- songs the server does not have.
CREATE TABLE enrichment_song_names (
    song_key  TEXT PRIMARY KEY,
    title     TEXT NOT NULL,
    artist    TEXT NOT NULL
) STRICT, WITHOUT ROWID;
