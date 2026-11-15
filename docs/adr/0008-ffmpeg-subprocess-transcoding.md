# ADR-0008: Transcode with ffmpeg as a subprocess

**Status:** Accepted
**Date:** 2026-11-14
**Deciders:** Tracks maintainer

## Context

Off-LAN playback needs AAC (and MP3, Opus, FLAC for other clients), HLS for seeking in uncached
transcodes, and gapless metadata. Rust has good pure decoders (symphonia) but no production AAC
encoder. Linking libav* makes static builds and licensing harder.

## Decision

Run `ffmpeg` as a child process with a fixed argument vector per profile, never a shell string,
never editable at runtime. Native `aac` encoder (libfdk_aac is not redistributable). Bit-exact
flags so the output and its ETag are reproducible. Single-flight jobs, a global concurrency limit
with priorities, an LRU cache, HLS fMP4 from one continuous encoder run, remuxed to a cached M4A
at the end. Without ffmpeg, direct play only.

## Options Considered

### Option A: ffmpeg subprocess (chosen)
| Dimension | Assessment |
|---|---|
| Complexity | Low to medium |
| Cost | One external binary |
| Scalability | Bounded by CPU, controlled by the semaphore |
| Team familiarity | High |

**Pros:** Every codec; process isolation from decoder bugs; no linking or licence entanglement.
**Cons:** A dependency to ship (static build in the image); process spawn cost (small).

### Option B: Link ffmpeg (ffmpeg-next)
| Dimension | Assessment |
|---|---|
| Complexity | High |
| Cost | Static linking pain, licence coupling |
| Scalability | Same |
| Team familiarity | Low |

**Pros:** No process boundary.
**Cons:** A crash in a decoder takes down the server; builds get hard.

### Option C: Pure Rust decode plus an encoder crate
| Dimension | Assessment |
|---|---|
| Complexity | High |
| Cost | No mature AAC encoder |
| Scalability | Good |
| Team familiarity | Low |

**Pros:** No external binary.
**Cons:** Not available at the needed quality.

## Trade-off Analysis

The subprocess model is how Navidrome and gonic work and is the most robust for a server that
must survive odd files.

## Consequences

- Easier: adding profiles; isolating failures.
- Harder: verifying gapless behaviour of ffmpeg's AAC output on Apple devices (planned device test).
- Revisit: native Opus in MP4 for iOS once verified on devices.

## Action Items

1. [ ] WP3 implements jobs, cache, HLS and the decision function with a fake transcoder for tests.
2. [ ] WP8 ships a static ffmpeg in the standard image.
