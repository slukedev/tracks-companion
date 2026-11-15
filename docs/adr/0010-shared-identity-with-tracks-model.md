# ADR-0010: Reuse tracks-model for every identity the server stores

**Status:** Accepted
**Date:** 2026-11-14
**Deciders:** Tracks maintainer

## Context

Tracks groups history by `song_key(title, artist)`, built from Foundation-compatible folding in
`tracks-model`. If the server folded names differently, a song in the history and the same song
on the server would not match, and Tracks would fall back to fuzzy matching as it does with other Subsonic servers.

## Decision

The server depends on `tracks-model` from the Tracks repository and uses it for `songs.song_key`,
`albums.album_key`, `artists.identity`, artist credits, and the text indexed for search. The
native API returns `song_key` and `album_key` so the client can compare directly. A folding change
in `tracks-model` requires a server migration that recomputes stored keys and bumps sync epochs,
enforced by a golden test.

## Options Considered

### Option A: Path dependency on tracks-model (chosen)
| Dimension | Assessment |
|---|---|
| Complexity | Low |
| Cost | Cross-workspace coupling |
| Scalability | n/a |
| Team familiarity | High |

**Pros:** Byte-identical keys by construction.
**Cons:** Engine changes can require server migrations.

### Option B: Copy the code into the server
| Dimension | Assessment |
|---|---|
| Complexity | Low |
| Cost | Drift over time |
| Scalability | n/a |
| Team familiarity | High |

**Pros:** Independence.
**Cons:** The exact problem this avoids.

### Option C: Publish tracks-model to crates.io
| Dimension | Assessment |
|---|---|
| Complexity | Medium |
| Cost | Release process |
| Scalability | n/a |
| Team familiarity | Medium |

**Pros:** Versioned dependency.
**Cons:** Overhead without benefit inside one repository.

## Trade-off Analysis

The coupling is the point: both sides must change together.

## Consequences

- Easier: exact history matching; simpler Tracks code.
- Harder: server CI must run on Engine changes.
- Revisit: if the server ever moves to its own repository, publish tracks-model.

## Update: own repository

The server moved to its own repository. Instead of publishing `tracks-model`, the workspace pins
it as a git dependency on `slukedev/tracks` at an exact revision, which keeps Option A's
byte-identical keys without a release process. The revision moves with app releases, and the wire
fixture tests catch drift between the two.

## Action Items

1. [ ] WP1 records the folding version in `settings` and adds the golden test.
2. [ ] Move the pinned `tracks-model` revision when the app's folding changes.
