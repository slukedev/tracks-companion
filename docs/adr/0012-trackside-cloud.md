# ADR-0012: Trackside Cloud, per-user sync of the apps' own data

**Status:** Accepted
**Date:** 2026-11-24
**Deciders:** Tracks maintainer

## Context

The Tracks apps (iOS, macOS, Android) keep their listening history, sessions, stations,
settings, blocked artists, excluded stations and forgotten songs in sync through iCloud
(SwiftData over CloudKit for records, the key-value store for settings). Android has no iCloud,
and people who run Trackside want their history on their own server. The app team wrote a client
contract: pull pages by a server-assigned cursor, push idempotent batches with compare-and-set,
tombstones, a "pull now" signal over the existing event stream, a device registry so a restored
device inherits ownership, open enums, and new playlists and likes.

Constraints:

- One SQLite writer (ADR 0002). A user importing 100k Last.fm plays must not stall other users'
  listens, streams or the dashboard.
- Privacy: each user reads and writes only their own data, admins included. The owner said
  "Don't show the cloud data on the dashboard." `nearbyKey`, the 32-byte TLS secret a user's
  nearby devices share, must never be readable by the server.
- Identity must agree with the apps byte for byte (ADR 0010).

## Decision

A new crate, `trackside-cloud`, holds the merge rules, the SQL and the chunked store; the routes
in `trackside-api` are thin. Migration 4 adds four tables, all `ON DELETE CASCADE` from `users`:

- `cloud_items`: one row per record, keyed by `(user, entity, client id)`. `fields` is the app's
  JSON object, kept as sent apart from what a merge rule decides, so unknown fields and enum
  values round-trip. A tombstone keeps its row with `fields` NULL and `deleted_at` set.
  `meta` holds what the server needs and never sends (per-field times, a session's owner).
- `cloud_state`: the user's own change seq. Every write takes the next value; it is the row's
  `version` and, with the server's sync epoch, the pull cursor (`<epoch>.<seq>`, opaque to
  clients), so cursors never depend on client clocks. Paging is
  `version > cursor ORDER BY version` on a unique `(user, entity, version)` index inside one read
  snapshot, and the last page's cursor is the user's current seq read in the same snapshot.
  Because one writer assigns seqs and commits in order, a page never skips a row committed
  before it, and a row changed after it was paged reappears later at its new version.
- `cloud_entities`: items and bytes per user and entity (quotas, admin counts), and the highest
  version whose tombstone was pruned; an older cursor is `410 sync_reset`, as is a cursor above
  the user's seq.
- `cloud_devices`: the app's device ids and the ids each was restored from, linked to the
  server's `devices` row when registered with a device key.

Merge rules, per entity, all pure functions with tests:

| Entity | Rule |
|---|---|
| plays | Core fields (`title`, `artistName`, `albumTitle`, `playedAtMs`, `kind`, `source`, `recordedByDeviceID`, `platform`) never change: a different value is a conflict. Other fields are first non-null; `artworkURL` is newest by `client_modified_at`. `songKey` is filled with `tracks_model::song_key` when absent. Tombstones (with `merged_into`) are kept forever. |
| sessions | Written only by the owner (`recordedByDeviceID`, else the first pusher) or a device restored from it; otherwise the last write wins. |
| stations | Keyed by `stationKey(name)` (`tracks_model::folded`); `firstSeenMs` min, `lastSeenMs` max, the rest first non-null. |
| forgotten-songs | Keyed by `songIdentity`; grows only, `forgottenAtMs` max. Deletes are refused. |
| blocked-artists, excluded-stations, likes | One newest-wins value per item by `client_modified_at`, deletes included, so a later add brings an item back without an undelete. |
| playlists, playlist-entries | Newest wins per field; an entry keeps its `playlistID`; deleting a playlist tombstones its live entries in the same transaction. |
| settings | Tracks' `SyncedSetting.decide`, run by the server: newest wins, ties keep the stored value, a 1970 seed never replaces anything, `forgottenSongs` and `forgottenSongDates` merge by union, `null` is an explicit clear. |

Restores. A restored backup lacks whatever was written after it, and its seqs restart below
what clients hold, so a plain seq cursor would silently skip the gap. Every cursor therefore
carries a sync epoch (`settings.cloud.epoch`, also in `capabilities.cloud.epoch`). The database
also holds an instance token (`settings.cloud.instance`), replaced at every start and after
every backup, and the server keeps the last token it wrote in a sidecar file
`<database>.cloud-instance`. At start, a database whose token differs from the sidecar's is not
the copy the server last ran with (a restore, a file put in place by hand, a lost sidecar): it
gets a new epoch, and every cursor from before is `410 sync_reset`. Because the token changes
after each backup, no backup shares the live token. Copying the live file while the server runs
and putting it back later is not detected; the docs already forbid copying the live file.

