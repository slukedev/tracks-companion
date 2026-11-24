# Trackside architecture

Trackside is a self-hosted music server for the Tracks app. It indexes music the user owns, serves
it to Tracks over a native API and to every other client over Subsonic 1.16.1 with OpenSubsonic
extensions, keeps per-user listens, stars and playlists, and can optionally acquire songs through
yt-dlp and slskd when an admin turns that on. It replaces the previous Subsonic proxy Tracks
used, one that fetched songs on demand and that Tracks talks to as a plain Subsonic server.

This document describes the server as built and is kept in step with the code. The wire formats are
in [api.md](api.md), the decisions and their alternatives in [adr/](adr/), and the work split the
first build followed in [build-plan.md](build-plan.md) (historical).

Contents:

1. Goals and non-goals
2. Crate layout and boundaries
3. Concurrency model
4. Data flow
5. Data model (migrations 1 and 2, full SQL)
6. Id strategy
7. Auth design
8. Streaming, transcoding and artwork
9. Acquisition
10. Remote access and discovery
11. Configuration reference
12. Observability
13. Backups, upgrades and on-disk stability
14. Security model and threat model
15. Performance budgets
16. Failure modes
17. Tracks client integration

---

## 1. Goals and non-goals

### Goals

- **Fast.** Every request path is a local SQLite read or a file read. Nothing on a request path
  waits on the internet. Budgets are in section 15.
- **Reliable away from home.** Candidate URLs for LAN and WAN, raced by the client; resumable
  downloads with strong ETags; a delta sync that lets Tracks browse the whole library offline;
  an idempotent, batched listen upload that survives days offline.
- **Multi-user.** Real accounts: first-run setup guarded by a one-time token, invites, the roles
  admin and member, per-device API keys, browser sessions, rate limiting, per-user history,
  stars and playlists, and per-library access.
- **Standards.** Subsonic 1.16.1 plus OpenSubsonic (apiKey auth, formPost, songLyrics,
  transcodeOffset, indexBasedQueue, topSongsByArtistId, playbackReport) so Amperfy, Symfonium,
  Feishin, Supersonic and others work unchanged.
- **Same identities as Tracks.** Name folding, song keys, album keys and artist credits come from
  `tracks-model` in the Tracks repository, the same code the Tracks apps use, so a song in the
  user's Tracks history and the same song on the server have byte-identical keys.
- **Stable song ids.** A song keeps its id across rescans, tag edits, file moves and renames, and
  even across a database rebuild, so `server:<entry>:<songID>` values stored by Tracks never go
  stale (section 6).
- **Easy to host.** One static binary, a multi-arch distroless image with ffmpeg, a one-service
  compose file, a hardened systemd unit, TOML plus environment configuration, and a server that
  starts with no configuration at all.
- **Longevity.** Stable on-disk formats, forward-only migrations with automatic pre-migration
  backups, scheduled `VACUUM INTO` backups, a semver'd API with a written deprecation policy.
- **Optional acquisition.** Admin-enabled providers (yt-dlp and slskd) behind a durable job
  queue, replacing the previous proxy's "star, then poll for twenty minutes" flow with a request
  that reports its own status and ends with a stable song id (section 9).

### Non-goals

- Ripping or decrypting streaming services (Tidal, Qobuz, Amazon, Deezer, Spotify), or
  gateways that do (SpotiFLAC, Mornye and similar). There is no provider interface aimed at
  them and none will be accepted.
- Speaking the Soulseek protocol. Trackside only talks to an slskd instance over its REST API.
- Being a proxy in front of another server. Trackside owns its scanner, database and users.
- Video, podcasts, internet radio management, jukebox mode, chat, and the Jellyfin API in v1.
  The Subsonic endpoints for them answer with an empty result or "not implemented" (api.md).
- Public share links in v1. The schema leaves room; the feature is planned for a later minor.
- Server-side scrobble forwarding to Last.fm or ListenBrainz in v1. Tracks scrobbles itself.
- A plugin system that loads code. Providers are compiled in.
- Automated captcha solving or bot-check evasion for any provider.

---

## 2. Crate layout and boundaries

The server is its own Cargo workspace, separate from the Tracks engine because it targets
Linux servers rather than Apple device static libraries, and has its own toolchain file.
It reuses `tracks-model` as a git dependency pinned to an exact revision of the Tracks repository.

```
./
  Cargo.toml                  workspace, shared dependency versions, lints (same lint set as the Tracks engine)
  rust-toolchain.toml         channel 1.100.0; targets x86_64/aarch64 linux musl and gnu
  crates/
    trackside-core/           domain types, ids, config, Db (writer + readers), migrations,
                              repositories, change log, sync, search, similarity, event bus,
                              the ScanControl, AcquireControl and EnrichControl traits
    trackside-scan/           library walking, tag reading (lofty), audio digests, watching
                              (notify), availability monitor, the importer (uploads, inbox)
    trackside-media/          range serving, ffmpeg transcoding, transcode cache, HLS,
                              playback decisions, artwork extraction and resizing
    trackside-auth/           passwords, sessions, API keys, app passwords, media tokens,
                              pairing, setup token, rate limits, axum extractors
    trackside-acquire/        Provider trait, ytdlp and soulseek providers, ranking,
                              verification, the durable acquisition queue
    trackside-enrich/         read-only Last.fm client and the background enrichment worker
    trackside-subsonic/       /rest router, XML and JSON encoding, Subsonic semantics
    trackside-api/            /api/v1 router (native and admin), wire types, SSE
    trackside-cloud/          Trackside Cloud: per-user sync of the apps' own data, its
                              merge rules, storage and paging (ADR 0012)
    trackside-server/         the `trackside` binary: CLI, wiring, TLS, mDNS, scheduler,
                              backups, shutdown, embedded dashboard
  dashboard/                  static HTML, CSS and JavaScript modules, embedded at build time
  packaging/                  Dockerfile, compose files, systemd unit, NAS templates
  tests-e2e/                  black-box end-to-end and performance tests (not shipped)
  docs/
```

Ten shipped crates. Acquisition is its own crate rather than part of `trackside-scan` so that
its code can be owned by one work package, reviewed on its own, and compiled out with the
`acquire` cargo feature of `trackside-server` (on by default) by anyone who builds without it.

### Dependency graph

| Crate | Workspace dependencies |
|---|---|
| trackside-core | tracks-model |
| trackside-media | core |
| trackside-auth | core |
| trackside-scan | core, media (for `ArtworkStore`) |
| trackside-acquire | core, media (probes, decode check, stream-through HLS) |
| trackside-enrich | core |
| trackside-subsonic | core, auth, media |
| trackside-cloud | core |
| trackside-api | core, auth, media, cloud |
| trackside-server | all of the above |

Rules:

- `trackside-core` depends on nothing else in the workspace. Every other crate depends on it.
- `trackside-subsonic` and `trackside-api` do **not** depend on `scan`, `acquire` or `enrich`;
  they reach those through the `ScanControl`, `AcquireControl` and `EnrichControl` traits
  declared in `core`, which `server`
  wires to the real implementations. This keeps the HTTP crates testable with fakes and the graph
  acyclic.
- `trackside-acquire` imports files through `ScanControl` from core, not by depending on `scan`.
- Only `trackside-server` depends on everything. It contains no business logic: config loading,
  construction, routing composition, listeners, the scheduler, signals and the CLI.
- SQL lives only in `trackside-core` (repositories) and the migrations. Other crates call
  repository functions with a `&Connection` or a `WriteTx`. The one exception is
  `trackside-cloud`, which owns the SQL for the `cloud_*` tables of migration 4 (ADR 0012);
  the migration itself stays in core.

### What each crate owns

| Crate | Owns | Does not own |
|---|---|---|
| core | schema and migrations, `Db`, ids, records, repositories, change log, sync paging, search, similarity, ingest (matching a scanned file to an existing song, section 6, and writing it), `Config` types and loading, `EventBus`, `Clock` | HTTP, files on disk other than the database, subprocesses |
| scan | walking, stat, tag parsing, digests, folder hashing, deciding what to rescan, the end-of-run missing pass and its guards, watcher, availability monitor, import of a single file, uploads processing | SQL (it calls `core::ingest`), HTTP, transcoding |
| media | HTTP range responses for files, transcode jobs and cache, HLS playlists and segments, playback decision, artwork extraction from files, resized artwork cache | auth decisions (it receives an already authorised request), database writes other than artwork rows |
| auth | Argon2id, session tokens, API keys, app password sealing, media tokens, pairing codes, setup token, invites validation, rate limiters, `Principal`, axum extractors | user CRUD SQL (calls core repositories) |
| acquire | providers, ranking, verification, tagging staged files, the job runner, staging cleanup, provider health | the scanner (calls `ScanControl::import_file`) |
| enrich | Last.fm requests and parsing, pacing and backoff, what to look up next, the on-demand queue | SQL (calls `core::repo::enrichment`), ranking (in `core::similar`) |
| subsonic | `/rest/*` parsing (GET and formPost), XML and JSON envelopes, error codes, mapping records to Subsonic shapes | domain logic beyond mapping |
| api | `/api/v1/*` handlers, wire types (serde), admin endpoints, SSE, fixtures for Tracks | domain logic beyond mapping |
| cloud | the apps' synced records: merge rules, the per-user change seq, chunked writes, pruning, quotas, the device registry | HTTP, anything outside the `cloud_*` tables |
| server | CLI, wiring, listeners, TLS, mDNS, scheduler, backups, health endpoints, dashboard asset serving, metrics text | domain logic |

### Shared code with Tracks

`tracks-model` provides `folded`, `song_key`, `album_key`, `search_folded`, `is_near` and
`credits_by_name`. The server calls them at ingest and stores the results (`songs.song_key`,
`albums.album_key`, `artists.identity`, `*.search_text`). The workspace pins `tracks-model` as a
git dependency on an exact revision of the Tracks repository, and the revision moves with app
releases. A change to `tracks-model` that alters folding output must come with a server migration
that recomputes stored keys (section 13).

---

## 3. Concurrency model

- **Runtime.** One tokio multi-threaded runtime for HTTP, SSE, timers and subprocess I/O.
  Worker threads default to the CPU count.
- **Database.** `Db` follows Engine's `tracks-store` pool: one writer connection behind a
  `Mutex`, and a pool of reader connections (default `min(8, cpus)`) over WAL. The API is
  synchronous; async callers use `Db::read` and `Db::write`, which run the closure on tokio's
  blocking pool. A write closure gets a `WriteTx` wrapping a `rusqlite::Transaction` and a change
  recorder. Writes are short: the scanner commits at most 200 files or 250 ms of work per
  transaction, so a listen submission never waits behind a long scan transaction for more than
  about a quarter of a second.
- **Change propagation.** After a write commits, `Db::write` publishes a
  `ServerEvent::Changed { seq, scope }` on the `EventBus` (a `tokio::sync::broadcast` with
  capacity 1024). SSE subscribers that lag get a `resync` event and the client calls `/sync`.
- **Scanner.** Dedicated OS threads (`scanner.threads`, default `min(4, cpus)`) read tags and
  digests. A walker thread feeds them paths over a bounded channel (capacity 1024), and a batcher
  sends results to the writer. Filesystem calls on library paths never run on runtime worker
  threads.
- **Hung network filesystems.** A hard-mounted NFS share can block `stat` forever. Opening and
  stat-ing a library file to serve it goes through a per-library semaphore (16 permits) on the
  blocking pool; the permit stays with the call until it returns, even if the client has gone,
  and a request that waits 2 s for one gets `503 library_unavailable`. The availability monitor
  runs its probe on a dedicated thread with a 5 s deadline. A hung share therefore marks its
  library unavailable and costs at most 16 blocked threads for serving, never the runtime.
  Async database writes wait for their turn before taking a blocking thread, so a stuck or slow
  write holds one thread, not one per queued request.
- **Transcoding.** ffmpeg runs as `tokio::process` children with `kill_on_drop`. A global
  semaphore (`media.transcode_max_concurrent`, default `max(1, cpus / 2)`) limits concurrent
  jobs, with three priorities: interactive playback, prefetch, download. Jobs are single-flight
  by `(song, content_version, profile)`; later requests join the running job.
- **Acquisition.** A runner task leases jobs from the `acquisitions` table. Each provider has its
  own semaphore and token-bucket rate limiter. Provider subprocesses (yt-dlp) and HTTP calls
  (slskd) honour a `CancellationToken`.
- **Password hashing.** Argon2id runs on the blocking pool behind a semaphore of 2 permits, so a
  burst of logins cannot starve the CPU.
- **Shutdown.** SIGTERM or SIGINT cancels a root `CancellationToken`: listeners stop accepting,
  open requests get `server.shutdown_grace` (10 s), the scanner stops at its next batch boundary,
  acquisition jobs release their leases, ffmpeg and yt-dlp children are killed, and the writer
  runs `PRAGMA optimize` and a WAL checkpoint before exit. With systemd, `sd_notify` reports
  `READY=1`, `STOPPING=1` and watchdog pings.
- **No locks across `.await`.** Shared in-memory state uses `std::sync::Mutex` held only for
  short synchronous sections, or `tokio::sync` primitives where waiting is part of the design.

---

## 4. Data flow

### Scan

