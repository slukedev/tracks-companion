-- Trackside migration 1.
-- Times are Unix milliseconds UTC. `pid` columns are public ids (section 6).
-- Booleans are INTEGER 0 or 1.

CREATE TABLE settings (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL,              -- JSON
    updated_at  INTEGER NOT NULL
) STRICT;
-- Keys written by migration 1's code path on first start:
--   server.id            UUID, the server's identity in pairing, mDNS and capabilities
--   setup.state          "needs_admin" | "done"
--   sync.pruned_through  highest change_log seq whose tombstones were pruned
--   schema.tracks_model  version of tracks-model folding the stored keys were built with

-- Accounts -------------------------------------------------------------------------------

CREATE TABLE users (
    id                   INTEGER PRIMARY KEY,
    pid                  TEXT NOT NULL UNIQUE,
    username             TEXT NOT NULL,
    username_key         TEXT NOT NULL UNIQUE,          -- tracks_model::folded(username)
    display_name         TEXT NOT NULL,
    password_hash        TEXT NOT NULL,                 -- Argon2id PHC string
    role                 TEXT NOT NULL CHECK (role IN ('admin', 'member')),
    can_download         INTEGER NOT NULL DEFAULT 1 CHECK (can_download IN (0, 1)),
    can_upload           INTEGER NOT NULL DEFAULT 0 CHECK (can_upload IN (0, 1)),
    can_request          INTEGER NOT NULL DEFAULT 1 CHECK (can_request IN (0, 1)),
    legacy_subsonic      INTEGER NOT NULL DEFAULT 0 CHECK (legacy_subsonic IN (0, 1)),
    max_bitrate_kbps     INTEGER CHECK (max_bitrate_kbps IS NULL OR max_bitrate_kbps > 0),
    sync_epoch           INTEGER NOT NULL DEFAULT 1,    -- bumped when visible libraries change
    created_at           INTEGER NOT NULL,
    updated_at           INTEGER NOT NULL,
    password_changed_at  INTEGER NOT NULL,
    disabled_at          INTEGER,
    must_change_password INTEGER NOT NULL DEFAULT 0 CHECK (must_change_password IN (0, 1))
) STRICT;

CREATE TABLE sessions (
    id               INTEGER PRIMARY KEY,
    token_hash       BLOB NOT NULL UNIQUE,              -- SHA-256 of the cookie value
    user_id          INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at       INTEGER NOT NULL,
    last_seen_at     INTEGER NOT NULL,
    idle_expires_at  INTEGER NOT NULL,
    expires_at       INTEGER NOT NULL,
    ip               TEXT,
    user_agent       TEXT
) STRICT;
CREATE INDEX sessions_user ON sessions(user_id);
CREATE INDEX sessions_expiry ON sessions(expires_at);

CREATE TABLE devices (
    id                INTEGER PRIMARY KEY,
    pid               TEXT NOT NULL UNIQUE,
    user_id           INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    client_device_id  TEXT NOT NULL,                    -- the app's own id for the install
    name              TEXT NOT NULL,
    platform          TEXT NOT NULL,                    -- ios, macos, android, web, other
    model             TEXT,
    app_name          TEXT,
    app_version       TEXT,
    media_epoch       INTEGER NOT NULL DEFAULT 1,       -- bump to invalidate media tokens
    created_at        INTEGER NOT NULL,
    last_seen_at      INTEGER NOT NULL,
    last_ip           TEXT,
    revoked_at        INTEGER,
    UNIQUE (user_id, client_device_id)
) STRICT;

CREATE TABLE api_keys (
    id             INTEGER PRIMARY KEY,
    pid            TEXT NOT NULL UNIQUE,
    user_id        INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_id      INTEGER REFERENCES devices(id) ON DELETE CASCADE,
    kind           TEXT NOT NULL CHECK (kind IN ('device', 'api_key', 'app_password')),
    name           TEXT NOT NULL,
    prefix         TEXT NOT NULL,                       -- first 10 characters, shown in lists
    secret_hash    BLOB NOT NULL UNIQUE,                -- SHA-256 of the whole secret
    secret_sealed  BLOB,                                -- app_password only: XChaCha20-Poly1305
    scopes         TEXT NOT NULL,                       -- space separated, see section 7
    created_at     INTEGER NOT NULL,
    last_used_at   INTEGER,
    last_used_ip   TEXT,
    expires_at     INTEGER,
    revoked_at     INTEGER
) STRICT;
CREATE INDEX api_keys_user ON api_keys(user_id);