Device binding. A device key speaks for one app device id: the one it registered with
`PUT /cloud/devices/{id}`, else the `client_device_id` it signed in with. Pushed items and
settings naming another id are `rejected` `device_mismatch`; registering another id from a bound
key, or an id bound to another live key, is `403 device_mismatch`. Predecessors must be the
user's own devices: registered, the `client_device_id` of one of their installs, or the
`recordedByDeviceID` of one of their own plays (an old phone that synced plays but never
registered). That last way never accepts another user's device or an id bound to a live key other
than the caller's, so nobody can claim ownership through someone else's device by writing its id
into their own plays. An expression index on the plays' `recordedByDeviceID` keeps the check one
index lookup at 300k plays.

Every push item gets `accepted`, `duplicate`, `conflict` (with the copy that stands) or
`rejected` (with a reason). A replay is a `duplicate` even with a stale `base_version`, because
the merge is computed first and only a change is checked against `base_version`.

A push runs in chunks of at most 250 items or 50 ms, each its own transaction. Between chunks
the writer's FIFO slot lets other writes in. A 100k-play import in a debug build takes about
6 s; another user pushing and pulling throughout waited a median of 6 ms and at most 53 ms.

Privacy and security:

- Only device keys reach `/cloud/*`: sessions (so the dashboard) and general API keys get `403`.
  Every SQL statement names the caller's user; there is no admin route to anyone's records.
  Admins see per-user item and byte counts on the user page, nothing else.
- Settings may hold a client-encrypted envelope `{"$encrypted": {"alg", "ct", ...}}`, stored and
  returned untouched. `nearbyKey` must be one (or `null`); plaintext is `encryption_required`,
  and a 32-byte `ct` (a raw key in disguise) is refused because a real AEAD adds a tag.
- Per-item limits (16 KiB of fields, 4 KiB strings, nesting depth 4), 1000 items per batch,
  an 8 MiB body, per-user quotas (2 000 000 items and 1 GiB by default, set in the dashboard
  with `cloud.quota_items` and `cloud.quota_bytes`), and a per-user push budget (20 batches and
  10 000 items a second with bursts) on top of the existing per-key limit.
- The change signal is a `cloud` event `{"entity": "plays"}` on `/api/v1/events`, to the owner
  only, never with data.

## Options Considered

### Option A: One generic table with merge rules in Rust (chosen)
| Dimension | Assessment |
|---|---|
| Complexity | Medium: one table, rules in one module |
| Cost | JSON parse per merged row |
| Scalability | 16k plays/s in a debug build; index-only paging |
| Team familiarity | High |

**Pros:** Unknown fields round-trip for free; new entities need a rule, not a migration.
**Cons:** Fields are opaque to SQL (fine: the server never queries them).

### Option B: A typed table per entity
| Dimension | Assessment |
|---|---|
| Complexity | High: ten tables, ten repositories |
| Cost | Every new app field is a migration |
| Scalability | Same |
| Team familiarity | High |

**Pros:** SQL could aggregate plays.
**Cons:** Unknown values would be lost or need a catch-all column anyway.

### Option C: Reuse `change_log` and the global seq
| Dimension | Assessment |
|---|---|
| Complexity | Low |
| Cost | Every cloud write wakes every library `/sync` client |
| Scalability | Change-log rows per play |
| Team familiarity | High |

**Pros:** One cursor mechanism.
**Cons:** Mixes library sync with personal data, and a 100k import would flood every user's
`changed` stream.

## Trade-off Analysis

Option A matches the contract's shape (opaque app fields, server-side decide rules) and keeps
cloud data apart from library sync. The cost is a small JSON parse per merge, measured well
within budget.

## Consequences

- Easier: adding an entity; backups (cloud tables are in the same database and `VACUUM INTO`);
  deleting a user (cascade).
- Harder: SQL now lives outside `trackside-core` for these tables; the migration stays in core.
- Deleting a user first purges their cloud rows in transactions of 5000, then deletes the
  account, whose cascade has little left; the purge is skipped when the delete would be refused
  (the last admin).
- Revisit: if an admin ever needs aggregate listening stats, compute them from `plays` in the
  cloud crate without exposing rows.

## Action Items

1. [x] Migration 4, `trackside-cloud`, routes, fixtures, api.md section 19.
2. [ ] The apps port against `crates/trackside-api/tests/fixtures/cloud-*.json`.
3. [ ] Dashboard: show the cloud counts on the user page and the quota settings.
