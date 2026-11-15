-- Re-authentication for sensitive dashboard actions, full permissions on invites, and the
-- rollups behind the dashboard's charts (api.md section 16, stats).

-- When the session last proved its password; NULL means at sign-in (created_at).
ALTER TABLE sessions ADD COLUMN recent_auth_at INTEGER;

-- One row per UTC hour the server was running: the most concurrent media responses and
-- ffmpeg jobs seen, and failed media requests. A missing hour means "not recorded".
CREATE TABLE stats_hourly (
    hour_start   INTEGER PRIMARY KEY,   -- UTC milliseconds at the start of the hour
    streams      INTEGER NOT NULL DEFAULT 0,
    transcodes   INTEGER NOT NULL DEFAULT 0,
    failed       INTEGER NOT NULL DEFAULT 0
) STRICT;

-- One row per server-local day: the latest storage snapshot of that day.
CREATE TABLE stats_daily (
    day              TEXT PRIMARY KEY,  -- YYYY-MM-DD in the server's zone
    data_free_bytes  INTEGER,
    database_bytes   INTEGER,
    cache_bytes      INTEGER,
    volume_bytes     INTEGER,
    updated_at       INTEGER NOT NULL
) STRICT;

-- Invites carry every permission the dashboard offers, not only requests and uploads.
ALTER TABLE invites ADD COLUMN can_download INTEGER NOT NULL DEFAULT 1 CHECK (can_download IN (0, 1));
ALTER TABLE invites ADD COLUMN legacy_subsonic INTEGER NOT NULL DEFAULT 0 CHECK (legacy_subsonic IN (0, 1));
ALTER TABLE invites ADD COLUMN max_bitrate_kbps INTEGER;