```
library root --walk--> folder (mtime, listing hash unchanged? skip) --> file
  --stat--> known (same size, mtime)? skip
  --tags (lofty) + audio digest--> ScannedFile
  --batch (200 / 250 ms)--> WriteTx:
        match to existing song (path, digest, MBID, tag identity)
        upsert artists (identity = folded name), albums (identity), song, credits, genres, lyrics, artwork row
        change_log upsert per touched entity
  --commit--> EventBus Changed
end of run: songs not seen and not under an unreadable folder -> missing_since = now
            library guard (section 16) can abort the run before anything is marked missing
```

### Play from Tracks

```
Tracks --POST /api/v1/playback/decisions (client profile, network)--> decision
   direct: /api/v1/songs/{id}/stream            -> range-served original
   file:   /api/v1/songs/{id}/stream?profile=x  -> range-served cached transcode
   hls:    /api/v1/transcodes/{job}/index.m3u8  -> segments as ffmpeg writes them
Tracks --POST /api/v1/listens (batch, client ids)--> listens, play_stats, change_log
```

### Delta sync

```
Tracks --GET /api/v1/sync (no cursor)--> snapshot pages pinned at seq S
Tracks --GET /api/v1/sync?cursor=(epoch, S)--> changes with seq > S (upserts carry full records,
                                                deletes carry ids), next cursor
SSE "changed" --> client calls /sync with its cursor
```

### Acquisition

```
Tracks --POST /api/v1/acquisitions--> dedupe (library, in-flight) --> queued
runner --> search enabled providers in parallel --> rank --> fetch best into staging
       --> verify --> tag --> move into the acquired library --> ScanControl::import_file
       --> song id --> state done --> SSE "acquisition" + "changed"
```

---

## 5. Data model

SQLite, WAL, `foreign_keys = ON`, `STRICT` tables. Every timestamp is Unix milliseconds in UTC,
named `*_at`. Library entities have an internal `INTEGER PRIMARY KEY` named `id` for joins and a
public text id named `pid` that clients see (section 6). Per-user tables refer to songs by
`song_pid`, not by internal key, so a user's stars, listens and playlists survive a song being
purged and coming back, and survive a full database rebuild of the library tables.

Paths on disk are stored as `BLOB` (the exact OS bytes, relative to the library root) with a
lossy UTF-8 `*_display` column for humans. Names from tags are stored NFC-normalised.

Connection pragmas (set per connection, not in migrations): `journal_mode = WAL` (writer),
`synchronous = NORMAL`, `foreign_keys = ON`, `busy_timeout = 5000`, `cache_size = -32000`,
`mmap_size = 268435456`, `temp_store = MEMORY`, `journal_size_limit = 67108864`.

Migrations use `rusqlite_migration` on `user_version`, as Engine does. Migration 1 follows in full;
migrations 2 and 3 are after it. The files themselves are in `crates/trackside-core/migrations/`.

```sql
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
```

Migration 2 adds re-authentication for sensitive dashboard actions (section 7), every
permission on invites, and the hourly and daily rollups behind the dashboard's charts
(api.md section 16, Stats):

```sql
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
```

Migration 3 adds what Last.fm enrichment stores beside the similarity tables (Suggestions and
enrichment, below):

```sql
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
```

Notes on the schema:

- `change_log.seq` is not `AUTOINCREMENT`. The writer keeps `next_seq` in memory, loaded as
  `max(seq) + 1` at open, and every change is an `INSERT ... ON CONFLICT (kind, entity_pid,
  user_id) DO UPDATE SET seq = excluded.seq, op = excluded.op, changed_at = excluded.changed_at`.
  Because there is one writer, seqs commit in order, so a reader never sees seq N+1 without N.
- Tombstones (`op = 'delete'`) older than `sync.tombstone_retention` are pruned daily and
  `sync.pruned_through` is raised to the highest pruned seq. A client whose cursor is below it
  must take a snapshot again.
- Library-wide entities carry `library_id` and `user_id = 0`; per-user entities carry the
  owner's `user_id`. A public playlist's change row has `user_id = 0`, so every user's sync sees
  it; a private one has the owner's id. Making a public playlist private writes a `delete` row with
  `user_id = 0` (a tombstone for everyone else) and an `upsert` row for the owner.
- Artists are global, aggregated over libraries through `artist_libraries`. An artist row is
  visible to a user when at least one of its `artist_libraries` rows is in the user's libraries.

### Delta sync rules

- A cursor is opaque base64url of `{v:1, epoch, seq}`.
- A request with no cursor starts a snapshot: the server records `S = max(seq)` and pages songs,
  albums, artists, playlists, stars, ratings and play stats visible to the user by internal id,
  every page carrying `S`. The final page returns cursor `(epoch, S)`. Changes committed during
  the snapshot are replayed by the first delta, and all records are full upserts, so replays are
  harmless.
- A delta returns change_log rows with `seq > cursor.seq` where `user_id IN (0, me)` and
  `library_id IS NULL OR library_id IN (my libraries)`, joined to the current record for upserts.
- If `cursor.epoch != users.sync_epoch` or `cursor.seq < sync.pruned_through`, the server answers
  `410 Gone` with code `sync_reset` and the client takes a new snapshot. The epoch is bumped when a
  user gains or loses a library, when a library is deleted, and after a migration that rewrites
  ids or keys.

### Trackside Cloud

