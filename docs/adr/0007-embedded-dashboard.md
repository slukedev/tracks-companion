# ADR-0007: An embedded, build-free admin dashboard

**Status:** Accepted
**Date:** 2026-11-14
**Deciders:** Tracks maintainer

## Context

Admins need a web page for setup, users and invites, libraries and scans, devices and keys,
acquisition providers, and server health. The dashboard of the previous Subsonic proxy Tracks
used went blank when a font CDN was unreachable. Self-hosters run the server for years; a Node
toolchain that rots is a maintenance risk.

## Decision

Static HTML, CSS and JavaScript modules in `Server/dashboard/`, with no build step and no runtime
dependencies, embedded into the binary with `rust-embed` and served from `/`. System font stack,
no external URLs of any kind, strict CSP (`default-src 'self'`). QR codes are SVGs rendered by the
server. The dashboard talks only to the documented JSON endpoints in api.md.

## Options Considered

### Option A: No-build static pages (chosen)
| Dimension | Assessment |
|---|---|
| Complexity | Low |
| Cost | Hand-written DOM code |
| Scalability | Fine for a dozen screens |
| Team familiarity | High |

**Pros:** Nothing to rot; works offline; small.
**Cons:** No component framework.

### Option B: A SPA framework with a Node build
| Dimension | Assessment |
|---|---|
| Complexity | Medium |
| Cost | Toolchain maintenance |
| Scalability | Good |
| Team familiarity | Medium |

**Pros:** Faster UI development.
**Cons:** Build dependency for every contributor and packager; churn.

### Option C: Server-rendered HTML templates in Rust
| Dimension | Assessment |
|---|---|
| Complexity | Medium |
| Cost | UI changes need Rust rebuilds |
| Scalability | Good |
| Team familiarity | Medium |

**Pros:** No JavaScript.
**Cons:** Mixes UI into server crates; harder to work on in parallel.

## Trade-off Analysis

The dashboard is small and rarely used. Longevity and zero external dependencies matter more
than development speed.

## Consequences

- Easier: packaging, offline use, parallel work against a fixed JSON contract.
- Harder: the UI needs care to stay accessible and tidy without a framework.
- Revisit: none expected.

## Action Items

1. [ ] WP7 builds the pages and a fixture-backed preview mode for development.
2. [ ] WP6 serves the embedded files with correct caching and CSP headers.
