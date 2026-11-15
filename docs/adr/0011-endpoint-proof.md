# ADR-0011: Prove an address is the server before sending it a credential

**Status:** Accepted
**Date:** 2026-11-15
**Deciders:** Tracks maintainer

## Context

ADR-0009 has Tracks race `GET /api/v1/ping` across a server's candidate addresses and use the
first that answers with the right `server_id`. The `server_id` is public (ping, mDNS, the QR
code), and the LAN candidates are plain `http://` or `name.local`. On a stranger's Wi-Fi anyone
can answer for `192.168.1.10` or `home.local`, return the right `server_id`, win the race, and
receive the device key in the clear. During QR pairing the same race hands over the QR secret.
The self-signed `fingerprint` in the candidate list was never checked.

Two things have to hold before a device key or QR secret is sent:

1. The address answering is the real server, not someone who learned its id.
2. The answer was not relayed: an attacker on the stranger's network who forwards the client's
   challenge to the server's public address must not get an answer the client accepts.

## Decision

`GET /api/v1/ping?challenge=<nonce>&host=<address>&device=<device id>` (or `&offer=<offer id>`
for a QR code) answers with `proof`:

```
HMAC-SHA256(K, "trackside endpoint proof v1" 0x00 server_id 0x00 host 0x00 nonce)
```

- For a device, `K` is SHA-256 of the device key: what the server already stores to look keys
  up, and what the client can compute. For a QR code, `K` is the server's stored hash of the
  QR secret (`SHA-256("offer qr" 0x00 secret)`), and the QR payload gains the offer's id
  (`offer`) so the server can find it. Neither the key nor the secret, nor `K`, crosses the
  network; the device id and offer id are not secret.
- `host` is the address as the client wrote it (`host` or `host:port`, lowercased, IPv6 in
  brackets, no default port), so a proof is only good for the address the client asked.
- A nearby address (a private, loopback, link-local, unique-local or Tailscale IP, a name
  without a dot, or a name under `.local`, `.lan`, `.internal`, `.home.arpa` or `.ts.net`) is
  only proven to a client that itself reached the server from such an address, directly or
  through a trusted proxy. A relay from a stranger's network arrives at the public address from
  a public address, so the server gives no proof for the nearby `host`; a proof it gives for its
  public address is bound to that address, and the client, which asked a nearby one, refuses it.
- The client sends a fresh 128-bit nonce per address and per race, compares in constant time,
  and uses an address only once it has proven itself. The proof lasts until the network
  changes, two requests in a row fail, or, for plain HTTP, ten minutes pass.
- Credentials (device key, password, typed or QR code, poll secret) are never sent over plain
  HTTP to an address that isn't nearby. Over HTTPS, a candidate with a `fingerprint` is held to
  it: its certificate is accepted when, and only when, its SHA-256 matches.
- The device-initiated pairing (a 6-digit code shown in the app and approved in the dashboard)
  needs no proof: the app sends nothing secret, and an impostor can only hand it a key that
  works nowhere.
- A ping without `challenge` still answers `server_id` and `time` as before. Nothing has
  shipped, so a server that gives no proof is simply not used.

## Options Considered

### Option A: HMAC keyed with a hash both sides already hold (chosen)
| Dimension | Assessment |
|---|---|
| Complexity | Low: one HMAC, one indexed read per challenge |
| Cost | No new keys, files or migrations |
| Security | Sound against impostors; relays stopped by the nearby-host rule |
| Team familiarity | High |

**Pros:** Uses data both sides have, per device, so revoking a device ends its proofs; works for
the QR secret the same way.
**Cons:** A ping with a challenge reads the database. Someone with the database can answer
proofs, but they already hold every key hash and the server itself.

### Option B: A server identity key (Ed25519), public half in the pairing payload
| Dimension | Assessment |
|---|---|
| Complexity | Medium: a new key, its storage and rotation, a new dependency on both sides |
| Cost | Every device breaks if the key file is replaced |
| Security | The same against impostors; relays need the same nearby-host rule |
| Team familiarity | Medium |

**Pros:** No database read per ping; one key for every device.
**Cons:** More to keep safe and back up, and no better against relays, which is the hard part.

### Option C: Remember the network a plain HTTP address was paired on
| Dimension | Assessment |
|---|---|
| Complexity | High: gateway or SSID fingerprints differ per platform and change on their own |
| Security | Spoofable: a stranger's network can copy a gateway address or SSID |

**Pros:** No server change.
**Cons:** Not sound on its own; at best a hint.

## Consequences

- Tracks never sends a device key or a QR secret to an address that hasn't proven it is the
  server, and never sends any credential in the clear beyond the nearby network.
- Behind a reverse proxy that isn't in `server.trusted_proxies`, nearby addresses can't be
  proven through it; the proxy is meant for the public address, which is proven over HTTPS.
- A server reachable only through a proxy that hides `/ping` can no longer be used; it has to
  pass `/api/v1/ping` through, as the docs already say.
- Media URLs carry only the short-lived media token, and are built from the address last proven.
  `AVPlayer` and background downloads don't use the pinned session, so a self-signed address
  that is pinned plays through the system's checks; that stays a known limit.
