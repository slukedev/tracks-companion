# ADR-0006: Optional acquisition through yt-dlp and slskd providers

**Status:** Accepted (supersedes the earlier draft decision to exclude external acquisition)
**Amended:** 2026-11-27, the slskd provider was removed; yt-dlp is the only provider.
**Date:** 2026-11-14
**Deciders:** Tracks maintainer

## Context

The main appeal of the previous Subsonic proxy Tracks used is getting songs the user does not
have: instant play from YouTube through yt-dlp and "keep" downloads from Soulseek through slskd.
Its flow is fragile: star a song, wait for it to reappear under a new id, poll full syncs for
twenty minutes, and reconcile by title. The user wants the capability in Trackside, made robust,
while ripping or decrypting streaming services (Tidal, Qobuz, Amazon, Deezer, Spotify via
SpotiFLAC, Mornye or similar gateways) stays out.

## Decision

- A small, compiled-in `Provider` trait (search, fetch into staging with progress, cancel by
  token, optional stream-through, health) in a new crate, `trackside-acquire`.
- Two built-in providers, both disabled by default and enabled only by an admin in config or the
  dashboard: `ytdlp` (runs the yt-dlp executable; prefers best audio, keeps the original codec
  without lossy re-encoding, embeds metadata and thumbnail, optional cookies file) and `soulseek`
  (HTTP client for slskd's REST API; slskd runs as its own container; Trackside never speaks the
  Soulseek protocol).
- A durable job queue in SQLite with leases, retries with backoff, per-provider concurrency and
  rate limits, quotas, dedupe against the library and against in-flight requests, verification of
  every file, tagging, import into a configured acquired library, and an SSE event on completion.
  A request returns an id with live status and, on success, the song's final stable id.
- A `can_request` permission (admins always; members default on, configurable).
- The native API never names providers; Tracks sees acquisition only through
  `features.acquisition` in `/api/v1/me`, present only when enabled and allowed for the user.
- No provider for streaming services, ever; no plugin loading.
- The shipped compose file shares only the acquired folder through slskd, read-only.

## Options Considered

### Option A: No acquisition
| Dimension | Assessment |
|---|---|
| Complexity | Lowest |
| Cost | Loses the feature users came to the previous proxy for |
| Scalability | n/a |
| Team familiarity | n/a |

**Pros:** Smallest legal and operational surface; clean App Store story.
**Cons:** Does not meet the user's requirement.

### Option B: Out-of-tree plugin interface, no built-in providers
| Dimension | Assessment |
|---|---|
| Complexity | High (stable plugin ABI or WASM host) |
| Cost | Long-term API commitment |
| Scalability | Good |
| Team familiarity | Low |

**Pros:** The project ships no sources.
**Cons:** Much more work, weaker robustness guarantees, and someone writes the same two plugins.

### Option C: Built-in yt-dlp and slskd providers behind a trait (chosen)
| Dimension | Assessment |
|---|---|
| Complexity | Medium |
| Cost | Two external tools to track; operational care |
| Scalability | Fine with per-provider limits |
| Team familiarity | Medium (the previous proxy did similar) |

**Pros:** Robust, testable pipeline; final ids at import; no polling; opt-in.
**Cons:** Depends on tools that break when sites change (yt-dlp), on peers that vanish
(Soulseek), and carries legal sensitivity for whoever enables it.

## Trade-off Analysis

Option C delivers the capability with the reliability properties the rest of the server has
(durable state, stable ids, events) and keeps it fully off unless an admin turns it on.
Streaming-service ripping is excluded because it means circumventing DRM or using credential
proxies, which is a different category from fetching a public video or a file a peer offers.

## Consequences

- Easier: "Get" in Tracks becomes one request with a status and an id, replacing `ServerKeeps`
  polling for Trackside.
- Harder: keeping yt-dlp current (image pin plus a hash-checked updater), handling throttling and
  bot checks without evasion, verifying untrusted files.
- Legal: users are responsible for what they download and share where they live. The server docs
  say this in one neutral paragraph and recommend sharing only the acquired folder.
- App Store: Tracks shows the feature only when a server reports it for the signed-in user and
  never names YouTube or Soulseek. Apple's guidelines also cover features that are hidden from
  review (2.3.1) and apps that facilitate downloading from third-party sources (5.2.3); a reviewer
  will not see the feature without such a server, but if one is connected it is visible. The
  maintainer should decide how to describe Tracks' server support in review notes with that in
  mind.
- Revisit: an OpenSubsonic-compatible way to expose requests to other clients, if wanted.

## Action Items

1. [ ] WP10 builds `trackside-acquire` with fake providers for tests and the two real ones.
2. [ ] WP8 ships the `-acquire` image variant and the optional `slskd` compose profile.
3. [ ] WP9 gates every acquisition UI element on `features.acquisition`.
