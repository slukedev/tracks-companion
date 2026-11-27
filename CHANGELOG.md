# Trackside changelog

What changes for people running the server. Each release lists anything you need to do when
upgrading, and says when the database changes (the server backs it up first; see
[upgrading](docs/ops/upgrading.md)).

Versions follow [semantic versioning](https://semver.org). Release tags are
`trackside-vX.Y.Z`.

## 0.1.0 (unreleased)

The first release of Trackside, a music server for Tracks that you run yourself. The database
starts at schema 7; there is nothing to upgrade from.

- Indexes your music and keeps song ids stable through retags, moves and renames, even across
  a rebuilt database.
- Serves Tracks over a native API with offline-friendly sync, and every other app over
  Subsonic 1.16.1 with OpenSubsonic extensions.
- Accounts with invites, roles and per-library access; a key per device; pairing by QR code,
  typed code or password. Invites carry every permission, including downloads, Subsonic app
  passwords and a bitrate limit.
- Pairing from the app: Tracks can show a 6-digit code to approve in the dashboard.
- Linking a TV: a TV that cannot scan a QR code shows a short code (and a QR for a phone).
  Approve it in Tracks on a phone or Mac, or in the dashboard under My account, Connect a TV,
  which also opens from the link the TV shows. The TV gets its own key under your account,
  never with more access than the phone or session that approved it, and appears in your
  devices. Codes last 10 minutes, are single use, and lock after 5 wrong tries.
- Every address proves it is your server before Tracks sends it a key, code or password
  (`/api/v1/ping` with a challenge). Tracks uses plain HTTP only for addresses on your own
  network or tailnet. A reverse proxy must pass `/api/v1/ping` through with its query string.
- A web dashboard built in, with charts for listening, playback, free space and requests.
  Sensitive changes ask for your password again after 10 minutes. Over HTTPS the plain-HTTP
  session cookie is ignored.
- Invite and reset links use your public address when one is set, and never trust a bare
  `Host` header.
- Limits per person: 8 streams and 3 conversions at once, and 8 uploads in progress. Phones
  that ask for the same conversion share it, so a household downloading one album for offline
  doesn't hit the limit. Uploads
  stop when free space runs low or the permission is taken away, and abandoned ones are
  cleaned up after a day.
- Members see only what their libraries allow everywhere: in other people's public playlists,
  artwork, and conversion links. Changing someone's libraries or role takes effect on their
  open apps at once.
- A proxy that sends forwarding headers but isn't in `server.trusted_proxies` is logged once
  and noted in `/readyz`. Metrics are labelled by route, never by id.
- `trackside config show` prints one `key = value  # source` line per setting.
- Converts songs with ffmpeg for phones on cellular and for formats Apple devices don't play.
- Nightly backups, and a backup before every database upgrade.
- Images for amd64 and arm64 (`latest`, `slim`, `acquire`), static Linux binaries, `.deb` and
  `.rpm` packages with a hardened systemd unit, and templates for Unraid, TrueNAS, Synology and
  CasaOS. Linux and Docker only; macOS isn't supported yet.
- `latest` is the newest release. To run `main`, `compose.build.yaml` builds the image from
  source on your machine; no images are published between releases. Release images are
  signed by `images.yml` and carry SBOM and provenance attestations; release binaries and
  packages carry build provenance.
- Speakers through Music Assistant (optional, off until you connect one under Speakers or
  `music_assistant.*`): the speakers Music Assistant reaches (Sonos, AirPlay, Chromecast, DLNA
  and more) appear in Tracks with play, pause, volume, grouping and power, and update live.
  Trackside adds itself to Music Assistant as a source, with its own account and a key that can
  only browse and stream, so your library plays on every speaker. The Music Assistant token is
  sealed at rest and never shown again. Two new permissions per person, on by default: use
  speakers, and play music services. `compose.music-assistant.yaml` runs Music Assistant beside
  Trackside. See [music-assistant.md](docs/ops/music-assistant.md).
- Optional acquisition through yt-dlp, off unless an admin turns it on. Playing a song the
  server doesn't have streams it through yt-dlp without keeping it; only Get adds a song to the
  library.
- **Action needed if you ran a pre-release with Soulseek:** Soulseek through slskd is no longer
  supported. Take the `slskd` service out of your compose setup (the new
  `compose.acquire.yaml` has none) and delete the `slskd` folder and the `SLSKD_*` and
  `SOULSEEK_*` lines in `.env` if you like. The `acquire.soulseek.*` settings, in the file,
  the environment or `_FILE` secrets, are ignored with one warning each at start-up, except
  `acquire.soulseek.min_bitrate`, which now sets the new `acquire.min_bitrate` unless that is
  set. Past requests keep their history, a file Soulseek had already delivered is still
  added, and a request that was still downloading from Soulseek fails as "no source
  reachable"; ask for the song again to get it through yt-dlp.
- Instant play keeps playing as the song lands in the library: a stream opened during the import
  waits for it and moves on to the library song instead of failing, a request for a song that
  is already downloading plays that download, and media tokens (`mt`) work on instant play
  streams and on songs played without being kept.
- Suggestions from Last.fm with one server-wide API key, set in the dashboard under Settings,
  Last.fm (with a Test button) or in the config: similar songs and artists, top songs and artist
  bios for every user, ranked by each user's own listens and stars, with no Last.fm account
  needed. Only public read calls with artist, track and album names are made, at most a few a
  second, and answers are kept for 30 days. Users who can request songs also see suggestions
  the server doesn't have yet, with their cover once it has been looked up; covers come only
  from Last.fm's image hosts, are checked and kept in the artwork cache, and apps load them
  from your server. Suggestions never wait for Last.fm, and the shelves (picks, Get More, the
  mixes, top songs, similar songs and artists) are kept between requests until something they
  read changes, so they answer in milliseconds on a large library.
- Apple Music for Tracks on Android: save your MusicKit key in the dashboard under Settings,
  Apple Music, and your server signs short-lived developer tokens for your apps, so the key never
  leaves it. See [configuration](docs/ops/configuration.md) for getting a key from Apple.
- Trackside Cloud: each person's Tracks data (listening history, sessions, stations, playlists
  and settings) syncs through your own server, kept apart per account and included in the
  nightly backups. The secret nearby devices share is stored encrypted by the apps, so neither
  the server nor its admin can read it. After a restore, including a Litestream one, the apps
  pull their data again instead of missing changes; delete `trackside.db.cloud-instance` after
  restoring a whole data folder or a Litestream copy (see [backups](docs/ops/backups.md)).
