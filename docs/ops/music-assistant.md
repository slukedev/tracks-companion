# Speakers and streaming services with Music Assistant

[Music Assistant](https://music-assistant.io) plays music on the speakers in your home (Sonos,
AirPlay, Chromecast, DLNA, Snapcast, squeezelite and more) and signs in to streaming services
with your own accounts (Spotify, Tidal, Qobuz, Deezer and others). Connected to Trackside, it
gives Tracks:

- your speakers in Devices, with play, pause, volume, grouping and power;
- your Trackside library on every speaker, with full metadata;
- your streaming services in search and on speakers, and on phones through the relay
  ([relay.md](relay.md)).

The Tracks apps never talk to Music Assistant. They talk to Trackside, which holds the one
connection, so Trackside's accounts, permissions and limits apply to everything.

Nothing from a streaming service is ever downloaded, stored or exported: speakers play it in
Music Assistant, phones get it in real time from memory.

## Run Music Assistant beside Trackside

Music Assistant needs the host's network to find speakers and to stream to AirPlay, Cast,
Sonos and DLNA. With Docker Compose, layer `packaging/compose.music-assistant.yaml` on
Trackside's `compose.yaml`; it runs Music Assistant on the host network and moves Trackside
there too:

```yaml
services:
  music-assistant:
    image: ghcr.io/music-assistant/server:2.10.13@sha256:7510aaa7ee5fc85b34fd0de17992bd14473895b23ce3eb9b310092743f4dbb84
    container_name: music-assistant
    restart: unless-stopped
    network_mode: host
    volumes:
      - music-assistant-data:/data

  trackside:
    ports: !reset []
    network_mode: host

volumes:
  music-assistant-data:
```

```sh
docker compose -f compose.yaml -f compose.music-assistant.yaml up -d
```

Music Assistant is then at `http://<this machine>:8095`. If you run it elsewhere (another
machine, the Home Assistant add-on), that works too: Trackside only needs to reach its web port,
and Music Assistant needs to reach Trackside's.

Trackside is tested with Music Assistant 2.10 (stable) and works with 2.11 (beta). Older
versions are refused with `incompatible`.

## Connect Trackside

1. In Music Assistant, create the first user if you have not (it is an admin), then go to
   Settings, Profile, Long-lived access tokens and create one named `Trackside`. Copy it.
2. In Trackside's dashboard, open Speakers. Enter Music Assistant's address
   (`http://127.0.0.1:8095` when both run on the host network) and paste the token. Use Test
   first if you like: it connects once and says what it found.
3. Save. The status turns `connected` within a few seconds, and your speakers appear in Tracks.

Or set it in the configuration, which makes those fields read-only in the dashboard:

```toml
[music_assistant]
url = "http://127.0.0.1:8095"
# token = "..."  better through TRACKSIDE_MUSIC_ASSISTANT__TOKEN_FILE
```

```sh
TRACKSIDE_MUSIC_ASSISTANT__URL=http://127.0.0.1:8095
TRACKSIDE_MUSIC_ASSISTANT__TOKEN_FILE=/run/secrets/ma_token
```

The token is sealed with the server's secret key in the database (or read from the file), is
never shown again, and never appears in logs or error messages.

**HTTPS with your own certificate.** If Music Assistant is behind HTTPS with a private CA or a
self-signed certificate, paste the CA certificate (PEM) under Speakers, or set
`music_assistant.ca_file`. Trackside then trusts only that CA for Music Assistant.

## Your library on speakers

With an admin token, Trackside adds itself to Music Assistant as an OpenSubsonic source
(Settings, Providers, "Trackside (your server's name)"), so library songs play on any speaker.
It does this on its own the first time it connects (`auto_register`), and repairs it when Music
Assistant loses it. Speakers, Registration shows the state.

- It uses a service account, `music-assistant`, which you will see under People. It has no
  password and cannot sign in. Its key is scoped `library stream`: Music Assistant can browse
  and play, nothing else (no stars, playlists, scrobbles or downloads), and only over Subsonic.
  The key is shown under Keys as managed by Music Assistant; revoking it there stops speakers
  playing the library until you register again.
- Which libraries Music Assistant sees is the service account's library access, all libraries
  at first. Change it under People like anyone else's.
- Music Assistant reaches Trackside at the address under Speakers, LAN address: the first LAN
  address Trackside found, unless you set one. A `localhost` address only works when Music
  Assistant runs on the same machine; the dashboard warns when it does not.
- Register again rotates the key. Unregister removes the source and revokes the key.

## Spotify

Set Spotify up in Music Assistant (Settings, Providers, Spotify): it needs a Premium account.
New setups use the Soloist playback backend, which needs your own Spotify developer API key and
a pairing; Music Assistant's setup walks you through both. What changes in Tracks:

- **One stream.** With Soloist, Spotify plays on one player at a time per account. Playing it
  somewhere new (a speaker, a phone) stops it where it was, as Spotify's own apps do. Playing
  in the Spotify app itself also takes the stream away from Music Assistant.
- **Visible in Spotify.** While Music Assistant plays, your Spotify apps show a device called
  "Music Assistant Playback"; pausing it there pauses the speaker or phone.
- **Quality.** Tracks shows Spotify as lossless only when Soloist's audio quality is set to
  lossless, which Trackside can only read with an admin Music Assistant token; otherwise high.
- **Saving.** The heart on a Spotify song saves it to Liked Songs (and albums, artists and
  playlists to your library), through Music Assistant. Keep Music Assistant's library sync-back
  on for Spotify, or the change stays in Music Assistant.
- **Radio and mixes.** Spotify radio uses Music Assistant's radio mode, which relies on a
  Spotify endpoint new apps can no longer use, so it may stop working; Tracks then plays its
  own radio. Your Discover Weekly, Release Radar and Daily Mixes show as Made for You once
  Music Assistant has synced your saved playlists.
- **Spotify Connect** is a separate Music Assistant plugin that makes speakers targets for the
  Spotify app. Trackside does not use it; a speaker playing from the Spotify app shows what
  plays, without Tracks' queue controls.
- A lost pairing shows Spotify as signed out; a revoked one only shows as songs failing to
  start. Pair again in Music Assistant.

## Who can use what

Two permissions per person, both on by default (People): **Use speakers** (see and control
speakers) and **Play music services** (search and play the streaming services, here and on
speakers). Admins have both. API keys without the `write` scope can see speakers but not
control them.

## When it doesn't work

| Symptom | Likely cause |
|---|---|
| `unreachable` | Music Assistant is not running, or the address is wrong. From the Trackside container, `http://<address>/info` must answer |
| `auth_failed` | the token was revoked or mistyped; create a new long-lived token |
| `setup_required` | Music Assistant has no user yet; open it once and create one |
| `incompatible` | Music Assistant is older than 2.10; upgrade it |
| Registration `needs_admin` | the token belongs to a non-admin Music Assistant user; use an admin's token, or add the OpenSubsonic source by hand |
| Registration `failed`, LAN address not reachable | Music Assistant cannot reach Trackside at the LAN address; set it under Speakers to an address Music Assistant can open |
| Speakers missing | Music Assistant has not found them, or they are hidden or disabled there; check its Players settings. Speakers only Music Assistant's protocol layer uses are never listed |
| Library songs do not play on speakers | the registration is not `registered`, or the service account cannot see that library |
| Everything says "Speakers aren't available right now" | Trackside lost Music Assistant for more than 30 seconds; it reconnects by itself and speakers come back |

Trackside logs the connection, registration and refused commands at `info` and `warn`; the token
and keys never appear in them.
