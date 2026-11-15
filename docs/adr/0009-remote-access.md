# ADR-0009: Remote access through candidate URLs, pairing and client-side racing

**Status:** Accepted, amended by [ADR-0011](0011-endpoint-proof.md)
**Date:** 2026-11-14
**Deciders:** Tracks maintainer

## Context

Tracks must play reliably at home and away: on Wi-Fi with a LAN address, on cellular through a
tunnel or a public domain, across network changes. Self-hosters use Tailscale, reverse proxies
with TLS, Cloudflare Tunnel, or nothing. Tracks today stores one URL per server.

## Decision

- The server publishes every candidate base URL it knows (LAN interfaces, `.local`, configured
  public and tailnet URLs) in pairing payloads and `/api/v1/me`.
- Pairing by QR code, typed code, device-initiated code, or password sign-in, each ending with a
  device key and the candidate list.
- Tracks races `GET /api/v1/ping` across candidates on connect, on network changes and after
  failures, verifies the `server_id`, prefers LAN, and sticks with the winner.
- mDNS `_trackside._tcp` for discovery on the LAN.
- TLS modes: off (default, for LAN, tailnet or a proxy), files, built-in ACME, self-signed with a
  pinned fingerprint. Trusted-proxy handling for forwarded headers.
- Documentation recommends Tailscale first, Caddy or built-in ACME for public access, and
  Cloudflare Tunnel only for the dashboard.
- No embedded Tailscale (tailscale-rs is a preview) and no relay service run by the project.

## Options Considered

### Option A: One URL, user-managed (today)
| Dimension | Assessment |
|---|---|
| Complexity | Low |
| Cost | Breaks when moving between networks |
| Scalability | n/a |
| Team familiarity | High |

**Pros:** Simple.
**Cons:** Either slow at home (public URL hairpinning) or broken away (LAN URL).

### Option B: Candidate URLs plus racing (chosen)
| Dimension | Assessment |
|---|---|
| Complexity | Medium |
| Cost | Client-side racing logic |
| Scalability | Good |
| Team familiarity | Medium |

**Pros:** Fast at home, works away, survives network changes; credentials are per server, not
per URL.
**Cons:** More moving parts in the client.

### Option C: A hosted relay
| Dimension | Assessment |
|---|---|
| Complexity | High |
| Cost | Running infrastructure; privacy |
| Scalability | Costly for audio |
| Team familiarity | Low |

**Pros:** Works behind CGNAT without setup.
**Cons:** Contradicts Tracks' privacy stance and costs money.

## Trade-off Analysis

Racing a handful of URLs is cheap and handles every hosting setup without the project running
infrastructure.

## Consequences

- Easier: reliable playback away from home; zero-typing setup by QR.
- Harder: Docker bridge networking hides mDNS and LAN addresses; docs must explain host
  networking and `public_urls`.
- Revisit: embedded Tailscale when a stable Rust library exists; native HTTP/3.
- Racing on `server_id` alone let anyone who learned the id answer for a plain `http://` or
  `.local` candidate on a stranger's network. [ADR-0011](0011-endpoint-proof.md) adds an endpoint
  proof to `/ping` that a candidate must give before Tracks sends it a credential.

## Action Items

1. [ ] WP6 implements candidate URL detection, pairing and mDNS.
2. [ ] WP9 implements `ServerEndpoints` racing in TracksCore.
