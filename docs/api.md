# Trackside API

Two APIs share one listener:

- `/api/v1`: the native API for Tracks and the dashboard, specified here in full.
- `/rest`: Subsonic 1.16.1 with OpenSubsonic extensions, for every other client. Coverage is in
  section 17.

Plus `/healthz`, `/readyz`, `/metrics` (opt-in) and the dashboard pages at `/`, `/setup`,
`/invite/<token>` and `/reset/<token>`.

Architecture, data model and the reasons behind these choices are in
[architecture.md](architecture.md).

Contents:

1. Conventions
2. Errors
3. Pagination, caching and idempotency
4. Shared objects
5. Public endpoints
6. Sign-in, setup, invites and pairing
7. The signed-in user, devices and keys
8. Library browsing
9. Search
10. Sync
11. Playback, media and artwork
12. Listens, stars, ratings, playlists, play queue, bookmarks
13. Events (SSE)
14. Uploads
15. Acquisition
16. Admin endpoints (dashboard)
17. Subsonic and OpenSubsonic coverage
18. Versioning and deprecation
19. Trackside Cloud

---

## 1. Conventions

- **Base URL.** `<candidate url>/api/v1`. Candidate URLs (`server.urls` in PairResult and
  `/me`, and the pairing QR) already include `server.base_path`, so a client appends `/api/v1`
  and nothing else. Examples below omit the prefix.
- **Relative URLs in responses** (`url` in a playback decision, `qr_svg_url`, acquisition
  `stream`) are absolute paths that start with `server.base_path` and `/api/v1`. Resolve them
  against the origin (scheme, host and port) of the candidate URL in use.
- **Content.** Requests and responses are `application/json; charset=utf-8` unless stated.
  Request bodies are limited to 1 MiB except uploads and Trackside Cloud batches (8 MiB,
  section 19). Responses are compressed with `br` or
  `gzip` when the client accepts it; media is never compressed.
- **Field names** are `snake_case`. Clients must ignore unknown fields and unknown enum values.
  Absent optional fields mean "unknown" or "none"; `null` is not sent, with three exceptions:
  the chart series of `/admin/stats/*`, where `null` marks a bucket with nothing recorded,
  `PATCH` bodies, where `null` clears a value (`max_bitrate_kbps`, `quick_scan_interval_s`), and
  a cleared Trackside Cloud setting (section 19). The apps' own `fields` in section 19 keep
  their own names.
- **Ids** are strings (architecture section 6), case-insensitive on input.
- **Times** are RFC 3339 UTC with milliseconds: `"2026-11-14T09:30:00.000Z"`. Durations and
  positions are integer milliseconds in fields ending `_ms`.
- **Authentication.**

  | Header or parameter | Accepted on | Notes |
  |---|---|---|
  | `Authorization: Bearer tsk_...` | every authenticated route | device keys and API keys |
  | `__Host-trackside_session` cookie | every authenticated route; required for `/admin/*` | requests that change state also need `X-Trackside-CSRF: 1` and a matching `Origin`. Over plain HTTP from a private or loopback address the cookie is `trackside_session`, without `Secure`; it is accepted only where a password would be (Plain HTTP, below) |
  | `?mt=<media token>` | media routes only (section 11, 15.6) | for AVPlayer, which cannot send headers reliably |

  Keys never travel in query strings on non-media routes; a key in a query string is rejected
  with `400 credential_in_query`.
- **Scopes.** Each endpoint lists the scope it needs: `library`, `stream`, `download`, `write`,
  `request`. Sessions have all scopes. "admin" means an admin's session.
- **Request ids.** Every response has `X-Request-Id`; error bodies repeat it.
- **Rate limit headers.** `RateLimit-Limit`, `RateLimit-Remaining`, `RateLimit-Reset` on
  limited routes, and `Retry-After` on `429`.
- **Plain HTTP.** A password, an API key or device key (`Authorization: Bearer`), or a setup,
  invite, reset or pairing secret is accepted only over HTTPS (directly, or through a proxy in
  `server.trusted_proxies` whose `X-Forwarded-Proto` says `https`, or whose `Forwarded` says
  `proto=https` when it sends no `X-Forwarded-For`), or over plain HTTP from a nearby client:
  loopback, RFC 1918, link-local, `fc00::/7`, Tailscale's `100.64.0.0/10` (unless
  `server.tailscale_is_local` is off), or a network listed in `server.local_networks`. Otherwise
  the request is refused with `403 https_required` before the credential is looked at. This covers
  `POST /setup`, `/auth/session`, `/auth/login`, `/auth/reauth`, `/me/password`,
  `/me/api-keys`, `/invites/{token}/accept`, `/password-resets/{token}`, `/pair/claim`,
  `/pair/requests` and `GET /pair/requests/{id}`, every route reached with a bearer key, and
  Subsonic `apiKey`, `u`+`p` and `u`+`t`+`s` (error 50, HTTP `403` on media endpoints). The
  plain `trackside_session` cookie follows the same rule: from a client that isn't nearby it is
  refused with `https_required`, and that response clears it (`Set-Cookie` with `Max-Age=0`).
  The `__Host-` cookie needs HTTPS anyway, and media tokens expire.
  - Forwarding headers (`X-Forwarded-For`, `X-Forwarded-Proto`, `Forwarded`, `X-Real-IP`) from a
    peer that isn't in `server.trusted_proxies` are never believed: the peer is judged by its own
    address, and a nearby peer that sends any of them is refused, since it may be forwarding
    clients from the internet. Add such a proxy to `server.trusted_proxies`.
  - From a trusted proxy, the client is the rightmost `X-Forwarded-For` hop that isn't itself a
    trusted proxy. `Forwarded` is read only when there is no `X-Forwarded-For`, because common
    proxies append to `X-Forwarded-For` and pass a client's own `Forwarded` through.
  - `https_required` carries `details.https_urls` when the server knows any HTTPS address: the
    `tls: true` entries of `GET /server/urls` (`url`, `kind`, `tls`, `fingerprint`?), so a
    refused client can switch without signing in first. They come from the server's own list,
    never from the request.
- **Concurrency per user.** At most 8 streams and downloads at once (including instant play
  and Subsonic `stream`, `download` and `hls`) and 3 transcodes at once (HLS jobs, transcoded
  downloads, Subsonic conversions) per user. Only a request that starts an encoder takes a
  transcode slot: waiting for a transcode that is cached or already being made takes none, and
  a transcoded download takes its stream slot once the file is ready to send. Over that, `429 rate_limited` with
  `Retry-After: 1`; the Subsonic media endpoints answer HTTP `429` with an error envelope.
- **Server time.** Every response has a `Date` header; clients use it to compute `client_time`
  skew if they want to.
- **Timeouts.** A handler that has not started its response within `server.request_timeout`
  (60 s) is answered with `503 unavailable`, `Retry-After: 5` and the usual error body with
  `request_id`. Downloads (which may wait for
  a transcode), uploads, backup downloads, the listen export and `/events` are exempt. A body
  that has started streaming is never cut off.
- **Security headers.** Every response has `X-Content-Type-Options: nosniff` and
  `Referrer-Policy: no-referrer`. `/api/v1` responses also have
  `Content-Security-Policy: default-src 'none'; frame-ancestors 'none'`, `X-Frame-Options: DENY`,
  `Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Resource-Policy: same-origin`,
  `Permissions-Policy: camera=(), microphone=(), geolocation=(), interest-cohort=()`, and
  `Cache-Control: no-store` unless the response sets its own caching (media, artwork, library
  reads with ETags). The dashboard pages get the same isolation headers with their own CSP
  (`default-src 'self'`, no inline code, `frame-ancestors 'none'`). `/rest` gets only the first
  two, because web Subsonic players on other origins load covers and audio from it.

---

## 2. Errors

Every non-2xx response with a body uses one shape:

```json
{
  "error": {
    "code": "not_found",
    "message": "Not found.",
    "request_id": "01JD5ZQ3W8X2V6N4M0K7B9C1DE"
  }
}
```

`code` is stable and machine-readable; `message` is English for logs and the dashboard, not for
display in Tracks. `details` is optional and code-specific.

| HTTP | `code` | When |
|---|---|---|
| 400 | `bad_request` | malformed JSON, missing or invalid field (`details.field`) |
| 400 | `credential_in_query` | a key sent as a query parameter outside media routes |
| 401 | `unauthenticated` | no or invalid credentials |
| 401 | `media_token_expired` | `mt` expired or its device was revoked |
| 403 | `forbidden` | authenticated but not allowed (role, scope, library) |
| 403 | `csrf` | session request without the CSRF header or with a foreign `Origin` |
| 403 | `setup_required` | an endpoint used before the first admin exists |
| 403 | `https_required` | a password, key, plain session cookie, or setup, invite, reset or pairing secret sent over plain HTTP from an address that isn't nearby (section 1, Plain HTTP); `details.https_urls` when the server has HTTPS addresses |
| 403 | `device_mismatch` | a Trackside Cloud route named a device id other than the one bound to the calling device key (section 19) |
| 403 | `reauth_required` | a sensitive change from a session whose last password check is over 10 minutes old; `POST /auth/reauth` and retry (section 6) |
| 404 | `not_found` | unknown id, or an id the caller cannot see (never `403`, to avoid leaking existence) |
| 405 | `method_not_allowed` | |
| 409 | `conflict` | uniqueness or a state conflict; `details.reason` is one of `username_taken`, `last_admin`, `running`, `set_by_config`, `not_writable`, `rejected`, `not_ready`, or `conflict` |
| 409 | `content_changed` | the file changed on disk since it was indexed; retry shortly |
| 409 | `idempotency_mismatch` | an `Idempotency-Key` reused with a different request |
| 410 | `sync_reset` | the cursor can no longer be served; take a snapshot |
| 410 | `candidate_expired` | an acquisition candidate token is no longer known; search again |
| 412 | `precondition_failed` | `If-Match` did not match |
| 413 | `too_large` | body or upload over the limit |
| 415 | `unsupported_media_type` | upload is not an allowed audio file |
| 416 | `range_not_satisfiable` | |
| 422 | `invalid_state` | e.g. cancelling a finished acquisition |
| 429 | `rate_limited` | `Retry-After` says when |
| 429 | `quota_exceeded` | acquisition quota; `details.resets_at` |
| 500 | `internal` | a bug; logged with the request id |
| 503 | `library_unavailable` | the song's library is offline; `Retry-After: 30` |
| 503 | `transcoder_busy` | no transcode slot and no fallback allowed |
| 503 | `unavailable` | migrating, shutting down, or the request took longer than `server.request_timeout` |
| 507 | `insufficient_storage` | the data volume is full; writes are refused |

---

## 3. Pagination, caching and idempotency

### Cursor pagination

List endpoints take `limit` (default 100, maximum 500 unless stated) and `cursor` (opaque,
from the previous page). Responses include `next_cursor`, absent on the last page. Cursors encode
the sort key of the last item, so pages stay consistent while the library changes: items are
neither skipped nor repeated, though items added before the cursor position will not appear
until the next full listing. Cursors are valid for 24 hours; an expired cursor gives
`400 bad_request` with `details.field = "cursor"`.

### ETags and conditional requests

- Library reads (`/albums/{id}`, `/artists/{id}`, `/songs/{id}`, list endpoints) return a weak
  `ETag` built from the highest change seq visible to the caller and the caller's sync epoch.
  `If-None-Match` gives `304`. `Cache-Control: private, no-cache`.
- Media (`/songs/{id}/stream`, `/download`) uses strong ETags, `If-Range`, `If-None-Match`
  (architecture section 8).
- Artwork uses `Cache-Control: private, max-age=31536000, immutable`.
- Playlists return `ETag: "<revision>"`; changes to entries take `If-Match`.

### Idempotency

- `POST` endpoints that create things accept `Idempotency-Key: <client uuid>`. The first
  response is stored for 24 hours per user and replayed for a repeat with the same key and the
  same method, path and body; a different request with the same key is `409
  idempotency_mismatch`. Marked "idempotent with key" below.
- Listens are idempotent per item by `client_listen_id` and need no header.
- `PUT` and `DELETE` are naturally idempotent.

---

## 4. Shared objects

### Song

```json
{
  "id": "s7m2k4q9x1v8b3n6c0de",
  "library_id": "l0a8f2c9e7d1b3k5m6n4",
  "title": "Halo",
  "artist": "Beyoncé",
  "artist_id": "r1q8z0c3m5n7b9v2x4w6",
  "artists": [{"id": "r1q8z0c3m5n7b9v2x4w6", "name": "Beyoncé", "role": "main"}],
  "album": "I Am... Sasha Fierce",
  "album_id": "a5c2v9b1n3m7q4w8e0r6",
  "album_artist": "Beyoncé",
  "track_number": 3,
  "disc_number": 1,
  "year": 2008,
  "release_date": "2008-11-12",
  "genres": ["R&B", "Pop"],
  "duration_ms": 261640,
  "format": {
    "container": "flac",
    "codec": "flac",
    "content_type": "audio/flac",
    "lossless": true,
    "bitrate_kbps": 1012,
    "sample_rate": 44100,
    "bit_depth": 16,
    "channels": 2,
    "size": 33102456
  },
  "artwork_id": "c9d0e1f2a3b4c5d6e7f8",
  "song_key": "halo\u001fbeyonce",
  "replay_gain": {"track_gain": -8.21, "track_peak": 0.988, "album_gain": -8.9, "album_peak": 1.0},
  "mbz": {"recording": "e6a7c1b2-3d4e-4f5a-8b6c-7d8e9f0a1b2c",
          "release": "9c8d0d3e-6d5b-4a2e-8f1a-0b2c3d4e5f60"},
  "isrc": "USSM10804555",
  "explicit": "clean",
  "has_lyrics": true,
  "added_at": "2026-03-02T18:04:11.512Z",
  "updated_at": "2026-03-02T18:04:11.512Z",
  "missing": false
}
```

