# Installing Trackside

Every method ends the same way: a running server, a setup token in its log, and
`http://<address>:4747/setup` to create the first admin ([first-run.md](first-run.md)).

Trackside needs:

- Linux (amd64 or arm64), directly or in Docker; macOS isn't supported yet. A Raspberry Pi 4
  or 5 is enough for a large library.
- A data folder on a local disk. The database must not live on a network share, because
  SQLite's locking is unreliable over NFS and SMB; the server refuses to start there unless
  you set `database.allow_network_fs`. Your music can be anywhere, including a NAS.
- Port 4747 reachable on your network (it serves the dashboard, the native API for Tracks
  and the Subsonic API).
- ffmpeg, optional. Without it every song plays as it is, and phones on cellular get the
  original file.

Contents: [Building from source with Compose](#building-from-source-with-compose),
[Docker](#docker), [Docker Compose](#docker-compose), [Binary](#binary), [systemd](#systemd),
[NAS](#nas).

Images, binaries and packages are published only for releases. Until the first release, and
whenever you want `main` between releases, build the image on your own machine.

## Building from source with Compose

[`packaging/compose.build.yaml`](../../packaging/compose.build.yaml) layers on top of
`compose.yaml` and replaces the published image with one Docker builds locally. Docker clones
the repository from GitHub at the ref you choose and builds it with BuildKit; nothing is
pulled from ghcr.io.

```sh
mkdir trackside && cd trackside
curl -fsSLO https://raw.githubusercontent.com/slukedev/tracks-companion/main/packaging/compose.yaml
curl -fsSLO https://raw.githubusercontent.com/slukedev/tracks-companion/main/packaging/compose.build.yaml
echo "MUSIC_DIR=/path/to/your/music" > .env
echo "COMPOSE_FILE=compose.yaml:compose.build.yaml" >> .env
docker compose up -d --build
```

Two variables in `.env` choose what is built:

| Variable | Default | Values |
|---|---|---|
| `TRACKSIDE_BUILD_TARGET` | `standard` | `standard`, `slim` or `acquire` (the [variants](#images-and-tags)) |
| `TRACKSIDE_BUILD_REF` | `main` | a branch, a release tag such as `trackside-v1.2.0`, or a full commit hash to stay on one build |

With [acquisition](acquisition.md), list `compose.build.yaml` last and build the `acquire`
target:

```sh
echo "COMPOSE_FILE=compose.yaml:compose.acquire.yaml:compose.build.yaml" >> .env
echo "TRACKSIDE_BUILD_TARGET=acquire" >> .env
```

The first build downloads a Rust toolchain image and compiles the server in release mode. It
takes about 10 to 15 minutes on a 4-core x86 machine, and much longer on a Raspberry Pi or a
small NAS (allow an hour or more). It needs about 10 GB of free disk, most of it build cache,
and at least 2 GB of memory. Later builds reuse the toolchain and the Cargo caches, so they
only recompile what changed. `docker builder prune` gives the cache space back, at the cost of
a full build next time.

To update, run `docker compose up -d --build` again: Docker fetches the ref anew and restarts
the server if the image changed. Read [upgrading.md](upgrading.md#building-from-main) first;
`main` may change the database before a release does.

The image a local build makes is the same as the published one, without the cosign signature
and attestations. Its labels carry the ref (`source-main`) instead of a version;
`trackside version` prints the version in `Cargo.toml`.

## Docker

```sh
docker run -d --name trackside \
  --restart unless-stopped \
  -p 4747:4747 \
  -v trackside-data:/data \
  -v /path/to/music:/music:ro \
  --read-only --tmpfs /tmp \
  ghcr.io/slukedev/trackside:latest
docker logs trackside
```

### Images and tags

Each release publishes images to `ghcr.io/slukedev/trackside` for `linux/amd64` and
`linux/arm64`, signed with cosign and shipped with SBOM and provenance attestations. Nothing
is published between releases.

| Variant | Tags | Base | Includes | For |
|---|---|---|---|---|
| standard | `latest`, `X.Y.Z`, `X.Y`, `X` | distroless static (Debian 13) | Trackside, ffmpeg | most people |
| slim | `slim` or `latest-slim`, `X.Y.Z-slim`, `X.Y-slim`, `X-slim` | distroless static (Debian 13) | Trackside | the smallest image, direct play only |
| acquire | `acquire` or `latest-acquire`, `X.Y.Z-acquire`, `X.Y-acquire`, `X-acquire` | distroless cc (Debian 13) | Trackside, ffmpeg, yt-dlp, Deno | [acquisition](acquisition.md) |

`latest` is the newest release, and `X.Y.Z` is one release that never changes. To run `main`
instead, [build it yourself](#building-from-source-with-compose): it has the newest changes
and may break.

Pin a version (`1.2.3`) on anything you depend on, and move it on purpose
([upgrading.md](upgrading.md)). `X.Y` follows patch releases of that minor version, and `X`
(from 1.0 on) follows that major version. Pre-releases get only their own `X.Y.Z-rc.N` tags.

Every image:

- runs as user and group `65532`, never as root;
- keeps everything it writes in `/data` (a volume), so the rest of the filesystem can be
  read-only (`--read-only`);
- reads music from `/music`, which becomes the first library on the first start if it has
  files in it;
- answers `HEALTHCHECK` with `trackside healthcheck`, which asks the server's `/healthz`;
- sets `TRACKSIDE_DATA_DIR=/data` and `TRACKSIDE_MUSIC_DIR=/music`.

To check a signature:

```sh
cosign verify ghcr.io/slukedev/trackside:latest \
  --certificate-identity-regexp '^https://github.com/slukedev/tracks-companion/\.github/workflows/images\.yml@' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

### Using a folder instead of a volume for /data

A named volume is the easiest choice: Docker creates it owned by the image's user. With a
folder on the host, give that user write access first, or run the container as your own user:

```sh
mkdir -p /srv/trackside
sudo chown 65532:65532 /srv/trackside
# or: docker run --user "$(id -u):$(id -g)" ...
```

## Docker Compose

The [quick start in the README](../../README.md#quick-start) uses
[`packaging/compose.yaml`](../../packaging/compose.yaml). Optional files layer on top of it:

| File | Adds |
|---|---|
| [`compose.build.yaml`](../../packaging/compose.build.yaml) | builds the image from source on this machine instead of pulling it ([above](#building-from-source-with-compose)); list it last |
| [`compose.acquire.yaml`](../../packaging/compose.acquire.yaml) | the `acquire` image, an slskd container and the folders acquisition uses ([acquisition.md](acquisition.md)) |
| [`compose.tailscale.yaml`](../../packaging/compose.tailscale.yaml) | a Tailscale container giving the server an HTTPS address on your tailnet ([remote-access.md](remote-access.md#tailscale)) |

Use them with `-f`, or once in `.env`:

```sh
echo "COMPOSE_FILE=compose.yaml:compose.tailscale.yaml" >> .env
docker compose up -d
```

[`env.example`](../../packaging/env.example) lists every variable the compose files read.

### Host networking

With Docker's default bridge network the server sees only its container address. Two things
follow: Bonjour discovery from Tracks doesn't work, and pairing QR codes don't carry your LAN
address unless you add it in the dashboard under **Settings**, Other addresses. Host networking fixes both, on
Linux:

```yaml
services:
  trackside:
    # remove the ports: section, then
    network_mode: host
```

Docker Desktop on a Mac or Windows runs containers in a VM, so host networking there doesn't
reach your LAN. Add the address in the dashboard instead.

Published ports can also hide who is connecting. Docker Desktop, and Docker's userland proxy
on Linux (used for IPv6 when the container has no IPv6 address, or with `userland-proxy`
on), hand every connection to Trackside from a private gateway address such as `172.17.0.1`.
Trackside then sees internet clients as local: they share one address for rate limits, and
over plain HTTP they may sign in and send keys, which Trackside otherwise refuses from the
internet, and get a session cookie that isn't marked secure. If the
server is reachable from the internet, use host networking on Linux, or put a reverse proxy in
front with HTTPS and list it in `server.trusted_proxies`.

## Binary

Each release has static Linux binaries (musl, amd64 and arm64) on the
[releases page](https://github.com/slukedev/tracks-companion/releases), with a `SHA256SUMS` file.

```sh
version=1.2.3
arch=x86_64-unknown-linux-musl   # or aarch64-unknown-linux-musl
curl -fsSLO "https://github.com/slukedev/tracks-companion/releases/download/trackside-v$version/trackside-$version-$arch.tar.gz"
curl -fsSLO "https://github.com/slukedev/tracks-companion/releases/download/trackside-v$version/SHA256SUMS"
sha256sum --check --ignore-missing SHA256SUMS
tar xzf "trackside-$version-$arch.tar.gz"
sudo install -m 0755 "trackside-$version-$arch/trackside" /usr/local/bin/trackside
```

Run it in the foreground to try it:

```sh
TRACKSIDE_MUSIC_DIR=~/Music trackside serve
```

Without `TRACKSIDE_DATA_DIR`, data goes to `~/.local/share/trackside`. For something that
keeps running, use systemd below.

## systemd

The `.deb` and `.rpm` packages install the binary, the unit
[`trackside.service`](../../packaging/systemd/trackside.service) and
`/etc/trackside/trackside.env`.

```sh
# Debian, Ubuntu, Raspberry Pi OS
sudo apt install ./trackside_1.2.3-1_arm64.deb
# Fedora, RHEL, openSUSE
sudo dnf install ./trackside-1.2.3-1.x86_64.rpm
```

Point it at your music and start it:

```sh
sudoedit /etc/trackside/trackside.env      # set TRACKSIDE_MUSIC_DIR if not /srv/music
sudo systemctl start trackside
sudo journalctl -u trackside -n 30         # the setup token is in the banner
```

From a release tarball instead, copy the binary to `/usr/bin/trackside` and the unit to
`/etc/systemd/system/`, then `sudo systemctl daemon-reload && sudo systemctl enable --now trackside`.

What the unit does:

- `DynamicUser=yes`: systemd makes an unprivileged user for the service each time it starts.
  Data lives in `/var/lib/trackside` and caches in `/var/cache/trackside`.
- `ProtectSystem=strict` and `ProtectHome=read-only`: the service can read your music
  anywhere but can write only to its own folders. `ReadOnlyPaths` lists the music folders.
- `Type=notify` with `WatchdogSec=30`: systemd knows when the server is ready and restarts
  it if it stops answering.

Change settings with a drop-in so upgrades keep them:

```sh
sudo systemctl edit trackside
```

```ini
[Service]
Environment=TRACKSIDE_MUSIC_DIR=/mnt/nas/music
ReadOnlyPaths=/mnt/nas/music
```

A music folder on a NAS must be readable by any user (the dynamic user has no fixed uid), for
example an NFS export with `all_squash` or files with world read permission. See
[libraries.md](libraries.md#permissions).

For a writable library (uploads or acquisition), allow writing to that folder only:

```ini
[Service]
ReadWritePaths=/srv/music/acquired
```

To serve HTTPS on port 443 directly with built-in ACME, the service needs the capability to
bind a low port:

```ini
[Service]
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
```

## NAS

Templates for each are in [`packaging/nas/`](../../packaging/nas/). All of them use the
`latest` image, keep `/data` on the NAS's app storage, mount music read-only, and run as a
user that already owns the app folder.

### Unraid

In the Docker tab choose **Add Container**, then under Template paste the URL of
[`unraid/trackside.xml`](../../packaging/nas/unraid/trackside.xml):

```
https://raw.githubusercontent.com/slukedev/tracks-companion/main/packaging/nas/unraid/trackside.xml
```

The template runs the container as `99:100` (Unraid's `nobody:users`, which owns
`/mnt/user/appdata`). Keep appdata on a cache pool, not on the array behind a network share.

### TrueNAS SCALE

On 24.10 or later: Apps, Discover Apps, the menu, **Install via YAML**, and paste
[`truenas/compose.yaml`](../../packaging/nas/truenas/compose.yaml) after changing the two
paths. Create a dataset for the data and give the `apps` user (568) Modify permission on it.

### Synology

With Container Manager on DSM 7.2 or later:

1. In File Station, create `docker/trackside/data`.
2. Find your user and group ids (`id` over SSH; often `1026` and `100`).
3. Container Manager, Project, **Create**: path `docker/trackside`, source "Create
   docker-compose.yml", and paste [`synology/compose.yaml`](../../packaging/nas/synology/compose.yaml)
   with your ids in `user:` and your music share in the second volume.

### CasaOS and ZimaOS

App Store, **Custom Install**, Import, and paste
[`casaos/docker-compose.yml`](../../packaging/nas/casaos/docker-compose.yml). It uses
`/DATA/AppData/trackside` and `/DATA/Media/Music`.
