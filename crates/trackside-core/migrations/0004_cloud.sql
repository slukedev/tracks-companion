-- Trackside migration 4: Trackside Cloud, each user's synced app data (ADR 0012).
-- Additive only. Everything here belongs to one user and goes with them (ON DELETE CASCADE).
-- The server stores the app's fields as JSON and never shows them on the dashboard.

-- One row per user who has synced anything: the user's own change seq. Every write to that
-- user's cloud rows takes the next value, so a cursor is a plain seq that never depends on
-- client time.
CREATE TABLE cloud_state (
    user_id    INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    seq        INTEGER NOT NULL DEFAULT 0,
    pruned_at  INTEGER NOT NULL DEFAULT 0        -- when expired tombstones were last pruned
) STRICT;

-- Every synced record: plays, sessions, stations, settings, playlists and the rest, keyed by
-- the client's id. A tombstone keeps its row with fields NULL and deleted_at set.
CREATE TABLE cloud_items (
    id           INTEGER PRIMARY KEY,
    user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    entity       TEXT NOT NULL,                  -- plays, sessions, settings, ...
    item_id      TEXT NOT NULL,                  -- the client's id or the natural key
    version      INTEGER NOT NULL,               -- cloud_state.seq of the last change
    fields       TEXT,                           -- JSON object; NULL for a tombstone
    meta         TEXT,                           -- JSON: per-field times, owning device
    parent       TEXT,                           -- the playlist of a playlist entry
    device_id    TEXT NOT NULL,                  -- the client device of the last change
    modified_at  INTEGER NOT NULL,               -- client time of the last change
    deleted_at   INTEGER,                        -- client time of the delete
    merged_into  TEXT,                           -- a play merged into another
    changed_at   INTEGER NOT NULL,               -- server time of the last change
    bytes        INTEGER NOT NULL,               -- counted against the user's quota
    UNIQUE (user_id, entity, item_id)
) STRICT;
CREATE UNIQUE INDEX cloud_items_version ON cloud_items(user_id, entity, version);
CREATE INDEX cloud_items_parent ON cloud_items(user_id, entity, parent) WHERE parent IS NOT NULL;
CREATE INDEX cloud_items_tombstones ON cloud_items(user_id, entity, changed_at)
    WHERE deleted_at IS NOT NULL;

-- Per user and entity: what is stored, for quotas and the admin's storage numbers, and the
-- highest version whose tombstone was pruned, below which a cursor can no longer be served.
CREATE TABLE cloud_entities (
    user_id         INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    entity          TEXT NOT NULL,
    items           INTEGER NOT NULL DEFAULT 0,
    bytes           INTEGER NOT NULL DEFAULT 0,
    pruned_through  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, entity)
) STRICT, WITHOUT ROWID;

-- The app's own device ids and which ones each was restored from, so a restored device
-- inherits what its predecessor owned. Linked to the server's device row when registered
-- with a device key.
CREATE TABLE cloud_devices (
    user_id        INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_id      TEXT NOT NULL,
    predecessors   TEXT NOT NULL DEFAULT '[]',   -- JSON array of device ids
    server_device  INTEGER REFERENCES devices(id) ON DELETE SET NULL,
    name           TEXT,
    platform       TEXT,
    registered_at  INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    PRIMARY KEY (user_id, device_id)
) STRICT;
CREATE INDEX cloud_devices_server ON cloud_devices(server_device) WHERE server_device IS NOT NULL;