`song_key` is `tracks_model::song_key(title, artist)`, byte for byte what Tracks computes as
`LocalTrack.identity`, including Foundation's case and diacritic folding (golden-tested in
`tracks-model`; the fixtures carry real keys). `missing: true` marks a song whose file is gone
but still within the grace period; it cannot be streamed. Also sent when known: `disc_subtitle`,
`bpm`. `mbz` keys are `recording`, `track`, `release`, `release_group` and `artist`; the object
is left out when there are none.

When the caller has per-user data, songs in browse responses (not in sync, which carries
annotations separately) also include:

```json
{
  "me": {"starred_at": "2026-05-01T10:00:00.000Z", "rating": 4, "play_count": 17,
         "last_played_at": "2026-11-13T21:14:09.000Z"}
}
```

Albums and artists in browse responses carry `me` the same way.

### Album

```json
{
  "id": "a5c2v9b1n3m7q4w8e0r6",
  "library_id": "l0a8f2c9e7d1b3k5m6n4",
  "name": "I Am... Sasha Fierce",
  "album_artist": "Beyoncé",
  "artist_id": "r1q8z0c3m5n7b9v2x4w6",
  "year": 2008,
  "release_date": "2008-11-12",
  "genres": ["R&B"],
  "is_compilation": false,
  "release_type": "album",
  "song_count": 11,
  "disc_count": 1,
  "duration_ms": 2486000,
  "artwork_id": "c9d0e1f2a3b4c5d6e7f8",
  "album_key": "i am... sasha fierce\u001fbeyonce",
  "mbz": {"release": "9c8d0d3e-6d5b-4a2e-8f1a-0b2c3d4e5f60"},
  "added_at": "2026-03-02T18:04:11.512Z",
  "updated_at": "2026-03-02T18:04:11.512Z"
}
```

### Artist

```json
{
  "id": "r1q8z0c3m5n7b9v2x4w6",
  "name": "Beyoncé",
  "identity": "beyonce",
  "album_count": 7,
  "song_count": 112,
  "artwork_id": "c0a1b2c3d4e5f6a7b8c9",
  "mbz": {"artist": "859d0860-d480-4efd-970c-c05d5f1776b8"}
}
```

Counts are over the caller's libraries only.

### Playlist

```json
{
  "id": "p3k9m2x7c1v5b8n0q4w6",
  "name": "Late Drive",
  "comment": "",
  "owner": {"id": "u8b1n4m7q0w3e6r9t2y5", "display_name": "Luke"},
  "is_public": false,
  "song_count": 42,
  "duration_ms": 9876000,
  "revision": 12,
  "created_at": "2026-01-02T12:00:00.000Z",
  "updated_at": "2026-11-10T08:00:00.000Z"
}
```

### Library

```json
{"id": "l0a8f2c9e7d1b3k5m6n4", "name": "Music", "state": "ok", "song_count": 48211}
```

`state` is `ok` or `unavailable`.

---

## 5. Public endpoints

### `GET /api/v1/ping`

No auth, no rate limit beyond connection limits. Used for URL racing, and to prove an address
is really the server before a client sends it a credential (ADR 0011).

```json
{"server_id": "5f0c2b9e-8a3d-4c1e-9b7a-2d6e4f8a1c3b", "time": "2026-11-14T09:30:00.000Z"}
```

With `?challenge=<nonce>&host=<address>&device=<device id>` (a paired device) or
`&offer=<offer id>` (during QR pairing, the `offer` from the QR payload), the answer adds
`proof`:

```json
{
  "server_id": "5f0c2b9e-8a3d-4c1e-9b7a-2d6e4f8a1c3b",
  "time": "2026-11-14T09:30:00.000Z",
  "proof": "682d7ce06987ceed051cff6a1f7f067f67a95506ce02dd709e0675240bbf6c13"
}
```

- `proof` is lowercase hex of `HMAC-SHA256(K, "trackside endpoint proof v1" 0x00 server_id
  0x00 host 0x00 challenge)`. For a device, `K` is SHA-256 of its device key; for an offer, `K`
  is `SHA-256("offer qr" 0x00 <QR secret>)`. Neither the key nor `K` is sent.
- `challenge` is 16 to 128 characters of letters, digits, `-` and `_`; clients send a fresh
  128-bit nonce per address and per race. `host` is the address as the client wrote it (`host`
  or `host:port`, lowercase, IPv6 in brackets, no default port).
- `proof` is left out, never an error, when the device or offer is unknown, revoked or expired,
  the challenge or host is malformed, or `host` is a nearby address (private, loopback,
  link-local, unique-local or Tailscale IP; a name without a dot; a name under `.local`, `.lan`,
  `.internal`, `.home.arpa` or `.ts.net`) and the request did not itself come from a nearby
  address, directly or through a trusted proxy. That last rule stops a relay on a stranger's
  network.
- Clients must not send a device key, password, code, QR secret or poll secret to an address
  until it has given a matching proof, and never over plain HTTP to an address that is not
  nearby. An HTTPS candidate with a `fingerprint` is held to it. Device-initiated pairing
  (`/pair/requests`) needs no proof, because the app sends nothing secret.
- A reverse proxy in front of Trackside must pass `/api/v1/ping` through with its query string.

### `GET /api/v1/capabilities`

No auth. Lets a client tell Trackside from other servers and decide what to call.

```json
{
  "product": "trackside",
  "server_id": "5f0c2b9e-8a3d-4c1e-9b7a-2d6e4f8a1c3b",
  "name": "Home",
  "version": "1.0.0",
  "api": {"versions": ["v1"], "current": "v1", "deprecated": []},
  "setup_required": false,
  "auth": {"password_login": true, "pairing": true, "pair_requests": true},
  "media": {
    "transcoding": true,
    "profiles": ["aac-256", "aac-128", "mp3-320", "mp3-128", "opus-128", "flac"],
    "hls": true,
    "suggested_cellular_profile": "aac-256"
  },
  "features": {"lyrics": true, "uploads": false, "events": true, "animated_art": false,
               "cloud": true},
  "subsonic": {"enabled": true, "path": "/rest"},
  "cloud": {"version": 1, "epoch": "3f9a0c1d7e2b4a65", "max_batch": 1000, "max_page": 2000,
            "entities": ["plays", "sessions", "stations", "forgotten-songs", "blocked-artists",
                         "excluded-stations", "playlists", "playlist-entries", "likes", "settings"]}
}
```

`features.cloud` and `cloud` say Trackside Cloud is on (section 19); `cloud` is left out when the
admin turned it off. `cloud.epoch` is the sync epoch: it changes when the server's database is
restored or replaced, and every cloud cursor from before is then refused.

The unauthenticated document never mentions acquisition. Authenticated feature flags are in
`GET /api/v1/me` (section 7).

### `GET /healthz`, `GET /readyz`

`/healthz`: `200` text `ok`. `/readyz`:

```json
{"status": "ready", "checks": {"database": "ok", "migrations": "ok", "libraries": "degraded",
 "ffmpeg": "ok"}, "notes": ["Library \"NAS\" is unavailable since 2026-11-14T08:00:00Z"]}
```

`503` with `"status": "starting"` or `"migrating"` until ready.

---

## 6. Sign-in, setup, invites and pairing

### `GET /api/v1/setup/status`

No auth. `{"setup_required": true}`.

### `POST /api/v1/setup`

No auth. Only while `setup_required`. Rate-limited per IP.

```json
{"setup_token": "7KQ2M9XD...", "username": "luke", "display_name": "Luke", "password": "..."}
```

`display_name` is optional and defaults to the username. Passwords are 1 to 1024 bytes. `201`
with the user (section 7 shape) and a session cookie. Errors: `401 unauthenticated` for a wrong
token, `409 conflict` once an admin exists.

### `POST /api/v1/auth/session` (dashboard login)

```json
{"username": "luke", "password": "..."}
```

`200` with the user and `Set-Cookie`. Failures are always `401 unauthenticated` with the same
message, whether the user exists or not. `DELETE /api/v1/auth/session` signs out (`204`).
`GET /api/v1/auth/session` returns the user or `401`.

For a session, the user also carries `recent_auth_until`: until when sensitive changes go ahead
without asking for the password again (10 minutes after sign-in or the last re-authentication).

```json
{
  "id": "uenspa9s0dhs6ps80kc00",
  "username": "luke",
  "display_name": "Luke",
  "role": "member",
  "permissions": {"can_download": true, "can_upload": false, "can_request": true,
                  "legacy_subsonic": false},
  "recent_auth_until": "2026-11-14T09:40:00.000Z"
}
```

### `POST /api/v1/auth/reauth` (session)

```json
{"password": "correct horse battery staple"}
```

Checks the signed-in user's password again and moves `recent_auth_until` 10 minutes ahead.
`204`. A wrong password is `401 unauthenticated` and counts against the sign-in rate limit; a
key instead of a session is `403 forbidden`.

These need a recent password check when the caller is a session (keys always pass), and answer
`403 reauth_required` otherwise:

- `POST /me/password`; `POST /me/api-keys`;
- `POST /admin/users` and `POST /admin/invites` with `role: admin`;
- `PATCH /admin/users/{id}` changing `role` or `disabled`;
- `POST /admin/users/{id}/password-reset`, `DELETE /admin/users/{id}`;
- `PATCH /admin/settings` with any `auth.*` key or `enrichment.lastfm_api_key`;
- `GET /admin/backups/{name}` (download) and `DELETE /admin/backups/{name}`;
- `PATCH /admin/acquire/providers/{name}` with `api_key`.

The dashboard asks for the password when it gets `reauth_required`, calls `/auth/reauth`, and
repeats the request.

### `POST /api/v1/auth/login` (app sign-in with a password)

No auth. Creates a device and a device key.

```json
{
  "username": "luke",
  "password": "...",
  "device": {"client_device_id": "8C2E...", "name": "Luke's iPhone", "platform": "ios",
             "model": "iPhone18,1", "app_name": "Tracks", "app_version": "3.2"}
}
```

`201`:

```json
{
  "api_key": "tsk_9pQ...",
  "device": {"id": "d2c4...", "name": "Luke's iPhone"},
  "user": {"id": "u8b1...", "username": "luke", "display_name": "Luke", "role": "admin"},
  "media_token": {"token": "eyJ1Ijoi...", "expires_at": "2026-11-21T09:30:00.000Z"},
  "server": {"server_id": "5f0c...", "name": "Home", "urls": [
    {"url": "http://192.168.1.20:4747", "kind": "lan", "tls": false},
    {"url": "https://box.tail4a2c.ts.net", "kind": "tailnet", "tls": true},
    {"url": "https://192.168.1.20:4747", "kind": "lan", "tls": true,
     "fingerprint": "3a:7f:0c:91:5e:2b:d4:68:aa:10:c7:3f:82:e9:51:06:bd:4c:77:e2:19:5a:f0:3d:68:c1:2e:9b:44:a5:d7:80"},
    {"url": "https://music.example.com", "kind": "public", "tls": true}]}
}
```

Signing in again from the same `client_device_id` revokes that device's previous key and returns
a new one. This shape is called **PairResult** below.

Each URL is a base URL that already includes `server.base_path`. `kind` is `lan` (detected on
the server's interfaces), `tailnet` (a `*.ts.net` name or a 100.64.0.0/10 address) or `public`.
`fingerprint` is present only with `tls.mode = "self-signed"`: the SHA-256 of the certificate,
lowercase hex pairs separated by colons, for the client to pin instead of trusting a CA.

### `POST /api/v1/me/pairing-codes` (scope: session)

Creates a pairing offer for the signed-in user.

```json
{"device_name": "Mac Studio"}
```

`device_name` is optional. `201`:

```json
{
  "id": "g4f7...",
  "code": "Q7RM-4X2K",
  "qr_payload": "trackside://pair?d=eyJ2IjoxLC...",
  "qr_svg_url": "/api/v1/me/pairing-codes/g4f7.../qr.svg",
  "expires_at": "2026-11-14T09:40:00.000Z"
}
```

`qr_svg_url` includes `server.base_path`.

The decoded `d` parameter is architecture section 10's JSON (`v`, `server_id`, `name`, `urls`,
`offer` with this offer's id for `/ping` proofs, `code` with the 128-bit secret, `exp` in Unix
seconds). `GET .../qr.svg` returns `image/svg+xml`.
`DELETE /api/v1/me/pairing-codes/{id}` cancels.

### `POST /api/v1/pair/claim`

No auth. Rate-limited per IP; an offer dies after 5 wrong attempts.

```json
{"code": "Q7RM-4X2K", "device": {"client_device_id": "8C2E...", "name": "Luke's iPhone",
 "platform": "ios", "app_name": "Tracks", "app_version": "3.2"}}
```

`code` is either the typed code or the QR secret. `201` with **PairResult**. `401` for an
unknown, expired or used code.

### Device-initiated pairing

`POST /api/v1/pair/requests` (no auth, rate-limited):

```json
{"device": {"client_device_id": "8C2E...", "name": "Living Room Mac", "platform": "macos"}}
```

`201`:

```json
{"id": "g9h2...", "code": "481 207", "poll_secret": "Zx81...", "expires_at": "2026-11-14T09:40:00.000Z",
 "poll_interval_ms": 2000}
```

`code` is six digits with a space in the middle, for the device to show.

`GET /api/v1/pair/requests/{id}` with `Authorization: Pair <poll_secret>`: `200` with
`{"state": "pending"}`, then `{"state": "approved", "result": <PairResult>}` exactly once, then
`404`. `{"state": "expired"}` after expiry.

`POST /api/v1/pair/requests/approve` (session): `{"code": "481207"}` (the space is optional)
binds the request to the signed-in user. `200` with what was approved, so the dashboard can
confirm it:

```json
{"device": {"name": "Living Room Mac", "platform": "macos"}}
```

Five wrong codes per session per 10 minutes, then `429`.

### Invites and resets

- `GET /api/v1/invites/{token}` (no auth): `404` for an unknown, used, revoked or expired
  invite, otherwise:

  ```json
  {"valid": true, "role": "member", "expires_at": "2026-11-21T09:30:00.000Z",
   "invited_by": "Luke", "note": "For Mum"}
  ```

  `invited_by` is the display name of the admin who made the invite, left out once their
  account is gone. `note` is the admin's own note on who the invite is for, left out when they
  wrote none. Whoever holds the link sees both, so an admin should not put anything in the note
  they would not tell the invitee.

