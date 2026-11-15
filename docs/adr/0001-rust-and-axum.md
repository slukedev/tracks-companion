# ADR-0001: Write the server in Rust with axum and tokio

**Status:** Accepted
**Date:** 2026-11-14
**Deciders:** Tracks maintainer

## Context

Trackside must be very fast on small hardware (a Raspberry Pi or a NAS), ship as one static
binary, and share identity code with the Tracks apps. That code, name folding, song and album
keys and artist credits, already lives in Rust in the Tracks repository's
`Engine/crates/tracks-model` and is checked byte for byte against Foundation. The newer half of
the previous Subsonic proxy Tracks used is also Rust on axum, and the maintainer already works in
Rust in the Tracks engine.

## Decision

Rust (edition 2024, toolchain 1.99.0 like the Tracks engine), tokio 1.54, axum 0.8, tower-http 0.7,
axum-server 0.9 with rustls 0.23 (ring provider) for TLS. A separate workspace that
depends on `tracks-model` (by path while the server lived in the Tracks repository, now by a
pinned git revision; see ADR 0010).

## Options Considered

### Option A: Rust, axum, tokio
| Dimension | Assessment |
|---|---|
| Complexity | Medium |
| Cost | Free, small static musl binaries |
| Scalability | Excellent per core; low memory |
| Team familiarity | High (Engine) |

**Pros:** Reuses `tracks-model` directly, so identities cannot drift; static binary; mature HTTP
stack; strong typing for wire formats.
**Cons:** Longer compile times; async plus blocking SQLite needs care.

### Option B: Go (like Navidrome and gonic)
| Dimension | Assessment |
|---|---|
| Complexity | Low to medium |
| Cost | Free, static binaries |
| Scalability | Very good |
| Team familiarity | Low |

**Pros:** Simple concurrency; proven by Navidrome.
**Cons:** Would need a port of `tracks-model` and golden tests to keep it identical; a second
language in the repo.

### Option C: Swift on Linux (Vapor or Hummingbird)
| Dimension | Assessment |
|---|---|
| Complexity | Medium |
| Cost | Larger runtime on Linux |
| Scalability | Good |
| Team familiarity | High |

**Pros:** Same language as the apps.
**Cons:** Foundation on Linux differs from Apple's in exactly the folding behaviour that matters;
weaker static-binary and musl story; smaller server ecosystem.

## Trade-off Analysis

Identity parity with Tracks is the deciding factor: it is already solved in Rust and verified
against Apple's Foundation, and every other option reopens it. Rust's cost is compile time and
async discipline, both manageable.

## Consequences

- Easier: one implementation of folding for apps and server; small, fast binaries.
- Harder: blocking work (SQLite, filesystem, tag parsing) must stay off async workers.
- Revisit: native HTTP/3 once `h3` reaches 1.0.

## Action Items

1. [ ] WP1 lands the workspace, toolchain file and lint set matching Engine.
2. [ ] CI builds the server against the pinned `tracks-model` revision.
