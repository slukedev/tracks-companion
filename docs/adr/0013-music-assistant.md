# ADR-0013: Music Assistant for streaming services and speakers

**Status:** Accepted (supersedes the "no provider for streaming services" line of ADR-0006 and
the matching non-goal in architecture.md; ADR-0006's ban on downloading from streaming services
stands)
**Date:** 2026-11-27
**Deciders:** Tracks maintainer

## Context

Tracks plays the music people own through Trackside and Apple Music natively on Apple
platforms. People also pay for Spotify, Tidal, Qobuz, Deezer and others, and they own speakers
(Sonos, AirPlay, Cast, DLNA, Snapcast) that Tracks cannot reach. The apps tried to fill the gap
on their own: a librespot Spotify engine and a MusicKit path on Android, each a separate
integration to keep working, each per platform.

Music Assistant (MA) already does both. It signs in to the services with the owner's own
accounts, plays them on every kind of speaker in the house, and has a documented WebSocket API
for clients. It deliberately offers no download or audio URL for provider tracks: audio only
reaches players, in real time.

ADR-0006 said there would never be a provider for streaming services. Its reason was ripping:
getting DRM-protected music as files, through decryption or credential gateways. That reason
still holds. What MA offers is different in kind: playback through the owner's own subscription,
the same thing the service's own app does.

## Decision

- Trackside talks to one MA server (`trackside-mass`, a WebSocket client built to run for
  years: supervised reconnects, tolerant decoding of MA 2.10 and 2.11, bounded requests and answers (8 MiB a message,
  16 MiB a chunked answer, 32 MiB of answers being assembled at once), a token
  that never reaches a log). The apps never talk to MA; every call and image goes through
  Trackside, under its own accounts, permissions and rate limits.
- **Speakers.** MA's players appear in Tracks' Devices. Trackside turns MA's player and queue
  state into speakers with stable ids and revisions, announces changes over its event stream
  (coalesced, cheap progress events), and runs commands that answer at once with the state they
  should lead to. `trackside-assistant` implements the `AssistantControl` trait in core, so the
  HTTP crates stay independent of MA, as they are of the scanner and the acquirer.
- **The library on speakers.** Trackside registers itself in MA as an OpenSubsonic source, with
  a service account and a key scoped `library stream`, so library songs play on every speaker
  with their metadata. The key works on `/rest` only, writes nothing and is rotated on every
  registration.
- **Streaming services.** The services connected in MA are browsed, searched and played through
  Trackside: on speakers by MA itself, and on a phone through the relay, which receives the
  audio as an MA player would and hands it on as live HLS.
- **The line that stays.** Nothing that comes from a streaming service or any DRM service is
  ever downloaded, recorded, cached on disk, added to the library or exported, by Trackside or
  through it. The relay is real time only and memory-bounded; segments are never written to
  disk and never offered for download. Acquisition (ADR-0006) keeps its sources and never takes
  MA as one.
- **Artwork is the one exception, and only as thumbnails.** Catalog images are fetched only
  from addresses MA gave for an item (MA's image proxy, or a public `https` URL; private
  addresses other than MA's are refused), checked by their magic bytes, decoded and kept
  re-encoded as JPEG of at most 1200 pixels in the existing artwork cache, under its byte cap
  and LRU eviction. No original image bytes, metadata or audio from a service are stored;
  search answers, track lists and image addresses stay in memory.
- MA is optional. Without it nothing changes, `/me` offers no speakers or catalog, and a build
  can leave it out (`assistant` feature of `trackside-server`).

## Options Considered

### Option A: Native integrations per service in Trackside or the apps

| Dimension | Assessment |
|---|---|
| Complexity | Very high: one client per service, per platform |
| Cost | Constant upkeep as services change; terms that often forbid third-party players |
| Scalability | Poor |
| Team familiarity | Low |

**Pros:** No extra server to run.
**Cons:** The work MA's community already does, done worse; speakers still unsolved.

### Option B: The apps talk to MA directly

| Dimension | Assessment |
|---|---|
| Complexity | Medium |
| Cost | MA tokens on every phone; MA's address reachable from wherever the phone is |
| Scalability | Fine |
| Team familiarity | Medium |

**Pros:** Less server code.
**Cons:** No Trackside accounts, permissions or per-user limits in front of the owner's service
accounts; the phone needs MA's network position and a second credential; no shared song ids.

### Option C: Trackside in front of MA (chosen)

| Dimension | Assessment |
|---|---|
| Complexity | Medium: one client, one adapter, the relay |
| Cost | MA to run next to Trackside (one compose service) |
| Scalability | Good: one connection, events fanned out |
| Team familiarity | Medium |

**Pros:** One integration for every platform; Trackside's auth, permissions and limits apply;
library songs and service tracks share one play path; speakers come with it.
**Cons:** A second server to host; MA's API can change (schema checks, tolerant decoding and an
admin status page keep that visible).

## Trade-off Analysis

Option C keeps the trust boundary where it is: people sign in to Trackside, Trackside decides
what each person may do, and the owner's service accounts stay inside MA. Streaming through MA
is listening, the way the service's own app listens; downloading is not, and stays forbidden.

## Consequences

- Easier: Spotify and the others on every platform, Android and TV included, without per-app
  engines; speakers in Devices; the library on speakers.
- Harder: running and upgrading MA; registration needs an MA admin token (`needs_admin`
  otherwise); MA's own limits per service account apply to relays.
- Security: the MA token is sealed at rest, write-only on the wire and never logged; the
  registration key is the narrowest that works and can be revoked from the dashboard.
- Revisit: when MA offers a stable way to register sources without its setup flow.

## Action Items

1. [x] `trackside-mass` client, `trackside-assistant` adapter, `/speakers` and
   `/admin/music-assistant` (api.md sections 16 and 20).
2. [x] Streaming catalog routes over the same client (api.md section 21).
3. [ ] Relay wiring for phones (docs/ops/relay.md).
4. [ ] Dashboard Speakers page.