- `POST /api/v1/invites/{token}/accept` (no auth): `{"username", "display_name"?, "password"}`;
  `201` with the user and a session cookie. The new account gets the invite's role,
  permissions and libraries.
- `GET /api/v1/password-resets/{token}`: `{"valid": true, "expires_at": "..."}` or `404`.
  `POST /api/v1/password-resets/{token}` `{"password"}`: `204`, all sessions ended.

---

## 7. The signed-in user, devices and keys

### `GET /api/v1/me` (scope: library)

```json
{
  "user": {"id": "u8b1...", "username": "luke", "display_name": "Luke", "role": "member",
           "permissions": {"can_download": true, "can_upload": false, "can_request": true,
                           "legacy_subsonic": false, "max_bitrate_kbps": 320}},
  "libraries": [{"id": "l0a8...", "name": "Music", "state": "ok", "song_count": 48211}],
  "device": {"id": "d2c4...", "name": "Luke's iPhone"},
  "server": {"server_id": "5f0c...", "name": "Home", "version": "1.0.0",
             "urls": [{"url": "http://192.168.1.20:4747", "kind": "lan", "tls": false}]},
  "features": {"acquisition": {"instant_play": true, "external_play": true},
               "apple_music": {"developer_token": true}}
}
```

`features.acquisition` is present only when acquisition is enabled on the server, at least one
provider is healthy or configured, the user has `can_request` (admins always), the caller's
credential has the `request` scope, and the user can see the acquired library. When any of these
is false the key is absent, not `false`. `instant_play` says a Get can play while it downloads
(section 15.5); `external_play` says a song the server does not have can be played without
keeping it (section 15.6). `features.apple_music` is present, with `developer_token: true`, only
when an admin has saved a MusicKit key that this server can use (section 16, Apple Music); it is
absent otherwise, never `false`. `max_bitrate_kbps` is left out when unlimited. `device`
is left out for a session or an API key.

`PATCH /api/v1/me` (scope: write) `{"display_name"}` returns the user. `POST /api/v1/me/password`
`{"current_password", "new_password", "sign_out_devices": false}`: `204`; from a session it needs
a recent password check (section 6).

### `GET /api/v1/server/urls` (scope: library)

The addresses a paired device may use now, so it learns new ones (a public domain or tailnet
name the owner added, a changed LAN address) without pairing again.

```json
{
  "server_id": "5f0c2b9e-8a3d-4c1e-9b7a-2d6e4f8a1c3b",
  "version": "a746df40293cc8d2b0f1b1ea782c9d87",
  "urls": [
    {"url": "http://192.168.1.20:4747", "kind": "lan", "tls": false},
    {"url": "http://home.local:4747", "kind": "lan", "tls": false},
    {"url": "https://music.example.com", "kind": "public", "tls": true}
  ]
}
```

- `urls` is the same list, in the same order and shape, as `server.urls` in PairResult and
  `/me` (section 6): detected LAN addresses, then `server.public_urls` from the configuration or
  the dashboard. It never comes from the request: no `Host`, `X-Forwarded-Host` or other header
  adds, removes or changes an address.
- `version` names the list: the same list always has the same version, across restarts. It is
  also the strong `ETag`; send `If-None-Match` to get `304` when nothing changed.
- SSE `server_urls` (section 13) says the list changed, with the new `version`. Clients connect to
  events only in the foreground, so they also ask with `If-None-Match` when they come to the
  foreground or reconnect.
- `401` without a credential. Any device key, session or API key with the `library` scope may
  read it.

How a client treats the list:

- It replaces the stored candidates with `urls` and races them as before (ADR 0009). An address
  that is gone from the list is no longer used.
- The list is only as good as the connection it came over, so a client reads it only from an
  address that has already proven itself (ADR 0011).
- Being on the list does not make an address trusted. A new address is used, like any other,
  only after it gives a matching `/ping` proof for itself; over HTTPS its certificate must pass
  the system's checks, or match `fingerprint` when one is given, and a certificate that does
  neither is refused. A plain `http://` address gets a credential only when it is nearby
  (section 5).

### Media token

`POST /api/v1/media-token` (device key, scope: stream), with no request body:
`{"token": "mt1....", "expires_at": "..."}`. Append as `mt=<token>` to media URLs. Minting a new
one does not invalidate older unexpired ones.

### `GET /api/v1/me/apple-music/developer-token` (scope: library)

A MusicKit developer token for Apple Music, signed by this server with the owner's own key, so
the private key never ships in an app. Any credential with the `library` scope may ask: device
keys, API keys and sessions, members as well as admins. Fixture:
`apple-music-developer-token.json`.

```json
{
  "token": "eyJhbGciOiJFUzI1NiIsImtpZCI6IkFCQzEyM0RFRkcifQ.eyJleHAiOjE3OTQ3MzUwMDAsImlhdCI6MTc5NDY0ODYwMCwiaXNzIjoiVEVBTTQ1NlhZWiJ9.<signature>",
  "expires_at": "2026-11-15T09:30:00.000Z",
  "refresh_after": "2026-11-15T08:30:00.000Z"
}
```

- `token` is a compact JWS. Header: `alg` `ES256` and `kid` (the key id). Claims: `iss` (the team
  id), `iat` and `exp`, in seconds. There is never an `origin` claim. The signature is the raw
  64-byte `r || s`, base64url, as JWS requires.
- A token lasts 24 hours (`exp - iat`); the server never signs one for longer than 7 days.
  `refresh_after` is an hour before `expires_at`: ask again after it, not before. Clients keep
  the token in memory and use it until then.
- The server signs one token and hands the same one to every caller until `refresh_after`, then
  signs the next. Saving or removing the key in the dashboard replaces or drops it at once, so a
  client that gets `401` or `403` from Apple asks again.
- `Cache-Control: no-store`. The token is not logged.
- `404 not_found` when no key is saved, or the saved key cannot be opened (the server's secret key
  was replaced). `401` without a credential, `403 https_required` over plain HTTP from an address
  that isn't nearby (section 1), `429 rate_limited` with `Retry-After` past 10 requests a minute per
  user.

### Devices

- `GET /api/v1/me/devices`: `{"devices": [{"id", "name", "platform", "model", "app_name",
  "app_version", "created_at", "last_seen_at", "last_ip", "current": true}]}`
- `PATCH /api/v1/me/devices/{id}` `{"name"}`.
- `DELETE /api/v1/me/devices/{id}`: revokes its key and media tokens. `204`.

### API keys and app passwords

- `GET /api/v1/me/api-keys`: `{"keys": [{"id", "kind": "api_key" | "app_password", "name",
  "prefix", "scopes": ["library","stream","write"], "created_at", "last_used_at", "last_used_ip"}]}`
- `POST /api/v1/me/api-keys` (session with a recent password check; idempotent with key):
  `{"kind": "api_key", "name": "Symfonium", "scopes": ["library","stream","write"]}`.
  `201` returns the record plus `"secret": "tsk_..."` once. For `kind: "app_password"`, requires the
  user's `legacy_subsonic` permission and the server's `auth.legacy_subsonic`; the secret is a
  24-character password for the client's password field, and the response includes
  `"username"` to type alongside it.
- `DELETE /api/v1/me/api-keys/{id}`: `204`.

### Listen export

`GET /api/v1/me/listens/export` (session): `application/x-ndjson`, one listen per line,
ListenBrainz-compatible fields (`listened_at`, `track_metadata{artist_name, track_name,
release_name, additional_info{duration_ms, recording_mbid, isrc, submission_client}}`).

---

## 8. Library browsing

All need scope `library` and filter by the caller's libraries. Optional `library_id` narrows to
one library.

| Method and path | Returns |
|---|---|
| `GET /libraries` | `{"libraries": [Library]}` |
| `GET /songs/{id}` | Song |
| `GET /songs?ids=s1,s2,...` (max 500) | `{"songs": [Song]}` in request order; unknown ids omitted |
| `GET /albums/{id}` | `{"album": Album, "songs": [Song]}` ordered by disc and track |
| `GET /albums?sort=<s>&limit&cursor&genre&year_from&year_to` | `{"albums": [Album], "next_cursor"}` |
| `GET /artists?limit&cursor` | `{"artists": [Artist], "next_cursor"}` by sort name |
| `GET /artists/{id}` | `{"artist": Artist, "albums": [Album], "appears_on": [Album], "top_songs": [Song], "bio"?}` |
| `GET /artists/{id}/top-songs?count=20` | `{"songs": [Song], "external"?: [Suggestion]}`, ranked below |
| `GET /artists/{id}/similar?count=15&in_library` | `{"artists": [{"artist": Artist?, "name", "identity", "score", "in_library": bool}]}`; `in_library=true` keeps only artists in the caller's libraries, chosen before the list is cut to `count` |
| `GET /songs/{id}/similar?count=20` | `{"songs": [Song], "external"?: [Suggestion]}`, ranked below |
| `GET /songs/random?count=40&genre&year_from&year_to` | `{"songs": [Song]}` |
| `GET /songs/top?count=50` (max 200) | `{"songs": [Song]}`, the most played songs on this server (Shelves below) |
| `GET /me/mixes/{id}?tz` | `{"songs": [Song]}`, one of Tracks' mixes from the caller's own listens, `id` as the apps name it (Shelves below) |
| `GET /me/picks?count=40&tz` (max 200) | `{"songs": [Song], "external"?: [Suggestion]}`, songs like the caller's favourites (Shelves below) |
| `GET /me/get-more?count=24` (max 100) | `{"songs": [Song], "external"?: [Suggestion]}`, Tracks' Get More ranking (Shelves below) |
| `GET /genres` | `{"genres": [{"name", "song_count", "album_count"}]}` |
| `GET /songs/{id}/lyrics` | section 11 |
| `GET /folders/{id}` | `{"folder": {"id","name","parent_id"}, "folders": [...], "songs": [Song]}` |

