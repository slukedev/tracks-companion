# Acquisition

Acquisition lets people on your server ask for songs they don't have yet. Trackside looks for
each one through the providers an admin has turned on, downloads the best match, checks it is
really the song and really audio, tags it, and files it into a library you choose. The person
who asked sees it arrive in Tracks with its final id, usually within a minute.

It's off until an admin turns it on, and nothing in Tracks mentions it unless the server
offers it to that person.

There are two providers:

- **yt-dlp** finds the song on YouTube and saves its audio track as it is, without
  converting it.
- **Soulseek**, through an [slskd](https://github.com/slskd/slskd) instance you run next to
  Trackside. Trackside talks only to slskd's API; slskd does the peer-to-peer part.

There is no provider for streaming services and there won't be one.

You are responsible for what you download and share under the laws where you live.

## What you need

- The `acquire` image (`ghcr.io/slukedev/trackside:acquire`), which adds yt-dlp and Deno
  (yt-dlp needs a JavaScript runtime for YouTube). Outside Docker, install
  [yt-dlp](https://github.com/yt-dlp/yt-dlp#installation) and
  [Deno](https://docs.deno.com/runtime/getting_started/installation/) yourself.
- A writable library for acquired songs, separate from your own music so your own folders stay
  read-only.
- For Soulseek, an slskd instance and a Soulseek account. slskd creates the account the first
  time it signs in, if the name is free.

## Setting it up with Docker Compose

[`compose.acquire.yaml`](../../packaging/compose.acquire.yaml) layers on top of the normal
`compose.yaml`. It switches to the `acquire` image, adds an `acquired` folder as the writable
library, and runs slskd with that folder as its only share.

1. Download it next to `compose.yaml` and make the folders, owned by the user both containers
   run as:

   ```sh
   curl -fsSLO https://raw.githubusercontent.com/slukedev/tracks/main/Server/packaging/compose.acquire.yaml
   mkdir -p acquired slskd/app slskd/downloads
   sudo chown -R 65532:65532 acquired slskd
   ```

2. Add the settings to `.env`:

   ```sh
   echo "SLSKD_API_KEY=$(openssl rand -hex 32)" >> .env
   cat >> .env <<'EOF'
   SOULSEEK_USERNAME=choose-a-name
   SOULSEEK_PASSWORD=choose-a-password
   SLSKD_WEB_PASSWORD=another-password
   COMPOSE_FILE=compose.yaml:compose.acquire.yaml
   EOF
   ```

3. Start both:

   ```sh
   docker compose up -d
   docker compose ps
   ```

4. In the dashboard, **Libraries**, **Add Library**: name it "Acquired", folder `/acquired`,
   and mark it writable.

5. On the **Requests** page, choose that library, turn on the providers you want, and turn
   acquisition on. The page shows whether each provider is healthy.

6. Decide who may ask: admins always can; for members it's a per-person permission on the
   **Users** page.

slskd's own web page is at `http://localhost:5030` on the server (sign in as `slskd` with
`SLSKD_WEB_PASSWORD`). It's published on localhost only. Use it to check that slskd is
connected to Soulseek and what it's sharing.

## What slskd shares

slskd shares the folders it's told to, and Soulseek works on the understanding that people
share. The shipped compose file gives slskd exactly one share: the `acquired` folder, mounted
read-only. Your own music isn't mounted into slskd at all, so it can't be shared by mistake.

Keep it that way. If you'd rather share nothing, slskd still downloads, but some peers will
refuse to upload to someone who shares nothing.

To check, open slskd's web page and look under Shares, or:

```sh
curl -s -H "X-API-Key: $SLSKD_API_KEY" http://localhost:5030/api/v0/shares
```

It should list one share, `/share`.

Port 50300 is published for incoming peer connections. Forwarding it on your router is optional
and only makes more transfers succeed.

## yt-dlp

### Keeping it current

YouTube changes often, and yt-dlp releases follow. The image pins a version that was tested
with it. When the copy in use is more than 60 days old (`acquire.ytdlp.max_age_days`), the
**Requests** page marks it outdated and offers **Update to <version>**. That downloads the newest official
release from yt-dlp's GitHub releases into `<data>/bin/yt-dlp`, checks it against the release's
`SHA2-256SUMS`, and uses it instead of the image's copy from then on. The server needs to reach
github.com for this.

To go back to the image's copy, delete `<data>/bin/yt-dlp`.

### Bot checks and cookies

When YouTube asks yt-dlp to prove it isn't a bot, or answers with 429 or 403, Trackside stops
using yt-dlp for 15 minutes, then 30, and so on up to 6 hours, and the dashboard says so.
Requests use the other provider meanwhile. Trackside doesn't try to get around these checks.

If it keeps happening, give yt-dlp the cookies of a signed-in browser session. Export them in
Netscape format (yt-dlp's
[FAQ](https://github.com/yt-dlp/yt-dlp/wiki/FAQ#how-do-i-pass-cookies-to-yt-dlp) explains how),
use an account you don't mind being rate limited, and keep the file private:

```sh
cp ~/Downloads/cookies.txt ./trackside-cookies.txt
sudo chown 65532:65532 trackside-cookies.txt
chmod 600 trackside-cookies.txt
```

```yaml
services:
  trackside:
    volumes:
      - ./trackside-cookies.txt:/run/secrets/yt-cookies.txt:ro
    environment:
      TRACKSIDE_ACQUIRE__YTDLP__COOKIES_FILE: /run/secrets/yt-cookies.txt
```

## Limits

| Setting | Default | |
|---|---|---|
| `acquire.member_daily` | 50 | requests per member per day |
| `acquire.member_active` | 5 | requests per member in progress at once |
| `acquire.global_daily` | 500 | requests for the whole server per day |
| `acquire.max_file_bytes` | `"1GiB"` | larger files are skipped |
| `acquire.instant_play` | false | let yt-dlp results play while they download |

Admins aren't limited per person. Asking for a song that's already in the library, or that
someone else already asked for, doesn't count.

## What gets checked

Every file is checked before it reaches a library: it must be a regular file inside the
staging folder, its first bytes must be one of FLAC, MP3, AAC or ALAC in MP4 without video, Ogg,
WAV or AIFF, its length must match the song, and ffmpeg must decode it end to end without
errors. Anything else is deleted. File names come from the tags, never from the uploader or
peer.

Downloads in progress live in `<data>/acquire/staging/`, and leftovers are cleaned up within a
day. A restart picks up where each request was.

## Turning it off

Turn acquisition off on the **Requests** page, or set `TRACKSIDE_ACQUIRE__ENABLED=false`.
Requests in progress stop and Tracks hides the feature. Songs already acquired stay in their
library. To remove slskd too, take `compose.acquire.yaml` out of `COMPOSE_FILE` and run
`docker compose up -d --remove-orphans`.
