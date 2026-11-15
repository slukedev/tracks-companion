# ADR-0003: A native /api/v1 alongside Subsonic and OpenSubsonic

**Status:** Accepted
**Date:** 2026-11-14
**Deciders:** Tracks maintainer

## Context

Tracks mirrors a whole catalog by paging `search3` with an empty query (capped at 60,000 songs),
fires scrobbles that are lost offline, works around stand-in covers, and has no way to learn of
changes except polling. Subsonic has no delta sync, no batched idempotent writes, no pairing, no
push, and authenticates with MD5 of a password. Other clients (Amperfy, Symfonium, Feishin) speak
Subsonic and OpenSubsonic and should keep working.

## Decision

Serve both. `/rest` implements Subsonic 1.16.1 with the OpenSubsonic extensions listed in
api.md. `/api/v1` is a JSON API designed for Tracks: delta sync with cursors, batched idempotent
listens, pairing, media tokens, playback decisions, server-sent events, capabilities, and the
admin endpoints for the dashboard. Both are thin layers over the same repositories in
`trackside-core`.

## Options Considered

### Option A: Subsonic only, with custom extensions
| Dimension | Assessment |
|---|---|
| Complexity | Low at first |
| Cost | Bends a query-string API into shapes it was not made for |
| Scalability | Full-catalog paging stays |
| Team familiarity | High |

**Pros:** One API.
**Cons:** No clean place for delta sync, JSON bodies, SSE or header auth; extensions would be
Trackside-only anyway.

### Option B: Native only
| Dimension | Assessment |
|---|---|
| Complexity | Low |
| Cost | Loses every other client |
| Scalability | Good |
| Team familiarity | High |

**Pros:** Smallest surface.
**Cons:** Users of other apps would have no reason to run it; Tracks loses the fallback path.

### Option C: Both (chosen)
| Dimension | Assessment |
|---|---|
| Complexity | Medium |
| Cost | Two thin layers |
| Scalability | Good |
| Team familiarity | High |

**Pros:** Tracks gets an API built for it; the ecosystem keeps working.
**Cons:** Two surfaces to test and keep consistent.

## Trade-off Analysis

The extra layer is cheap because neither layer holds logic. The native API removes most of the
complexity in Tracks' server code (throttles, polling, probing), which is the main reason for the
project.

## Consequences

- Easier: delta sync, offline listens, pairing, live updates, header auth for downloads.
- Harder: semantics must match across both (stars, plays, ids); tests assert parity.
- Revisit: OpenSubsonic `transcoding` extension in 1.1.

## Action Items

1. [ ] WP6 and WP5 build on the same repository functions; no SQL in either.
2. [ ] WP6 maintains wire fixtures used by Tracks' tests.