Migration 4 adds `cloud_state` (each user's own change seq), `cloud_items` (every synced record
as the app's JSON, keyed by user, entity and client id, with its `version`), `cloud_entities`
(items, bytes and the pruned-through version per user and entity), `cloud_meta` and `cloud_devices` (the
apps' device ids and what each was restored from). All cascade from `users`. They are separate
from `change_log`: cloud writes never wake library `/sync` clients, and the cloud cursor is the
user's own seq plus a sync epoch that changes when the database is restored or replaced (the
`<database>.cloud-instance` sidecar file next to the database tells the server which copy it last
ran, and the highest count of cloud changes it saw, so a rolled-back file is caught too). A small
`cloud_meta` row holds that count. api.md section 19 has the API and ADR 0012 the merge rules.

---

### Suggestions and enrichment

Similar songs, similar artists, top songs and artist biographies come from local signals and,
when the admin sets a Last.fm API key, from Last.fm. The key is one for the whole server, set in
the dashboard (sealed at rest with the secret key, like app passwords) or by
`enrichment.lastfm_api_key`; nobody needs a Last.fm account. `trackside-enrich` calls only four
unauthenticated read methods, `artist.getSimilar`, `artist.getTopTracks`, `artist.getInfo` and
`track.getSimilar`, with the key, artist and track names, `autocorrect=1` and a
`Trackside/<version>` User-Agent. It never signs a request, holds a session, or writes to
Last.fm, and sends nothing about users.

- **Worker.** One task makes one call at a time, at least 400 ms apart (Last.fm allows 5 a
  second). An artist takes three calls, a song one. It works through, in order: lookups a
  request is waiting for; then artists by plays on this server and songs anyone has played, by
  plays, whose lookup is missing or expired. With nothing due it looks again hourly.
- **Storage.** Similar artists go to `similar_artists` and similar songs to `similar_songs`
  with `source = 'lastfm'` and Last.fm's match as `score`. Top songs, biographies and the names
  of suggested songs go to the migration 3 tables. `enrichment_fetches` records every lookup
  with `expires_at`: 30 days after an answer, 7 days after "not found" (Last.fm error 6), so a
  restart fetches nothing that has not expired.
- **Failures.** Error 29, 8, 11 and 16, HTTP 429 and 5xx, and transport errors keep the target
  at the front and wait 30 s, doubling to an hour, before the next call. Errors 4, 10 and 26
  (the key) stop all calls until the key changes. Anything else is stored as not found. The
  latest error is shown in the dashboard until the next success.
- **On demand.** Similar songs, top songs, similar artists and the Subsonic equivalents ask for
  their seed song and artist first in line and wait at most 1.5 s (`ON_DEMAND_WAIT`), then
  answer from what is stored. The artist page never waits.
- **Ranking.** `core::similar` adds Last.fm rows to the local signals: a similar song is worth
  4 times its match, a song by a similar artist 2 times. The caller's own star (1.5) and plays
  (0.3 each, up to 5) raise a candidate, so the same seed gives each user their own order.
  Similar artists get up to 0.2 from the caller's star and plays. Top songs score plays on the
  server, the caller's plays again, 5 for the caller's star and `20 / (1 + rank)` for Last.fm's
  rank.
- **Access.** Library results come from repository reads filtered by `Principal.libraries`, as
  everywhere. Songs Last.fm suggests that the caller cannot see are returned only by name, in
  the native API's `external` list, and only to callers with `features.acquisition` and the
  `request` scope. Nothing on the wire names Last.fm.

## 6. Id strategy

Public ids are a one-letter kind prefix and 20 lowercase Crockford base32 characters (100 bits):

| Prefix | Entity | Derivation |
|---|---|---|
| `s` | song | from the audio digest, then persisted (below) |
| `a` | album | `BLAKE3("album" ‖ library.pid ‖ album.identity)` |
| `r` | artist | `BLAKE3("artist" ‖ artist.identity)` |
| `l` | library | random |
| `f` | folder | `BLAKE3("folder" ‖ library.pid ‖ rel_path)` |
| `c` | artwork | `BLAKE3(image bytes)` (content-addressed) |
| `p` | playlist | random |
| `u` | user | random |
| `d` | device | random |
| `k` | api key | random |
| `q` | acquisition | random |
| `x` | candidate | `BLAKE3("cand" ‖ provider ‖ provider_ref)` |

Other ids (scan runs, imports, pairings, invites) use the same alphabet with prefixes `n`, `i`,
`g`, `v`. Ids are case-insensitive on input and always emitted lowercase. Subsonic receives them
unchanged as strings.

### Song ids

The song id must survive rescans, tag edits, renames, moves between folders and libraries,
case-only renames on case-insensitive filesystems, a library share that disappears and returns,
and a rebuild of the database from an empty file.

**Audio digest.** For each audio file the scanner finds the audio payload, the bytes that are
not tags: after the ID3v2 tag and before ID3v1, APEv2 and Lyrics3 for MP3 and raw AAC; after the
last metadata block for FLAC; the `mdat` box contents for MP4 and M4A; the pages after the
comment and setup headers for Ogg Vorbis and Opus; the `data` chunk for WAV and the `SSND` chunk
for AIFF. The digest is

```
BLAKE3("trackside audio v1" ‖ payload_length_le64 ‖ window(0) ‖ window(mid) ‖ window(end))
```

where each window is 64 KiB of payload (or all of it for short payloads). That is at most
192 KiB read per file, so first scans stay fast on a NAS, and a tag edit, which rewrites only
the tag region, leaves the digest unchanged. If the payload cannot be located the digest falls
back to the same formula over the whole file, recorded in `tag_problem` as `digest_whole_file`.

**Assigning an id.** When a file is new to the database the scanner resolves it in this order:

1. **Same path** in the same library (`UNIQUE(library_id, rel_path)`): same song. If size or
   mtime changed, re-read tags and digest and bump `content_version` when the digest changed.
2. **Missing song with the same digest** (any library, `missing_since IS NOT NULL`, or present
   but at a path that this scan found gone): a move. Keep the id, update path, folder and library.
   If several match, prefer the one whose file name is unchanged, then the lowest internal id.
3. **Missing song with the same `mbz_track_id`**, or with the same `song_key`, `album_key`,
   disc and track number, file name and duration within 1 s: a re-encode or replacement at a new
   path. Keep the id, bump `content_version`.
4. **Otherwise a new song.** Its id is derived from the digest:
   `'s' + base32(BLAKE3("song" ‖ audio_digest))[..20]`. If that id is already taken by a present
   song (an exact duplicate file at another path), derive again with the path mixed in:
   `BLAKE3("song" ‖ audio_digest ‖ library.pid ‖ rel_path)`.

Once assigned, an id is stored and never recomputed. Derivation only matters for new songs, and
it means a database rebuilt from scratch gives the same file the same id it had before, so
Tracks' stored `server:<entry>:<songID>` values, playlists and stars keep working even after a lost
database.

Missing songs are kept with `missing_since` set for `scanner.missing_grace` (30 days), still
listed by sync with `missing: true` (Tracks greys them out), and purged after the grace period.
Purging deletes the library row and writes a tombstone; per-user rows that name the song by
`song_pid` stay, so if the file ever returns its derived id matches again and everything
reattaches.

**Imported and acquired songs** get their id at the moment of import (section 9), by the same
rules, inside the same write transaction that creates the row. A later watcher event or rescan
for that file finds the same path, size and mtime and does nothing.

### Album and artist ids

Album identity is `'mb:' + release MBID` when tagged, otherwise `tracks_model::album_key(album,
lead album artist identity)`, and for a blank album title `'folder:' + folder pid`. Artist
identity is `tracks_model::folded(name)` of each credited name from
`tracks_model::credits_by_name`, so "A feat. B" credits both artists exactly as Tracks does.
These ids are derived and therefore stable across rebuilds; they change only when the tags that
define the identity change, which is the correct behaviour (the album really is a different
album). When an album loses its last song it is deleted and tombstoned.

---

## 7. Auth design

### Principals and credentials

A request is authenticated into a `Principal { user, role, permissions, libraries, via }`, where
`via` says how: session, device key, API key, app password, or media token. Libraries are the
resolved set of visible library ids (all for admins).

| Credential | Used by | Format | Stored as | Lifetime |
|---|---|---|---|---|
| Login password | people, at login | chosen by the user, 1 to 1024 bytes | Argon2id PHC (m 19456 KiB, t 2, p 1) | until changed |
| Session | dashboard | 32 random bytes, base64url, `__Host-trackside_session` cookie | SHA-256 | idle 14 days, absolute 90 days |
| Device key | Tracks and native clients | `tsk_` + 43 base64url chars (256 bits) | SHA-256 | until revoked |
| API key | OpenSubsonic `apiKey` clients | same format, `kind = api_key` | SHA-256 | until revoked |
| App password | legacy Subsonic clients (`u`+`t`+`s`, `u`+`p`) | 24 chars from an unambiguous alphabet, server-generated | SHA-256 for lookup plus XChaCha20-Poly1305 sealed copy | until revoked |
| Media token | AVPlayer URLs, art URLs | `mt=` payload `{u, d, e, x}` plus BLAKE3 keyed MAC | not stored; checked against the device's `media_epoch` | 7 days |
| Setup token | first admin | 32 random bytes, base32 | file `<data>/setup-token` (0600) and log | until an admin exists |
| Invite | new users | 32 random bytes, in a link | SHA-256 | default 7 days, single use |
| Password reset | locked-out members | 32 random bytes, in a link | SHA-256 | 24 hours, single use |
| Pairing offer | QR and typed code | 128-bit QR secret, 8-character typed code | SHA-256 | 10 minutes, 5 attempts |
| Pairing request | device-initiated | 6 digits shown on the device, plus a poll secret | SHA-256 | 10 minutes |

Login passwords are never accepted by the Subsonic layer and never stored reversibly.

**Scopes.** `library` (read the library and own data), `stream`, `download`, `write` (stars,
ratings, playlists, listens, play queue), `request` (acquisition, if the user also has
`can_request`). Device keys get all five; API keys and app passwords default to `library stream
write`; admins choose when creating them. Admin endpoints require a session; no key can reach
them, which keeps a stolen phone key from becoming a server takeover.

**Secret key.** `<data>/secret.key` holds 32 random bytes (0600), created at first start, or read
from `auth.secret_key_file`. Subkeys are derived with `blake3::derive_key` for: sealing app
passwords and pending pairing keys, media token MACs, and invite and reset link MACs. Losing the
file invalidates app passwords and media tokens only; device keys, sessions and passwords keep
working because they are hashes. Backups include it when `backup.include_secret_key` is true
(default) and say so.

### First run

On start with no users the server writes `setup.state = needs_admin`, generates a setup token,
writes it to `<data>/setup-token` and logs it in a banner:

```
Trackside needs an admin account. Open http://<lan-address>:4747/setup and enter this setup token:
  7KQ2-M9XD-...-P4TA
The token is also in /data/setup-token. It stops working once the admin exists.
```

`POST /api/v1/setup` with the token creates the first admin, deletes the token file and signs the
browser in. For unattended installs, `TRACKSIDE_ADMIN_USERNAME` with
`TRACKSIDE_ADMIN_PASSWORD_FILE` creates the admin at start instead. `trackside admin
create-setup-token` makes a new token when there is still no admin.

### Invites, roles and access

Admins create invites with a role, permission flags and library grants. The invitee chooses a
username and password at `/invite/<token>`. Roles are `admin` (everything, all libraries) and
`member` (own data, granted libraries, flags `can_download`, `can_upload`, `can_request`,
`legacy_subsonic`, `max_bitrate_kbps`). The last active admin cannot be demoted, disabled or
deleted.

### Login, sessions, CSRF

Dashboard login sets `__Host-trackside_session` with `HttpOnly; Secure; SameSite=Strict;
Path=/` (the `Secure` flag and `__Host-` prefix are dropped only when the request arrived over
plain HTTP from a private or loopback address, so a LAN-only install still works; a request with
forwarding headers counts only when its peer is in `server.trusted_proxies`, so a tunnel on the
host never makes internet clients look local). State-changing
requests with a session must carry `X-Trackside-CSRF: 1` and an `Origin` matching the request's
host; browsers cannot send that header cross-origin without a CORS preflight, which the server
never grants. Changing a password ends every other session; revoking devices is a separate choice.
Passwords and keys (login, sessions, re-authentication, password changes, setup, invites,
resets, pairing, bearer keys and Subsonic credentials) are refused with `403 https_required`
over plain HTTP from any client that isn't nearby, before they are checked; `server.local_networks`
adds networks such as a LAN's global IPv6 prefix to the nearby ranges, and
`server.tailscale_is_local = false` takes 100.64.0.0/10 out of them for hosts behind
carrier-grade NAT. A peer that isn't a trusted proxy is judged by its own address only; if it
sends forwarding headers it may be relaying the internet, so it is refused until the admin
trusts it. The refusal carries `details.https_urls`, the HTTPS entries of the server's own
address list, so a client can move to one without signing in first.
Over plain HTTP the cookie is named `trackside_session`, because browsers refuse the `__Host-`
prefix without `Secure`. A request over HTTPS ignores a `trackside_session` cookie, so a cookie
set over plain HTTP (or planted by someone on the network) can't stand in for the secure one.
The plain cookie itself is accepted only where a password would be: brought over plain HTTP
from a client that isn't nearby (split-horizon DNS, a laptop away from home) it is refused with
`https_required` and cleared.

### Re-authentication

A session remembers when it last proved its password (`sessions.recent_auth_at`, migration 2;
sign-in counts). For 10 minutes after that, sensitive changes go ahead; later they answer `403
reauth_required` until the browser calls `POST /api/v1/auth/reauth` with the password, which
goes through the same rate limits as sign-in. The session user carries `recent_auth_until` so
the dashboard can ask before it is refused. Keys are not affected: they cannot reach admin
routes or create keys, and a password change from a device key goes ahead as it is (it needs the
current password anyway). The list of guarded actions is in api.md section 6: making an admin, changing a
role or disabling an account, reset links, deleting users, `auth.*` settings, downloading or
deleting a backup, and setting a provider's API key. This limits what a session cookie left in
an unlocked browser can do.

### Legacy Subsonic

`u`+`t`+`s` and `u`+`p` (plain or `enc:`) are verified against the user's active app passwords
(decrypting each, there are few), only when the user has `legacy_subsonic` and the server has
`auth.legacy_subsonic = true` (default true; per-user default off). `apiKey` is always accepted.
Mixing `apiKey` with `u` is error 43. ADR 0005 has the reasoning. Turning `auth.legacy_subsonic`
off in the dashboard applies to the next request, so it stops every app password at once.

### Media tokens

AVPlayer cannot reliably send an `Authorization` header, so stream and art URLs carry `mt`, a
token minted by `POST /api/v1/media-token` for the calling device. It is accepted only on media
routes (`/api/v1/songs/{id}/stream`, `/download`, `/api/v1/art/*`, `/api/v1/transcodes/*`,
`/api/v1/acquisitions/{id}/stream*`), is bound to the user, device and `media_epoch`, and expires
after 7 days. Revoking a device bumps its epoch. The server redacts `mt`, `apiKey`, `t`, `s` and
`p` from every log line.

### Rate limits

- Per IP, on login, setup, invite acceptance, pairing and Subsonic auth failures: token bucket,
  10 per minute, burst 5. Failures beyond it get `429` with `Retry-After` (Subsonic: error 40 with
  the same header).
- Per username and IP pair, after 5 consecutive failures: exponential delay from 2 s up to
  15 minutes. Keyed by the pair so an attacker cannot lock a user out from elsewhere; a global
  per-username ceiling of 100 failures an hour slows distributed guessing. The ceiling does not
  apply at an address the account signed in from successfully in the last 30 days, so guesses
  from elsewhere cannot lock the owner out at home. App password failures (`/rest`) are counted
  apart from the login password, so a legacy client with a stale app password cannot hold up
  sign-in.
- Per server: `server.max_connections` open connections across every listener; one over that
  is closed at once. `server.header_timeout` bounds every request's headers and idle HTTP/1
  keep-alive connections; HTTP/2 connections are pinged every 30 s and TCP keepalive probes find
  peers that vanished in the middle of a request.
- Per key: 50 requests a second, burst 200, excluding media bytes.
- Per user: 8 concurrent streams and 3 concurrent transcodes; over that, `429` with
  `Retry-After: 1`. 16 open event streams; over that, `429` with `Retry-After: 5`.
- Limiter state is pruned every minute, or sooner once it tracks 50,000 addresses or names, so
  addresses that failed once are not kept forever.
- Auth failures log `auth_failure user=<name> ip=<ip> via=<method>` at warn, in a fixed format
  for fail2ban.

### Recovery

`trackside admin reset-password <username>` (run on the host, works while the server runs)
prints a one-time password, forces a change at next login, ends sessions, and audits as `cli`.
`trackside admin set-role <username> admin` restores an admin. Members locked out get a reset
link from an admin.

---

## 8. Streaming, transcoding and artwork

### Original files

`trackside-media` implements RFC 9110 range serving itself, so that auth, ETags and library
availability are handled in one place:

- `Accept-Ranges: bytes`, exact `Content-Length`, `Content-Type` from the container
  (`audio/flac`, `audio/mp4`, `audio/mpeg`, `audio/ogg`, `audio/wav`, `audio/aiff`).
- Strong `ETag: "<song pid>.<content_version>"`, `Last-Modified` from the file's mtime,
  `If-None-Match`, `If-Modified-Since`, `If-Range` (ETag or date).
- Single ranges including open (`bytes=100-`) and suffix (`bytes=-500`) forms. Multiple ranges
  are answered with the whole file as `200`, which RFC 9110 allows. An unsatisfiable range gives
  `416` with `Content-Range: bytes */<len>`. `HEAD` is supported.
- Reads with a 256 KiB buffer from `tokio::fs::File`; hyper applies backpressure, so a slow
  client holds one buffer, not the file. A stream with no write progress for 120 s is closed.
- If the file changed since the database row (size or mtime differ at open), the response is
  `409` with `content_changed` and a quick folder scan is queued, rather than serving bytes that
  do not match the ETag.
- Never compressed.

### Playback decision

Tracks sends its device profile and network to `POST /api/v1/playback/decisions` and gets one of:

- `direct`: the original, when its codec and container are in the client's direct-play list and
  its bitrate is under the limit for this network and the user's `max_bitrate_kbps`.
- `file`: a complete cached transcode for the chosen profile, range-served exactly like an
  original, with ETag `"<pid>.<content_version>.<profile>.<encoder id>"`.
- `hls`: a transcode that is not cached yet, as an HLS VOD playlist with fMP4 segments that
  ffmpeg is writing.

Profiles (`media.profiles`, fixed list in v1):

| Profile | Codec and container | Use |
|---|---|---|
| `aac-256` | AAC-LC 256 kbps, M4A | default for cellular, transparent for most listeners |
| `aac-128` | AAC-LC 128 kbps, M4A | data saver |
| `mp3-320`, `mp3-128` | MP3 CBR | Subsonic clients asking for `format=mp3` |
| `opus-128` | Opus in Ogg | Subsonic clients asking for opus |
| `flac` | FLAC | lossless for clients that cannot play ALAC or WAV originals |

The encoder is ffmpeg's native `aac`; libfdk_aac is never shipped. The command is built as an
argument vector, never a shell string, and is not editable at runtime (an editable command is
remote code execution):

```
ffmpeg -nostdin -hide_banner -loglevel error -i <file> -map 0:a:0 -map_metadata -1
       -fflags +bitexact -flags:a +bitexact -c:a aac -b:a 256k -ar <44100|48000>
       -movflags +faststart -f ipod <cache>/<pid>.<ver>.aac-256.m4a.part
```

Bit-exact flags make the output identical for the same input and ffmpeg build, so the ETag (which
includes a hash of `ffmpeg -version` output as the encoder id) stays valid across cache
evictions and background download resumes keep working.

**HLS path.** For an uncached transcode the job runs one ffmpeg process with the HLS muxer
(`-f hls -hls_segment_type fmp4 -hls_time 6 -hls_playlist_type vod`) into
`<cache>/hls/<job>/`. The server does not serve ffmpeg's playlist; it writes its own VOD playlist
up front from the known duration (`ceil(duration / 6)` segments, the last one shorter), so
AVPlayer sees the full length and can seek at once. A segment request waits up to 10 s for that
segment's file to be complete (ffmpeg renames on completion). A request for a segment more than
three segments ahead of the encoder starts a second ffmpeg process at that offset
(`-ss`, `-output_ts_offset`, `-start_number`), and the first is stopped once the second has
overtaken it. When the single continuous run finishes, the segments are remuxed
(`-c copy +faststart`) into the cached M4A, so the next play is `file`. Since one encoder run
produces all segments of a normal play, there are no priming gaps at segment boundaries.

**Gapless.** Files and cached M4A transcodes carry encoder delay and padding (ffmpeg writes an
`elst` edit list); AVQueuePlayer plays them gaplessly. HLS items are not guaranteed gapless, so
Tracks calls `POST /api/v1/transcodes/prepare` for the next queued song when the current one
starts, which turns the next play into a `file` decision in almost every case.
Implementation must confirm on a device that AVFoundation honours ffmpeg's edit list for AAC;
if it does not, the media package writes an `iTunSMPB` atom as well.

**Subsonic `stream`.** `format=raw`, or no `format` and no `maxBitRate`, serves the original. A
transcode request serves a cached file with ranges if present; otherwise it streams ffmpeg's
progressive output (chunked, no ranges) while teeing it into the cache, which is what Subsonic
clients expect. `estimateContentLength=true` sets `Content-Length` to the bitrate times duration
estimate; if the encoder finishes short, the connection is closed at the real end and clients
that asked for the estimate handle the short body. `timeOffset` (transcodeOffset) runs ffmpeg
with `-ss` and is never cached.

**Overload.** If an interactive job would wait more than 2 s for a transcode slot and the client
allowed fallback, the decision returns `direct` with `reason: "transcoder_busy"`. Otherwise the
request gets `503` with `Retry-After`. Prefetch and download jobs wait.

**Without ffmpeg.** At start the server runs `ffmpeg -version` (path from `media.ffmpeg_path`,
then `PATH`). If missing, capabilities report `transcoding: false`, decisions are always `direct`,
Subsonic transcode requests get the original (Subsonic allows a server to ignore the request,
and Tracks' quality badge already detects it), and the dashboard shows why.

**Cache.** `<cache>/transcodes`, LRU by last access, capped by `media.transcode_cache_max`
(default 10 GiB) and kept above `media.cache_min_free` free space on the volume. When free space
is too low even after eviction, transcodes stream without caching.

### Artwork

Sources in order (configurable): sidecar `cover.*`, `folder.*`, `front.*`, `album.*` in the album
folder; embedded front cover; embedded first picture. Artist pictures: `artist.*` in the artist
folder or one level up. The original bytes are hashed into an artwork id and copied to
`<cache>/art/<id[1..3]>/<id>`, reproducible from `artwork.source_path`. Resized JPEGs (quality 85,
Lanczos3) for requested sizes are made on first request, single-flight, and cached next to the
original; sizes are snapped to 64, 128, 256, 300, 600, 1200 or the original to bound the cache.
Responses carry `Cache-Control: private, max-age=31536000, immutable` because ids are
content-addressed. Missing artwork is a `404`, never a placeholder image.

---

## 9. Acquisition

Admins can let users get songs they do not have yet from two built-in providers, both off by
default:

- **ytdlp**: runs the `yt-dlp` executable as a subprocess.
- **soulseek**: an HTTP client for an slskd instance's REST API. slskd runs as its own
  container; Trackside never speaks the Soulseek protocol.

There is no provider for streaming services and the `Provider` trait is not public API for
third-party code. ADR 0006 records the decision.

### The provider interface

```rust
pub trait Provider: Send + Sync + 'static {
    fn name(&self) -> ProviderName;
    fn search<'a>(&'a self, query: &'a SearchQuery, cancel: CancellationToken)
        -> BoxFuture<'a, Result<Vec<Candidate>, ProviderError>>;
    fn fetch<'a>(&'a self, candidate: &'a Candidate, staging: &'a Path,
                 progress: ProgressSink, cancel: CancellationToken)
        -> BoxFuture<'a, Result<FetchedFile, ProviderError>>;
    fn fetch_streaming<'a>(&'a self, candidate: &'a Candidate, staging: &'a Path,
                           progress: ProgressSink, cancel: CancellationToken)
        -> Option<BoxFuture<'a, Result<StreamingFetch, ProviderError>>> { None }
    fn resume<'a>(&'a self, candidate: &'a Candidate, staging: &'a Path,
                  progress: ProgressSink, cancel: CancellationToken)
        -> BoxFuture<'a, Result<FetchedFile, ProviderError>>;
    fn health<'a>(&'a self) -> BoxFuture<'a, ProviderHealth>;
}
```

The full types are in build-plan.md section 3.5. Cancelling the token is how a fetch is
cancelled; `fetch_streaming` is implemented only by providers that can play while downloading
(ytdlp).

### Lifecycle of a request

```
queued -> searching -> downloading -> verifying -> importing -> done
   \           \             \             \            \
    +-----------+-------------+-------------+------------+--> failed(reason) | cancelled
```

1. **Request.** `POST /api/v1/acquisitions` with title and artist (and optionally album, duration,
   a candidate token, `allow_versions`, `instant`). Checks: acquisition enabled, user has
   `can_request` and the `request` scope, the acquired library exists and the user can see it,
   quotas.
2. **Dedupe.** Compute `request_key = song_key(title, artist)`.
   - A present song in the user's libraries with that `song_key` (and duration within tolerance
     when a duration was given): answer `200` with `state: done, existing: true, song_id`.
   - An active acquisition with the same key (from anyone): create a row with `joined_to` set and
     return it; it finishes when the other does. The user's quota is not charged.
3. **Search.** All enabled providers in parallel, each with `acquire.search_timeout` (8 s).
   Candidates are stored in `acquisition_candidates` with deterministic tokens. A provider that
   times out or fails is skipped and noted.
4. **Rank.** Hard filters, then a score (section 9.1). No candidate left: `failed(no_match)`.
5. **Fetch** the best candidate into `<data>/acquire/staging/<request pid>/`, with progress
   written to the row at most once a second and published as an SSE `acquisition` event.
   On failure, mark the candidate `tried` with its reason and move to the next candidate. When all
   are exhausted: retry the job with backoff (section 9.3).
6. **Verify** (section 9.2).
7. **Tag** the staged copy with lofty: title, artist, album, album artist, track and disc numbers,
   year, cover; from the candidate, the request, and optionally MusicBrainz when
   `enrichment.musicbrainz = true`.
8. **Import.** Move into the acquired library using `acquire.path_template` (default
   `{album_artist}/{album}/{disc}-{track:02} {title}.{ext}`, or `{artist}/Singles/{title}.{ext}`
   without an album), sanitised. Then `ScanControl::import_file(library, rel_path)` reads it,
   assigns the song id and writes the row and change log in one transaction. If the digest matches
   an existing song, the staged file is deleted and the request ends `done` with that song's id
   and `existing: true`.
9. **Done.** `song_id` is set, the row and change log commit, SSE sends `acquisition` and
   `changed`. Tracks sees the song within a second or two; nothing polls.

### 9.1 Ranking

Hard filters (a candidate failing any is dropped):

- Duration within `max(3 s, 3 %)` of the request's duration when known; within 20 % of the
  median candidate duration otherwise.
- Version words: `live`, `remix`, `karaoke`, `instrumental`, `cover`, `nightcore`, `sped up`,
  `slowed`, `8d`, `reverb`, `acapella`, `tribute` (list in `acquire.reject_words`) present in the
  candidate's folded title or folder name but not in the requested title, unless
  `allow_versions`.
- Artist: one of the candidate's credited identities (`tracks_model::credits_by_name`) equals the
  request's lead identity, or the uploader is `<artist> - Topic`, or the Soulseek path contains the
  folded artist as a folder name.
- Title: folded candidate title, with bracketed feat. credits and "official audio/video" noise
  removed, equals or contains the folded requested title.
- Size and format: container in the allowlist, size under `acquire.max_file_bytes`.

Score (higher is better), weights in code with tests:

- Lossless (FLAC, ALAC, WAV) +40; for lossy, `min(bitrate, 320) / 8`.
- Exact folded title +20, exact artist +15, album matches +10.
- Duration delta penalty, 2 points a second.
- Provider preference `acquire.prefer` (default `soulseek` then `ytdlp`) +10 for the first.
- ytdlp: `- Topic` channel +15, "official audio" +5, music video −5.
- soulseek: peer has a free upload slot +10, queue length −1 each up to −10, reported upload
  speed up to +5.

### 9.2 Verification

A staged file is accepted only if all hold:

- It is a regular file (not a symlink) inside the staging directory, after `canonicalize`.
- Magic bytes match an allowed container: FLAC, MP3 (frame sync or ID3), MP4/M4A with an audio
  track and no video track, Ogg (Opus or Vorbis), WAV, AIFF. Archives, executables, scripts and
  anything else are rejected and deleted.
- Size within limits; lofty reads properties; duration within the same tolerance as ranking.
- When ffmpeg is present, a full decode `ffmpeg -v error -i f -f null -` exits 0 with no errors
  (catches truncated downloads and corrupt streams). Without ffmpeg, lofty's properties suffice.
- The final file name is built only from sanitised tag text, never from a provider's or peer's
  path: NFC, no `/`, `\`, NUL or control characters, no leading dots, no Windows reserved names,
  at most 180 bytes per component, a ` (2)` suffix on collision.

### 9.3 Durable queue and retries

- The runner selects `state IN ('queued','searching','downloading','verifying','importing') AND
  next_attempt_at <= now AND (lease_until IS NULL OR lease_until < now)` ordered by priority and
  age, sets `lease_until = now + 5 min`, and renews the lease every minute while working.
- At start, rows with expired leases are resumed from their state: a `downloading` job with
  slskd first asks slskd whether the transfer finished; a ytdlp job reruns yt-dlp with
  `--continue` on the same staging directory; later states redo verification.
- Backoff between whole-job attempts: 1 min, 5 min, 30 min, 2 h; after 4 attempts
  `failed(exhausted)`.
- Per-provider concurrency (ytdlp 2 fetches, soulseek 3) and rate limits (ytdlp 10 searches a
  minute, soulseek 1 search every 3 s).
- Quotas: members 50 requests a day and 5 active at once (`acquire.member_daily`,
  `acquire.member_active`), server-wide 500 a day; admins are exempt from per-user quotas.
  Joined requests and dedupe hits do not count.
- Staging: a request's staging directory is removed when it ends. A janitor at start and hourly
  removes staging directories with no active request and `.part` files older than 24 h.
- Every request, cancellation and admin action is in `audit_log` (`acquire.request`,
  `acquire.cancel`, `acquire.retry`, `acquire.provider_update`), with the requester and what was
  asked for.

### 9.4 Provider details

**ytdlp.**

- Search: `yt-dlp --ignore-config --no-plugin-dirs --flat-playlist -J "ytsearch8:<artist> <title>"`,
  parsing id, title, uploader, duration.
- Fetch: `yt-dlp --ignore-config --no-plugin-dirs --no-playlist -f "bestaudio[ext=m4a]/bestaudio"
  -x --embed-metadata --embed-thumbnail --max-filesize <max> --newline
  --progress-template "%(progress)j" -P <staging> -o "audio.%(ext)s" <url>`. `-x` without
  `--audio-format` keeps the original codec, so there is no lossy re-encode; `m4a` is preferred
  because Apple devices play AAC directly (Opus results are transcoded for Tracks at play time).
- The URL is built only from a stored candidate's video id, checked against `^[A-Za-z0-9_-]{11}$`;
  clients cannot pass URLs.
- `acquire.ytdlp.cookies_file` is passed with `--cookies` when set. `acquire.ytdlp.js_runtime`
  names the JavaScript runtime recent yt-dlp versions need for YouTube (the `-acquire` image ships
  Deno); verify the exact option against the yt-dlp release pinned in the image.
- Health: `yt-dlp --version` at start and daily. The version is a date; older than
  `acquire.ytdlp.max_age_days` (60) shows as outdated. Admins can update from the dashboard:
  the server downloads the pinned-channel standalone binary and its `SHA2-256SUMS` from the
  project's GitHub releases into `<data>/bin/yt-dlp`, checks the hash, and uses it in preference to
  the image's copy. Needs outbound access to github.com.
- Bot checks and throttling: output matching "confirm you're not a bot", "Sign in to confirm",
  HTTP 429 or 403 puts the provider in cooldown (15 min doubling to 6 h), shown in the dashboard
  with a hint to add a cookies file. No evasion beyond that.
- Instant play (`fetch_streaming`): yt-dlp writes to stdout (`-o -`) and Trackside tees the
  bytes into the staging file and into an ffmpeg HLS job (section 9.5). After the download,
  tagging uses a second `--skip-download --write-thumbnail` call for the cover.

**soulseek (slskd).**

- Configured with `acquire.soulseek.url` and an API key from `acquire.soulseek.api_key_file`,
  sent as `X-API-Key`. Endpoints used (slskd API v0; check against the pinned slskd version):
  `POST /api/v0/searches`, `GET /api/v0/searches/{id}` and `/responses`,
  `POST /api/v0/transfers/downloads/{username}`, `GET /api/v0/transfers/downloads/{username}`,
  `DELETE` on a transfer to cancel, `GET /api/v0/application` for health.
- Search text `<artist> <title>`, `acquire.soulseek.search_timeout` (15 s), responses limited to
  audio files; candidates carry peer, path, size, bitrate, sample rate, bit depth, length,
  free-slot and queue information.
- slskd downloads into its own downloads directory, which is also mounted into Trackside at
  `acquire.soulseek.downloads_dir`. When the transfer completes, Trackside locates the file
  under that directory by the transfer's reported file name, checks the canonical path is inside
  the directory, moves it into staging and verifies it.
- Peer offline, transfer rejected, or no progress for `acquire.soulseek.stall_timeout` (120 s):
  cancel the transfer, mark the candidate, try the next one.
- slskd unreachable or not configured: the provider reports unhealthy, searches skip it, and the
  dashboard says so. A request with only that provider enabled fails `provider_unavailable` after
  its retries.
- **Sharing.** slskd shares folders by default. The shipped compose file mounts only the acquired
  folder into slskd, read-only, as its share, and the docs recommend keeping it that way. Users
  are responsible for what they download and share under the laws where they live.

### 9.5 Instant play

For a ytdlp candidate the client may ask to play before the download finishes
(`instant: true`). The response includes a stream:

- `hls`: `/api/v1/acquisitions/{id}/stream.m3u8`. A VOD playlist is written from the candidate's
  duration; ffmpeg reads the bytes yt-dlp produces (through a pipe fed by the tee) and writes AAC
  fMP4 segments as in section 8. Segment requests wait for production as usual. Seeking past the
  downloaded point waits.
- `progressive`: `/api/v1/acquisitions/{id}/stream` serves the staging file as it grows. Until
  the download completes: `200`, no `Content-Length`, `Accept-Ranges: none`, and range requests
  are answered with `200` from the start. After completion: the complete staged file with ranges
  and a strong ETag. After import it redirects (`308`) to the song's normal stream URL.

When the import finishes, the SSE event carries the song id. Tracks keeps playing the instant
stream to the end and uses the song id from then on. Instant play counts toward quotas like any
request and requires `acquire.instant_play = true`. It happens only as part of a request.

### 9.7 Playing without keeping

Playing a song the server does not have (a search's candidates, Last.fm's `external`
suggestions, Get More) never keeps it. `POST /api/v1/external/play` takes a candidate token or
a title and artist, resolves it through the providers that stream (ytdlp), and streams it with
the same tee and HLS machinery as instant play, into `<data>/cache/external/<token>/` instead of
staging. It writes no acquisition row, no library song and no change log, and is not audited;
only a request (section 9, lifecycle) fetches a song to keep.

- A song asked by name that the caller's libraries have answers with that song's id.
- Fetched songs stay for replays, which are served from the file without asking the provider
  again, within two limits: `acquire.fetched_keep` songs (default 500) and
  `acquire.fetched_max_bytes` (default 20 GiB). See retention below.
- Limits: the provider's fetch concurrency and cooldown, the user's search rate limit for
  title-and-artist plays, at most 2 songs being fetched per user, and the user's usual stream
  and transcode slots when the stream is served.
- It needs acquisition enabled, the caller allowed to request (`features.acquisition`) and an
  enabled provider that streams (`features.acquisition.external_play`); it does not need
  `acquire.instant_play`.
- A later request for the same song downloads and keeps it as usual.

Retention of fetched songs:

- The index is in memory, and on disk as `entry.json` beside each finished song's file (file
  name, size, title, artist, who started it, when it was fetched and last played, and the
  `song_key`s it answered by name). It is written through a temporary file and a rename once the
  fetch completes, and again when the last-played time changed, at the next sweep.
- A sweep runs after every fetch, when either limit is changed, and on the acquirer's hourly
  janitor round. It evicts finished songs, least recently played first (a song never streamed
  counts from when it was fetched), until both limits hold. It never evicts a song still being
  fetched, one whose file a stream or a transcode holds, or one played in the last 30 minutes;
  those count toward the limits, which can be exceeded until they are free.
- Eviction holds the same lock as starting a fetch, so it never races a new fetch of the same
  song. A song picked for eviction is not handed out any more; a play of it waits for the sweep
  and then fetches it again. Its folder is renamed into `<data>/cache/external/.trash/` (the
  moment it leaves the index), then deleted. A failed rename leaves it in the index for the next
  sweep. These songs are not in the library, so nothing goes through `/sync`; HLS for them runs
  from the file through a per-user job, and nothing of theirs is in the transcode cache.
- At start, and on each janitor round, the folder is reconciled with the index: the trash is
  emptied, and a folder without a whole `entry.json`, or whose file is missing or the wrong
  size (a crash midway), is deleted. Links are never followed.
- `<data>/cache/external/.trackside-play-cache` is written at the first load. While it cannot be
  read (an unmounted share), or any step takes longer than 5 s (a hung one), nothing is evicted
  and the index is left as it is.
- `GET /admin/overview` shows the songs, bytes, songs being fetched and the limits under
  `acquisition.fetched`.

### 9.6 What Tracks sees

The native API never names a provider. Candidates carry quality facts (lossless, codec,
bitrate, duration) and an opaque token. `GET /api/v1/me` includes `features.acquisition` only when
acquisition is enabled **and** the user can request; otherwise the key is absent. The
unauthenticated capabilities document never mentions acquisition. Subsonic endpoints never expose
it: `star` is a favourite, `search3` returns library results only.

---

## 10. Remote access and discovery

### Candidate URLs

The server keeps a list of base URLs a client may use:

- LAN: each non-loopback interface address on the listening port (IPv4, and IPv6 global and
  unique-local; link-local is skipped because it needs a zone id), plus `http://<hostname>.local:<port>`
  when mDNS is on.
- Configured `server.public_urls` (for example `https://music.example.com`,
  `https://box.tailnet-name.ts.net`). These can also be set in the dashboard.
- Each candidate has a `kind` (`lan`, `tailnet` for `*.ts.net` names and 100.64.0.0/10
  addresses, `public` otherwise) and a `tls` flag, and, for a self-signed certificate, a
  `fingerprint`: the certificate's SHA-256 as lowercase hex pairs separated by colons, for
  pinning.
- Every candidate is a base URL that already includes `server.base_path`; clients append
  `/api/v1/...`. Relative URLs in responses (decisions, QR links, instant-play streams) are
  absolute paths that start with the base path and resolve against the candidate's origin.
- The list is built only from the interfaces and the configuration or settings, never from a
  request header.

### Address updates

Paired devices learn a changed list without pairing again. `GET /api/v1/server/urls` (any
authenticated caller with the `library` scope) returns the list with a `version`: the first 16
bytes of SHA-256 over the list's JSON, in hex, also the `ETag`. The server compares the list
with the last one it announced every 15 seconds and right after `server.public_urls` changes in
the dashboard, and sends SSE `server_urls` with the new version when it differs. Tracks asks
with `If-None-Match` when it comes to the foreground or reconnects to events, and on that event.
An address new to the list is held to the same rules as any other: it is used only after its own
endpoint proof and, over HTTPS, a certificate that passes the system's checks or matches
`fingerprint`.

### Pairing

A signed-in user opens My Account, Pair a Device, in the dashboard and chooses Show Code. The
server creates a pairing offer and shows a QR code (SVG rendered by the server) and an
8-character code. The QR holds `trackside://pair?d=<base64url JSON>`, where `exp` is Unix
seconds:

```json
{"v":1,"server_id":"5f0c...","name":"Home","urls":[
  {"url":"http://192.168.1.20:4747","kind":"lan","tls":false},
  {"url":"https://music.example.com","kind":"public","tls":true}],
 "offer":"ge1gnmkv9dsa20vtnqdjq","code":"Q7RM4X2K9T...","exp":1763100000}
```

Tracks races the URLs with `GET /api/v1/ping?challenge=...&host=...&offer=<offer>`, calls
`POST /api/v1/pair/claim` on the first that answers with the right `server_id` and a valid
endpoint proof (below), and receives a device key, a media token and the server info. A typed 8-character
code works the same way once the user has entered the server address or picked it from Bonjour.

The other direction, like Jellyfin's Quick Connect: a client calls `POST /api/v1/pair/requests`,
shows a 6-digit code, and polls; a signed-in user enters the code in the dashboard (My Account,
or Devices and Keys, Approve a Code) and approves. In Tracks this is **Approval** in the pairing
form, offered when `capabilities.auth.pair_requests` is true. It needs no endpoint proof, because
the app sends nothing secret.

Username and password sign-in from the app (`POST /api/v1/auth/login`) also returns a device key.

### URL racing (client side, specified for WP9)

On connect, on every `NWPathMonitor` change, and after two consecutive request failures, Tracks
sends `GET /api/v1/ping?challenge=<nonce>&host=<address>&device=<device id>` to every candidate
in parallel with a 2 s timeout, discards answers whose `server_id` differs or whose `proof` does
not match, and picks the first LAN answer if any arrives within 300 ms of the first answer,
otherwise the fastest. The choice is sticky until one of those triggers, and for plain HTTP for
at most 10 minutes. The same device key works on every URL because identity is the server id,
not the URL.

### Endpoint proof

The `server_id` is public, and LAN candidates are often `http://` or `name.local`, so on a
stranger's network anyone could answer for them. ADR 0011 has the reasoning; in short:

- `proof = HMAC-SHA256(K, "trackside endpoint proof v1" 0x00 server_id 0x00 host 0x00 nonce)`,
  where `K` is SHA-256 of the device key (what `api_keys` already stores) or, during QR pairing,
  the stored hash of the offer's QR secret, found by the payload's `offer` id. Nothing secret
  crosses the network, and revoking a device ends its proofs.
- `host` is the address as the client wrote it, so a proof is good only for that address.
- A nearby `host` (private, loopback, link-local, unique-local or Tailscale IP, a dotless name,
  or `.local`, `.lan`, `.internal`, `.home.arpa`, `.ts.net`) is proven only to a client that came
  from a nearby address itself, directly or through a trusted proxy. A relay from elsewhere
  reaches the public address from a public address and gets no proof for the nearby name.
- The client sends no credential (device key, password, code, QR secret, poll secret) to an
  address until it has proven itself, never over plain HTTP to an address that isn't nearby,
  and holds an HTTPS candidate with a `fingerprint` to that certificate.
- Proxies must pass `/api/v1/ping` with its query string; a proxy that hides it makes that
  address unusable.

### mDNS

`_trackside._tcp` on the listening port, instance name `server.name`, TXT records
`id=<server uuid>`, `v=1` (native API major), `tls=0|1`, `path=<base path>`. Uses `mdns-sd`
(pure Rust, no Avahi). In Docker it needs `network_mode: host`; with bridge networking it is
silently useless, and the docs say so. Discovery is optional; pairing payloads carry URLs anyway.

### TLS and proxies

- `tls.mode = "off"` (default): plain HTTP, for LAN, Tailscale, or a reverse proxy in front.
- `tls.mode = "files"`: PEM certificate and key, reloaded on change.
- `tls.mode = "acme"`: Let's Encrypt via `rustls-acme` (TLS-ALPN-01 on port 443), behind the
  `acme` cargo feature (on in release builds).
- `tls.mode = "self-signed"`: generated with `rcgen` at first start, fingerprint in pairing.
- rustls with the `ring` provider (keeps musl cross builds free of C toolchains beyond `cc`).
- HTTP/2 over TLS through ALPN. HTTP/3 is left to Caddy in front.
- `server.trusted_proxies` (CIDRs; default empty) decides whose `Forwarded` and
  `X-Forwarded-For/Proto/Host` headers are believed; the client address is the rightmost
  untrusted hop. `X-Forwarded-For` wins over `Forwarded` when both are present, because nginx,
  Caddy, Traefik and Cloudflare append to the former and pass a client's own `Forwarded`
  through untouched. `server.base_path` serves everything under a prefix.
- When requests with forwarding headers arrive from a local address that is not trusted (a proxy
  the admin forgot to list), the server logs one warning and `/readyz` adds a note: every client
  then shares the proxy's address for rate limits, none counts as nearby for endpoint proofs,
  and passwords and keys through it over plain HTTP are refused.
- Documented recipes: Tailscale (default recommendation), Caddy with automatic TLS, built-in
  ACME, and Cloudflare Tunnel for the dashboard only (its terms make it a poor audio path).

---

## 11. Configuration reference

Sources in increasing precedence: built-in defaults, the TOML file
(`<data>/config.toml`, or `--config`, or `TRACKSIDE_CONFIG`), environment variables, and
command-line overrides (the CLI takes only `--config` and `--data-dir`; `--data-dir` is an
override, reported as `command line`). Environment names are
`TRACKSIDE_` plus the upper-cased path with `__` between levels; any key holding a secret also
accepts `<NAME>_FILE` with a path to read it from. Durations are strings such as `"500ms"`,
`"30s"`, `"15m"`, `"6h"`, `"30d"` or a number of seconds; sizes such as `"512MiB"`, `"10GiB"`,
`"1GB"` or a number of bytes. Unknown keys in the file and unknown `TRACKSIDE_*` variables are
logged as warnings and ignored. The key table lives in `trackside-core/src/config/loader.rs`
(`KEYS`); [ops/configuration.md](ops/configuration.md) is the operator's copy of this table.

Some settings are also editable in the dashboard and stored in the `settings` table. A value set
in the file or environment wins and the dashboard shows it as "set by config" and read-only.
Those keys are marked **D**.

The server starts with no file at all: data in the platform default directory, listening on
`[::]:4747` (falling back to `0.0.0.0:4747` when IPv6 is unavailable), and, if
`TRACKSIDE_MUSIC_DIR` (or `/music` in Docker) exists and is non-empty, a first library named
"Music" created from it.

| Key | Env | Default | Meaning |
|---|---|---|---|
| `data_dir` | `TRACKSIDE_DATA_DIR` | `$STATE_DIRECTORY`, else `$XDG_DATA_HOME/trackside`, else `~/.local/share/trackside`; Docker `/data` | database, secret key, backups, caches unless moved |
| `music_dir` | `TRACKSIDE_MUSIC_DIR` | unset (`/music` in the image) | creates the first library at first start only |
| `server.name` **D** | `TRACKSIDE_SERVER__NAME` | host name | shown in apps, mDNS and pairing |
| `server.listen` | `TRACKSIDE_SERVER__LISTEN` | `["[::]:4747"]` | addresses; comma-separated in env |
| `server.base_path` | `TRACKSIDE_SERVER__BASE_PATH` | `""` | URL prefix behind a proxy |
| `server.public_urls` **D** | `TRACKSIDE_SERVER__PUBLIC_URLS` | `[]` | extra candidate URLs for pairing |
| `server.trusted_proxies` | `TRACKSIDE_SERVER__TRUSTED_PROXIES` | `[]` | CIDRs whose forwarding headers are believed |
| `server.local_networks` | `TRACKSIDE_SERVER__LOCAL_NETWORKS` | `[]` | CIDRs that count as your own network besides the private, link-local and Tailscale ranges, such as your LAN's global IPv6 prefix: passwords and keys are accepted from them over plain HTTP |
| `server.tailscale_is_local` | `TRACKSIDE_SERVER__TAILSCALE_IS_LOCAL` | `true` | whether Tailscale's 100.64.0.0/10 counts as your own network; turn it off if your ISP puts your server behind carrier-grade NAT in that range, where it is the internet |
| `server.max_connections` | `TRACKSIDE_SERVER__MAX_CONNECTIONS` | `1024` | open connections at once, across listeners; more are closed at once |
| `server.header_timeout` | `TRACKSIDE_SERVER__HEADER_TIMEOUT` | `"30s"` | time to receive request headers; also how long an idle HTTP/1 keep-alive connection stays open |
| `server.request_timeout` | `TRACKSIDE_SERVER__REQUEST_TIMEOUT` | `"60s"` | non-streaming handlers |
| `server.shutdown_grace` | `TRACKSIDE_SERVER__SHUTDOWN_GRACE` | `"10s"` | drain time on stop |
| `tls.mode` | `TRACKSIDE_TLS__MODE` | `"off"` | `off`, `files`, `acme`, `self-signed` |
| `tls.cert_file` | `TRACKSIDE_TLS__CERT_FILE` | unset | PEM chain for `files` |
| `tls.key_file` | `TRACKSIDE_TLS__KEY_FILE` | unset | PEM key for `files` |
| `tls.acme_domains` | `TRACKSIDE_TLS__ACME_DOMAINS` | `[]` | domains for `acme` |
| `tls.acme_email` | `TRACKSIDE_TLS__ACME_EMAIL` | unset | contact for `acme` |
| `tls.acme_directory` | `TRACKSIDE_TLS__ACME_DIRECTORY` | Let's Encrypt production | ACME directory URL |
| `tls.acme_cache` | `TRACKSIDE_TLS__ACME_CACHE` | `<data>/acme` | account and certificates |
| `database.path` | `TRACKSIDE_DATABASE__PATH` | `<data>/trackside.db` | SQLite file |
| `database.readers` | `TRACKSIDE_DATABASE__READERS` | `min(8, cpus)` | reader connections |
| `database.cache` | `TRACKSIDE_DATABASE__CACHE` | `"32MiB"` | page cache per connection |
| `database.mmap` | `TRACKSIDE_DATABASE__MMAP` | `"256MiB"` | mmap size |
| `database.allow_network_fs` | `TRACKSIDE_DATABASE__ALLOW_NETWORK_FS` | `false` | permit the database on NFS or SMB |
| `cache_dir` | `TRACKSIDE_CACHE_DIR` | `<data>/cache` | artwork and transcodes, safe to delete |
| `scanner.threads` | `TRACKSIDE_SCANNER__THREADS` | `min(4, cpus)` | tag reader threads |
| `scanner.watch` **D** | `TRACKSIDE_SCANNER__WATCH` | `true` | filesystem events |
| `scanner.watch_debounce` | `TRACKSIDE_SCANNER__WATCH_DEBOUNCE` | `"5s"` | wait after the last event in a folder |
| `scanner.quick_interval` **D** | `TRACKSIDE_SCANNER__QUICK_INTERVAL` | `"30m"` | periodic quick scan |
| `scanner.full_interval` **D** | `TRACKSIDE_SCANNER__FULL_INTERVAL` | `"off"` | periodic full scan that rereads every tag |
| `scanner.settle` | `TRACKSIDE_SCANNER__SETTLE` | `"10s"` | files modified more recently are deferred |
| `scanner.follow_symlinks` | `TRACKSIDE_SCANNER__FOLLOW_SYMLINKS` | `true` | loops are detected by device and inode |
| `scanner.missing_grace` **D** | `TRACKSIDE_SCANNER__MISSING_GRACE` | `"30d"` | before missing songs are purged |
| `scanner.empty_guard` | `TRACKSIDE_SCANNER__EMPTY_GUARD` | `0.5` | abort if fewer than this share of known files are found |
| `scanner.file_timeout` | `TRACKSIDE_SCANNER__FILE_TIMEOUT` | `"30s"` | per-file tag read before quarantine |
| `scanner.ignore` | `TRACKSIDE_SCANNER__IGNORE` | `[".trackside-ignore", ".nomedia", ".ndignore"]` | marker files that skip a folder |
| `scanner.extensions` | `TRACKSIDE_SCANNER__EXTENSIONS` | `flac m4a mp4 alac mp3 aac ogg oga opus wav aif aiff wv ape` | files to read |
| `media.ffmpeg_path` | `TRACKSIDE_MEDIA__FFMPEG_PATH` | `ffmpeg` on PATH | encoder |
| `media.transcode_max_concurrent` | `TRACKSIDE_MEDIA__TRANSCODE_MAX_CONCURRENT` | `max(1, cpus / 2)` | ffmpeg processes |
| `media.transcode_cache_max` **D** | `TRACKSIDE_MEDIA__TRANSCODE_CACHE_MAX` | `"10GiB"` | LRU cap |
| `media.art_cache_max` **D** | `TRACKSIDE_MEDIA__ART_CACHE_MAX` | `"2GiB"` | LRU cap for artwork |
| `media.cache_min_free` | `TRACKSIDE_MEDIA__CACHE_MIN_FREE` | `"1GiB"` | free space kept on the cache volume |
| `media.hls_segment` | `TRACKSIDE_MEDIA__HLS_SEGMENT` | `"6s"` | HLS segment length |
| `media.cellular_profile` **D** | `TRACKSIDE_MEDIA__CELLULAR_PROFILE` | `"aac-256"` | suggested to clients |
| `auth.secret_key_file` | `TRACKSIDE_AUTH__SECRET_KEY_FILE` | `<data>/secret.key` | 32-byte key |
| `auth.legacy_subsonic` **D** | `TRACKSIDE_AUTH__LEGACY_SUBSONIC` | `true` | allow app passwords at all |
| `auth.session_idle` | `TRACKSIDE_AUTH__SESSION_IDLE` | `"14d"` | |
| `auth.session_max` | `TRACKSIDE_AUTH__SESSION_MAX` | `"90d"` | |
| `auth.media_token_ttl` | `TRACKSIDE_AUTH__MEDIA_TOKEN_TTL` | `"7d"` | |
| `auth.argon2_memory` | `TRACKSIDE_AUTH__ARGON2_MEMORY` | `"19MiB"` | |
| `auth.argon2_iterations` | `TRACKSIDE_AUTH__ARGON2_ITERATIONS` | `2` | |
| `auth.login_rate` | `TRACKSIDE_AUTH__LOGIN_RATE` | `"10/1m"` | per IP |
| `auth.admin_username` | `TRACKSIDE_ADMIN_USERNAME` | unset | create this admin at first start |
| `auth.admin_password` | `TRACKSIDE_ADMIN_PASSWORD` / `_FILE` | unset | its password |
| `sync.tombstone_retention` | `TRACKSIDE_SYNC__TOMBSTONE_RETENTION` | `"90d"` | |
| `sync.max_page` | `TRACKSIDE_SYNC__MAX_PAGE` | `5000` | records per sync page |
| `discovery.mdns` **D** | `TRACKSIDE_DISCOVERY__MDNS` | `true` | |
| `backup.dir` | `TRACKSIDE_BACKUP__DIR` | `<data>/backups` | |
| `backup.schedule` **D** | `TRACKSIDE_BACKUP__SCHEDULE` | `"daily 03:30"` | `off`, `daily HH:MM`, `weekly <day> HH:MM`, server local time |
| `backup.keep` **D** | `TRACKSIDE_BACKUP__KEEP` | `7` | scheduled backups kept |
| `backup.include_secret_key` | `TRACKSIDE_BACKUP__INCLUDE_SECRET_KEY` | `true` | copy `secret.key` beside each backup |
| `log.level` | `TRACKSIDE_LOG__LEVEL` (also `RUST_LOG`) | `"info"` | tracing filter |
| `log.format` | `TRACKSIDE_LOG__FORMAT` | `"pretty"` on a TTY, else `"json"` | |
| `metrics.enabled` | `TRACKSIDE_METRICS__ENABLED` | `false` | `/metrics` |
| `metrics.listen` | `TRACKSIDE_METRICS__LISTEN` | unset (main listener) | separate address for `/metrics` |
| `uploads.enabled` **D** | `TRACKSIDE_UPLOADS__ENABLED` | `false` | browser and API uploads |
| `uploads.max_file` | `TRACKSIDE_UPLOADS__MAX_FILE` | `"2GiB"` | |
| `uploads.inbox_dir` | `TRACKSIDE_UPLOADS__INBOX_DIR` | unset | watched drop folder imported into the uploads library |
| `uploads.path_template` | `TRACKSIDE_UPLOADS__PATH_TEMPLATE` | `"{album_artist}/{album}/{disc}-{track:02} {title}.{ext}"` | |
| `acquire.enabled` **D** | `TRACKSIDE_ACQUIRE__ENABLED` | `false` | master switch |
| `acquire.library` **D** | `TRACKSIDE_ACQUIRE__LIBRARY` | unset | pid or root path of the writable acquired library |
| `acquire.staging_dir` | `TRACKSIDE_ACQUIRE__STAGING_DIR` | `<data>/acquire/staging` | |
| `acquire.prefer` **D** | `TRACKSIDE_ACQUIRE__PREFER` | `["soulseek", "ytdlp"]` | provider order bonus |
| `acquire.search_timeout` | `TRACKSIDE_ACQUIRE__SEARCH_TIMEOUT` | `"8s"` | per provider |
| `acquire.max_file_bytes` | `TRACKSIDE_ACQUIRE__MAX_FILE_BYTES` | `"1GiB"` | |
| `acquire.member_daily` **D** | `TRACKSIDE_ACQUIRE__MEMBER_DAILY` | `50` | |
| `acquire.member_active` **D** | `TRACKSIDE_ACQUIRE__MEMBER_ACTIVE` | `5` | |
| `acquire.global_daily` **D** | `TRACKSIDE_ACQUIRE__GLOBAL_DAILY` | `500` | |
| `acquire.instant_play` **D** | `TRACKSIDE_ACQUIRE__INSTANT_PLAY` | `false` | stream-through for ytdlp candidates |
| `acquire.fetched_keep` **D** | `TRACKSIDE_ACQUIRE__FETCHED_KEEP` | `500` | songs played without being kept that stay fetched |
| `acquire.fetched_max_bytes` **D** | `TRACKSIDE_ACQUIRE__FETCHED_MAX_BYTES` | `"20GiB"` | the most those songs may take |
| `acquire.reject_words` | `TRACKSIDE_ACQUIRE__REJECT_WORDS` | list in 9.1 | |
| `acquire.path_template` | `TRACKSIDE_ACQUIRE__PATH_TEMPLATE` | see 9 | |
| `acquire.ytdlp.enabled` **D** | `TRACKSIDE_ACQUIRE__YTDLP__ENABLED` | `false` | |
| `acquire.ytdlp.path` | `TRACKSIDE_ACQUIRE__YTDLP__PATH` | `<data>/bin/yt-dlp`, else `yt-dlp` on PATH | |
| `acquire.ytdlp.js_runtime` | `TRACKSIDE_ACQUIRE__YTDLP__JS_RUNTIME` | `deno` on PATH if present | |
| `acquire.ytdlp.cookies_file` | `TRACKSIDE_ACQUIRE__YTDLP__COOKIES_FILE` | unset | |
| `acquire.ytdlp.concurrency` | `TRACKSIDE_ACQUIRE__YTDLP__CONCURRENCY` | `2` | |
| `acquire.ytdlp.search_rate` | `TRACKSIDE_ACQUIRE__YTDLP__SEARCH_RATE` | `"10/1m"` | |
| `acquire.ytdlp.max_age_days` | `TRACKSIDE_ACQUIRE__YTDLP__MAX_AGE_DAYS` | `60` | outdated warning |
| `acquire.soulseek.enabled` **D** | `TRACKSIDE_ACQUIRE__SOULSEEK__ENABLED` | `false` | |
| `acquire.soulseek.url` **D** | `TRACKSIDE_ACQUIRE__SOULSEEK__URL` | unset | e.g. `http://slskd:5030` |
| `acquire.soulseek.api_key` | `TRACKSIDE_ACQUIRE__SOULSEEK__API_KEY` / `_FILE` | unset | |
| `acquire.soulseek.downloads_dir` | `TRACKSIDE_ACQUIRE__SOULSEEK__DOWNLOADS_DIR` | unset | slskd's downloads, as mounted here |
| `acquire.soulseek.concurrency` | `TRACKSIDE_ACQUIRE__SOULSEEK__CONCURRENCY` | `3` | |
| `acquire.soulseek.search_timeout` | `TRACKSIDE_ACQUIRE__SOULSEEK__SEARCH_TIMEOUT` | `"15s"` | |
| `acquire.soulseek.stall_timeout` | `TRACKSIDE_ACQUIRE__SOULSEEK__STALL_TIMEOUT` | `"120s"` | |
| `acquire.soulseek.min_bitrate` | `TRACKSIDE_ACQUIRE__SOULSEEK__MIN_BITRATE` | `256` | kbps for lossy candidates |
| `enrichment.musicbrainz` **D** | `TRACKSIDE_ENRICHMENT__MUSICBRAINZ` | `false` | tag lookups for acquired files |
| `enrichment.lastfm_api_key` **D** | `TRACKSIDE_ENRICHMENT__LASTFM_API_KEY` / `_FILE` | unset | one server-wide Last.fm key for suggestions (section 5, Suggestions and enrichment); secret, write-only in the dashboard |

`trackside config show` prints one `key = value  # source` line per key in this table's order,
values as TOML with durations and sizes in their largest whole unit and secrets as `<redacted>`,
the source one of `default`, `file`, `environment` or `command line`; then any warnings as
`# warning: ...` lines.

---

## 12. Observability

- **Logs.** `tracing` with `tracing-subscriber`; JSON when not on a TTY. Every request has a
  `request_id` (honouring an incoming `X-Request-Id` from a trusted proxy), echoed in
  `X-Request-Id` and in error bodies. Secrets in query strings are redacted before logging.
  Scanner and acquisition jobs log one summary line per run and one line per error.
- **`/healthz`.** `200 ok` while the process runs. For Docker's `HEALTHCHECK`, the image runs
  `trackside healthcheck`, which requests it (distroless has no curl).
- **`/readyz`.** `200` with a JSON body once the database is open and migrated; `503` during
  migrations or if the database cannot be read. Library unavailability, a missing ffmpeg or an
  unhealthy provider is reported in the body as `degraded` but does not fail readiness, so an
  orchestrator does not restart a healthy server because a NAS is off.
- **`/metrics`** (opt-in). Prometheus text written by hand from atomic counters and gauges, no
  metrics crate: requests by route template (`/api/v1/songs/{id}/stream`, never the ids;
  unmatched paths count as `other`) and status, latency histograms for list endpoints and stream
  first byte, scan files per second, songs per library, transcode queue depth and running jobs,
  cache sizes, acquisition jobs by state, provider health, database size and WAL size.
- **Admin overview.** `GET /api/v1/admin/overview` (api.md) is what the dashboard's health page
  shows: version, uptime, database and disk sizes, free space, ffmpeg, libraries and scans,
  transcodes, providers, and problems, each with a short title, a message and what to do.
- **Charts.** The scheduler samples concurrent media responses and ffmpeg jobs every 10 s and
  writes the hour's peak, plus failed media requests, to `stats_hourly`; every 10 minutes it
  writes the day's free space, database and cache size to `stats_daily` (migration 2). Hourly
  rows are kept 90 days and also mark when the server was running, so a chart can tell "nothing
  recorded" from a measured zero. Plays and requests come from `listens` and `acquisitions`.
  `GET /api/v1/admin/stats/*` serves them (api.md section 16).

---

## 13. Backups, upgrades and on-disk stability

### Backups

- Scheduled per `backup.schedule` with `VACUUM INTO '<dir>/trackside-<UTC timestamp>.db.partial'`,
  then `fsync` and rename to `.db`, so a crash never leaves a file that looks complete. The secret
  key is copied beside it as `<same name>.key` (0600) when configured.
- Before starting, the server checks free space for the current database size plus 10 %; if short,
  it skips, logs an error and shows it on the overview.
- `backup.keep` most recent scheduled backups are kept; pre-migration and manual backups are kept
  until deleted.
- `trackside backup create`, `backup list`, and `backup restore <file>`. Restore refuses while the
  server holds `<data>/trackside.lock`, runs `PRAGMA integrity_check` on the file, moves the
  current database to `trackside.db.before-restore-<ts>`, and copies the backup in.
- Litestream works alongside (WAL mode, no foreign checkpointing); a documented recipe only.
- `GET /api/v1/me/listens/export` gives a user their listens as JSON Lines in a
  ListenBrainz-compatible shape.
- Trackside Cloud data is in the same database, so every backup holds it. Encrypted settings
  (`nearbyKey`) stay encrypted with a key only the user's devices have.

### Migrations and upgrades

- Forward-only migrations, each in a transaction, via `rusqlite_migration`. Before applying any
  migration to a non-empty database the server copies it (with its WAL) to
  `<backup.dir>/pre-migrate-v<from>-v<to>-<ts>.db` and logs `pre-migration backup written`. A
  crash during migration rolls back; the next start retries.
- A database newer than the binary (`user_version` above the highest known migration) is never
  opened for writing. The server exits with a message naming the schema versions and the backup
  folder, and says to restore a pre-migration backup from it with `trackside backup restore` to
  downgrade.
- Migrations may not rewrite public ids. If a `tracks-model` folding change requires recomputing
  `song_key`, `album_key` or `identity`, the migration recomputes them in place and bumps every
  user's `sync_epoch`; album and artist ids derived from changed identities are remapped and the old
  ids are kept in an alias table added by that migration so Subsonic clients' stored ids still
  resolve.
- `settings.schema.tracks_model` records the folding version used, and start-up refuses to run
  with a different `tracks-model` folding version without the matching migration (checked by a
  golden test in CI).

### Versioning

- Releases are semver. The native API is versioned by path (`/api/v1`). Within v1 changes are
  additive: new endpoints, new optional request fields, new response fields. Clients must ignore
  unknown fields and enum values.
- Removing or changing anything requires `/api/v2`, served alongside v1 for at least 12 months
  and two minor releases. Deprecated endpoints send `Deprecation` and `Sunset` headers and are
  listed in `/api/v1/capabilities` under `deprecated`.
- The on-disk formats are: the SQLite schema (migrations), `config.toml` (unknown keys are a
  warning, renamed keys keep their old name as an alias for one major version), the cache layout
  (disposable), and `secret.key` (32 raw bytes, never changes).

---

## 14. Security model and threat model

### Assets

The user's music files (read access only, except writable target libraries), listening history and
playlists, the apps' synced data in Trackside Cloud, credentials (password hashes, key hashes, sealed app passwords, the secret key), the
host running the server, and the user's network reputation when acquisition is on.

### Trust boundaries and actors

| Actor | Can reach | Main risks | Mitigations |
|---|---|---|---|
| Someone on the LAN before setup | `/setup` | claiming a new install | setup token from the log or data dir; nothing else is served before setup except health, ping and capabilities |
| Internet attacker | every public route | credential stuffing, enumeration, DoS | Argon2id, per-IP and per-account backoff, generic auth errors, constant-time comparisons, connection and request limits, header and body size limits, no user enumeration on login or invites |
| Member | own data, granted libraries | reading others' data or libraries, admin escalation | every query filtered by `Principal.libraries` and `user_id` in repositories (not in handlers), admin routes require an admin session, tests per endpoint for cross-user access |
| Admin | users, settings, backups | reading members' synced history and settings | no admin route or dashboard view returns Trackside Cloud records (counts only); cloud routes take device keys only, so an admin session cannot reach them; secrets like `nearbyKey` are stored only client-encrypted (ADR 0012). The database file itself is readable by whoever runs the server |
| Stolen device key | that user's library and data | long-lived access | keys listed with last use and IP, one-click revoke, no admin scope, media tokens bound to the device epoch |
| Network observer on plain HTTP | traffic | credential and token theft | passwords, keys, the plain session cookie and setup, invite, reset and pairing secrets are refused over plain HTTP from addresses that aren't nearby (`403 https_required`, api.md section 1); the docs, and Tracks' server form, warn about plain HTTP addresses; media tokens expire; legacy MD5 auth is opt-in |
| Malicious file in the library | tag parser, image decoder | parser crash, memory exhaustion, hang | lofty and image are pure Rust; per-file timeout and quarantine; `catch_unwind` around parsing; image dimension cap (12k by 12k) before decode |
| Malicious peer or video (acquisition) | files in staging | path traversal, non-audio payloads, decoder exploits | canonical path checks, magic-byte allowlist, size limits, full decode in an ffmpeg subprocess, names built from sanitised tags |
| Compromised yt-dlp release | runs as the server user | code execution | hash-checked updates from the official release, disabled by default, runs with the server's privileges only (non-root, read-only music in the shipped configs), optional `acquire.ytdlp.path` to a sandboxed wrapper |
| Reverse proxy | forwarding headers | IP spoofing | forwarding headers ignored unless the peer is in `trusted_proxies` |
| Dashboard in a browser | admin API | CSRF, XSS | SameSite=Strict cookie, custom CSRF header, Origin check, strict CSP (`default-src 'self'`), no inline scripts, all text inserted with `textContent` |
| Someone at an unlocked, signed-in browser | the dashboard session | taking over accounts, downloading the database | sensitive changes and backup downloads need the password again after 10 minutes (section 7, Re-authentication) |

### Hardening defaults

Runs as non-root (image user 65532, systemd `DynamicUser`), music mounted read-only in the shipped
configs, `unsafe_code = "forbid"` across the workspace, no shell anywhere (subprocess argument
vectors only), no runtime-editable commands, no outbound network calls unless acquisition,
enrichment or a yt-dlp update is enabled.

Response headers (set in `trackside-server/src/web.rs`):

| Where | Headers |
|---|---|
| every response | `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `X-Request-Id` |
| `/api/v1` | `Content-Security-Policy: default-src 'none'; frame-ancestors 'none'`, `Cache-Control: no-store` unless the route sets its own, plus the isolation set |
| dashboard pages and files | `Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self'; font-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'`, plus the isolation set |
| isolation set | `X-Frame-Options: DENY`, `Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Resource-Policy: same-origin`, `Permissions-Policy: camera=(), microphone=(), geolocation=(), interest-cohort=()` |
| `/rest` | only the first row, because web Subsonic players on other origins load covers and audio |

With `tls.mode = "acme"` or `"files"`, responses on the TLS listeners also carry
`Strict-Transport-Security: max-age=31536000` (this host only, no `includeSubDomains`). It is
not sent with a self-signed certificate, where browsers would then refuse to let anyone click
through the warning, nor over plain HTTP; behind a proxy, the proxy sets it.

---

## 15. Performance budgets

Reference machines: "small" is a Raspberry Pi 5 (4 cores, 8 GB, USB SSD); "medium" is a 4-core
x86 NAS with the library on spinning disks and the data directory on SSD. Library sizes 50k and
500k songs (synthetic generator in `tests-e2e`).

| Measure | Budget |
|---|---|
| Process start to `/readyz` 200, no migrations, 500k songs | < 300 ms |
| Idle RSS, 500k songs | < 120 MB excluding the OS page cache |
| First full scan, local SSD | ≥ 400 files/s medium, ≥ 150 files/s small |
| First full scan, 1 GbE NAS | ≥ 80 files/s (bounded by 192 KiB digest reads plus tag reads) |
| Quick scan with no changes, 500k songs in 40k folders | < 20 s on SSD, < 90 s on NAS |
| Watcher event to song visible in sync | < 8 s (5 s debounce plus import) |
| Acquired file imported to SSE event | < 1 s |
| `GET` list endpoints (albums, artist, album, search), 500k songs | p50 < 5 ms, p99 < 40 ms server time |
| Search as you type | p99 < 25 ms |
| `/api/v1/sync` snapshot page of 5000 songs | < 120 ms server time, about 400 KB brotli |
| Full initial sync of 500k songs on LAN | < 45 s end to end |
| Delta sync with 100 changes | < 10 ms |
| `POST /api/v1/listens` with 500 items | < 80 ms |
| Stream time to first byte, direct, LAN, warm disk | p99 < 25 ms |
| Stream time to first byte, cached transcode | p99 < 30 ms |
| HLS first segment, uncached transcode | < 700 ms medium, < 1.5 s small |
| Full AAC 256 transcode of a 4-minute song | < 4 s medium, < 10 s small |
| Artwork 600 px, cached / first resize | < 5 ms / < 120 ms |
| Concurrent streams | 50 direct streams on small without stalls |

`tests-e2e` measures each budget; CI runs the 50k variants and fails on a regression of more than
25 % against the stored baseline. The 500k variants run before each release.

---

## 16. Failure modes

| Situation | Detection | Handling |
|---|---|---|
| Disk full (data volume) | `SQLITE_FULL`, write errors, free-space checks | Writes fail with `507 insufficient_storage`; reads and streaming continue. Clients keep listens in their outbox and retry. Backups and caches check free space first; caches evict. The overview shows free space and a warning under 2 GiB. |
| Disk full (target library) | `ENOSPC` on move | Import or acquisition fails `insufficient_space`, staged file kept until the janitor; the acquisition queue pauses and resumes when space returns. |
| Library on a NAS disappears (unmounted, powered off) | availability probe (5 s deadline), empty root, `st_dev` changed, `ENOENT`/`EIO` | Library marked `unavailable`; scans are skipped; nothing is marked missing; streams return `503 library_unavailable` with `Retry-After: 30`; sync keeps the songs. When the probe succeeds again a quick scan runs. |
| NAS mount point present but empty | root empty, or found files below `scanner.empty_guard` of known files | Scan aborted as `unavailable` before marking anything missing; overview explains; an admin can force with "This library really is empty now". |
| Hung NFS (hard mount) | probe misses deadline | As above; blocked threads limited by the per-library semaphore. |
| Files changing during a scan | mtime within `scanner.settle`, stat differs before and after reading | File deferred to a follow-up pass 15 s later; a partial copy is never imported. |
| Huge library (500k+) | | Streaming walk, folder skip by mtime and listing hash, 200-file transactions, bounded channels, change log bounded by entities, snapshot paging, FTS5 prefix indexes. |
| Corrupt or missing tags | lofty error | Retry in relaxed mode; then title from the file name, artist "Unknown Artist", `tag_problem` set, listed in the dashboard. Files without readable audio properties are skipped and recorded. |
| Tag parser hang or panic | per-file timeout, `catch_unwind` | File quarantined until its size or mtime changes; scan continues. |
| Duplicate files | equal digests | Both songs exist with distinct ids (section 6); the dashboard lists duplicate groups; move detection prefers the same file name. |
| Clock skew between client and server | `client_time` in listen batches | Server timestamps use only the server clock. Listen times are stored as sent (they are the user's history), but items more than 24 h in the future of the client's own `client_time` are rejected. Sync uses seqs, never times. Media token expiry is checked with the server clock only. |
| Concurrent writers | | One process per data dir (`flock` on `trackside.lock`); one writer connection; CLI commands use `busy_timeout` and short transactions. |
| Crash mid-migration | `user_version` unchanged | Transaction rolled back; pre-migration backup exists; next start retries. |
| Crash mid-backup | `.partial` file | Ignored and deleted at next start. |
| Crash mid-scan | `scan_runs.state = running` at start | Marked `aborted`; a quick scan runs. Already committed batches stand; missing marking only happens at the end of a complete run. |
| ffmpeg missing or broken | `ffmpeg -version` fails, job exit codes | Direct play only; capabilities say so; jobs that fail three times for a song mark that song `transcode_failed` for 1 h and decisions fall back to direct when allowed. |
| Transcoding overload | queue wait | Priorities, fallback to direct when the client allows, `503` with `Retry-After` otherwise. |
| Slow clients | write stall timer | Bounded buffers; 120 s without progress closes; ffmpeg writes to cache independently of readers. |
| Range requests | | Full RFC 9110 handling (section 8), tested with AVPlayer's `bytes=0-1` probe. |
| Reverse proxies | | `trusted_proxies`, `base_path`, relative URLs in HLS playlists, `X-Accel-Buffering: no` and 20 s keep-alive comments on SSE; recipes for Caddy and nginx including `proxy_read_timeout` and buffering off. |
| IPv6 | | Binds `[::]` dual-stack, falls back to IPv4; candidate URLs include global and ULA IPv6 in brackets; trusted proxy CIDRs accept both families. |
| Time zones | | All stored times UTC milliseconds; the dashboard renders in the browser's zone; backup schedules use the server's local zone via jiff, with skipped and repeated DST times resolved to the later instant. |
| Unicode normalization | | Tag text stored NFC; identities from tracks-model are NFC by construction; paths stored as raw bytes so NFD names from macOS shares round-trip exactly. |
| Case-insensitive filesystems | | Paths compared as stored bytes; a case-only rename is seen as a move and matched by digest, keeping the id. |
| Symlink loops | | Directories tracked by `(st_dev, st_ino)` per walk; revisits skipped with a scan warning. A file reachable by two paths is indexed once. |
| Permission errors | `EACCES` | Recorded in `scan_errors`; the folder is marked `unreadable` and its songs are not marked missing. |
| Admin locked out | | `trackside admin reset-password` or `set-role` on the host. |
| Upgrade | | Pre-migration backup, forward migrations, additive API. |
| Downgrade | newer `user_version` | Refuses to start; names the backup to restore. |
| Several Tracks devices for one user | | One device row and key each; listens deduplicated by `client_listen_id` (Tracks' play id, which iCloud gives every device the same value for); play queue last writer wins with `changed_by`; stars and ratings are set operations; each device keeps its own sync cursor. |
| Two users sharing one device | | Two server entries in Tracks with separate keys and caches; the server sees two devices; `UNIQUE(user_id, client_device_id)` allows the same install id for both. |
| Database on NFS or SMB | `statfs` type at start | Refuses unless `database.allow_network_fs`, because SQLite locking there is unreliable. |
| inotify watch limit | `ENOSPC` adding watches | Falls back to periodic quick scans for that library and shows how to raise `fs.inotify.max_user_watches`. |
| Network filesystems deliver no events | | Periodic quick scans regardless of watching. |
| Secret key lost | missing file with existing users | New key generated; app passwords and pending pairings stop working and are marked so in the dashboard; everything else continues. |
| Corrupt database | `integrity_check` in `trackside check`, open errors | Server refuses to start with the error and the newest backup's name; `trackside backup restore`. |
| slskd down or not configured | health check, connection errors | Provider unhealthy; searches skip it; jobs that need it retry with backoff, then `provider_unavailable`. |
| yt-dlp missing or outdated | `--version` at start and daily | Provider unhealthy or "outdated" in the dashboard with an update button; image pins a known-good version. |
| YouTube throttling or bot checks | output patterns, 429 or 403 | Provider cooldown, 15 min doubling to 6 h; next candidate from another provider; hint to add cookies. |
| Partial downloads | size, decode check | `.part` files resumed (`--continue`) on retry or rejected by verification; janitor removes leftovers after 24 h. |
| Peer offline mid-transfer | stall timer, transfer state | Transfer cancelled, next candidate tried. |
| Same song requested twice | `request_key` | Second request joins the first; already in the library returns the existing id at once. |
| Acquired file is a duplicate of an existing song | digest at import | Staged file deleted, existing id returned. |
| Restart during acquisition | expired lease | Job resumed from its state (section 9.3). |
| Wrong version downloaded (live, remix) | ranking filters, duration check | Rejected before download where metadata shows it, or at verification by duration; next candidate. |

---

## 17. Tracks client integration

This section is the high-level design for WP9. Everything here lives in `TracksCore` and
`Apps/Shared`, so iOS and macOS share it; the few platform-specific spots are listed at the end.

### Server kinds

- `SubsonicServer` (`TracksCore/Sources/TracksCore/Library/SubsonicClient.swift`) gains
  `kind: ServerKind` (`.subsonic` or `.trackside`), `serverUUID` (Trackside's `server_id`),
  `candidateURLs`, and `deviceID`, all optional in `Codable` so existing `UserDefaults["musicServers"]`
  entries decode unchanged as `.subsonic`. The entry's own `id` stays the value used in
  `server:<id>:<songID>`, so ids stored in downloads, playlists and catalogs keep their shape.
- Detection when adding or re-checking a server: `GET <url>/api/v1/capabilities` (unauthenticated).
  A `200` with `"product": "trackside"` means Trackside; anything else falls back to today's
  Subsonic path (`ping`, `getOpenSubsonicExtensions`). Navidrome and other Subsonic servers keep
  working exactly as now.
- A protocol `MusicServerClient` (search, album, artist, top songs, similar songs and artists,
  random songs, album lists, stream and download URLs, artwork URL, star, listens) is extracted
  from what `MusicServers`, `ServerDiscovery`, `YourMusic`, `Downloads`, `LocalPlayerEngine` and the
  server pages call today. `SubsonicClient` conforms unchanged; a new `TracksideClient` (a
  `Sendable` struct with a transport protocol like `SubsonicTransport`, `Codable` wire types
  matching api.md) conforms too. Views that call `SubsonicClient` directly
  (`ServerArtistPage`, `ServerAlbumPage`, `LocalArtistPage.loadFromServer`, `SeedArtistPicker`)
  move to the protocol.
- The device key is stored in the Keychain under the existing service
  `com.luke.tracks.music-server`, account = server entry id (where the password is today). No
  identifier that lives on devices is renamed.

### Pairing and adding a server

`ServerForm` offers, for Trackside: scan the QR code (or open a `trackside://pair` link), pick a
server found with Bonjour (`NWBrowser` for `_trackside._tcp`; add it to `NSBonjourServices` and
keep the existing `NSLocalNetworkUsageDescription`), enter a pairing code, choose Approval to
show a 6-digit code to approve in the dashboard, or sign in with a username and password. Each path ends with a device key and the candidate URL list. The plain
Subsonic form stays for other servers. Server settings show "Paired as <device name>" instead of
a password field for Trackside entries.

### URL racing

A new `ServerEndpoints` actor in TracksCore holds each Trackside server's candidate URLs and the
current choice, races them as section 10 specifies, and re-races on `NetworkStatus` changes (the
existing `NWPathMonitor` wrapper) and after two failures. URLs for media are built at the moment
of use from the current base, so a retry by `StreamRetry` after a network change goes to the new
base. The candidate list is refreshed from `/api/v1/me` on every connect.

### Delta sync replaces search3 paging

`MusicServers.performSync` for Trackside calls `/api/v1/sync` with the stored cursor
(`Index/server-<id>-cursor.json`), applies upserts and deletes to the existing mirror
(`Index/server-<id>.json` and `server-<id>-artists.json`), and saves. The 60,000-song cap and the
album-walk fallback do not apply to Trackside. A `410 sync_reset` clears the cursor and takes a
snapshot. Sync runs on connect, on an SSE `changed` event while in the foreground, and on the
existing daily cadence as a fallback. Because the whole-mirror JSON rewrite is the next cost at
500k songs, the plan notes moving the mirror into the Rust engine's SQLite as later work, not part
of WP9.

### Playback, downloads, artwork

- `YourMusic.playback(for:)` asks `POST /api/v1/playback/decisions` once per song with the device
  profile (AVPlayer's direct-play codecs and containers: FLAC, ALAC, AAC, MP3, WAV, AIFF; HLS
  support; max bitrates from the existing `StreamQuality` settings for Wi-Fi and cellular) and
  plays the returned URL. Cellular defaults to `aac-256` (High) or `aac-128` (High Efficiency),
  replacing `format=mp3`. The response states the delivered format, so the quality badge uses it
  instead of detecting `serverSentOriginal`.
- When a song starts, the next queued song is prepared with `POST /api/v1/transcodes/prepare`, so
  gapless playback gets a cached file.
- Downloads use `/api/v1/songs/{id}/download?profile=` with the device key in the
  `Authorization` header of the background `URLSession` request; resume data works because ETags
  are strong. Error bodies are JSON with non-2xx statuses; `DownloadedAudio`'s sniffing stays
  for Subsonic servers.
- Artwork uses `/api/v1/art/{artwork_id}?size=600&mt=...`. `ArtworkImages` adds `mt` to the
  parameters it strips from cache keys, skips the stand-in probing for Trackside (missing art is a
  404), and keeps the 256 MB disk cache. The macOS `LibrarySongsTable` and `LibraryDownloads` cover
  URL builders go through `YourMusic.artworkURL` instead of `SubsonicClient`.
- Media tokens are fetched on connect and renewed a day before expiry.

### Listens

The fire-and-forget `scrobbleToServerIfDue` becomes, for Trackside, a durable outbox
(`Index/server-<id>-listens.json`) flushed in batches to `POST /api/v1/listens` with
`client_listen_id` = the Tracks play's id, so retries and a second device uploading the same play
are harmless. "Now playing" uses `POST /api/v1/now-playing`. The per-server "Tell Server About
Plays" toggle and the history pause keep applying. Tracks still scrobbles to Last.fm itself; the
server does not forward.

### What goes away when talking to Trackside

- Workarounds for a proxy that fetches songs on demand: the `ext-`/stand-in cover probe, the
  "keep = star, poll full syncs for 20 minutes, reconcile by title" flow in `ServerKeeps` and
  `MusicServers.keep`, `isExternal` handling, the 12/60 search constants and the non-external
  offset arithmetic, and `getAnimatedArt` (capabilities say it is absent, so `MotionCovers` does
  not ask).
- Throttles sized for a slow, rate-limited proxy: `AsyncLimiter` for lookups goes from 3 to 8 and
  the cover limiter from 1 to 6 for Trackside; `RequestBudget` is skipped; the 12 h answer cache
  shortens to 15 minutes because answers are cheap and change events invalidate them.
- Name-based joins: each Trackside song carries `song_key` from tracks-model, which is exactly
  `LocalTrack.identity`, so history matching (`YourMusic.track(for:)`) and availability checks
  match by key. `ServerSongMatcher` remains for Apple Music titles that differ (remaster and
  feat. stripping).
- All of these stay in place for Subsonic servers.

### Acquisition in Tracks

Acquisition is not advertised in Tracks. The "Get" action, request status and the separate
"elsewhere" search section exist in the app only when the connected Trackside server's
`/api/v1/me` contains `features.acquisition` (present only when the server has acquisition
enabled and this user can request). Otherwise nothing in the app refers to it: no onboarding, no
empty-state copy, no settings, no README or App Store text. The app never names a provider; the
wire format has no provider names to show. When present:

- Search shows library results as today, and below them a separate section of not-yet-owned
  candidates from `POST /api/v1/acquisitions/search`, never mixed into library results.
- "Get" posts a request and shows its live state from SSE `acquisition` events (queued,
  searching, getting, adding, added, couldn't get with the reason in plain words). On `done` the
  song id is final and the song is already in the mirror through the same SSE `changed` event.
  This replaces `ServerKeeps` polling for Trackside entirely.
- Playing a song the server does not have uses `POST /api/v1/external/play` and keeps nothing.
  Only Get requests a song; neither the queue, preparing the next song, Automatic Downloads nor
  Tracks Radio ever does.

ADR 0006 notes the App Review consideration of a feature that appears only with some servers.

### Platform notes

Shared: everything above. iOS only: the background session hand-off in
`Apps/iOS/App/TracksAppDelegate.swift` is unchanged; CarPlay reads downloads as today. macOS only:
the two cover URL builders named above. Both: `Info.plist` gains `_trackside._tcp` in
`NSBonjourServices`; ATS settings are already permissive for local servers.

### Moving an existing Subsonic entry to Trackside

Song ids differ between servers, so switching an entry's URL from another Subsonic server to
Trackside offers a one-time "match my downloads and playlists" step: each stored
`server:<entry>:<old id>` is matched to a Trackside song by `song_key`, album key and duration
within 2 s, and rewritten; unmatched items are listed. Downloads are kept as files either way.
