-- Device-code linking (api.md section 6, Linking a TV): a device that cannot scan a QR or type
-- a long code shows a short user code; someone signed in approves it; the device collects its
-- key with a device code it keeps to itself. Only hashes of the codes are stored.

CREATE TABLE device_links (
    id                INTEGER PRIMARY KEY,
    device_code_hash  BLOB NOT NULL UNIQUE,            -- the 128-bit-plus secret the device polls with
    user_code_locator BLOB NOT NULL UNIQUE,            -- the first half of the user code
    user_code_hash    BLOB NOT NULL,                   -- the whole user code, checked in constant time
    client_device_id  TEXT NOT NULL,
    device_name       TEXT NOT NULL,
    platform          TEXT NOT NULL,
    device_model      TEXT,
    app_name          TEXT,
    app_version       TEXT,
    nearby            INTEGER NOT NULL,                -- 1 when it was started from a nearby address
    state             TEXT NOT NULL CHECK (state IN ('pending', 'approved', 'denied', 'locked', 'completed')),
    user_id           INTEGER REFERENCES users(id) ON DELETE CASCADE, -- the approver or denier
    scopes            TEXT,                            -- what the approver's credential could do
    failed_attempts   INTEGER NOT NULL DEFAULT 0,
    interval_ms       INTEGER NOT NULL,
    last_polled_at    INTEGER,
    created_at        INTEGER NOT NULL,
    expires_at        INTEGER NOT NULL,
    decided_at        INTEGER,
    completed_at      INTEGER
) STRICT;
CREATE INDEX device_links_expires ON device_links(expires_at);
CREATE INDEX device_links_pending ON device_links(expires_at) WHERE state = 'pending';