Album `sort` values: `newest` (added), `recent` (caller's last played), `frequent` (caller's
plays), `random`, `alphabetical`, `alphabetical_by_artist`, `starred`, `year`, `highest`
(caller's rating). `random` ignores `cursor` and takes `seed` for repeatable pages.

Suggestions (similar songs, similar artists, top songs, the artist's `bio`) come from local
signals (shared credits, the same album, genres, decade, co-listening in this server's listens,
co-occurrence in playlists) and, when the admin sets a Last.fm API key (section 16, Suggestions),
from Last.fm. Similar artists use shared credits, co-listening and shared genres, so a library
without genre tags still gets them once people listen. Everything but `bio` answers without the
key; Last.fm data, once stored, is merged into the same ranking. The
key is one for the whole server: nobody needs a Last.fm account, and the server only calls Last.fm's
public read methods with artist and track names. Last.fm data is fetched in the background,
most played artists and songs first, and kept for 30 days. A request for something not looked up
yet starts the lookup and waits at most 1.5 s for it before answering from what is stored.

Ranking is per user: the caller's own stars and plays raise a song or artist. Top songs are
ordered by a score of plays on this server, the caller's own plays counted twice, the caller's
star and the song's rank on Last.fm; then the caller's rating, then album order. `songs` only
ever holds songs the caller can see. `bio` is a short plain-text summary, left out when there is
none.

`external` lists songs Last.fm suggests that the caller cannot play here, as `Suggestion`
objects `{"title", "artist"}`, best first, at most 20. It is present only when `/me` has
`features.acquisition` and the credential has the `request` scope, and then may be empty. A
client offers to request one with `POST /acquisitions` (section 15.3). Nothing names the source.

#### Shelves

Lists made from listens (section 12) and the library alone, for a client's home shelves. The
mixes and Get More come from Tracks' own engine (`tracks-mixes` and `tracks-library`, at the
`tracks-model` revision in `Cargo.toml`), so the server and the apps give the same answer for
the same plays. Each list holds library songs the caller can see, one per `song_key`, each with
the caller's `me`. A listen sent by title and artist without `song_id` counts toward the song
with the same `song_key`. Only the caller's own listens are read, except for top songs.

- **Mixes**, `GET /me/mixes/{id}`: the mix the apps' `MixBuilder` builds from the caller's
  listens, in its order: `onRepeat` (three plays or more in 30 days, recent plays counting
  most), `rediscover` (four plays or more, none in 60 days, with 90 days of history), `newFinds`,
  `allTimeFavorites`, `deepCuts`, `throwback.<months>` and `rightNow.<part>.<weekend>`. `tz` is
  the listener's IANA time zone (default UTC) for the parts of the day and the daily order; an
  unknown one is `400`. A mix the engine does not offer for these listens (it needs 6 songs,
  8 for `rightNow`), or an id it does not make, is `{"songs": []}`. `radioFinds` is always empty:
  listens do not say whether a station played them. Songs not on this server are left out.
- **Top songs**, `GET /songs/top`: the most played songs on this server. The caller's plays count
  twice. Other people's plays of a song count only when at least two people other than the
  caller played it, so the list never shows what one other person plays. Ties go to the most
  recently played. Empty while nobody has played anything.
- **Picks**, `GET /me/picks`: songs like the ones the caller plays, by way of similar songs
  (above), new to them. It starts from up to 5 songs by different artists: from the caller's
  `onRepeat`, `newFinds` and `allTimeFavorites` mixes, then their latest listens, then their
  starred songs; with none of those, this server's top songs, then songs picked at random, so
  a new server and a new listener get picks too. It takes from each in turn, leaves out the
  starting songs and anything the caller played in the last 30 days, and orders the result with
  the engine's daily artist-spread shuffle (`tz` as for mixes). `external` is as above, from
  Last.fm's similar songs for the starting songs; lookups for them start in the background and
  the answer never waits for Last.fm.
- **Get More**, `GET /me/get-more`: the engine's Get More ranking (`tracks_library::rank`). The
  taste is the caller's listens by artist (plays, last heard); the similar artists are this
  server's (above) for their top 5 artists. `songs` are library songs by those artists the caller
  has never played; `external`, as above, Last.fm's top songs by them that the caller cannot play
  here. Empty until the caller has listened.

A client shows On Repeat from `/me/mixes/onRepeat`, Rediscover from `/me/mixes/rediscover`, a
Most Played shelf from `/songs/top`, server picks from `/me/picks`, Get More from
`/me/get-more`, songs like one song from `/songs/{id}/similar`, and artists to explore from
`/artists/{id}/similar` (with `in_library=true` when it can show only artists the server has).
Fixtures: `mix.json`, `top-songs.json`, `picks.json`, `get-more.json`, `similar-songs.json`,
`similar-artists.json`.

---

## 9. Search

### `GET /api/v1/search?q=<text>&types=song,album,artist&limit=20&cursor` (scope: library)

`q` is folded with `tracks_model::search_folded`, then every word matches as a prefix.
Results are ranked by bm25 with exact folded matches first. `types` is a comma-separated list
of `song`, `album`, `artist` (plurals accepted) and `external`; absent means the three library
types. `limit` (default 20, maximum 500) applies to each type separately. `library_id` narrows
to one library. Sections that were not asked for are left out. Song, album and artist objects
are shortened here; the server sends the full shapes of section 4.

```json
{
  "songs":   {"items": [{"id": "sdjcz08vf67h7cx28snq6", "title": "Halo"}],
              "next_cursor": "eyJ0IjoicyIsIm8iOjIwfQ"},
  "albums":  {"items": [{"id": "a3pqh4wnvzkjak75rxbft", "name": "I Am... Sasha Fierce"}]},
  "artists": {"items": [{"id": "rgtm6gw8kzkjyq611mzzp", "name": "Beyoncé"}]}
}
```

A cursor pages a single type: pass `types=song` with the cursor from `songs.next_cursor`; a
cursor with more than one library type is `400 bad_request` (`details.field = "types"`).

Library results never include anything the server does not have. With `types=external`, and
only when `/me` has `features.acquisition` and the credential has the `request` scope, the
response adds a separate `external` section: `{"search_id", "complete", "candidates":
[Candidate]}`, the same as `POST /acquisitions/search` (section 15.2), waiting up to 8 s. It is
left out otherwise, never an error.

---

## 10. Sync

### `GET /api/v1/sync?cursor=<c>&limit=<n>` (scope: library)

`limit` default 2000, maximum `sync.max_page` (5000). Responses should be requested with
`Accept-Encoding: br, gzip`.

**Snapshot** (no cursor, or continuing a snapshot). Records are shortened here:

```json
{
  "mode": "snapshot",
  "snapshot": {"seq": 918274, "kind": "songs", "next": "eyJ2IjoxLCJlIjoxLCJzIjo5MTgyNzR9"},
  "libraries": [{"id": "ldhmwjvv1e9320kfeh1mp", "name": "Music", "state": "ok", "song_count": 48211}],
  "songs": [{"id": "sdjcz08vf67h7cx28snq6", "title": "Halo"}],
  "next_cursor": "eyJ2IjoxLCJlIjoxLCJzIjo5MTgyNzR9"
}
```

Snapshot pages go through kinds in order `libraries`, `artists`, `albums`, `songs`,
`playlists` (with entries), `stars`, `ratings`, `play_stats`, `play_queue`. Each page holds one
kind, under the same key and in the same shape as that kind in a delta's `upserts`. Every page
also carries `libraries` (the caller's visible libraries, with their current `state`).
`snapshot.next` repeats `next_cursor`. The last page has `"complete": true`, no
`snapshot.next`, and a delta cursor in `next_cursor`.

**Delta** (delta cursor):

```json
{
  "mode": "delta",
  "upserts": {
    "songs": [{"id": "sdjcz08vf67h7cx28snq6", "title": "Halo"}],
    "albums": [{"id": "a3pqh4wnvzkjak75rxbft", "name": "I Am... Sasha Fierce"}],
    "playlists": [{"playlist": {"id": "pe13g4nkc9a478837bstf", "name": "Late Drive"},
                   "entries": ["sdjcz08vf67h7cx28snq6", "st5atawrhvfej1d3h554d"]}],
    "stars": [{"kind": "song", "id": "sdjcz08vf67h7cx28snq6", "starred_at": "2026-11-14T09:29:00.000Z"}],
    "ratings": [{"kind": "album", "id": "a3pqh4wnvzkjak75rxbft", "rating": 5}],
    "play_stats": [{"id": "sdjcz08vf67h7cx28snq6", "play_count": 18,
                    "last_played_at": "2026-11-14T09:29:30.000Z"}],
    "play_queue": {"entries": ["sdjcz08vf67h7cx28snq6"], "current": 1, "position_ms": 81000,
                   "changed_by": "Tracks", "device_id": "dchjkb9k3cnapjw0kmhq6",
                   "updated_at": "2026-11-14T09:25:00.000Z"}
  },
  "deletes": {
    "songs": ["st5atawrhvfej1d3h554d"],
    "stars": [{"kind": "song", "id": "skskbcsede1n6haz492vm"}]
  },
  "next_cursor": "eyJ2IjoxLCJlIjoxLCJzIjo5MTgzMDF9",
  "has_more": false
}
```

`upserts` can hold `libraries`, `artists`, `albums`, `songs`, `playlists`, `stars`, `ratings`,
`play_stats` and `play_queue`; `deletes` can hold id lists for `songs`, `albums`, `artists`,
`libraries` and `playlists`, and `{"kind", "id"}` items for `stars` and `ratings`.

Rules:

- Apply upserts and deletes in any order within a page; each id appears at most once per page
  (the change log keeps only an entity's latest change).
- Keep requesting while `has_more` is true; then wait for an SSE `changed` event or the next
  periodic sync.
- `410 sync_reset`: discard the cursor (keep local data until the new snapshot completes, then
  replace it).
- A song moving to a library the caller cannot see arrives as a delete.
- Renaming an album (or anything else a song shows, such as its album artist) also records an
  upsert for every song on it, so a client that shows album names from its song mirror sees the
  change.
- Empty arrays are left out.

---

## 11. Playback, media and artwork

### `POST /api/v1/playback/decisions` (scope: stream)

```json
{
  "song_id": "s7m2k4q9x1v8b3n6c0de",
  "network": "cellular",
  "client": {
    "direct": [
      {"container": "flac", "codecs": ["flac"]},
      {"container": "m4a", "codecs": ["aac", "alac"]},
      {"container": "mp3", "codecs": ["mp3"]},
      {"container": "wav", "codecs": ["pcm"]}
    ],
    "hls": true,
    "max_bitrate_kbps": 320,
    "preferred_profile": "aac-256",
    "allow_original_fallback": true
  }
}
```

`network` is `wifi`, `wired`, `cellular` or `unknown` and is recorded for diagnostics; the limit
the server enforces is `max_bitrate_kbps` combined with the user's own limit.

`200`:

```json
{
  "mode": "file",
  "url": "/api/v1/songs/s7m2.../stream?profile=aac-256",
  "delivered": {"container": "m4a", "codec": "aac", "bitrate_kbps": 256, "sample_rate": 44100,
                "lossless": false, "size": 8413002, "duration_ms": 261640},
  "reason": "over_bitrate",
  "etag": "\"s7m2....3.aac-256.f3c1a9\""
}
```

`mode`:

- `direct`: `url` is the original stream URL; `delivered` describes the original; `reason`
  absent, or `transcoder_busy` or `no_transcoder` when the original was sent because no
  conversion was possible (`allow_original_fallback`).
- `file`: a complete cached transcode, range-served.
- `hls`: `url` is `/api/v1/transcodes/{job}/index.m3u8`; `delivered.size` absent.

`reason` is `over_bitrate`, `unsupported_codec`, `transcoder_busy` or `no_transcoder`. `etag`
(the cached file's strong ETag) is present only for `file`. `url` is an absolute path that includes `server.base_path`
(section 1); the client appends `mt`. Without a fallback, a song that needs converting when no
transcoder is free or installed is `503 transcoder_busy`.

### `POST /api/v1/transcodes/prepare` (scope: stream)

```json
{"songs": ["s1...", "s2..."], "profile": "aac-256"}
```

`202 {"jobs": [{"song_id": "s1...", "state": "cached" | "queued" | "running"}]}`. Prepared jobs run
at prefetch priority. Maximum 5 songs per call.

### `GET /api/v1/songs/{id}/stream?profile=<p>&mt=<token>` (scope: stream)

Without `profile`: the original file. With `profile`: the cached transcode if complete, otherwise
`409 conflict` with `details.reason = "not_ready"` (use the decision endpoint, which picks HLS).
Range, `HEAD`, `If-Range` and ETags as in architecture section 8. Does not count a play.

### `GET /api/v1/songs/{id}/download?profile=<p>` (scope: download)

Same as stream but `Content-Disposition: attachment` with a sanitised file name, requires
`can_download`, and for a profile that is not cached yet the request waits for the transcode
(download priority) and then sends the complete file with `Content-Length`. Background sessions
use the `Authorization` header; `mt` also works.

### HLS

- `GET /api/v1/transcodes/{job}/index.m3u8`: `application/vnd.apple.mpegurl`, VOD, `#EXT-X-VERSION:7`,
  `#EXT-X-MAP:URI="init.mp4"`, segments `seg-<n>.m4s` with relative URLs that carry the same `mt`.
- `GET /api/v1/transcodes/{job}/init.mp4` and `/seg-<n>.m4s`: wait up to 10 s for production,
  then `503 transcoder_busy` with `Retry-After: 1`.
- Jobs expire 30 minutes after their last request; an expired job's playlist is `404` and the
  client asks for a new decision.
- A job's URLs work only for the user it was made for (admins too), and only while that user
  can still see its song; anyone else gets `404`.

### `GET /api/v1/art/{artwork_id}?size=<px>&mt=<token>` (scope: stream)

`size` snaps up to 64, 128, 256, 300, 600, 1200; absent means original. `image/jpeg` for resized,
the original type otherwise. `404 not_found` when there is no artwork. Song, album and artist ids
are also accepted in place of an artwork id and resolve to their artwork (`404` if none). An
artwork id (`c...`) is served only when it belongs to a song, album or artist the caller can see;
otherwise `404`.

### `GET /api/v1/songs/{id}/lyrics` (scope: library)

```json
{
  "lyrics": [
    {"kind": "synced", "lang": "eng", "source": "sidecar", "offset_ms": 0,
     "lines": [{"start_ms": 12340, "text": "Remember those walls I built?"}]},
    {"kind": "plain", "lang": "eng", "source": "embedded",
     "lines": [{"text": "Remember those walls I built?"}]}
  ]
}
```

`404` when the song has none.

---

## 12. Listens, stars, ratings, playlists, play queue, bookmarks

### `POST /api/v1/listens` (scope: write)

Up to 1000 items. Each item is idempotent by `client_listen_id`.

```json
{
  "client_time": "2026-11-14T09:30:00.000Z",
  "listens": [
    {"client_listen_id": "6F1D9C2A-3B4E-4F5A-8B7C-1D2E3F4A5B6C",
     "song_id": "s7m2k4q9x1v8b3n6c0de",
     "started_at": "2026-11-13T21:10:02.000Z",
     "played_ms": 254000,
     "title": "Halo", "artist": "Beyoncé", "album": "I Am... Sasha Fierce",
     "duration_ms": 261640}
  ]
}
```

`song_id` is optional: a listen of a song not on this server is kept with its text and
`song_key`. `200`:

```json
{"results": [{"client_listen_id": "6F1D...", "status": "accepted"}]}
```

`status` is `accepted`, `duplicate` (already stored; success for the client) or `rejected` with
`reason` (`future`, `invalid`). A rejected item should be dropped by the client, not retried. The
whole batch fails only for auth, size or malformed JSON.

### `POST /api/v1/now-playing` (scope: write)

`{"song_id": "s7m2...", "position_ms": 0, "state": "playing" | "paused" | "stopped"}`. `204`. Feeds
the Subsonic `getNowPlaying` view and the dashboard; expires 30 minutes after the song's
expected end.

### Stars and ratings (scope: write)

- `PUT /api/v1/me/stars/{kind}/{id}` where kind is `song`, `album`, `artist`: `204`.
  Body optional `{"at": "..."}` (client time, used only for display).
- `DELETE /api/v1/me/stars/{kind}/{id}`: `204`.
- `PUT /api/v1/me/ratings/{kind}/{id}` `{"rating": 1..5}`; `DELETE` clears. `204`.
- `GET /api/v1/me/stars?kind=song&limit&cursor`: `{"items": [Song|Album|Artist], "next_cursor"}`.

Stars are favourites. They never trigger acquisition or any other side effect.

### Playlists (scope: library to read, write to change)

- `GET /api/v1/playlists`: own and public playlists, `{"playlists": [Playlist]}`.
- `POST /api/v1/playlists` (idempotent with key): `{"name", "comment"?, "is_public"?, "entries"?: [song ids]}`; `201` Playlist.
- `GET /api/v1/playlists/{id}`: `{"playlist": Playlist, "entries": [{"position", "song": Song?, "song_id", "added_at"}]}` with `ETag: "<revision>"`. An entry whose song is gone keeps `song_id` and omits `song`. For
anyone but the owner, a public playlist lists only the entries whose songs are in libraries the
caller can see, in sync as well.
- `PATCH /api/v1/playlists/{id}`: `{"name"?, "comment"?, "is_public"?}`. Owner only.
- `PUT /api/v1/playlists/{id}/entries` with `If-Match: "<revision>"`: `{"entries": [song ids]}`
  replaces the list. `412` if the revision moved; the client refetches and merges.
- `POST /api/v1/playlists/{id}/entries`: `{"append": [song ids]}`, no `If-Match` needed.
- `DELETE /api/v1/playlists/{id}`: `204`.

### Play queue (scope: write)

- `GET /api/v1/me/play-queue`: `{"entries": [song ids], "current": 3, "position_ms": 81000,
  "changed_by": "Tracks", "device_id": "d2c4...", "updated_at": "..."}` or `204` when empty.
- `PUT /api/v1/me/play-queue` with the same fields (without `updated_at`). Last writer wins; the
  response is the stored queue. At most 2000 entries.

### Bookmarks (scope: write)

`GET /api/v1/me/bookmarks`, `PUT /api/v1/me/bookmarks/{song_id}` `{"position_ms", "comment"?}`,
`DELETE /api/v1/me/bookmarks/{song_id}`.

---

## 13. Events (SSE)

### `GET /api/v1/events` (scope: library)

`text/event-stream`. Headers `Cache-Control: no-cache`, `X-Accel-Buffering: no`. A comment line
(`: keep-alive`) every 20 s. `Last-Event-ID` is accepted; the server replies with a single
`changed` event at the current seq if anything happened since.

| `event` | `data` | Meaning |
|---|---|---|
| `hello` | `{"server_id", "seq"}` | sent first |
| `changed` | `{"seq": 918301}` | something visible to this user changed; call `/sync` |
| `resync` | `{}` | the stream fell behind, or the caller's role or libraries changed (the stream re-reads them first); call `/sync` |
| `library` | `{"id", "state"}` | a library became available or unavailable |
| `scan` | `{"library_id", "state", "files_seen", "added", "updated", "missing"}` | admins only, at most once a second |
| `acquisition` | Acquisition (15.3) | the caller's requests; admins see all |
| `upload` | `{"id", "state", "song_id"?}` | the caller's uploads |
| `server_urls` | `{"version"}` | the server's addresses changed; call `GET /server/urls` (section 7) |
| `session` | `{"reason": "revoked"}` | the credential was revoked; the stream ends |
| `cloud` | `{"entity": "plays"}` | the caller's Trackside Cloud data for `entity` (or `devices`) changed; pull it (section 19). Never carries the data |

`id` of each event is the seq at the time. Each event type is sent only to callers allowed to see
it. Clients connect only while in the foreground. A user may have 16 streams open at once; a
17th is `429 rate_limited` with `Retry-After: 5`.

---

## 14. Uploads

Requires `uploads.enabled` (otherwise `404`), the user's `can_upload`, and a writable uploads
library the user can see. Permission is checked on every request, so an upload under way gets
`403 forbidden` on its next chunk once `can_upload` is taken away.

- A user may have 8 uploads receiving at once; a ninth is `429 quota_exceeded`.
- A new upload is `507 insufficient_storage` when the data volume would keep less than 512 MiB
  free after it and the user's other unfinished uploads.
- An upload still receiving that gets no data for 24 hours is deleted, with its partial file.


- `POST /api/v1/uploads` (scope: write, idempotent with key): `{"file_name": "01 Halo.flac",
  "size": 33102456, "sha256"?: "..."}` → `201 {"id": "i5...", "state": "receiving",
  "received": 0, "chunk_max": 16777216}`.
- `PUT /api/v1/uploads/{id}` with `Content-Range: bytes <start>-<end>/<size>` and raw bytes,
  at most `chunk_max` per request. `200 {"received": n}`. A chunk that does not start at
  `received` is `409` with `details.received` so the client resumes.
- `POST /api/v1/uploads/{id}/complete` → `202 {"state": "staged"}`; then the importer verifies
  (same checks as acquisition, architecture 9.2), tags stay as uploaded, and the file is moved
  into the uploads library with `uploads.path_template`.
- `GET /api/v1/uploads/{id}`: `{"id", "state": "receiving" | "staged" | "imported" | "duplicate" |
  "rejected" | "failed", "song_id"?, "error"?}`. `duplicate` returns the existing song's id.
- `DELETE /api/v1/uploads/{id}` abandons a receiving upload.

The inbox folder (`uploads.inbox_dir`) runs the same importer for files dropped on disk.

---

## 15. Acquisition

Present only when `GET /api/v1/me` has `features.acquisition`. Otherwise every endpoint in this
section answers `404 not_found`, as if it did not exist. Provider names never appear in these
responses.

### 15.1 Objects

**Candidate**

```json
{
  "token": "x3n8b2v6c1m9q4w7e0r5t8y2",
  "title": "Halo",
  "artist": "Beyoncé",
  "album": "I Am... Sasha Fierce",
  "duration_ms": 261000,
  "quality": "lossless",
  "format": {"container": "flac", "codec": "flac", "bitrate_kbps": 1010, "sample_rate": 44100, "bit_depth": 16},
  "size": 33000000,
  "instant": false,
  "match": "exact"
}
```

`quality` is `lossless`, `high` (lossy ≥ 256 kbps) or `standard`. `instant` says whether
instant play is possible for this candidate. `match` is `exact` or `close`. Tokens are stable for
the same source item across searches and valid for at least 24 hours.

**Acquisition**

```json
{
  "id": "q8w1e4r7t0y3u6i9o2p5",
  "state": "downloading",
  "title": "Halo",
  "artist": "Beyoncé",
  "album": "I Am... Sasha Fierce",
  "progress": {"bytes": 12000000, "total": 33000000},
  "quality": "lossless",
  "existing": false,
  "created_at": "2026-11-14T09:30:00.000Z",
  "updated_at": "2026-11-14T09:30:07.000Z"
}
```

`state`: `queued`, `searching`, `downloading`, `verifying`, `importing`, `done`, `failed`,
`cancelled`. On `done`, `song_id` is the final, stable id of the song in the library and
`existing` says whether it was already there. On `failed`, `failure` is one of:

| `failure` | Meaning |
|---|---|
| `no_match` | nothing found that passed the filters |
| `all_candidates_failed` | every candidate failed to download or verify |
| `exhausted` | retries used up |
| `provider_unavailable` | no enabled source could be reached |
| `insufficient_space` | the acquired library or staging volume is full |
| `rejected_file` | the downloaded file failed verification |
| `quota_exceeded` | quota reached while queued |
| `disabled` | acquisition was turned off while queued |

`progress`, `quality`, `song_id`, `failure` and `stream` are left out until they apply.
`stream`, for instant play, is `{"hls": "/api/v1/acquisitions/q8w1.../stream.m3u8",
"progressive": "/api/v1/acquisitions/q8w1.../stream"}` (append `mt`).

### 15.2 `POST /api/v1/acquisitions/search` (scope: request)

```json
{"query": "beyonce halo"}
```

or `{"title": "Halo", "artist": "Beyoncé", "album"?: "...", "duration_ms"?: 261640}`.

Searches enabled sources in parallel, waiting up to 8 s. `200`:

```json
{
  "search_id": "v2b5...",
  "complete": true,
  "in_library": [],
  "candidates": [{"token": "x3n8b2v6c1m9q4w7e0r5t8y2", "title": "Halo", "artist": "Beyoncé",
                  "quality": "lossless", "format": {"container": "flac", "codec": "flac"},
                  "instant": false, "match": "exact"}]
}
```

`in_library` holds full Songs; `candidates` holds full Candidates (shortened here).

`in_library` holds library songs with the same `song_key`, so a client can show "you have this"
instead of offering to get it. When `complete` is false, `GET /api/v1/acquisitions/searches/{search_id}`
returns the same shape with more candidates; searches expire after 10 minutes. Rate limited to
20 searches a minute per user.

### 15.3 `POST /api/v1/acquisitions` (scope: request, idempotent with key)

```json
{
  "title": "Halo",
  "artist": "Beyoncé",
  "album": "I Am... Sasha Fierce",
  "duration_ms": 261640,
  "candidate": "x3n8b2v6c1m9q4w7e0r5t8y2",
  "allow_versions": false,
  "instant": false
}
```

Only `title` and `artist` are required. With `candidate`, that candidate is tried first and the
others from a fresh search follow if it fails.

- `200` Acquisition with `state: done, existing: true` when the library already has it.
- `202` Acquisition otherwise (a request joining an identical active one is also `202`).
- `429 quota_exceeded` with `details.resets_at`.
- `410 candidate_expired`.

### 15.4 Status and control

- `GET /api/v1/acquisitions/{id}`: Acquisition.
- `GET /api/v1/acquisitions?state=active|finished&limit&cursor`: the caller's requests,
  newest first, as `{"acquisitions": [Acquisition], "next_cursor"?}`.
- `DELETE /api/v1/acquisitions/{id}`: cancels a request that has not finished; `204`. A joined
  request is detached and cancelled without affecting the one it joined. `422 invalid_state` once
  finished.
- SSE `acquisition` events carry the full object on every state change and at most once a second
  for progress.

### 15.5 Instant play

`instant: true` requires a candidate with `instant: true` and `acquire.instant_play`. The `202`
response includes `stream` as soon as bytes are flowing (usually within 2 s); until then the
object has no `stream` and an SSE event delivers it.

- `GET /api/v1/acquisitions/{id}/stream.m3u8` and its segments: HLS as in section 11.
- `GET /api/v1/acquisitions/{id}/stream`: before the download completes, `200` with no
  `Content-Length`, `Accept-Ranges: none`; `Range` is ignored and the body starts at byte 0.
  After completion, ranges and a strong ETag. After import, `308` to
  `/api/v1/songs/{song_id}/stream`.

Media token rules apply (scope: stream).

### 15.6 Playing without keeping (scope: request)

`POST /api/v1/external/play` plays a song the server does not have without keeping it: no
acquisition, no library song, no sync change, nothing in the caller's requests. Only a request
(section 15.3) keeps a song. Present only with `features.acquisition.external_play`.

```json
{"candidate": "x3n8b2v6c1m9q4w7e0r5t8y2"}
```

or `{"title": "Halo", "artist": "Beyoncé", "album"?: "...", "duration_ms"?: 261640}`, for a
suggestion that came without a candidate (`external` in section 8). A candidate that cannot
stream is played from one that can. `200`:

```json
{
  "title": "Halo",
  "artist": "Beyoncé",
  "duration_ms": 261000,
  "stream": {"hls": "/api/v1/external/plays/x3n8.../stream.m3u8",
             "progressive": "/api/v1/external/plays/x3n8.../stream"}
}
```

`stream` works as in section 15.5 (append `mt`; scope: stream). When the caller's library has
the song after all, the answer has `song_id` and no `stream`; play that song instead. The same
song asked again plays from what the server already fetched, while it keeps it: the server keeps
the most recently played of these songs within limits the admin sets (section 16), so a
`stream` URL can stop working (`404`) once the song has not been played for a while. Ask again
with the same body to get it back; it may be fetched again.

- `400 bad_request` without a candidate or title and artist.
- `404 not_found` when nothing matched.
- `410 candidate_expired`.
- `429 rate_limited`: two songs of the caller's are still being fetched, or too many searches.
- `503 unavailable` with `Retry-After`: the source is paused or busy.

---

## 16. Admin endpoints (dashboard)

All under `/api/v1/admin`, admin session only, CSRF header on changes. Every change is written to
the audit log. Changes marked "recent password" need a password check within the last 10 minutes
(section 6, `POST /auth/reauth`) and otherwise answer `403 reauth_required`. Fixtures for every
shape here are in `crates/trackside-api/tests/fixtures/admin/`, and the dashboard's preview
fixtures in `dashboard/dev/fixtures/` use the same shapes.

### Overview

`GET /admin/overview`:

```json
{
  "server": {"name": "Home", "server_id": "5f0c2b9e-8a3d-4c1e-9b7a-2d6e4f8a1c3b",
             "version": "1.0.0", "uptime_s": 86400, "started_at": "2026-11-13T09:30:00.000Z",
             "data_dir": "/data", "tls": "off", "listen": ["[::]:4747"]},
  "storage": {"database_bytes": 812000000, "wal_bytes": 4100000, "cache_bytes": 3200000000,
              "data_free_bytes": 120000000000, "warnings": []},
  "libraries": [{"id": "ldhmwjvv1e9320kfeh1mp", "name": "Music", "state": "ok",
                 "song_count": 48211, "scanning": false}],
  "media": {"ffmpeg": {"found": true, "version": "7.1"}, "transcodes": {"running": 1, "queued": 0},
            "transcode_cache_bytes": 2100000000},
  "acquisition": {"enabled": true, "providers": [], "active": 2, "today": 14,
                  "fetched": {"songs": 212, "bytes": 7400000000, "fetching": 1, "keep": 500,
                              "max_bytes": 21474836480}},
  "users": {"count": 4, "devices": 7},
  "backups": {"last": {"name": "trackside-20261114T033000Z.db", "at": "2026-11-14T03:30:00.000Z",
                       "bytes": 790000000},
              "next_at": "2026-11-15T03:30:00.000Z"},
  "problems": [{"severity": "warning", "code": "library_unavailable", "title": "Music offline",
                "message": "Music is unavailable.",
                "help": "Check that the disk or share is mounted at /music. Its songs stay in the library until it is back.",
                "target": {"kind": "library", "id": "ldhmwjvv1e9320kfeh1mp"}}]
}
```

- `server.tls` is `off`, `files`, `acme` or `self-signed`.
- Each library may carry `last_scan` (a ScanRun).
- `acquisition` is left out when the server was built without acquisition, and has
  `"enabled": false` when it can do it but it is switched off. `providers` holds ProviderStatus
  objects (below).
- `acquisition.fetched` is what songs played without being kept (section 15.6) take now:
  `songs` and `bytes` that are fetched and kept for replays, `fetching` still coming in, and the
  limits `keep` and `max_bytes` (the settings `acquire.fetched_keep` and
  `acquire.fetched_max_bytes`). `songs` and `bytes` can be over the limits for a while: a song
  being fetched, streamed, transcoded or played in the last 30 minutes is never evicted. Left out
  when the acquirer cannot tell.
- `backups.last`, `backups.next_at` and `backups.last_error` are left out when there is none.
- A problem has `severity` (`info`, `warning` or `error`), a stable `code` (`library_unavailable`,
  `inotify_limit`, `low_disk`, `no_ffmpeg`, `backup_failed`), a `title` of six words or fewer
  for its heading, a `message`, and an optional `help` that says what to do.
- A problem about one record has `target`, `{"kind", "id"}`, so a client can link to it without
  matching names: `library_unavailable` and `inotify_limit` name their library with
  `{"kind": "library", "id": "..."}`. Other kinds may come later; ignore ones you don't know.

### Users

| Method and path | Body or result |
|---|---|
| `GET /admin/users` | `{"users": [AdminUser]}` |
| `POST /admin/users` | `{"username", "display_name"?, "password", "role"?, "permissions"?, "library_ids"}` → `201` AdminUser. `role` defaults to `member`; `role: admin` needs a recent password |
| `GET /admin/users/{id}` | AdminUser plus integer `devices`, `api_keys` and `sessions`, and `cloud: {"items", "bytes"}` when the user keeps anything in Trackside Cloud (counts only, never the content) |
| `PATCH /admin/users/{id}` | any of `display_name`, `role`, `permissions` (`can_download`, `can_upload`, `can_request`, `legacy_subsonic`, `max_bitrate_kbps`; `null` removes the limit), `disabled`. Changing `role` or `disabled` needs a recent password |
| `PUT /admin/users/{id}/libraries` | `{"library_ids": [...]}`; bumps the user's sync epoch |
| `POST /admin/users/{id}/password-reset` | `201 {"url": "https://.../reset/<token>", "expires_at"}`; recent password |
| `POST /admin/users/{id}/sign-out` | ends sessions; `{"devices": true}` also revokes device keys |
| `DELETE /admin/users/{id}` | `204`; deletes their listens, playlists, stars, devices and Trackside Cloud data. Refused for the last active admin (`409`, `details.reason = "last_admin"`); recent password. The user is disabled first, in the same transaction as that check, so a demotion racing with the delete is refused instead of leaving no admin |

AdminUser:

```json
{
  "id": "uenspa9s0dhs6ps80kc00",
  "username": "luke",
  "display_name": "Luke",
  "role": "admin",
  "permissions": {"can_download": true, "can_upload": true, "can_request": true,
                  "legacy_subsonic": true},
  "library_ids": ["ldhmwjvv1e9320kfeh1mp"],
  "created_at": "2026-09-05T09:30:00.000Z",
  "last_seen_at": "2026-11-14T09:29:00.000Z",
  "disabled": false
}
```

### Invites

Invite and reset links (`url` here and in `POST /admin/users/{id}/password-reset`) start with
the first configured public URL; else with the address the request reached, when it is a host a
trusted proxy named in `X-Forwarded-Host` or one of the server's own candidate addresses; else
with the first detected address. A bare `Host` header is never used on its own.

- `GET /admin/invites`: `{"invites": [{"id", "role", "permissions", "library_ids", "note",
  "created_by"?, "created_at", "expires_at", "state": "open" | "used" | "expired" | "revoked",
  "used_by"?}]}`. `permissions` is the full set of section 7 (`can_download`, `can_upload`,
  `can_request`, `legacy_subsonic`, and `max_bitrate_kbps` when limited); the account made from
  the invite gets exactly these.
- `POST /admin/invites`: `{"role"?, "permissions"?, "library_ids", "note"?, "expires_in_days"?}`
  (default `member` and 7 days, 1 to 90; absent permission flags take the defaults: download
  and requests on, uploads and legacy Subsonic off, no bitrate limit) → `201` with `"url"` and `"qr_svg_url"` (shown once). `role: admin` needs a
  recent password. The QR is served at `/admin/invites/{id}/qr.svg` (`image/svg+xml`). It is held in
  memory, so it is `404` once the invite is revoked or the server restarts.
- `DELETE /admin/invites/{id}`: revokes.

```json
{
  "id": "vd5q0azkmcnapwrb59r00",
  "role": "member",
  "permissions": {"can_download": true, "can_upload": false, "can_request": true,
                  "legacy_subsonic": false},
  "library_ids": ["ldhmwjvv1e9320kfeh1mp"],
  "note": "For Nana",
  "created_by": "luke",
  "created_at": "2026-11-14T09:15:00.000Z",
  "expires_at": "2026-11-21T09:30:00.000Z",
  "state": "open",
  "url": "http://192.168.1.20:4747/invite/7KQ2M9XDp3XvQ8rL2mN6tY0w",
  "qr_svg_url": "/api/v1/admin/invites/vd5q0azkmcnapwrb59r00/qr.svg"
}
```

### Libraries and scans

- `GET /admin/libraries`: `{"libraries": [AdminLibrary]}` where AdminLibrary adds `root`,
  `enabled`, `writable`, `watch`, `quick_scan_interval_s`, `unavailable_reason`,
  `unavailable_since`, `watching: "events" | "polling" | "off"`, and the role flags
  `uploads_target`, `acquire_target`.
- `POST /admin/libraries`: `{"name", "root", "writable"?, "watch"?, "grant_all_users"?}`. The server
  checks the path exists, is a directory, is readable (and writable if asked), is not inside the
  data directory, and does not overlap another library. `201` and a full scan starts.
- `PATCH /admin/libraries/{id}`: `name`, `enabled`, `writable`, `watch`, `quick_scan_interval_s`
  (`null` returns to the default). Changing `root` is refused with `400`; create a new library
  (ids follow files by digest anyway).
- `DELETE /admin/libraries/{id}`: removes the library's songs from the index (files are never
  touched). Bumps every affected user's sync epoch.
- `POST /admin/libraries/{id}/scan`: `{"kind": "quick" | "full", "force_empty": false}` →
  `202 ScanRun`. `force_empty` overrides the empty-library guard.
- `GET /admin/libraries/{id}/scans?limit`: `{"scans": [ScanRun]}`.
- `GET /admin/scans/{id}`: ScanRun plus `errors: [{"path", "error"}]` (first 1000).
- `GET /admin/libraries/{id}/problems`: `{"tag_problems": [{"song_id", "path", "problem"}],
  "quarantined": [{"path", "error", "at"}], "unreadable_folders": [{"path", "since"?}]}`.
- `GET /admin/duplicates?limit&cursor`: groups of songs sharing a digest,
  `{"groups": [{"songs": [{"id", "title", "artist", "library_id", "path"}]}], "next_cursor"?}`.
- `PUT /admin/settings/targets`: `{"uploads_library": "<id>"?, "acquire_library": "<id>"?}`;
  answers with the targets now in effect, same shape.

ScanRun: `{"id", "library_id", "kind", "trigger", "state", "started_at", "finished_at"?,
"folders_seen", "files_seen", "added", "updated", "moved", "missing", "errors", "message"?}`.
`errors` is the number of files that failed. `state` is `running`, `done`, `failed` or `aborted`.
`message` explains an aborted run (the empty-library guard), and on a finished run says when
files were left for later because they were still being written (`scanner.settle`):
`"3 files were still being written; a follow-up scan in 15 s imports them."`

### Devices and keys

- `GET /admin/devices`: `{"devices": [{"id", "name", "platform", "model"?, "app_name"?,
  "app_version"?, "user": {"id", "username", "display_name"}, "created_at", "last_seen_at",
  "last_ip"?}]}`.
- `DELETE /admin/devices/{id}`: revoke.
- `GET /admin/api-keys`: `{"keys": [{"id", "kind", "name", "prefix", "scopes", "user",
  "created_at", "last_used_at"?, "last_used_ip"?}]}`, never the secrets.
- `DELETE /admin/api-keys/{id}`: revoke.
- `GET /admin/sessions`: `{"sessions": [{"id", "user", "user_agent"?, "ip"?, "created_at",
  "last_seen_at", "current"}]}`, and `DELETE /admin/sessions/{id}`.

### Server settings

- `GET /admin/settings`: every key marked **D** in architecture section 11, with
  `{"value", "source": "default" | "dashboard" | "config", "editable": bool}`.
- `PATCH /admin/settings`: `{"key": value, ...}`; keys set by config are `409` with
  `details.reason = "set_by_config"`. Any `auth.*` key needs a recent password. `null` returns a
  key to its default.
- `acquire.fetched_keep` (a whole number, default `500`) and `acquire.fetched_max_bytes` (a size,
  default `"20GiB"`) limit songs played without being kept. Lowering either evicts at once, least
  recently played first. `0` keeps none beyond those in use.
- `cloud.enabled` (default `true`), `cloud.quota_items` (a whole number, default `2000000`) and
  `cloud.quota_bytes` (a size, default `"1GiB"`) control Trackside Cloud (section 19). They are
  dashboard settings only, not configuration keys. A lower quota refuses new items and keeps
  what is stored.
- Secret keys (`enrichment.lastfm_api_key`) are write-only: `PATCH` takes the text, which is
  sealed with the server's secret key before it is stored, and needs a recent password; `GET`
  and every response show only whether one is set, as `"value": true` or `false`.
- `GET /admin/server/urls`: detected LAN URLs and configured public URLs, with a reachability test
  result from the server's side: `{"urls": [{"url", "kind", "tls", "source": "detected" |
  "configured", "reachable", "checked_at"}]}`.
- `POST /admin/pairing/approve` is the same as `/pair/requests/approve`.

### Apple Music

The MusicKit key `GET /me/apple-music/developer-token` signs with (section 7). It is not one of
the settings in `GET /admin/settings`: it has its own write-only endpoints.

- `GET /admin/apple-music`: `{"configured": true, "key_id": "ABC123DEFG", "team_id":
  "TEAM456XYZ", "set_at": "2026-11-11T09:30:00.000Z"}`, or `{"configured": false}`. The key itself
  is never returned by this or any other endpoint. Fixtures: `admin/apple-music.json`,
  `admin/apple-music-unset.json`.
- `PUT /admin/apple-music` (needs a recent password): `{"private_key": "-----BEGIN PRIVATE
  KEY-----\n...", "key_id": "ABC123DEFG", "team_id": "TEAM456XYZ"}`. `private_key` is the text of
  the `.p8` file Apple gives (PEM, or its base64 body alone); it must be an unencrypted P-256
  key in PKCS#8 form. `key_id` and `team_id` are Apple's 10 letters and digits, upper-cased on
  input. `400 bad_request` with `details.field` (`private_key`, `key_id` or `team_id`) and a
  message saying what is wrong. `200` returns the `GET` shape. Saving a key replaces any earlier
  one, and the next token is signed with the new key.
- `DELETE /admin/apple-music` (needs a recent password): `204`. Tokens stop at once and `/me`
  drops `features.apple_music`.
- The key is sealed with the server's secret key, like the other secret settings, in the
  `settings` table, so backups carry it sealed and it opens only with the same `secret.key`.
  Audit actions: `apple_music.update` (target: the key id) and `apple_music.remove`.

### Suggestions

The Last.fm API key is the setting `enrichment.lastfm_api_key` above, also settable as
`TRACKSIDE_ENRICHMENT__LASTFM_API_KEY` (then shown with `"source": "config"`, not editable).

- `GET /admin/enrichment`:

  ```json
  {"configured": true, "artists": 1840, "songs": 5210,
   "last_fetch_at": "2026-11-14T09:28:00.000Z",
   "last_error": {"at": "2026-11-14T08:30:00.000Z",
                  "message": "Last.fm error 29: Rate Limit Exceeded"}}
  ```

  `artists` and `songs` count lookups Last.fm had an answer for. `last_fetch_at` and
  `last_error` are left out when there is none; `last_error` clears after the next success.
- `POST /admin/enrichment/test`: `{"api_key"?}` makes one Last.fm call with that key, or the
  saved one: `200 {"ok", "elapsed_ms", "detail"?}`, `detail` saying what went wrong. The key is
  never echoed.

### Backups

- `GET /admin/backups`: `{"backups": [{"name", "kind": "scheduled" | "manual" | "pre_migrate",
  "at", "bytes", "has_key": true}], "next_at"?}`. `next_at` is when the next scheduled backup
  runs, the same as the overview's `backups.next_at`; left out while scheduled backups are off.
- `POST /admin/backups`: `202 {}`, then the new backup appears in the list; `409` with
  `details.reason = "running"` if one is running.
- `GET /admin/backups/{name}`: downloads the file (`application/vnd.sqlite3`); recent password.
  The secret key is not downloadable over HTTP.
- `DELETE /admin/backups/{name}`: recent password.

Restore is CLI only (`trackside backup restore`), because it needs the server stopped.

### Audit log

`GET /admin/audit?action&actor&limit&cursor`, newest first:

```json
{
  "entries": [
    {"at": "2026-11-14T09:29:00.000Z", "actor": "luke", "ip": "192.168.1.41",
     "action": "invite.create", "target": "member", "detail": "For Nana",
     "summary": "Created an invite"}
  ]
}
```

`actor` is a username or `system`; `target` is a display string; `detail` is any JSON value and
optional. `summary`, when the action has one, is a short sentence for the list. `next_cursor`
is present when there are more.

### Uploads

`GET /admin/imports?state&limit&cursor`: every upload and inbox import, `{"imports": [{"id",
"file_name", "source": "upload" | "inbox", "user"?, "state", "song_id"?, "error"?,
"created_at"}], "next_cursor"?}`.

### Stats

The dashboard's charts. Every series is aligned with its bucket list, oldest first, ending with
the current bucket. `null` means nothing was recorded (the server was not running then); `0` is
a measured zero. Responses are `Cache-Control: no-store`. Out-of-range parameters are
`400 bad_request` naming the field.

| Method and path | Parameters | Returns |
|---|---|---|
| `GET /admin/stats/listening` | `days` 7 to 90 (default 30), `tz` (IANA zone; default the server's) | plays per user per calendar day in `tz` |
| `GET /admin/stats/streams` | `hours` 6 to 48 (default 24) | per UTC hour: most media responses at once, most transcodes at once, failed media requests; the current hour is live |
| `GET /admin/stats/storage` | `days` 7 to 365 (default 90) | per server-local day: free space, database and cache size |
| `GET /admin/stats/requests` | `days` 7 to 90 (default 30), `tz` | acquisitions finished per day: added, failed, cancelled. `404` when the server has no acquisition providers |

```json
{"tz": "Europe/London", "bucket": "day", "days": ["2026-11-12", "2026-11-13", "2026-11-14"],
 "users": [{"id": "uenspa9s0dhs6ps80kc00", "username": "luke", "display_name": "Luke",
            "plays": [4, null, 0]}]}
```

```json
{"bucket": "hour",
 "hours": ["2026-11-14T07:00:00.000Z", "2026-11-14T08:00:00.000Z", "2026-11-14T09:00:00.000Z"],
 "streams": [null, 1, 2], "transcodes": [null, 0, 1], "failed": [null, 0, 0],
 "transcode_limit": 4}
```

```json
{"bucket": "day", "days": ["2026-11-12", "2026-11-13", "2026-11-14"],
 "data_free_bytes": [null, 151000000000, 150000000000],
 "database_bytes": [null, 699000000, 700000000],
 "cache_bytes": [null, 2100000000, 2200000000],
 "volume_bytes": 500000000000}
```

```json
{"tz": "Europe/London", "bucket": "day", "days": ["2026-11-12", "2026-11-13", "2026-11-14"],
 "added": [1, null, 0], "failed": [0, null, 0], "cancelled": [0, null, 0]}
```

`transcode_limit` (`media.transcode_max_concurrent`) is left out without ffmpeg; `volume_bytes`
(the data volume's size) is left out when unknown.

### Acquisition (provider names appear only here)

- `GET /admin/acquire/providers`: `{"providers": [ProviderStatus]}`

  ```json
  {"name": "ytdlp", "enabled": true, "configured": true, "health": "outdated",
   "detail": "yt-dlp 2026.10.22",
   "version": "2026.10.22", "latest_known": "2026.11.03",
   "settings": {"concurrency": 2, "search_rate": "10/1m", "cookies_file": "/data/cookies.txt"},
   "stats": {"searches_today": 31, "fetches_today": 12, "failures_today": 1}}
  ```

  `health` is `ok`, `degraded`, `cooldown`, `outdated`, `unavailable`, `setup` or `disabled`.
  `setup` means the source lacks configuration or tooling on this install (no yt-dlp that runs
  here; slskd address, API key or downloads folder unset) and `detail` says what; `unavailable`
  means it is set up but failing.
  `cooldown_until` is present during a cooldown; `version`, `latest_known` and `detail` when
  known.

- `PATCH /admin/acquire/providers/{name}`: `{"enabled"?, "concurrency"?, "url"? (soulseek),
  "api_key"? (soulseek, write-only)}`. Paths to executables and cookie files are config-file only.
- `PATCH /admin/acquire/providers/{name}` with `api_key` needs a recent password.
- `POST /admin/acquire/providers/{name}/test`: runs a health check and a sample search
  (`{"query"?}`); `200 {"ok", "elapsed_ms", "candidates", "detail"?}`.
- `POST /admin/acquire/providers/ytdlp/update`: downloads and verifies the latest release into
  `<data>/bin/yt-dlp`, installing it only if it runs here; `202`, progress through
  `GET /admin/acquire/providers`.
- `PATCH /admin/acquire`: `{"enabled", "instant_play", "member_daily", "member_active",
  "global_daily", "prefer"}`.
- `GET /admin/acquisitions?state&user&limit&cursor`: `{"acquisitions": [...], "next_cursor"?}`.
  Each is an Acquisition (15.1) plus `user`, `provider`, the chosen `candidate` (`{"token",
  "provider", "source", "title", "artist", "score", "quality"?, "format"?, "size"?}`),
  `attempts` (`[{"provider", "source", "at"?, "error"?}]`), `attempt_count` and
  `failure_detail`.
- `POST /admin/acquisitions/{id}/retry`, `DELETE /admin/acquisitions/{id}`.

---

## 17. Subsonic and OpenSubsonic coverage

### Transport

- `GET` and `POST` with `application/x-www-form-urlencoded` (formPost) on `/rest/<name>` and
  `/rest/<name>.view`.
- `f=xml` (default, as the spec says), `f=json`; `f=jsonp` returns error 0 "not supported".
- Envelope attributes: `status`, `version="1.16.1"`, `type="trackside"`, `serverVersion`,
  `openSubsonic=true`.
- Auth: `apiKey` (OpenSubsonic, any key with the needed scope), `u`+`t`+`s` and `u`+`p` against
  app passwords only. Errors: 40 wrong credentials, 41 token auth not supported (user has no app
  password or legacy auth is off; with `helpUrl` to the dashboard's key page), 42 mechanism not
  supported, 43 conflicting mechanisms, 44 invalid API key, 50 not authorised, 70 not found.
  Auth failures are rate-limited like native ones and also send `Retry-After`.
- Ids are the native public ids. `getCoverArt` accepts artwork, song, album and artist ids.
- Media endpoints that fail return the Subsonic error envelope in the requested format **with a
  non-200 HTTP status** (`404`, `401`, `503`), never an error body with `200`.
- An endpoint the server does not know answers HTTP `404` with error 0. Endpoints listed below
  as "error 0" or "error 50" are known and answer `200` with that error, as the spec expects.

### Endpoint table

| Endpoint | Status | Notes |
|---|---|---|
| `ping`, `getLicense` | yes | license always valid |
| `getOpenSubsonicExtensions` | yes | no auth required |
| `tokenInfo` | yes | for `apiKey` |
| `getMusicFolders` | yes | one per visible library |
| `getIndexes` | yes | artist index, as Navidrome; `ifModifiedSince` honoured |
| `getMusicDirectory` | yes | real folders (folder ids `f...`), plus artist and album ids for clients that pass those |
| `getGenres` | yes | |
| `getArtists`, `getArtist`, `getAlbum`, `getSong` | yes | `musicFolderId` honoured |
| `getArtistInfo`, `getArtistInfo2` | yes | similar artists in the caller's libraries, from local signals and Last.fm, chosen before `count` cuts the list; `biography` from Last.fm; no images |
| `getAlbumInfo`, `getAlbumInfo2` | yes | notes empty unless enrichment |
| `getSimilarSongs`, `getSimilarSongs2` | yes | `id` may be a song, album or artist id (Tracks passes a song id) |
| `getTopSongs` | yes | by artist name, as the spec says; ranked as the native artist top songs (section 8) |
| `getTopSongs` with `id` | yes | `topSongsByArtistId` extension |
| `getAlbumList`, `getAlbumList2` | yes | all types: random, newest, highest, frequent, recent, alphabeticalByName, alphabeticalByArtist, starred, byYear, byGenre |
| `getRandomSongs`, `getSongsByGenre` | yes | |
| `getNowPlaying` | yes | from now-playing and `reportPlayback`; admins see everyone's, members only their own |
| `getStarred`, `getStarred2` | yes | |
| `search`, `search2`, `search3` | yes | library only; all three offset and count pairs honoured; empty `query` returns everything in stable internal-id order (older Tracks builds page all songs this way) |
| `getPlaylists`, `getPlaylist`, `createPlaylist`, `updatePlaylist`, `deletePlaylist` | yes | |
| `stream` | yes | `maxBitRate`, `format` (`raw`, `mp3`, `aac`, `m4a`, `opus`, `flac`), `timeOffset` (transcodeOffset), `estimateContentLength`; does not count a play |
| `download` | yes | original file, needs `can_download` |
| `hls` | yes | also at `/rest/hls.m3u8`, the name the spec documents; an `.m3u8` from the native HLS transcoder (AAC), `bitRate` and the user's limit pick the profile |
| `getCoverArt` | yes | `size` snapped as native; **404 when there is no art** |
| `getLyrics` | yes | by artist and title, plain text |
| `getLyricsBySongId` | yes | `songLyrics` extension, structured, synced and plain |
| `getAvatar` | no | error 70 |
| `star`, `unstar`, `setRating` | yes | favourites and ratings only |
| `scrobble` | yes | `time` in milliseconds; `submission=false` is now playing; `submission=true` stores a listen with `client_listen_id = sub:<user>:<id>:<time>` |
| `reportPlayback` | yes | `playbackReport` extension |
| `getPlayQueue`, `savePlayQueue` | yes | |
| `getPlayQueueByIndex`, `savePlayQueueByIndex` | yes | `indexBasedQueue` extension |
| `getBookmarks`, `createBookmark`, `deleteBookmark` | yes | |
| `getScanStatus`, `startScan` | yes | `startScan` needs admin |
| `getUser` | yes | self; admins any user |
| `getUsers` | yes | admins all; members themselves |
| `createUser`, `updateUser`, `deleteUser`, `changePassword` | no | error 50 with a pointer to the dashboard |
| `getShares` | empty | `createShare`, `updateShare`, `deleteShare` error 0 "not supported" until shares ship |
| podcasts (all), `getPodcastEpisode` | empty / error 0 | `getPodcasts` and `getNewestPodcasts` return empty lists |
| internet radio (all) | empty / error 0 | `getInternetRadioStations` returns an empty list |
| `getVideos`, `getVideoInfo`, `getCaptions` | empty / error 0 | |
| `jukeboxControl` | error 50 | |
| `getChatMessages`, `addChatMessage` | empty / error 0 | |
| `getTranscodeDecision`, `getTranscodeStream` | planned for 1.1 | wraps the native playback decision; until then unknown (HTTP 404) |
| `getSonicSimilarTracks`, `findSonicPath` | no | unknown (HTTP 404) |
| `getAnimatedArt` (a third-party extension) | no | error 70, so Tracks marks it unavailable |

### Extensions advertised by `getOpenSubsonicExtensions`

`apiKeyAuthentication` v1, `formPost` v1, `songLyrics` v1, `transcodeOffset` v1,
`indexBasedQueue` v1, `topSongsByArtistId` v1, `playbackReport` v1.

### Song fields

All Subsonic `Child` fields, plus the OpenSubsonic fields Trackside knows: `bitDepth`,
`samplingRate`, `channelCount`, `mediaType`, `played`, `bpm`, `comment`, `sortName`,
`musicBrainzId`, `isrc`, `genres`, `artists`, `displayArtist`, `albumArtists`,
`displayAlbumArtist`, `replayGain`, `explicitStatus`. `created` is RFC 3339 with milliseconds and
`Z`. `suffix`, `contentType`, `bitRate`, `size` and `duration` (seconds) describe the original.
`isExternal` is never sent.

### Behaviour Tracks relied on from its previous Subsonic proxy, and what Trackside does instead

The previous Subsonic proxy Tracks used fetched songs on demand from services elsewhere.

| Previous proxy's behaviour | Trackside |
|---|---|
| `getCoverArt` returns a stand-in image with HTTP 200 for missing covers; Tracks probes and hashes it | `404` with a Subsonic error 70 body. Tracks' probe finds no stand-in and stores nothing to compare against, so it treats every 404 as "no cover". |
| `search3` mixes up to 60 external songs (`isExternal: true`) into the first page when `songCount > 12`, slowly | Library songs only, fast, standard paging for every offset. Not-yet-owned results exist only in the native acquisition search, for users allowed to request. |
| `star` on an external song means "fetch it"; it reappears minutes later under a new id | `star` marks a favourite and nothing else. Acquisition is a native request with a status and the final id. |
| Similar songs and artists from Last.fm; empty without the proxy's Last.fm key | Local signals always; Last.fm too when the admin sets one server-wide key (`enrichment.lastfm_api_key`), for every user. |
| Song names follow Last.fm | Names are the file's tags. |
| `getAnimatedArt` | Not implemented (error 70). |
| Navidrome-shaped ids, numeric or string | Strings with kind prefixes; Tracks' lenient decoding is unaffected. |
| Download error bodies sometimes with 200 | Errors always have a non-2xx status. |
| `stream format=mp3` only transcode | `mp3`, `aac`/`m4a`, `opus`, `flac`; Tracks on the native API uses AAC. |
| Rate limits on lookups | None needed beyond the per-key limit; responses are local. |

---

## 18. Versioning and deprecation

- The server follows semver; `/api/v1/capabilities` reports `version`.
- `/api/v1` changes only additively within v1. Clients must ignore unknown fields, unknown enum
  values and unknown SSE event types.
- A breaking change goes into `/api/v2`, which is served alongside `/api/v1` for at least 12
  months and at least two minor releases after v2 ships.
- A deprecated endpoint or field is listed in `capabilities.api.deprecated` as
  `{"what": "GET /api/v1/...", "since": "1.4.0", "sunset": "2028-01-01", "use": "..."}`, and
  responses using it carry `Deprecation: @<unix time>` and `Sunset: <HTTP date>` headers
  (RFC 9745, RFC 8594).
- The Subsonic layer keeps Subsonic's own versioning (`1.16.1`) and adds OpenSubsonic extension
  versions only upward.
- Wire fixtures for every response shape live in `crates/trackside-api/tests/fixtures/`.
  The server's tests serialise the real types and compare against them; Tracks' tests decode the
  same files. A fixture change is an API change and is reviewed as one.

---

## 19. Trackside Cloud

Per-user sync of the Tracks apps' own data (history, sessions, stations, settings, lists,
playlists, likes), for people who choose their Trackside server over iCloud. ADR 0012 has the
design. Fixtures: `crates/trackside-api/tests/fixtures/cloud-*.json`.

- **Only your own data.** Every route works on the caller's records and nobody else's, admins
  included. There is no admin route to anyone's records, and the dashboard never shows them;
  admins see item and byte counts on the user page (section 16).
- **Device keys only**, scope `library` to read and `write` to change. A session or an API key is
  `403 forbidden`. With `cloud.enabled` off (section 16, Server settings) every route is `404`.
- **One device id per key.** A device key speaks for one app device id: the one it registered
  with `PUT /cloud/devices/{device_id}` (Devices below), or, until it registers one, the
  `client_device_id` it signed in or paired with (section 6). Every pushed item's and setting's
  `device_id` must be that id, or it is `rejected` with `device_mismatch`.
- **Shapes.** The envelope follows section 1 (`snake_case`, RFC 3339 times). `fields` is the
  app's own object under the app's own names; the server keeps it as sent apart from the rules
  below, so unknown fields and enum values round-trip. The times inside `fields` that the server
  reads (`playedAtMs`, `firstSeenMs`, `lastSeenMs`, `forgottenAtMs`) are integer milliseconds.
  `null` inside `fields` is dropped on a new record, clears a field on a newest-wins field, and
  is ignored elsewhere.
- **Versions.** Each user has their own change seq. Every write takes the next value, which
  becomes the record's `version`. Cursors are opaque strings that carry that seq and the
  server's sync epoch; they never depend on client time. Send a cursor back exactly as it came.

### Entities

| Path segment | `id` | Rule |
|---|---|---|
| `plays` | the client's playID UUID, a UUIDv5 for Apple backfill, or `lastfm:<played_at_s>:<hash>` | `title`, `artistName` and `playedAtMs` are required. Core fields (`title`, `artistName`, `albumTitle`, `playedAtMs`, `kind`, `source`, `recordedByDeviceID`, `platform`) never change: a different value is `conflict`. `artworkURL` is newest-wins. Every other field (`songKey`, `catalogID`, `serverSongID`, `spotifyID`, `stationID`, `sessionID`, `playedMs`, `durationMs`, `scrobbledAt`, `addedToPlaylistAt`, and fields the server does not know) is first non-null wins. `songKey` is filled with `tracks_model::song_key(title, artistName)` when absent. Tombstones are kept forever. |
| `sessions` | UUID | Written only by its owner (its `recordedByDeviceID` field, else the device that first pushed it) or a device restored from the owner (Devices below); anyone else is `rejected` `not_owner`. Otherwise the last write wins, field by field. |
| `stations` | `stationKey(name)`, which the server derives when `id` is empty | `name` is required. `firstSeenMs` takes the minimum, `lastSeenMs` the maximum, every other field the first non-null. |
| `forgotten-songs` | `songIdentity` (`title` 0x1F `artistName`, folded) | Grows only: `forgottenAtMs` takes the maximum; a delete is `rejected` `grow_only`. |
| `blocked-artists`, `excluded-stations` | the folded `name`, derived when `id` is empty | One value per item, newest `client_modified_at` wins, deletes included: an add newer than the delete brings the item back, an older one is `conflict`. |
| `likes` | `songIdentity`, derived when `fields` has `title` and `artistName` | `{"loved", "rating"?}`, newest `client_modified_at` wins as above. |
| `playlists` | UUID | Newest `client_modified_at` wins per field. Deleting a playlist deletes its entries. |
| `playlist-entries` | UUID (the entry) | `playlistID` and `order` (a fractional order key, compared as text by the client) are required; `track` is `{"title", "artist", "album"?, "ids": {"<source>": "<id>"}}`. Newest wins per field; `playlistID` never changes. |

A keyed `id` that does not match its fields (a station whose `id` is not `stationKey(name)`) is
`rejected` `identity_mismatch`. Ids and device ids are up to 256 and 128 bytes of text.

### `GET /api/v1/cloud/{entity}?cursor=&limit=` (scope: library)

`limit` defaults to 500, at most 2000. No `cursor` (or an empty one) pages a full snapshot from
the start; tombstones are included, so a snapshot can also be applied as a delta.

```json
{
  "upserts": [{"id": "6A8C0E2F-4B6D-4F8A-9C1E-3B5D7F9A1C3E", "version": 917,
               "fields": {"title": "Halo", "artistName": "Beyoncé", "playedAtMs": 1794645000000,
                          "kind": "onDemand", "songKey": "halo\u001fbeyonce"},
               "device_id": "6F1D9C2A-3B4E-4F5A-8B7C-1D2E3F4A5B6C",
               "client_modified_at": "2026-11-14T09:25:00.000Z"}],
  "deletes": [{"id": "1F3B5D7F-9A1C-4E3B-8D5F-7A9C1E3B5D7F", "version": 918,
               "device_id": "0C7B5E3A-9D1F-4E2B-8A6C-5F4E3D2C1B0A",
               "client_modified_at": "2026-11-14T09:29:30.000Z",
               "deleted_at": "2026-11-14T09:29:30.000Z",
               "merged_into": "6A8C0E2F-4B6D-4F8A-9C1E-3B5D7F9A1C3E"}],
  "cursor": "3f9a0c1d7e2b4a65.918",
  "has_more": false
}
```

- Records come in `version` order and each appears at most once per page. A record that changes
  after it was paged comes again later at its new version; nothing committed before a page is
  skipped by it.
- Commit `cursor` only after applying the page, and keep pulling while `has_more`.
- `410 sync_reset`: the cursor is from another sync epoch (the server's database was restored
  from a backup or replaced), older than tombstones the server pruned, or newer than anything it
  has. Pull again without a cursor, keeping local data until done, then push what the server
  lacks.
- `400 bad_request` (`details.field = "cursor"`) for a cursor the server did not make; `404` for
  an unknown entity.

### `POST /api/v1/cloud/{entity}:batch` (scope: write)

The `:batch` suffix is part of the last path segment (`/cloud/plays:batch`); a percent-encoded
colon (`/cloud/plays%3Abatch`) is the same route. Up to 1000 items and
an 8 MiB body; more is `413 too_large`. Each item is idempotent by `id`.

```json
{"items": [
  {"id": "6A8C0E2F-4B6D-4F8A-9C1E-3B5D7F9A1C3E",
   "fields": {"title": "Halo", "artistName": "Beyoncé", "playedAtMs": 1794645000000,
              "kind": "onDemand", "source": "appleMusic",
              "recordedByDeviceID": "6F1D9C2A-3B4E-4F5A-8B7C-1D2E3F4A5B6C"},
   "client_modified_at": "2026-11-14T09:25:00.000Z",
   "device_id": "6F1D9C2A-3B4E-4F5A-8B7C-1D2E3F4A5B6C"},
  {"id": "1F3B5D7F-9A1C-4E3B-8D5F-7A9C1E3B5D7F", "base_version": 902,
   "client_modified_at": "2026-11-14T09:29:30.000Z", "device_id": "0C7B5E3A-...",
   "deleted_at": "2026-11-14T09:29:30.000Z",
   "merged_into": "6A8C0E2F-4B6D-4F8A-9C1E-3B5D7F9A1C3E"}
]}
```

- An upsert carries `fields`; a delete carries `deleted_at` (and `merged_into` for a play the
  client merged into another) and no `fields`.
- `base_version` is compare-and-set: the `version` the client last saw, or `0` for "must not
  exist yet". A mismatch is `conflict`, except that an item that changes nothing is always
  `duplicate`, so replaying a batch is safe.
- An upsert of a tombstoned id is `rejected` `deleted` unless it carries `"undelete": true`
  (the newest-wins lists decide by time instead).
- `client_modified_at` and `deleted_at` more than a day ahead of the server's clock, and a play
  whose `playedAtMs` is, are `rejected` `future`.

`200`, one result per item in order:

```json
{"results": [
  {"id": "6A8C0E2F-...", "status": "accepted", "version": 918},
  {"id": "lastfm:1700000000:5f2b9c1d", "status": "duplicate", "version": 640},
  {"id": "1F3B5D7F-...", "status": "conflict", "current": {"id": "1F3B5D7F-...", "version": 911,
   "fields": {"title": "Halo"}, "device_id": "...", "client_modified_at": "..."}},
  {"id": "8E0A2C4E-...", "status": "rejected", "reason": "deleted"}
]}
```

- `accepted`: stored at `version`. `duplicate`: already stored, nothing changed; success.
- `conflict`: the stored copy stands; `current` is it (a record as in a pull, or a delete), absent
  when nothing is stored. Apply `current`, or rebase and push again with its `version`.
- `rejected` with `reason`: `invalid`, `too_large`, `deleted`, `not_owner`, `grow_only`,
  `future`, `identity_mismatch`, `encryption_required`, `device_mismatch` or `quota_exceeded`. Drop the item, except
  for `quota_exceeded`, which clears when space is freed or the admin raises the quota. Reasons
  are an open set.
- For a keyed entity pushed with an empty `id`, `id` in the result is the key the server derived.
- The whole batch fails only for auth, size, malformed JSON or `429 rate_limited`.

Writes are applied in short transactions of at most 250 items, so other users' requests run
between them during a large import.

### Settings

`GET /api/v1/cloud/settings?cursor=` (scope: library): every setting, or those changed after
`cursor`.

```json
{
  "settings": {
    "playlistName": {"value": "Heard on the radio", "modified_at": "2026-11-13T09:30:00.000Z",
                     "device_id": "0C7B5E3A-...", "version": 12},
    "playlistID": {"value": null, "modified_at": "2026-11-14T08:30:00.000Z",
                   "device_id": "6F1D9C2A-...", "version": 310},
    "nearbyKey": {"value": {"$encrypted": {"v": 1, "alg": "chachapoly", "kid": "icloud-keychain-1",
                                           "nonce": "3q2+7wABAgMEBQYH", "ct": "kJ4w0bQ2..."}},
                  "modified_at": "2026-11-14T09:20:00.000Z", "device_id": "6F1D9C2A-...",
                  "version": 412}
  },
  "cursor": "3f9a0c1d7e2b4a65.918"
}
```

`PATCH /api/v1/cloud/settings` (scope: write), up to 1000 keys:

```json
{"settings": {
  "playlistName": {"value": "Tracks", "modified_at": "2026-11-14T09:29:50.000Z", "device_id": "6F1D9C2A-..."},
  "playlistID": {"value": null, "modified_at": "2026-11-14T09:29:50.000Z", "device_id": "6F1D9C2A-..."}
}}
```

`value` must be present; `null` clears. `200 {"results": [{"key", "status", "reason"?,
"current"?}]}`, where `current` is the value that stands after the decision. The server decides,
the way Tracks' `SyncedSetting.decide` does, and is authoritative:

- The newest `modified_at` wins per key; a tie keeps the stored value (`conflict`).
- `forgottenSongs` and `forgottenSongDates` are lists of text that merge by union (sorted) and
  keep the later time; `null` adds nothing.
- A cleared value is an explicit `null` with its time and wins like any value.
- A seed (`modified_at` `1970-01-01T00:00:00.000Z`) never replaces a stored value, real or seed.
- Keys the server does not know, such as platform-only settings, round-trip with newest-wins.
- Keys are up to 128 bytes, values up to 256 KiB of JSON, at most 1000 keys per user.

### Encrypted values

A setting may hold a value the client encrypted, which the server stores and returns untouched
and cannot read:

```json
{"$encrypted": {"v": 1, "alg": "chachapoly", "kid": "icloud-keychain-1",
                "nonce": "<base64>", "ct": "<base64>"}}
```

The object has the single key `$encrypted`. `alg` (required) and `kid` are the client's own
labels; `ct` (required) and `nonce` are base64, standard or URL-safe, padded or not; other members
must be text or numbers. `ct` is 17 bytes to 16 KiB decoded and not exactly 32 bytes, since an
AEAD adds a tag and a 32-byte `ct` would be a raw key.

`nearbyKey`, the 32-byte TLS secret a user's nearby devices share, must be such an envelope or
`null`: anything else is `rejected` `encryption_required`. The key that encrypts it never reaches
the server (the apps keep it in iCloud Keychain or the platform's own key store), so the server,
its backups and its admin can never read the nearby secret.

### Devices

The app's own device ids and which devices each was restored from, so a restored device
inherits what its predecessors owned (sessions here; the right to scrobble a play, which the
clients decide: the server only stores `scrobbledAt` and `recordedByDeviceID` and never
scrobbles).

- `GET /api/v1/cloud/devices` (scope: library): `{"devices": [{"device_id", "predecessors",
  "name"?, "platform"?, "server_device_id"?, "current", "registered_at", "updated_at"}]}`.
  `server_device_id` is the signed-in install (section 7, Devices) that registered it, and
  `current` marks the caller's own.
- `PUT /api/v1/cloud/devices/{device_id}` (scope: write) `{"predecessors": [...], "name"?,
  "platform"?}`: registers the device or adds predecessors (they only grow, at most 32; a
  device cannot be its own). `200` with the device. At most 200 devices per user.
  - It binds the calling device key to `device_id`. A key already bound to another id, or a
    `device_id` bound to another live (not revoked) key, is `403 device_mismatch`; a re-signed-in
    install keeps its key's device and so its binding.
  - Each predecessor must be one of the caller's own devices: registered here, the
    `client_device_id` of one of their signed-in installs (revoked ones included), or the
    `recordedByDeviceID` of one of their own plays. An id found only in plays is refused when it
    is another user's device or is bound to a live key other than the caller's. Anything else
    is `400 bad_request` with `details.field = "predecessors"`. A device restored from a backup
    therefore registers its new id with the old one as predecessor, even when the old one never
    registered.

Device `A` succeeds device `B` when `A` is `B` or `B` is among `A`'s predecessors, directly or
through others.

### Usage

`GET /api/v1/cloud/usage` (scope: library):

```json
{"items": 152340, "bytes": 98312448, "quota": {"items": 2000000, "bytes": 1073741824},
 "entities": {"plays": {"items": 151900, "bytes": 97800000}}}
```

### Limits

- Fields of one item: 16 KiB of JSON, 100 keys, field names of 64 bytes, strings of 4 KiB,
  arrays of 1000, nesting 4 deep; over is `rejected` `too_large`.
- Per user: `cloud.quota_items` items and `cloud.quota_bytes` bytes (section 16), tombstones
  included. Deletes are always allowed.
- Pushes (batches and settings patches) per user: 20 a second with bursts of 40, and 10 000
  items a second with bursts of 50 000, on top of the per-key request limit. Over that,
  `429 rate_limited` with `Retry-After`.
- Tombstones are pruned 90 days after they were written, except plays', which are kept forever.

### Change signal

After a write, the owner's event streams (section 13) get `cloud` with `{"entity": "<path
segment>"}`, or `"devices"` for the registry: pull that entity. The event never carries data and
goes to nobody else. After reconnecting, pull every entity: the stream does not replay `cloud`
events.

### Backups, restores and deletion

Cloud data lives in the same database, so every backup holds it (section 16, Backups), as
readable as the rest of the database apart from encrypted values. When the server starts on a
restored backup, or on a database file put in place by hand, it takes a new sync epoch
(`capabilities.cloud.epoch`), so every cursor from before is `410 sync_reset` instead of silently
missing what the restore lost; the clients pull again and push what the server no longer has.
Deleting a user deletes their cloud data with them, in short transactions before the account
itself.
