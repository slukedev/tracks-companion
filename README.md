# Trackside

Trackside is a music server you run yourself, made for the
[Tracks](https://github.com/slukedev/tracks) app. It indexes the music you own and serves it to
Tracks on your iPhone and Mac, at home and away, and to any app that speaks Subsonic or
OpenSubsonic (Amperfy, Symfonium, Feishin, Supersonic and others). Everyone in the house gets
their own account, with their own listens, favourites and playlists. With
[Music Assistant](docs/ops/music-assistant.md) beside it, Tracks also plays on the speakers in
your home and from your streaming services.

It's one program with a web dashboard built in. It starts with no configuration at all,
keeps everything in one data folder, backs itself up every night, and never touches your
music files.

Trackside is new and its version is still 0.x. Expect rough edges, and keep the nightly
backups it makes.

## Quick start

You need Docker with Compose (Docker Desktop on a Mac or Windows, or Docker Engine on Linux)
and a folder of music. There is no release yet, so Docker builds Trackside from `main` on your
machine: the first build takes about 10 to 15 minutes and needs about 10 GB of free disk.

**1. Get the compose files**

```sh
mkdir trackside && cd trackside
curl -fsSLO https://raw.githubusercontent.com/slukedev/tracks-companion/main/packaging/compose.yaml
curl -fsSLO https://raw.githubusercontent.com/slukedev/tracks-companion/main/packaging/compose.build.yaml
```

**2. Tell it where your music is, and to build from source**

```sh
echo "MUSIC_DIR=/path/to/your/music" > .env
echo "COMPOSE_FILE=compose.yaml:compose.build.yaml" >> .env
```

Trackside mounts the folder read-only. Files and folders need to be readable by everyone
(the usual default), because the server runs as its own unprivileged user.

**3. Build and start it**

```sh
docker compose up -d --build
docker compose logs trackside
```

To update later, run `docker compose up -d --build` again. Once a release is out, you can
remove the `COMPOSE_FILE` line instead and pull the published `latest` image
([install.md](docs/ops/install.md#building-from-source-with-compose)).

The log shows a banner with a one-time setup token:

```
Trackside needs an admin account. Open http://192.168.1.20:4747/setup and enter this setup token:
  7KQ2-M9XD-...-P4TA
```

**4. Create your account**

Open `http://<this machine's address>:4747/setup`, paste the token and choose a username and
password. The first scan of your music starts on its own; the dashboard shows how it's going.

To find this machine's address, run `hostname -I` on Linux or `ipconfig getifaddr en0` on a
Mac.

**5. Connect Tracks**

In Tracks, open Settings, then Play, and under Servers choose **Connect a Server…**. Enter
`http://<this machine's address>:4747`, choose **Password**, and sign in with the account you
just made. Your library appears as the scan finds it.

You can also pair without typing a password: in the dashboard open **My Account** and choose
**Show Code** under Pair a Device. On iPhone, tap **Scan Pairing Code** in Tracks' Connect a
Server form and scan the QR code; on a Mac, enter the address and type the code shown under it.
Docker hides your LAN address from the server, so for the QR code to carry it, add
`http://<this machine's address>:4747` under **Settings**, Other addresses (or set
`TRACKSIDE_SERVER__PUBLIC_URLS` in `compose.yaml`), or use host networking.

That's it. To play away from home, read [remote access](docs/ops/remote-access.md); the short
version is to install Tailscale on the server and your phone. Passwords and keys travel over
plain `http://` only on your own network or tailnet; from anywhere else the server refuses them
and asks for its HTTPS address, and Tracks never sends them. A reverse proxy in front of
Trackside has to be listed in `server.trusted_proxies` and pass `/api/v1/ping` through with its
query string, because Tracks checks each address with it before sending any credential. Paired
devices learn new addresses by themselves, so adding one later needs no new pairing.

## Other ways to install

| How | Guide |
|---|---|
| Docker without Compose, image variants and tags | [install.md](docs/ops/install.md#docker) |
| A single binary on Linux | [install.md](docs/ops/install.md#binary) |
| systemd, `.deb` and `.rpm` packages | [install.md](docs/ops/install.md#systemd) |
| Unraid, TrueNAS, Synology, CasaOS and ZimaOS | [install.md](docs/ops/install.md#nas) |

There are three images, all built on Debian 13 distroless bases. `latest` has ffmpeg, so
Trackside can convert songs for phones on cellular and for formats Apple devices don't play.
`slim` leaves ffmpeg out and plays every song as it is. `acquire` adds what the optional
acquisition feature needs.

`latest` is the newest release. To run `main`, build it on your machine with
[`compose.build.yaml`](docs/ops/install.md#building-from-source-with-compose); it has the newest
changes and may break.

## Running it

- [First run, accounts and invites](docs/ops/first-run.md)
- [Libraries and file permissions](docs/ops/libraries.md)
- [Remote access](docs/ops/remote-access.md): Tailscale, Caddy, built-in HTTPS, Cloudflare
  Tunnel, and discovery on your network
- [Reverse proxies](docs/ops/reverse-proxy.md): Caddy and nginx settings that keep streaming
  and live updates working
- [Backups and restore](docs/ops/backups.md), including Litestream
- [Upgrading and downgrading](docs/ops/upgrading.md)
- [Troubleshooting](docs/ops/troubleshooting.md)
- [Configuration reference](docs/ops/configuration.md)
- [Acquisition](docs/ops/acquisition.md), off unless an admin turns it on
- [Speakers and streaming services](docs/ops/music-assistant.md) through Music Assistant, and
  the [phone relay](docs/ops/relay.md)

## Other music apps

Any Subsonic or OpenSubsonic app works with the same server. Apps that support OpenSubsonic
API keys should use one: in the dashboard, **My Account**, App Keys, **New Key**. Older apps
that only know a username and password need an app password, made the same way, which an admin
has to allow for that account. Your login password never works in a Subsonic app, on purpose.

## Building from source

Trackside is a Rust Cargo workspace.

```sh
git clone https://github.com/slukedev/tracks-companion.git
cd tracks-companion
cargo build --release
./target/release/trackside serve
```

`rust-toolchain.toml` picks the compiler (Rust 1.100), so rustup installs the right one on
the first build. ffmpeg on your `PATH` is optional. Building the container images:

```sh
docker buildx build -f packaging/docker/Dockerfile --target standard -t trackside .
```

Trackside takes its name folding and song keys from `tracks-model`, the same crate the
[Tracks](https://github.com/slukedev/tracks) apps use, as a git dependency pinned to an exact
revision in `Cargo.toml`. Song keys must match the app byte for byte, so the revision moves with
app releases, and the wire fixture tests catch drift.

## Contributing

The Tracks app's [CONTRIBUTING.md](https://github.com/slukedev/tracks/blob/main/CONTRIBUTING.md)
applies here too. What's different for the server is in
[docs/ops/contributing.md](docs/ops/contributing.md): the checks to run before a pull request, how the design documents in [docs/](docs/) are used, and how to test
packaging changes.

How the server is built and why: [architecture.md](docs/architecture.md),
[api.md](docs/api.md) and the decision records in [docs/adr/](docs/adr/).

## Security

Please report security problems privately, through GitHub's
[private vulnerability reporting](https://github.com/slukedev/tracks-companion/security/advisories/new),
not in a public issue. The server's own threat model is in
[architecture.md, section 14](docs/architecture.md#14-security-model-and-threat-model).

## License

Trackside is licensed under the [GNU General Public License v3.0 only](LICENSE)
(`GPL-3.0-only`), the same license as the Tracks apps and the `tracks-model` crate it shares
with them. Every crate in this workspace declares it through `license.workspace`, and the
dashboard in `dashboard/` is covered by the same license.

The `latest` and `acquire` images include ffmpeg, built by
[wader/static-ffmpeg](https://github.com/wader/static-ffmpeg) under the GPL v3 without
nonfree parts. The `acquire` image also includes [yt-dlp](https://github.com/yt-dlp/yt-dlp)
(Unlicense) and [Deno](https://github.com/denoland/deno) (MIT). The exact versions are pinned
in [the Dockerfile](packaging/docker/Dockerfile), and their sources are at those projects'
release pages.
