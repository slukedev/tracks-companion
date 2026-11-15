# Trackside changelog

What changes for people running the server. Each release lists anything you need to do when
upgrading, and says when the database changes (the server backs it up first; see
[upgrading](docs/ops/upgrading.md)).

Versions follow [semantic versioning](https://semver.org). Release tags are
`trackside-vX.Y.Z`.

## 0.1.0 (unreleased)

The first release of Trackside, a music server for Tracks that you run yourself. The database
starts at schema 3; there is nothing to upgrade from.

- Indexes your music and keeps song ids stable through retags, moves and renames, even across
  a rebuilt database.
- Serves Tracks over a native API with offline-friendly sync, and every other app over
  Subsonic 1.16.1 with OpenSubsonic extensions.
- Accounts with invites, roles and per-library access; a key per device; pairing by QR code,
  typed code or password. Invites carry every permission, including downloads, Subsonic app
  passwords and a bitrate limit.
- Pairing from the app: Tracks can show a 6-digit code to approve in the dashboard.
- Every address proves it is your server before Tracks sends it a key, code or password
  (`/api/v1/ping` with a challenge). Tracks uses plain HTTP only for addresses on your own
  network or tailnet. A reverse proxy must pass `/api/v1/ping` through with its query string.
- A web dashboard built in, with charts for listening, playback, free space and requests.
  Sensitive changes ask for your password again after 10 minutes. Over HTTPS the plain-HTTP
  session cookie is ignored.
- Invite and reset links use your public address when one is set, and never trust a bare
  `Host` header.
- Limits per person: 8 streams and 3 conversions at once, and 8 uploads in progress. Uploads
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
- `latest` is the newest release; `edge` is built from every commit to `main` and may break.
  Images are signed by the workflow that built them (`ci.yml` for edge, `images.yml` for
  releases) and carry SBOM and provenance attestations; release binaries and packages carry
  build provenance.
- Optional acquisition through yt-dlp and slskd, off unless an admin turns it on.
- Suggestions from Last.fm with one server-wide API key, set in the dashboard under Settings,
  Last.fm (with a Test button) or in the config: similar songs and artists, top songs and artist
  bios for every user, ranked by each user's own listens and stars, with no Last.fm account
  needed. Only public read calls with artist and track names are made, at most a few a second,
  and answers are kept for 30 days. Users who can request songs also see suggestions the
  server doesn't have yet.
