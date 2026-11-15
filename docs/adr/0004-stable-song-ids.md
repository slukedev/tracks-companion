# ADR-0004: Song ids derived from an audio digest, then persisted

**Status:** Accepted
**Date:** 2026-11-14
**Deciders:** Tracks maintainer

## Context

Tracks stores `server:<entry>:<songID>` in downloads, playlists and catalogs. If a song's id
changes when a file is retagged, moved, renamed, or when the database is rebuilt, those
references break. The previous Subsonic proxy Tracks used gave a kept song a new id on arrival,
forcing title-based reconciliation.

## Decision

A new song's id is derived from a digest of its audio payload (tag regions excluded; 64 KiB
windows at the start, middle and end plus the payload length, hashed with BLAKE3). Once assigned
the id is stored and never recomputed. Rescans match files to existing songs by path, then by
digest (moves), then by MusicBrainz track id or tag identity (replacements). Missing songs stay
for 30 days. Per-user rows refer to songs by public id. Architecture section 6 has the details.

## Options Considered

### Option A: Random id persisted, matched by path and tags
| Dimension | Assessment |
|---|---|
| Complexity | Low |
| Cost | Ids lost if the database is lost |
| Scalability | Good |
| Team familiarity | High |

**Pros:** Simple.
**Cons:** A rebuilt database gives every song a new id; tag-based move matching is fragile.

### Option B: Id = hash of path
| Dimension | Assessment |
|---|---|
| Complexity | Low |
| Cost | Breaks on every move or rename |
| Scalability | Good |
| Team familiarity | High |

**Pros:** Deterministic.
**Cons:** Fails the main requirement.

### Option C: Full-file content hash
| Dimension | Assessment |
|---|---|
| Complexity | Low |
| Cost | Reads every byte on first scan; changes on tag edits |
| Scalability | Poor on a NAS (terabytes read) |
| Team familiarity | High |

**Pros:** Simple and exact.
**Cons:** Too slow, and tag edits change it.

### Option D: Audio-payload digest, derived once, persisted (chosen)
| Dimension | Assessment |
|---|---|
| Complexity | Medium (payload locators per container) |
| Cost | At most 192 KiB read per new file |
| Scalability | Good |
| Team familiarity | Medium |

**Pros:** Survives tag edits, moves, renames, case-only renames and database rebuilds.
**Cons:** Exact duplicate files need disambiguation (path mixed in for the second); payload
locators must be tested per format.

## Trade-off Analysis

Option D costs a small amount of format-specific code and gains ids that survive everything
short of re-encoding, which itself is caught by the replacement rule.

## Consequences

- Easier: Tracks never reconciles by title for Trackside; acquired and uploaded songs have their
  final id at import.
- Harder: the payload locator is security-relevant parsing and needs fuzz tests.
- Revisit: audio fingerprinting (Chromaprint) for re-encode matching, if ever needed.

## Action Items

1. [ ] WP2 implements payload locators for FLAC, MP3, AAC, MP4, Ogg, WAV, AIFF with tests that a
   tag rewrite leaves the digest unchanged.
2. [ ] WP1 provides id derivation functions and their golden tests.