- Passwords, keys and the dashboard's plain-HTTP sign-in work over plain `http://` only from
  your own network: private and link-local addresses, Tailscale, and networks you add in
  `server.local_networks`. Anywhere else the server answers "Use the server's https address"
  and lists its HTTPS addresses so the app can switch. If your provider uses Tailscale's
  `100.64.0.0/10` range for carrier-grade NAT, set `server.tailscale_is_local = false`.
- **Action needed if you use a reverse proxy:** list it in `server.trusted_proxies`. A proxy
  that isn't listed is no longer believed about anything, so signing in through it over plain
  HTTP is refused, even from home. When a proxy sends `X-Forwarded-For`, a `Forwarded` header
  beside it is ignored. [Remote access](docs/ops/remote-access.md#plain-http) explains each case.
- Paired devices pick up new addresses by themselves: add one under **Settings** and open apps
  learn it within seconds, with no new pairing.
- Songs played without being kept are held for quick replays, up to 500 songs and 20 GiB by
  default (**Settings**, `acquire.fetched_keep` and `acquire.fetched_max_bytes`).
- A library on a network share that hangs answers "library unavailable" at once instead of
  slowing the whole server, and a share that briefly fails to list a file no longer marks its
  songs missing.
- At most 16 live event connections per person.
- In Subsonic apps, Now Playing shows members only their own listening; admins see everyone's.
- Backups and the backups folder are readable by the server's user only.
- Invite links show who sent the invite and their note on who it is for, so an admin shouldn't
  put anything in that note they wouldn't tell the person invited.
- Images are built on Debian 13 distroless bases with ffmpeg 9.1. Building from source uses
  Rust 1.100, picked by `rust-toolchain.toml`.