CREATE TABLE invites (
    id           INTEGER PRIMARY KEY,
    pid          TEXT NOT NULL UNIQUE,
    token_hash   BLOB NOT NULL UNIQUE,
    created_by   INTEGER REFERENCES users(id) ON DELETE SET NULL,
    role         TEXT NOT NULL CHECK (role IN ('admin', 'member')),
    can_request  INTEGER NOT NULL DEFAULT 1 CHECK (can_request IN (0, 1)),
    can_upload   INTEGER NOT NULL DEFAULT 0 CHECK (can_upload IN (0, 1)),
    note         TEXT,
    created_at   INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL,
    used_at      INTEGER,
    used_by      INTEGER REFERENCES users(id) ON DELETE SET NULL,
    revoked_at   INTEGER
) STRICT;

CREATE TABLE invite_libraries (
    invite_id   INTEGER NOT NULL REFERENCES invites(id) ON DELETE CASCADE,
    library_id  INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    PRIMARY KEY (invite_id, library_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE password_resets (
    id          INTEGER PRIMARY KEY,
    token_hash  BLOB NOT NULL UNIQUE,
    user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_by  INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,
    used_at     INTEGER
) STRICT;

CREATE TABLE pairings (
    id                 INTEGER PRIMARY KEY,
    pid                TEXT NOT NULL UNIQUE,
    kind               TEXT NOT NULL CHECK (kind IN ('offer', 'request')),
    -- offer: made by a signed-in user, shown as a QR code and a typed code.
    -- request: started by a device, approved by a signed-in user who types its 6-digit code.
    user_id            INTEGER REFERENCES users(id) ON DELETE CASCADE,
    code_hash          BLOB NOT NULL UNIQUE,            -- typed code (offer) or 6 digits (request)
    qr_secret_hash     BLOB UNIQUE,                     -- offer only: the 128-bit QR secret
    poll_secret_hash   BLOB,                            -- request: poll secret; offer: the auth crate's code check
    device_name        TEXT,
    platform           TEXT,
    client_device_id   TEXT,                            -- request: the app's install id
    device_model       TEXT,
    app_name           TEXT,
    app_version        TEXT,
    issued_key_sealed  BLOB,                            -- request only, cleared when claimed
    api_key_id         INTEGER REFERENCES api_keys(id) ON DELETE SET NULL,
    failed_attempts    INTEGER NOT NULL DEFAULT 0,
    created_at         INTEGER NOT NULL,
    expires_at         INTEGER NOT NULL,
    approved_at        INTEGER,
    claimed_at         INTEGER
) STRICT;

CREATE TABLE audit_log (
    id         INTEGER PRIMARY KEY,
    at         INTEGER NOT NULL,
    actor_id   INTEGER REFERENCES users(id) ON DELETE SET NULL,
    actor      TEXT NOT NULL,                           -- username at the time, or "cli", "system"
    ip         TEXT,
    action     TEXT NOT NULL,                           -- e.g. user.create, key.revoke, acquire.request
    target     TEXT,
    detail     TEXT                                     -- JSON
) STRICT;
CREATE INDEX audit_log_at ON audit_log(at);

CREATE TABLE idempotency_keys (
    user_id          INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    key              TEXT NOT NULL,
    request_hash     BLOB NOT NULL,                     -- SHA-256 of method, path and body
    response_status  INTEGER NOT NULL,
    response_body    BLOB NOT NULL,
    created_at       INTEGER NOT NULL,
    PRIMARY KEY (user_id, key)
) STRICT, WITHOUT ROWID;

-- Libraries ------------------------------------------------------------------------------

CREATE TABLE libraries (
    id                    INTEGER PRIMARY KEY,
    pid                   TEXT NOT NULL UNIQUE,
    name                  TEXT NOT NULL,
    root                  BLOB NOT NULL UNIQUE,         -- absolute path bytes
    root_display          TEXT NOT NULL,
    enabled               INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    writable              INTEGER NOT NULL DEFAULT 0 CHECK (writable IN (0, 1)),
    watch                 INTEGER NOT NULL DEFAULT 1 CHECK (watch IN (0, 1)),
    quick_scan_interval_s INTEGER,                      -- NULL: scanner.quick_interval
    root_dev              INTEGER,                      -- st_dev at the last healthy scan
    root_entry_count      INTEGER,                      -- entries in the root at that scan
    state                 TEXT NOT NULL DEFAULT 'ok' CHECK (state IN ('ok', 'unavailable')),
    unavailable_reason    TEXT,
    unavailable_since     INTEGER,
    song_count            INTEGER NOT NULL DEFAULT 0,
    created_at            INTEGER NOT NULL,
    updated_at            INTEGER NOT NULL,
    last_scan_at          INTEGER
) STRICT;
-- settings "uploads.library" and "acquire.library" name the writable libraries used as targets.

CREATE TABLE library_access (
    user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    library_id  INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    granted_at  INTEGER NOT NULL,
    granted_by  INTEGER REFERENCES users(id) ON DELETE SET NULL,
    PRIMARY KEY (user_id, library_id)
) STRICT, WITHOUT ROWID;
-- Admins see every library without rows here.

CREATE TABLE folders (
    id               INTEGER PRIMARY KEY,
    pid              TEXT NOT NULL UNIQUE,
    library_id       INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    parent_id        INTEGER REFERENCES folders(id) ON DELETE CASCADE,
    rel_path         BLOB NOT NULL,                     -- empty for the root
    name             TEXT NOT NULL,
    mtime_ns         INTEGER,
    listing_hash     BLOB,                              -- BLAKE3 of sorted (name, size, mtime) entries
    unreadable       INTEGER NOT NULL DEFAULT 0 CHECK (unreadable IN (0, 1)),
    last_scanned_at  INTEGER,
    missing_since    INTEGER,
    UNIQUE (library_id, rel_path)
) STRICT;
CREATE INDEX folders_parent ON folders(parent_id);

CREATE TABLE artwork (
    id           TEXT PRIMARY KEY,                      -- 'c' + digest, section 6
    mime         TEXT NOT NULL,
    width        INTEGER NOT NULL,
    height       INTEGER NOT NULL,
    bytes        INTEGER NOT NULL,
    source_kind  TEXT NOT NULL CHECK (source_kind IN ('embedded', 'sidecar', 'upload', 'acquired')),
    source_path  BLOB,                                  -- absolute path to re-extract from
    created_at   INTEGER NOT NULL
) STRICT;

CREATE TABLE artists (
    id             INTEGER PRIMARY KEY,
    pid            TEXT NOT NULL UNIQUE,
    identity       TEXT NOT NULL UNIQUE,                -- tracks_model::folded(name)
    name           TEXT NOT NULL,
    sort_name      TEXT NOT NULL,
    mbz_artist_id  TEXT,
    artwork_id     TEXT REFERENCES artwork(id) ON DELETE SET NULL,
    search_text    TEXT NOT NULL,                       -- tracks_model::search_folded
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
) STRICT;
CREATE INDEX artists_sort ON artists(sort_name);

CREATE TABLE artist_libraries (
    artist_id    INTEGER NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
    library_id   INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    album_count  INTEGER NOT NULL,
    song_count   INTEGER NOT NULL,
    PRIMARY KEY (artist_id, library_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE albums (
    id                    INTEGER PRIMARY KEY,
    pid                   TEXT NOT NULL UNIQUE,
    library_id            INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    identity              TEXT NOT NULL,                -- 'mb:' + release MBID, or album_key
    album_key             TEXT,                         -- tracks_model::album_key, NULL for blank titles
    name                  TEXT NOT NULL,
    sort_name             TEXT NOT NULL,
    album_artist          TEXT NOT NULL,
    artist_id             INTEGER REFERENCES artists(id) ON DELETE SET NULL,
    year                  INTEGER,
    release_date          TEXT,                         -- YYYY, YYYY-MM or YYYY-MM-DD as tagged
    original_date         TEXT,
    genre                 TEXT,
    is_compilation        INTEGER NOT NULL DEFAULT 0 CHECK (is_compilation IN (0, 1)),
    release_type          TEXT,
    mbz_release_id        TEXT,
    mbz_release_group_id  TEXT,
    song_count            INTEGER NOT NULL DEFAULT 0,
    disc_count            INTEGER NOT NULL DEFAULT 1,
    duration_ms           INTEGER NOT NULL DEFAULT 0,
    size_bytes            INTEGER NOT NULL DEFAULT 0,
    artwork_id            TEXT REFERENCES artwork(id) ON DELETE SET NULL,
    search_text           TEXT NOT NULL,
    added_at              INTEGER NOT NULL,             -- earliest added_at of its songs
    updated_at            INTEGER NOT NULL,
    UNIQUE (library_id, identity)
) STRICT;
CREATE INDEX albums_added ON albums(added_at DESC);
CREATE INDEX albums_sort ON albums(sort_name);
CREATE INDEX albums_artist ON albums(artist_id);
-- Expression indexes matching the album list orders, so every order is an index walk.
CREATE INDEX albums_year ON albums(coalesce(year, 0), sort_name);
CREATE INDEX albums_by_artist ON albums(lower(album_artist), sort_name);

CREATE TABLE genres (
    id        INTEGER PRIMARY KEY,
    name      TEXT NOT NULL,
    name_key  TEXT NOT NULL UNIQUE                      -- folded
) STRICT;

CREATE TABLE songs (
    id                   INTEGER PRIMARY KEY,
    pid                  TEXT NOT NULL UNIQUE,
    library_id           INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    folder_id            INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    rel_path             BLOB NOT NULL,
    rel_path_display     TEXT NOT NULL,
    file_size            INTEGER NOT NULL,
    file_mtime_ns        INTEGER NOT NULL,
    file_dev             INTEGER,
    file_ino             INTEGER,
    audio_digest         BLOB NOT NULL,                 -- 32 bytes, section 6
    content_version      INTEGER NOT NULL DEFAULT 1,    -- bumped when the file's bytes change
    title                TEXT NOT NULL,
    sort_title           TEXT NOT NULL,
    artist               TEXT NOT NULL,                 -- display artist as tagged
    artist_id            INTEGER REFERENCES artists(id) ON DELETE SET NULL,   -- lead credit
    album_id             INTEGER REFERENCES albums(id) ON DELETE SET NULL,
    album                TEXT,
    album_artist         TEXT,
    track_number         INTEGER,
    disc_number          INTEGER,
    disc_subtitle        TEXT,
    year                 INTEGER,
    release_date         TEXT,
    genre                TEXT,                          -- first genre, for Subsonic `genre`
    duration_ms          INTEGER NOT NULL,
    bitrate_kbps         INTEGER,
    sample_rate          INTEGER,
    bit_depth            INTEGER,
    channels             INTEGER,
    codec                TEXT NOT NULL,                 -- flac, alac, aac, mp3, opus, vorbis, pcm
    container            TEXT NOT NULL,                 -- file suffix: flac, m4a, mp3, opus, ogg, wav, aiff
    content_type         TEXT NOT NULL,
    lossless             INTEGER NOT NULL CHECK (lossless IN (0, 1)),
    song_key             TEXT NOT NULL,                 -- tracks_model::song_key(title, artist)
    mbz_recording_id     TEXT,
    mbz_track_id         TEXT,
    mbz_release_id       TEXT,
    isrc                 TEXT,
    bpm                  INTEGER,
    comment              TEXT,
    explicit             TEXT CHECK (explicit IN ('explicit', 'clean')),
    rg_track_gain        REAL,
    rg_track_peak        REAL,
    rg_album_gain        REAL,
    rg_album_peak        REAL,
    has_lyrics           INTEGER NOT NULL DEFAULT 0 CHECK (has_lyrics IN (0, 1)),
    artwork_id           TEXT REFERENCES artwork(id) ON DELETE SET NULL,
    tag_problem          TEXT,                          -- why tags fell back to the file name
    search_text          TEXT NOT NULL,
    origin               TEXT NOT NULL DEFAULT 'scan' CHECK (origin IN ('scan', 'upload', 'acquired')),
    added_at             INTEGER NOT NULL,
    updated_at           INTEGER NOT NULL,
    missing_since        INTEGER,                       -- NULL while the file is present
    UNIQUE (library_id, rel_path)
) STRICT;
CREATE INDEX songs_album ON songs(album_id, disc_number, track_number);
CREATE INDEX songs_artist ON songs(artist_id);
CREATE INDEX songs_folder ON songs(folder_id);
CREATE INDEX songs_digest ON songs(audio_digest);
CREATE INDEX songs_key ON songs(song_key);
CREATE INDEX songs_added ON songs(added_at DESC);
CREATE INDEX songs_mbz_track ON songs(mbz_track_id) WHERE mbz_track_id IS NOT NULL;
CREATE INDEX songs_missing ON songs(missing_since) WHERE missing_since IS NOT NULL;

CREATE TABLE song_artists (
    song_id    INTEGER NOT NULL REFERENCES songs(id) ON DELETE CASCADE,
    artist_id  INTEGER NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
    role       TEXT NOT NULL CHECK (role IN ('main', 'featured', 'album_artist', 'composer')),
    position   INTEGER NOT NULL,
    PRIMARY KEY (song_id, role, position)
) STRICT, WITHOUT ROWID;
CREATE INDEX song_artists_artist ON song_artists(artist_id, role);

CREATE TABLE song_genres (
    song_id   INTEGER NOT NULL REFERENCES songs(id) ON DELETE CASCADE,
    genre_id  INTEGER NOT NULL REFERENCES genres(id) ON DELETE CASCADE,
    position  INTEGER NOT NULL,
    PRIMARY KEY (song_id, genre_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX song_genres_genre ON song_genres(genre_id);

CREATE TABLE lyrics (
    song_id   INTEGER NOT NULL REFERENCES songs(id) ON DELETE CASCADE,
    kind      TEXT NOT NULL CHECK (kind IN ('synced', 'plain')),
    lang      TEXT NOT NULL DEFAULT 'und',
    source    TEXT NOT NULL CHECK (source IN ('embedded', 'sidecar')),
    content   TEXT NOT NULL,                            -- LRC for synced, text for plain
    PRIMARY KEY (song_id, kind, lang, source)
) STRICT, WITHOUT ROWID;

-- Full-text search over pre-folded text (tracks_model::search_folded), so matching is the
-- same as Tracks' own search. External-content tables kept in step by triggers.
CREATE VIRTUAL TABLE songs_fts USING fts5(
    search_text, content='songs', content_rowid='id',
    tokenize = 'unicode61 remove_diacritics 0', prefix = '1 2 3'
);
CREATE VIRTUAL TABLE albums_fts USING fts5(
    search_text, content='albums', content_rowid='id',
    tokenize = 'unicode61 remove_diacritics 0', prefix = '1 2 3'
);
CREATE VIRTUAL TABLE artists_fts USING fts5(
    search_text, content='artists', content_rowid='id',
    tokenize = 'unicode61 remove_diacritics 0', prefix = '1 2 3'
);
CREATE TRIGGER songs_fts_ai AFTER INSERT ON songs BEGIN
    INSERT INTO songs_fts(rowid, search_text) VALUES (new.id, new.search_text);
END;
CREATE TRIGGER songs_fts_ad AFTER DELETE ON songs BEGIN
    INSERT INTO songs_fts(songs_fts, rowid, search_text) VALUES ('delete', old.id, old.search_text);
END;
CREATE TRIGGER songs_fts_au AFTER UPDATE OF search_text ON songs BEGIN
    INSERT INTO songs_fts(songs_fts, rowid, search_text) VALUES ('delete', old.id, old.search_text);
    INSERT INTO songs_fts(rowid, search_text) VALUES (new.id, new.search_text);
END;
CREATE TRIGGER albums_fts_ai AFTER INSERT ON albums BEGIN
    INSERT INTO albums_fts(rowid, search_text) VALUES (new.id, new.search_text);
END;
CREATE TRIGGER albums_fts_ad AFTER DELETE ON albums BEGIN
    INSERT INTO albums_fts(albums_fts, rowid, search_text) VALUES ('delete', old.id, old.search_text);
END;
CREATE TRIGGER albums_fts_au AFTER UPDATE OF search_text ON albums BEGIN
    INSERT INTO albums_fts(albums_fts, rowid, search_text) VALUES ('delete', old.id, old.search_text);
    INSERT INTO albums_fts(rowid, search_text) VALUES (new.id, new.search_text);
END;
CREATE TRIGGER artists_fts_ai AFTER INSERT ON artists BEGIN
    INSERT INTO artists_fts(rowid, search_text) VALUES (new.id, new.search_text);
END;
CREATE TRIGGER artists_fts_ad AFTER DELETE ON artists BEGIN
    INSERT INTO artists_fts(artists_fts, rowid, search_text) VALUES ('delete', old.id, old.search_text);
END;
CREATE TRIGGER artists_fts_au AFTER UPDATE OF search_text ON artists BEGIN
    INSERT INTO artists_fts(artists_fts, rowid, search_text) VALUES ('delete', old.id, old.search_text);
    INSERT INTO artists_fts(rowid, search_text) VALUES (new.id, new.search_text);
END;

-- Similarity (local signals and optional enrichment) -------------------------------------

CREATE TABLE similar_artists (
    artist_identity   TEXT NOT NULL,                    -- folded
    similar_identity  TEXT NOT NULL,
    similar_name      TEXT NOT NULL,
    score             REAL NOT NULL,                    -- 0..1
    source            TEXT NOT NULL CHECK (source IN ('local', 'lastfm')),
    fetched_at        INTEGER NOT NULL,
    PRIMARY KEY (artist_identity, source, similar_identity)
) STRICT, WITHOUT ROWID;

CREATE TABLE similar_songs (
    song_key          TEXT NOT NULL,
    similar_song_key  TEXT NOT NULL,
    score             REAL NOT NULL,
    source            TEXT NOT NULL CHECK (source IN ('local', 'lastfm')),
    fetched_at        INTEGER NOT NULL,
    PRIMARY KEY (song_key, source, similar_song_key)
) STRICT, WITHOUT ROWID;

-- Per-user data --------------------------------------------------------------------------

CREATE TABLE listens (
    id                INTEGER PRIMARY KEY,
    user_id           INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_id         INTEGER REFERENCES devices(id) ON DELETE SET NULL,
    client_listen_id  TEXT NOT NULL,                    -- the client's id; Tracks sends its play id
    song_pid          TEXT,                             -- NULL when the song is not on this server
    song_key          TEXT NOT NULL,
    title             TEXT NOT NULL,
    artist            TEXT NOT NULL,
    album             TEXT,
    started_at        INTEGER NOT NULL,                 -- client clock, as the client recorded it
    played_ms         INTEGER,
    duration_ms       INTEGER,
    source            TEXT NOT NULL CHECK (source IN ('native', 'subsonic', 'report')),
    received_at       INTEGER NOT NULL,                 -- server clock
    UNIQUE (user_id, client_listen_id)
) STRICT;
CREATE INDEX listens_user_time ON listens(user_id, started_at);
CREATE INDEX listens_song ON listens(song_pid, started_at) WHERE song_pid IS NOT NULL;

CREATE TABLE play_stats (
    user_id         INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    song_pid        TEXT NOT NULL,
    play_count      INTEGER NOT NULL,
    last_played_at  INTEGER NOT NULL,
    PRIMARY KEY (user_id, song_pid)
) STRICT, WITHOUT ROWID;
CREATE INDEX play_stats_song ON play_stats(song_pid);

CREATE TABLE stars (
    user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    item_kind   TEXT NOT NULL CHECK (item_kind IN ('song', 'album', 'artist')),
    item_pid    TEXT NOT NULL,
    starred_at  INTEGER NOT NULL,
    PRIMARY KEY (user_id, item_kind, item_pid)
) STRICT, WITHOUT ROWID;

CREATE TABLE ratings (
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    item_kind  TEXT NOT NULL CHECK (item_kind IN ('song', 'album', 'artist')),
    item_pid   TEXT NOT NULL,
    rating     INTEGER NOT NULL CHECK (rating BETWEEN 1 AND 5),
    rated_at   INTEGER NOT NULL,
    PRIMARY KEY (user_id, item_kind, item_pid)
) STRICT, WITHOUT ROWID;

CREATE TABLE bookmarks (
    user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    song_pid     TEXT NOT NULL,
    position_ms  INTEGER NOT NULL,
    comment      TEXT,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (user_id, song_pid)
) STRICT, WITHOUT ROWID;

CREATE TABLE playlists (
    id           INTEGER PRIMARY KEY,
    pid          TEXT NOT NULL UNIQUE,
    owner_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    comment      TEXT,
    is_public    INTEGER NOT NULL DEFAULT 0 CHECK (is_public IN (0, 1)),
    song_count   INTEGER NOT NULL DEFAULT 0,
    duration_ms  INTEGER NOT NULL DEFAULT 0,
    revision     INTEGER NOT NULL DEFAULT 1,            -- ETag and If-Match
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
) STRICT;
CREATE INDEX playlists_owner ON playlists(owner_id);

CREATE TABLE playlist_entries (
    playlist_id  INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    position     INTEGER NOT NULL,
    song_pid     TEXT NOT NULL,
    added_at     INTEGER NOT NULL,
    PRIMARY KEY (playlist_id, position)
) STRICT, WITHOUT ROWID;
CREATE INDEX playlist_entries_song ON playlist_entries(song_pid);

CREATE TABLE play_queues (
    user_id           INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    current_position  INTEGER,
    position_ms       INTEGER NOT NULL DEFAULT 0,
    changed_by        TEXT NOT NULL,                    -- client name
    device_id         INTEGER REFERENCES devices(id) ON DELETE SET NULL,
    updated_at        INTEGER NOT NULL
) STRICT;

CREATE TABLE play_queue_entries (
    user_id   INTEGER NOT NULL REFERENCES play_queues(user_id) ON DELETE CASCADE,
    position  INTEGER NOT NULL,
    song_pid  TEXT NOT NULL,
    PRIMARY KEY (user_id, position)
) STRICT, WITHOUT ROWID;

-- Delta sync -----------------------------------------------------------------------------

-- One row per entity: a change replaces the entity's row with a new, higher seq, so the
-- table's size follows the number of entities and tombstones, not the number of edits.
CREATE TABLE change_log (
    seq         INTEGER PRIMARY KEY,                    -- assigned by the single writer, never reused
    kind        TEXT NOT NULL CHECK (kind IN (
                    'song', 'album', 'artist', 'library', 'playlist',
                    'star', 'rating', 'play_stat', 'play_queue', 'acquisition')),
    entity_pid  TEXT NOT NULL,                          -- for star and rating: '<item_kind>:<item_pid>'
    user_id     INTEGER NOT NULL DEFAULT 0,             -- 0: visible to everyone with library access
    library_id  INTEGER,                                -- library-scoped kinds only
    op          TEXT NOT NULL CHECK (op IN ('upsert', 'delete')),
    changed_at  INTEGER NOT NULL,
    UNIQUE (kind, entity_pid, user_id)
) STRICT;
CREATE INDEX change_log_user_seq ON change_log(user_id, seq);

-- Scanning and imports -------------------------------------------------------------------

CREATE TABLE scan_runs (
    id             INTEGER PRIMARY KEY,
    pid            TEXT NOT NULL UNIQUE,
    library_id     INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    kind           TEXT NOT NULL CHECK (kind IN ('full', 'quick', 'folder', 'file')),
    trigger        TEXT NOT NULL CHECK (trigger IN ('startup', 'schedule', 'watch', 'admin', 'import', 'cli')),
    state          TEXT NOT NULL CHECK (state IN ('running', 'done', 'failed', 'aborted', 'unavailable')),
    started_at     INTEGER NOT NULL,
    finished_at    INTEGER,
    folders_seen   INTEGER NOT NULL DEFAULT 0,
    files_seen     INTEGER NOT NULL DEFAULT 0,
    added          INTEGER NOT NULL DEFAULT 0,
    updated        INTEGER NOT NULL DEFAULT 0,
    moved          INTEGER NOT NULL DEFAULT 0,
    missing        INTEGER NOT NULL DEFAULT 0,
    errors         INTEGER NOT NULL DEFAULT 0,
    message        TEXT
) STRICT;
CREATE INDEX scan_runs_library ON scan_runs(library_id, started_at DESC);

CREATE TABLE scan_errors (
    scan_run_id  INTEGER NOT NULL REFERENCES scan_runs(id) ON DELETE CASCADE,
    rel_path     BLOB NOT NULL,
    path_display TEXT NOT NULL,
    error        TEXT NOT NULL,
    PRIMARY KEY (scan_run_id, rel_path)
) STRICT, WITHOUT ROWID;

-- A file that hung or crashed the tag reader is skipped until its size or mtime changes.
CREATE TABLE scan_quarantine (
    library_id     INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    rel_path       BLOB NOT NULL,
    file_size      INTEGER NOT NULL,
    file_mtime_ns  INTEGER NOT NULL,
    reason         TEXT NOT NULL,
    at             INTEGER NOT NULL,
    PRIMARY KEY (library_id, rel_path)
) STRICT, WITHOUT ROWID;

CREATE TABLE imports (
    id                 INTEGER PRIMARY KEY,
    pid                TEXT NOT NULL UNIQUE,
    user_id            INTEGER REFERENCES users(id) ON DELETE SET NULL,
    source             TEXT NOT NULL CHECK (source IN ('upload', 'inbox')),
    original_name      TEXT NOT NULL,
    staged_path        BLOB,
    expected_bytes     INTEGER,
    received_bytes     INTEGER NOT NULL DEFAULT 0,
    state              TEXT NOT NULL CHECK (state IN ('receiving', 'staged', 'imported', 'duplicate', 'rejected', 'failed')),
    target_library_id  INTEGER REFERENCES libraries(id) ON DELETE SET NULL,
    song_pid           TEXT,
    error              TEXT,
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL
) STRICT;

-- Acquisition ----------------------------------------------------------------------------

CREATE TABLE acquisitions (
    id               INTEGER PRIMARY KEY,
    pid              TEXT NOT NULL UNIQUE,
    user_id          INTEGER REFERENCES users(id) ON DELETE SET NULL,
    joined_to        INTEGER REFERENCES acquisitions(id) ON DELETE SET NULL,
    request_key      TEXT NOT NULL,                     -- tracks_model::song_key of the request
    title            TEXT NOT NULL,
    artist           TEXT NOT NULL,
    album            TEXT,
    duration_ms      INTEGER,
    candidate_token  TEXT,                              -- a candidate the user picked
    allow_versions   INTEGER NOT NULL DEFAULT 0 CHECK (allow_versions IN (0, 1)),
    instant          INTEGER NOT NULL DEFAULT 0 CHECK (instant IN (0, 1)),
    priority         INTEGER NOT NULL DEFAULT 1,        -- 0 instant play, 1 interactive, 2 batch
    state            TEXT NOT NULL CHECK (state IN (
                         'queued', 'searching', 'downloading', 'verifying', 'importing',
                         'done', 'failed', 'cancelled')),
    failure          TEXT,                              -- machine code, api.md
    failure_detail   TEXT,
    provider         TEXT,                              -- internal; never sent to Tracks
    candidate_id     INTEGER REFERENCES acquisition_candidates(id) ON DELETE SET NULL,
    attempts         INTEGER NOT NULL DEFAULT 0,
    next_attempt_at  INTEGER NOT NULL,
    lease_until      INTEGER,
    progress_bytes   INTEGER,
    total_bytes      INTEGER,
    staging_dir      BLOB,
    song_pid         TEXT,
    existing         INTEGER NOT NULL DEFAULT 0 CHECK (existing IN (0, 1)),
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    finished_at      INTEGER
) STRICT;
CREATE INDEX acquisitions_ready ON acquisitions(state, priority, next_attempt_at);
CREATE INDEX acquisitions_user ON acquisitions(user_id, created_at DESC);
CREATE INDEX acquisitions_active_key ON acquisitions(request_key)
    WHERE state NOT IN ('done', 'failed', 'cancelled');

CREATE TABLE acquisition_candidates (
    id              INTEGER PRIMARY KEY,
    token           TEXT NOT NULL UNIQUE,               -- 'x' + digest of provider and provider_ref
    acquisition_id  INTEGER REFERENCES acquisitions(id) ON DELETE CASCADE,
    provider        TEXT NOT NULL,
    provider_ref    TEXT NOT NULL,                      -- JSON, provider-specific
    title           TEXT NOT NULL,
    artist          TEXT NOT NULL,
    album           TEXT,
    duration_ms     INTEGER,
    container       TEXT,
    codec           TEXT,
    bitrate_kbps    INTEGER,
    lossless        INTEGER NOT NULL DEFAULT 0 CHECK (lossless IN (0, 1)),
    size_bytes      INTEGER,
    score           REAL NOT NULL,
    tried_at        INTEGER,
    failure         TEXT,
    created_at      INTEGER NOT NULL,
    expires_at      INTEGER NOT NULL
) STRICT;
CREATE INDEX acquisition_candidates_job ON acquisition_candidates(acquisition_id, score DESC);
CREATE INDEX acquisition_candidates_expiry ON acquisition_candidates(expires_at);
