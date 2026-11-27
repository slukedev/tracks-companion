# Acquisition

Acquisition lets people on your server ask for songs they don't have yet. Trackside looks for
each one through yt-dlp once an admin has turned it on, downloads the best match, checks it is
really the song and really audio, tags it, and files it into a library you choose. The person
who asked sees it arrive in Tracks with its final id, usually within a minute.

It's off until an admin turns it on, and nothing in Tracks mentions it unless the server
offers it to that person.

The provider is **yt-dlp**, which finds the song on YouTube and saves its audio track as it
is, without converting it.

There is no provider for streaming services and there won't be one.

You are responsible for what you download under the laws where you live.

## What you need

- The `acquire` image (`ghcr.io/slukedev/trackside:acquire`), which adds yt-dlp and Deno
  (yt-dlp needs a JavaScript runtime for YouTube). Outside Docker, install
  [yt-dlp](https://github.com/yt-dlp/yt-dlp#installation) and
  [Deno](https://docs.deno.com/runtime/getting_started/installation/) yourself.
- A writable library for acquired songs, separate from your own music so your own folders stay
  read-only.

## Setting it up with Docker Compose

[`compose.acquire.yaml`](../../packaging/compose.acquire.yaml) layers on top of the normal
`compose.yaml`. It switches to the `acquire` image and adds an `acquired` folder as the
writable library.

1. Download it next to `compose.yaml` and make the folder, owned by the user the container
   runs as:

   ```sh
   curl -fsSLO https://raw.githubusercontent.com/slukedev/tracks-companion/main/packaging/compose.acquire.yaml
   mkdir -p acquired
   sudo chown -R 65532:65532 acquired
   ```

2. Use both files on every command:

   ```sh
   echo "COMPOSE_FILE=compose.yaml:compose.acquire.yaml" >> .env
   ```

3. Start it:

   ```sh
   docker compose up -d
   docker compose ps
   ```

4. In the dashboard, **Libraries**, **Add Library**: name it "Acquired", folder `/acquired`,
   and mark it writable.

5. On the **Requests** page, choose that library, turn on yt-dlp, and turn acquisition on.
   The page shows whether yt-dlp is healthy.

6. Decide who may ask: admins always can; for members it's a per-person permission on the
   **Users** page.

## "Not set up"

The **Requests** page shows yt-dlp as **Not set up** when this install is missing what it
needs, and **Not reachable** when it is set up but failing.

"Needs the acquire image" means the default `standard` image can't run yt-dlp, and **Check for
Update** can't fix that. Switch to the `acquire` image with `compose.acquire.yaml` as above.

## yt-dlp

### Keeping it current

YouTube changes often, and yt-dlp releases follow. The image pins a version that was tested
with it. When the copy in use is more than 60 days old (`acquire.ytdlp.max_age_days`), the
**Requests** page marks it outdated and offers **Update to <version>**. That downloads the newest official
release from yt-dlp's GitHub releases into `<data>/bin/yt-dlp`, checks it against the release's
`SHA2-256SUMS`, and uses it instead of the image's copy from then on, as long as it runs here.
The server needs to reach github.com for this.

To go back to the image's copy, delete `<data>/bin/yt-dlp`.

### Bot checks and cookies

When YouTube asks yt-dlp to prove it isn't a bot, or answers with 429 or 403, Trackside stops
using yt-dlp for 15 minutes, then 30, and so on up to 6 hours, and the dashboard says so.
Requests wait and try again meanwhile. Trackside doesn't try to get around these checks.

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
| `acquire.min_bitrate` | 256 | lossy files below this many kbps are skipped |
| `acquire.instant_play` | false | let yt-dlp results play while they download |

Admins aren't limited per person. Asking for a song that's already in the library, or that
someone else already asked for, doesn't count.

## Playing songs you don't have

Playing a song the server doesn't have, such as a suggestion or a search result you haven't
got, streams it through yt-dlp without keeping it: nothing is added to a library and it doesn't
count as a request. Only Get keeps a song.

The server holds on to those songs in `<data>/cache/external/` so a replay starts at once, even
after a restart. It keeps the 500 most recently played, up to 20 GiB; change both in the
dashboard under **Settings** (`acquire.fetched_keep`, `acquire.fetched_max_bytes`). When either
limit is passed, the songs played longest ago go first. A song someone is listening to, or
played in the last half hour, is never removed, so the folder can briefly go over. The
dashboard's overview shows how much they take. If `<data>` is on a network share that goes
away, nothing is removed until it is back.

## What gets checked

Every file is checked before it reaches a library: it must be a regular file inside the
staging folder, its first bytes must be one of FLAC, MP3, AAC or ALAC in MP4 without video, Ogg,
WAV or AIFF, its length must match the song, and ffmpeg must decode it end to end without
errors. Anything else is deleted. File names come from the tags, never from the uploader.

Downloads in progress live in `<data>/acquire/staging/`, and leftovers are cleaned up within a
day. A restart picks up where each request was.

## Turning it off

Turn acquisition off on the **Requests** page, or set `TRACKSIDE_ACQUIRE__ENABLED=false`.
Requests in progress stop and Tracks hides the feature. Songs already acquired stay in their
library. To go back to the standard image, take `compose.acquire.yaml` out of `COMPOSE_FILE`
and run `docker compose up -d`.
