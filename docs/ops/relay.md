# Phone playback relay

When someone plays a streaming service (Spotify, Tidal, Qobuz and the others Music Assistant
connects) on their phone, Trackside joins Music Assistant as a player for that phone and passes
the audio on as it plays. This page is what a self-hoster needs to know about it.

## What it needs

- Music Assistant 2.10 or later, connected in the dashboard (Speakers). Trackside reaches its
  Sendspin server through Music Assistant's own web port (`/sendspin`), with the same token, so
  no extra port has to be open. The Sendspin provider must be enabled in Music Assistant (it is
  by default).
- For the cellular tier (AAC), ffmpeg. The `standard` and `acquire` images have it; on `slim`
  phones get the lossless tier only.
- Nothing else. The relay needs no pairing: Music Assistant accepts it as a guest player because
  it only asks to play.

## What it costs

- Network between Trackside and Music Assistant: FLAC, about 0.7 to 1.1 Mbit/s per phone.
- To the phone: the same FLAC on Wi-Fi, or AAC at 256 kbit/s on cellular.
- CPU: almost none for lossless (the audio is repackaged, not decoded); one ffmpeg process per
  cellular listener, roughly 1 to 3 % of a core.
- Memory: under 3 MiB of audio per listener. At most four listeners at once by default
  (`relay_max_sessions` in the Speakers settings), three per person
  (`music_assistant.relay_max_per_user` in the configuration), one per phone. Each
  listener is a separate stream inside Music Assistant, and services limit how many streams one
  account may run at a time.

## What it never does

The relay is real-time only. Audio is held in memory for a few seconds and dropped; it is never
written to disk, cached, added to the library or offered for download. Playlist and segment
responses carry `Cache-Control: no-store`.

## How it behaves

- Phones hear the music about four seconds after Music Assistant sends it. Pause, skip and seek
  feel instant because the app acts first and the relay follows.
- In Music Assistant each phone appears as a hidden player named after the phone. It keeps the
  same player (and its queue) across sessions.
- If nobody fetches audio for 30 seconds while it plays (the phone went out of range, or the app
  was killed), Trackside pauses that player in Music Assistant. After ten minutes paused the
  relay leaves Music Assistant until the phone plays again.
- If the connection to Music Assistant drops, Trackside reconnects and resumes playback. If
  Music Assistant stays away for 30 seconds, or its address or token changes, the phones'
  relays end and the apps fall back to the calm "not available" state.
- Each phone is only visible to its owner: other people never see it among the speakers or
  in Music Assistant groups through Trackside.
- When the server stops, every relay says goodbye to Music Assistant first.
- Removing or revoking a device in Tracks or the dashboard ends its relay and deletes its
  player key; Music Assistant forgets that player a few minutes later.
- Only the phone that started a relay can fetch its audio: playlists and segments need that
  device's media token (`mt=`), which the server log never shows.

## When it doesn't work

| Symptom | Likely cause |
|---|---|
| Phones can't play services; the dashboard says the relay is unsupported | Music Assistant older than 2.10, or its Sendspin provider is disabled |
| The relay session ends with `failed` right away | the Music Assistant token was refused; set a new one in Speakers |
| It ends with `failed` after 30 seconds | Music Assistant did not activate the player; its log (Sendspin provider) says why. Guest players that only play are normally approved automatically |
| Cellular quality is missing | ffmpeg is not installed (the `slim` image) |
| "Too many listeners" | the server limit is reached; raise `relay_max_sessions` if the machine and your services allow it, or `music_assistant.relay_max_per_user` for one person's devices |
| The phone's music stops after 30 seconds in the background | the app stopped fetching audio, so Trackside paused Music Assistant; it resumes when the app plays again |
| Phones can't relay with an `https` Music Assistant behind a private CA | the relay trusts the system certificate store only; add the CA to the system store |

Tokens and player keys never appear in the server log.
