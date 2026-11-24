# Remote access

At home, Tracks talks to the server's LAN address. Away from home it needs another way in.
Trackside keeps a list of every address it can be reached at and puts it in pairing codes;
Tracks tries them all, checks each answer comes from your server, prefers the LAN one, and
switches when your network changes. So the job here is to give the server one address that
works from outside, and tell it about that address.

| Option | Good for | Needs |
|---|---|---|
| [Tailscale](#tailscale) | most people; nothing exposed to the internet | the Tailscale app on each device |
| [Caddy in front](#caddy) | a public HTTPS address without a VPN app | a domain, port 443 forwarded |
| [Built-in HTTPS](#built-in-https) | the same, without a proxy | a domain, port 443 forwarded |
| [Cloudflare Tunnel](#cloudflare-tunnel) | the dashboard only | a Cloudflare account |

Whichever you choose, add the address in the dashboard under **Settings**, Other addresses (or
in `server.public_urls`). Devices that are already paired pick it up without pairing again: the
server tells open apps within seconds, and Tracks checks again whenever it comes to the
foreground. The Addresses list on the same page shows every address the server knows and
whether it could reach each one.

## How devices learn new addresses

Paired devices ask the server for its current list of addresses (`GET /api/v1/server/urls`), and
the server tells connected apps when the list changes. Only signed-in devices can read it. The
list comes only from what the server detects on its own network interfaces and from the
addresses you set; nothing a visitor sends can add one.

An address on the list still has to prove it's your server before Tracks sends it anything
secret, exactly as at pairing. Over HTTPS its certificate must be valid for the name, or match
the fingerprint of the server's self-signed certificate. So a wrong or stale address on the
list is harmless: Tracks just doesn't use it. Remove an address you no longer use, such as an
old domain, so Tracks stops trying it.

If you use `tls.mode = "self-signed"`, only the server's own LAN addresses carry the
fingerprint. A public `https://` address must have a certificate from a real authority, such
as Caddy's or the built-in ACME one.

## Tailscale

Tailscale puts your devices on a private network that works from anywhere, with no open
ports. The iPhone needs the Tailscale app connected when you're away.

### Tailscale on the host

If the server runs directly on a machine with Tailscale, it's already reachable at the
machine's tailnet name. For HTTPS (recommended, and needed for some iOS networks), turn on
MagicDNS and HTTPS certificates in the Tailscale admin console, then:

```sh
sudo tailscale serve --bg 4747
```

That serves `https://<machine>.<tailnet>.ts.net` to your tailnet only. Add that URL as an
address and set `server.trusted_proxies` to `127.0.0.1/32,::1/128`, because `tailscale serve`
connects from loopback.

### Tailscale next to the container

[`compose.tailscale.yaml`](../../packaging/compose.tailscale.yaml) runs a Tailscale container
that the server shares a network with, and serves it over HTTPS on your tailnet:

1. In the Tailscale admin console, turn on MagicDNS and HTTPS, and create an auth key
   (Settings, Keys).
2. Get the serve configuration next to your compose file:

   ```sh
   curl -fsSLO https://raw.githubusercontent.com/slukedev/tracks-companion/main/packaging/compose.tailscale.yaml
   curl -fsSLO https://raw.githubusercontent.com/slukedev/tracks-companion/main/packaging/tailscale-serve.json
   ```

3. Add to `.env`:

   ```sh
   TS_AUTHKEY=tskey-auth-...
   TRACKSIDE_TAILNET_URL=https://trackside.your-tailnet.ts.net
   COMPOSE_FILE=compose.yaml:compose.tailscale.yaml
   ```

4. `docker compose up -d`. The machine appears in your tailnet as `trackside`.

The LAN port is still published, so Tracks keeps using the faster LAN address at home.

Funnel (making the address public) is off in the shipped configuration. Turn it on only if
you know you want the server on the internet.

## Caddy

Caddy gets and renews certificates by itself and speaks HTTP/3. Point a DNS name at your home
address (a dynamic DNS name works), forward ports 80 and 443 to the machine running Caddy,
and use the recipe in [reverse-proxy.md](reverse-proxy.md#caddy). Then add
`https://music.example.com` as an address, and list Caddy's address in `server.trusted_proxies`:
without it, Trackside can't tell that the app used HTTPS and refuses to sign it in (see
[Plain HTTP](#plain-http)).

## Built-in HTTPS

Trackside can get a Let's Encrypt certificate itself, with no proxy:

```toml
[server]
listen = ["[::]:443", "[::]:4747"]
public_urls = ["https://music.example.com"]

[tls]
mode = "acme"
acme_domains = ["music.example.com"]
acme_email = "you@example.com"
```

It answers the TLS-ALPN-01 challenge on port 443, so 443 must reach it from the internet. With
a real certificate (this mode or `files`) Trackside sends `Strict-Transport-Security`, so browsers
use HTTPS for that name from then on. Certificates and the ACME account key in `<data>/acme` are
kept readable by the server's user only. In
Docker, publish `443:443`; under systemd, give it `CAP_NET_BIND_SERVICE`
([install.md](install.md#systemd)). Certificates are kept in `<data>/acme`.

Other TLS modes:

- `files`: your own certificate and key (`tls.cert_file`, `tls.key_file`), reloaded when they
  change. Good with certificates from a DNS challenge elsewhere.
- `self-signed`: generated at first start. Pairing codes carry its fingerprint so Tracks can
  trust exactly that certificate. Other apps will warn.

## Cloudflare Tunnel

A tunnel is handy for reaching the dashboard without opening ports. Don't use it for music:
Cloudflare's terms restrict serving audio and other large files through its network, and
accounts have been warned for it. Use Tailscale or a direct HTTPS address for Tracks, and
don't add the tunnel's address as an address for Tracks.

## On your network

Tracks finds the server by itself with Bonjour (`_trackside._tcp`) when it's on the same
network. Inside Docker this needs host networking ([install.md](install.md#host-networking));
with bridge networking it silently does nothing, and you add the server by address instead.

## Plain HTTP

Plain `http://` is fine on your own network and over Tailscale, which encrypts everything
itself. Anywhere else, use HTTPS: passwords, device keys and media tokens travel with every
request, and anyone on the way could read them.

Trackside enforces this. Over plain HTTP it accepts a password, app password, API key or device
key only from a device that is nearby:

- your own network: loopback, private addresses (`10.x`, `172.16-31.x`, `192.168.x`),
  link-local addresses and IPv6 `fc00::/7`;
- Tailscale's `100.64.0.0/10`;
- any network you list in `server.local_networks`.

From anywhere else, signing in, the dashboard, Subsonic apps and the Tracks app get "Use the
server's https address" (`403 https_required`) instead, before the password or key is even
checked. Nothing changes over HTTPS.

### If you see "Use the server's https address"

- **Away from home over plain HTTP.** Expected. Set up one of the options above and use its
  `https://` address.
- **At home, through a reverse proxy.** Trackside judges a request by the address it came
  from. A proxy (Caddy, nginx, Traefik, `tailscale serve`, a tunnel) is that address for every
  client behind it, so Trackside only believes what the proxy says about the real client
  (`X-Forwarded-For`) and about HTTPS (`X-Forwarded-Proto`) when the proxy is listed in
  `server.trusted_proxies`. Until it is, requests through it that carry forwarding headers are
  refused, even when the proxy is on your own network: it could be passing on requests from the
  internet. Add the proxy's address, for example `trusted_proxies = ["127.0.0.1/32", "::1/128"]`
  for a proxy on the same machine, or the proxy container's network such as `"172.18.0.0/16"`.
  The dashboard's Overview says when requests arrive through a proxy that isn't trusted.
- **At home, with global IPv6.** If your network hands out public IPv6 addresses rather than
  private ones, Trackside can't tell they are yours. Add the prefix to `server.local_networks`,
  for example `TRACKSIDE_SERVER__LOCAL_NETWORKS=2001:db8:1234:5678::/64`.
- **A name that points home from outside too.** If `music.example.com` resolves to your public
  IP even at home (or your router sends you out and back in), the server sees a public address
  and wants HTTPS. Use the LAN address at home, or HTTPS everywhere.

### Carrier-grade NAT and Tailscale's range

Tailscale gives out addresses in `100.64.0.0/10`, so Trackside counts that range as nearby.
Some internet providers use the same range for carrier-grade NAT, where it is the internet, not
your network. If your server is reached through such a provider without Tailscale in between,
set `server.tailscale_is_local = false` (`TRACKSIDE_SERVER__TAILSCALE_IS_LOCAL=false`) so plain
HTTP from that range needs HTTPS like any other public address. Tailscale users who turn it off
should use `tailscale serve` for HTTPS ([above](#tailscale-on-the-host)).

### What Tracks does

Tracks holds to the same rule itself: it sends a key, code or password over plain HTTP only to
a nearby address (a private or Tailscale IP, or a `.local`, `.lan`, `.internal`, `.home.arpa`
or `.ts.net` name), and only after that address has proven it is your server. A public
`http://` address is never used for signing in, and Tracks warns when you enter one.

### The dashboard over plain HTTP

Over plain HTTP the dashboard's sign-in works only from a nearby device, and its session
cookie can't be marked secure. That cookie is accepted only on the same terms: if a browser
brings it to the server over plain HTTP from somewhere that isn't nearby (a laptop that left
home, a name that resolves differently outside), Trackside answers `https_required` and clears
the cookie, so it stops being sent in the clear. Sign in again over HTTPS. Over HTTPS only the
secure cookie counts.
