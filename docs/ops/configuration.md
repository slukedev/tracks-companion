# Configuration reference

Trackside runs with no configuration at all. Everything below is optional.

## Where settings come from

Later sources win:

1. built-in defaults;
2. the TOML file: `<data>/config.toml` when it exists, or the file named by `--config` or
   `TRACKSIDE_CONFIG`;
3. environment variables;
4. the dashboard, for settings marked **D**, but only where neither the file nor the
   environment sets them.

The command line has two options, both also usable as environment variables:

| Option | Environment | Meaning |
|---|---|---|
| `--config <file>` | `TRACKSIDE_CONFIG` | the TOML file to read |
| `--data-dir <dir>` | `TRACKSIDE_DATA_DIR` | the data folder; the other paths default to folders inside it |

Environment names are `TRACKSIDE_` and the key in capitals with `__` between levels:
`scanner.quick_interval` is `TRACKSIDE_SCANNER__QUICK_INTERVAL`. The exceptions are
`TRACKSIDE_DATA_DIR`, `TRACKSIDE_MUSIC_DIR`, `TRACKSIDE_CACHE_DIR`, `TRACKSIDE_ADMIN_USERNAME`
and `TRACKSIDE_ADMIN_PASSWORD`. The three keys that hold a secret (`auth.admin_password`,
`enrichment.lastfm_api_key` and `music_assistant.token`) also accept `<NAME>_FILE`, a path to read the value from, which is
how Docker secrets work. If both are set, the plain variable wins and the log warns.

Value formats, in the file and the environment:

| Kind | Examples |
|---|---|
| duration | `"500ms"`, `"30s"`, `"15m"`, `"6h"`, `"30d"`, or a number of seconds. `scanner.quick_interval` and `scanner.full_interval` also take `"off"` |
| size | `"512MiB"`, `"10GiB"`, `"1GB"`, or a number of bytes (`KiB`, `MiB`, `GiB`, `TiB` are powers of 1024; `KB`, `MB`, `GB`, `TB` of 1000) |
| rate | `"10/1m"`: that many events per duration |
| true or false | in the environment also `1`/`0`, `yes`/`no`, `on`/`off` |
| list | in the environment, comma-separated, or space-separated when there is no comma. `acquire.reject_words` is comma-separated only, because its words may hold spaces |

Settings marked **D** can also be changed in the dashboard. If the file or the environment
sets one, the dashboard shows it as set by config and won't change it.

To see what's in effect (secrets show as `<redacted>`):

```sh
trackside config show
docker compose exec trackside trackside config show
```

It prints one line per key, in the order of the tables below: the effective value as TOML and,
after `#`, where it came from (`default`, `file`, `environment` or `command line`). Warnings
follow, each starting `# warning:`. For example, with `name = "Home"` in the file,
`TRACKSIDE_BACKUP__KEEP=14` and `TRACKSIDE_ADMIN_PASSWORD` set, and a misspelt variable:

```
data_dir = "/data"  # command line
music_dir = unset  # default
server.name = "Home"  # file
server.listen = ["[::]:4747"]  # default
server.request_timeout = "1m"  # default
...
auth.admin_password = <redacted>  # environment
...
backup.keep = 14  # environment
...
enrichment.lastfm_api_key = unset  # default
# warning: unknown environment variable TRACKSIDE_BOGUS is ignored
```

Durations and sizes are shown in the largest whole unit (`"1m"`, `"30d"`, `"10GiB"`).
`--data-dir` shows as `command line`; `TRACKSIDE_DATA_DIR` as `environment`.

An unknown key in the file, or an unknown `TRACKSIDE_*` variable, is a warning (in
`config show` and in the log at start), not an error, so a typo doesn't stop the server; check
the warnings after editing. A value that can't be read, such as `"30 seconds"` for a duration,
is an error naming the key, and the server doesn't start.

A key that an earlier version read is accepted and ignored, with one warning naming it, so an
upgrade doesn't stop the server. The `acquire.soulseek.*` keys of the removed Soulseek provider
are such keys, as are their `TRACKSIDE_ACQUIRE__SOULSEEK__*` variables and
`TRACKSIDE_ACQUIRE__SOULSEEK__API_KEY_FILE`. `acquire.soulseek.min_bitrate` still counts: it
sets `acquire.min_bitrate` when that isn't set.

`trackside check` loads the configuration the same way and also checks the data folder and the
database, without starting the server.

## An example config.toml

```toml
[server]
name = "Home"
public_urls = ["https://music.example.com", "https://trackside.tailnet-name.ts.net"]
trusted_proxies = ["127.0.0.1/32", "::1/128"]

[scanner]
quick_interval = "1h"

[media]
transcode_cache_max = "20GiB"

[backup]
schedule = "daily 04:15"
keep = 14
```

## Every setting

### Top level

| Key | Environment | Default | Meaning |
|---|---|---|---|
| `data_dir` | `TRACKSIDE_DATA_DIR` | `$STATE_DIRECTORY`, else `$XDG_DATA_HOME/trackside`, else `~/.local/share/trackside`; Docker `/data` | database, secret key, backups, caches unless moved |
| `music_dir` | `TRACKSIDE_MUSIC_DIR` | unset (`/music` in the image) | creates the first library at first start only |
| `cache_dir` | `TRACKSIDE_CACHE_DIR` | `<data>/cache` | artwork and transcodes, safe to delete |

### server

| Key | Environment | Default | Meaning |
|---|---|---|---|
| `server.name` **D** | `TRACKSIDE_SERVER__NAME` | host name (`$HOSTNAME` or `/etc/hostname`), else `Trackside` | shown in apps, mDNS and pairing |
| `server.listen` | `TRACKSIDE_SERVER__LISTEN` | `["[::]:4747"]` | addresses; IPv4 `0.0.0.0:4747` is used when IPv6 is unavailable |
| `server.base_path` | `TRACKSIDE_SERVER__BASE_PATH` | `""` | URL prefix behind a proxy, such as `/music`; a missing leading `/` is added and a trailing one removed |
| `server.public_urls` **D** | `TRACKSIDE_SERVER__PUBLIC_URLS` | `[]` | extra candidate URLs for pairing |
| `server.trusted_proxies` | `TRACKSIDE_SERVER__TRUSTED_PROXIES` | `[]` | CIDRs whose forwarding headers are believed |
| `server.local_networks` | `TRACKSIDE_SERVER__LOCAL_NETWORKS` | `[]` | CIDRs that count as your own network besides the private, link-local and Tailscale ranges, such as your LAN's global IPv6 prefix: passwords and keys are accepted from them over plain HTTP |
| `server.tailscale_is_local` | `TRACKSIDE_SERVER__TAILSCALE_IS_LOCAL` | `true` | whether Tailscale's 100.64.0.0/10 counts as your own network; turn it off if your ISP puts your server behind carrier-grade NAT in that range, where it is the internet |
| `server.max_connections` | `TRACKSIDE_SERVER__MAX_CONNECTIONS` | `1024` | open connections at once, across listeners; more are closed at once |
| `server.header_timeout` | `TRACKSIDE_SERVER__HEADER_TIMEOUT` | `"30s"` | time to receive request headers; also how long an idle HTTP/1 keep-alive connection stays open |
| `server.request_timeout` | `TRACKSIDE_SERVER__REQUEST_TIMEOUT` | `"60s"` | non-streaming handlers |
| `server.shutdown_grace` | `TRACKSIDE_SERVER__SHUTDOWN_GRACE` | `"10s"` | drain time on stop |

### tls

| Key | Environment | Default | Meaning |
|---|---|---|---|
| `tls.mode` | `TRACKSIDE_TLS__MODE` | `"off"` | `off`, `files` (needs both files below), `acme` (needs `acme_domains`), `self-signed` |
| `tls.cert_file` | `TRACKSIDE_TLS__CERT_FILE` | unset | PEM chain for `files` |
| `tls.key_file` | `TRACKSIDE_TLS__KEY_FILE` | unset | PEM key for `files` |
| `tls.acme_domains` | `TRACKSIDE_TLS__ACME_DOMAINS` | `[]` | domains for `acme` |
| `tls.acme_email` | `TRACKSIDE_TLS__ACME_EMAIL` | unset | contact for `acme` |
| `tls.acme_directory` | `TRACKSIDE_TLS__ACME_DIRECTORY` | Let's Encrypt production | ACME directory URL |
| `tls.acme_cache` | `TRACKSIDE_TLS__ACME_CACHE` | `<data>/acme` | account and certificates |

### database

| Key | Environment | Default | Meaning |
|---|---|---|---|
| `database.path` | `TRACKSIDE_DATABASE__PATH` | `<data>/trackside.db` | SQLite file |
| `database.readers` | `TRACKSIDE_DATABASE__READERS` | `min(8, cpus)` | reader connections |
| `database.cache` | `TRACKSIDE_DATABASE__CACHE` | `"32MiB"` | page cache per connection |
| `database.mmap` | `TRACKSIDE_DATABASE__MMAP` | `"256MiB"` | mmap size |
| `database.allow_network_fs` | `TRACKSIDE_DATABASE__ALLOW_NETWORK_FS` | `false` | permit the database on NFS or SMB |

### scanner

| Key | Environment | Default | Meaning |
|---|---|---|---|
| `scanner.threads` | `TRACKSIDE_SCANNER__THREADS` | `min(4, cpus)` | tag reader threads |
| `scanner.watch` **D** | `TRACKSIDE_SCANNER__WATCH` | `true` | filesystem events |
| `scanner.watch_debounce` | `TRACKSIDE_SCANNER__WATCH_DEBOUNCE` | `"5s"` | wait after the last event in a folder |
| `scanner.quick_interval` **D** | `TRACKSIDE_SCANNER__QUICK_INTERVAL` | `"30m"` | periodic quick scan |
| `scanner.full_interval` **D** | `TRACKSIDE_SCANNER__FULL_INTERVAL` | `"off"` | periodic full scan that rereads every tag |
| `scanner.settle` | `TRACKSIDE_SCANNER__SETTLE` | `"10s"` | files modified more recently are deferred |
| `scanner.follow_symlinks` | `TRACKSIDE_SCANNER__FOLLOW_SYMLINKS` | `true` | loops are detected by device and inode |
| `scanner.missing_grace` **D** | `TRACKSIDE_SCANNER__MISSING_GRACE` | `"30d"` | before missing songs are purged |
| `scanner.empty_guard` | `TRACKSIDE_SCANNER__EMPTY_GUARD` | `0.5` | abort if fewer than this share of known files are found |
| `scanner.file_timeout` | `TRACKSIDE_SCANNER__FILE_TIMEOUT` | `"30s"` | per-file tag read before quarantine |
| `scanner.ignore` | `TRACKSIDE_SCANNER__IGNORE` | `[".trackside-ignore", ".nomedia", ".ndignore"]` | marker files that skip a folder |
| `scanner.extensions` | `TRACKSIDE_SCANNER__EXTENSIONS` | `flac m4a mp4 alac mp3 aac ogg oga opus wav aif aiff wv ape` | files to read |

### media

| Key | Environment | Default | Meaning |
|---|---|---|---|
| `media.ffmpeg_path` | `TRACKSIDE_MEDIA__FFMPEG_PATH` | `ffmpeg` on PATH | encoder |
| `media.transcode_max_concurrent` | `TRACKSIDE_MEDIA__TRANSCODE_MAX_CONCURRENT` | `max(1, cpus / 2)` | ffmpeg processes |
| `media.transcode_cache_max` **D** | `TRACKSIDE_MEDIA__TRANSCODE_CACHE_MAX` | `"10GiB"` | LRU cap |
| `media.art_cache_max` **D** | `TRACKSIDE_MEDIA__ART_CACHE_MAX` | `"2GiB"` | LRU cap for artwork |
| `media.cache_min_free` | `TRACKSIDE_MEDIA__CACHE_MIN_FREE` | `"1GiB"` | free space kept on the cache volume |
| `media.hls_segment` | `TRACKSIDE_MEDIA__HLS_SEGMENT` | `"6s"` | HLS segment length |
| `media.cellular_profile` **D** | `TRACKSIDE_MEDIA__CELLULAR_PROFILE` | `"aac-256"` | suggested to clients |

### auth

| Key | Environment | Default | Meaning |
|---|---|---|---|
| `auth.secret_key_file` | `TRACKSIDE_AUTH__SECRET_KEY_FILE` | `<data>/secret.key` | 32-byte key |
| `auth.legacy_subsonic` **D** | `TRACKSIDE_AUTH__LEGACY_SUBSONIC` | `true` | allow app passwords at all |
| `auth.session_idle` | `TRACKSIDE_AUTH__SESSION_IDLE` | `"14d"` | |
| `auth.session_max` | `TRACKSIDE_AUTH__SESSION_MAX` | `"90d"` | |
| `auth.media_token_ttl` | `TRACKSIDE_AUTH__MEDIA_TOKEN_TTL` | `"7d"` | |
| `auth.argon2_memory` | `TRACKSIDE_AUTH__ARGON2_MEMORY` | `"19MiB"` | |
| `auth.argon2_iterations` | `TRACKSIDE_AUTH__ARGON2_ITERATIONS` | `2` | |
| `auth.login_rate` | `TRACKSIDE_AUTH__LOGIN_RATE` | `"10/1m"` | per IP |
| `auth.admin_username` | `TRACKSIDE_ADMIN_USERNAME` | unset | create this admin at first start |
| `auth.admin_password` | `TRACKSIDE_ADMIN_PASSWORD` / `_FILE` | unset | its password |

### sync

| Key | Environment | Default | Meaning |
|---|---|---|---|
| `sync.tombstone_retention` | `TRACKSIDE_SYNC__TOMBSTONE_RETENTION` | `"90d"` | |
| `sync.max_page` | `TRACKSIDE_SYNC__MAX_PAGE` | `5000` | records per sync page |

### discovery

| Key | Environment | Default | Meaning |
|---|---|---|---|
| `discovery.mdns` **D** | `TRACKSIDE_DISCOVERY__MDNS` | `true` | |

### backup

| Key | Environment | Default | Meaning |
|---|---|---|---|
| `backup.dir` | `TRACKSIDE_BACKUP__DIR` | `<data>/backups` | |
| `backup.schedule` **D** | `TRACKSIDE_BACKUP__SCHEDULE` | `"daily 03:30"` | `off`, `daily HH:MM`, `weekly <day> HH:MM`, server local time |
| `backup.keep` **D** | `TRACKSIDE_BACKUP__KEEP` | `7` | scheduled backups kept |
| `backup.include_secret_key` | `TRACKSIDE_BACKUP__INCLUDE_SECRET_KEY` | `true` | copy `secret.key` beside each backup |

### log

| Key | Environment | Default | Meaning |
|---|---|---|---|
| `log.level` | `TRACKSIDE_LOG__LEVEL` | `"info"` | tracing filter, such as `info,trackside_scan=debug`. `RUST_LOG`, when set, takes precedence over every other source |
| `log.format` | `TRACKSIDE_LOG__FORMAT` | `pretty` on a terminal, else `json` | `pretty` or `json` |

### metrics

| Key | Environment | Default | Meaning |
|---|---|---|---|
| `metrics.enabled` | `TRACKSIDE_METRICS__ENABLED` | `false` | `/metrics` |
| `metrics.listen` | `TRACKSIDE_METRICS__LISTEN` | unset (main listener) | separate address for `/metrics` |

### uploads

| Key | Environment | Default | Meaning |
|---|---|---|---|
| `uploads.enabled` **D** | `TRACKSIDE_UPLOADS__ENABLED` | `false` | browser and API uploads |
| `uploads.max_file` | `TRACKSIDE_UPLOADS__MAX_FILE` | `"2GiB"` | |
| `uploads.inbox_dir` | `TRACKSIDE_UPLOADS__INBOX_DIR` | unset | watched drop folder imported into the uploads library |
| `uploads.path_template` | `TRACKSIDE_UPLOADS__PATH_TEMPLATE` | `"{album_artist}/{album}/{disc}-{track:02} {title}.{ext}"` | |

### acquire

| Key | Environment | Default | Meaning |
|---|---|---|---|
| `acquire.enabled` **D** | `TRACKSIDE_ACQUIRE__ENABLED` | `false` | master switch |
| `acquire.library` **D** | `TRACKSIDE_ACQUIRE__LIBRARY` | unset | pid or root path of the writable acquired library |
| `acquire.staging_dir` | `TRACKSIDE_ACQUIRE__STAGING_DIR` | `<data>/acquire/staging` | |
| `acquire.prefer` **D** | `TRACKSIDE_ACQUIRE__PREFER` | `["ytdlp"]` | provider order bonus |
| `acquire.search_timeout` | `TRACKSIDE_ACQUIRE__SEARCH_TIMEOUT` | `"8s"` | per provider |
| `acquire.max_file_bytes` | `TRACKSIDE_ACQUIRE__MAX_FILE_BYTES` | `"1GiB"` | |
| `acquire.member_daily` **D** | `TRACKSIDE_ACQUIRE__MEMBER_DAILY` | `50` | |
| `acquire.member_active` **D** | `TRACKSIDE_ACQUIRE__MEMBER_ACTIVE` | `5` | |
| `acquire.global_daily` **D** | `TRACKSIDE_ACQUIRE__GLOBAL_DAILY` | `500` | |
| `acquire.instant_play` **D** | `TRACKSIDE_ACQUIRE__INSTANT_PLAY` | `false` | stream-through for ytdlp candidates |
| `acquire.fetched_keep` **D** | `TRACKSIDE_ACQUIRE__FETCHED_KEEP` | `500` | songs played without being kept that stay for replays |
| `acquire.fetched_max_bytes` **D** | `TRACKSIDE_ACQUIRE__FETCHED_MAX_BYTES` | `"20GiB"` | the most those songs may take |
| `acquire.reject_words` | `TRACKSIDE_ACQUIRE__REJECT_WORDS` | `live`, `remix`, `karaoke`, `instrumental`, `cover`, `nightcore`, `sped up`, `slowed`, `8d`, `reverb`, `acapella`, `tribute` | |
| `acquire.min_bitrate` | `TRACKSIDE_ACQUIRE__MIN_BITRATE` | `256` | kbps below which lossy candidates are skipped |
| `acquire.path_template` | `TRACKSIDE_ACQUIRE__PATH_TEMPLATE` | `"{album_artist}/{album}/{disc}-{track:02} {title}.{ext}"`, or `{artist}/Singles/{title}.{ext}` without an album | |
| `acquire.ytdlp.enabled` **D** | `TRACKSIDE_ACQUIRE__YTDLP__ENABLED` | `false` | |
| `acquire.ytdlp.path` | `TRACKSIDE_ACQUIRE__YTDLP__PATH` | `<data>/bin/yt-dlp`, else `yt-dlp` on PATH | |
| `acquire.ytdlp.js_runtime` | `TRACKSIDE_ACQUIRE__YTDLP__JS_RUNTIME` | `deno` on PATH if present | |
| `acquire.ytdlp.cookies_file` | `TRACKSIDE_ACQUIRE__YTDLP__COOKIES_FILE` | unset | |
| `acquire.ytdlp.concurrency` | `TRACKSIDE_ACQUIRE__YTDLP__CONCURRENCY` | `2` | |
| `acquire.ytdlp.search_rate` | `TRACKSIDE_ACQUIRE__YTDLP__SEARCH_RATE` | `"10/1m"` | |
| `acquire.ytdlp.max_age_days` | `TRACKSIDE_ACQUIRE__YTDLP__MAX_AGE_DAYS` | `60` | outdated warning |

### enrichment

| Key | Environment | Default | Meaning |
|---|---|---|---|
| `enrichment.musicbrainz` **D** | `TRACKSIDE_ENRICHMENT__MUSICBRAINZ` | `false` | tag lookups for acquired files |
| `enrichment.lastfm_api_key` **D** | `TRACKSIDE_ENRICHMENT__LASTFM_API_KEY` / `_FILE` | unset | Last.fm API key for suggestions |

With a Last.fm API key, similar songs, similar artists, top songs and artist biographies also use
Last.fm's data. Get a key at <https://www.last.fm/api/account/create> and paste it in the
dashboard under Settings, Last.fm (Test checks it), or set it here.

### music_assistant

| Key | Environment | Default | Meaning |
|---|---|---|---|
| `music_assistant.url` **D** | `TRACKSIDE_MUSIC_ASSISTANT__URL` | unset | Music Assistant's web address, such as `http://192.168.1.20:8095`; `ws` paths are derived |
| `music_assistant.token` **D** | `TRACKSIDE_MUSIC_ASSISTANT__TOKEN` / `_FILE` | unset | a long-lived Music Assistant token; secret, write-only in the dashboard, sealed at rest |
| `music_assistant.enabled` **D** | `TRACKSIDE_MUSIC_ASSISTANT__ENABLED` | `true` | use Music Assistant once an address and token are set |
| `music_assistant.auto_register` **D** | `TRACKSIDE_MUSIC_ASSISTANT__AUTO_REGISTER` | `true` | add Trackside to Music Assistant as an OpenSubsonic source and keep it there |
| `music_assistant.lan_url` **D** | `TRACKSIDE_MUSIC_ASSISTANT__LAN_URL` | the first LAN address | the address Music Assistant reaches Trackside at |
| `music_assistant.ca_file` **D** | `TRACKSIDE_MUSIC_ASSISTANT__CA_FILE` | unset | a PEM file of the CA (or self-signed certificate) Music Assistant's `https` uses; trusted instead of the system store |
| `music_assistant.hidden_services` **D** | `TRACKSIDE_MUSIC_ASSISTANT__HIDDEN_SERVICES` | empty | Music Assistant music provider instance ids not offered as streaming services |
| `music_assistant.relay_max_sessions` **D** | `TRACKSIDE_MUSIC_ASSISTANT__RELAY_MAX_SESSIONS` | `4` | phone relays at once, server-wide |

Speakers and streaming services through Music Assistant: see [music-assistant.md](music-assistant.md).
A key set here is read-only in the dashboard.

It is one key for the whole server, set by an admin. It powers suggestions for every user, and
nobody needs a Last.fm account or connects one. The server only calls Last.fm's public read
methods (`artist.getSimilar`, `track.getSimilar`, `artist.getTopTracks`, `artist.getInfo`) with
the key and artist and track names: it never signs requests, uses a session or shared secret,
scrobbles or writes anything, so the account that owns the key is never touched, and nothing
about your users is sent. Each user's suggestions are still their own: Last.fm's data is ranked
against that user's listens and stars.

Lookups run in the background, at most a few a second, for the artists and songs people play
first, and are kept for 30 days. Set in the dashboard, the key is stored sealed with the server's
secret key and never shown again; a key from this file or the environment shows there as locked.

This page follows the key table in `crates/trackside-core/src/config/loader.rs`. If
`trackside config show` and this page disagree, the server is right; please open an issue.

## Apple Music (dashboard only)

Tracks on Android can play from Apple Music. Apple only lets an app do that with a developer
token signed by a MusicKit key, and that key must stay private, so your server keeps it and signs
short-lived tokens for your apps. You need a paid Apple Developer Program membership.

1. Sign in at developer.apple.com, open Certificates, Identifiers and Profiles, then
   Identifiers, and register a Media ID (for example `media.com.example.trackside`).
2. Under Keys, add a key, tick Media Services (MusicKit), choose that Media ID, and register it.
3. Download the key. Apple names it `AuthKey_<KEY ID>.p8` and lets you download it only once,
   so keep a copy somewhere safe. Note the 10-character key ID shown beside the key.
4. Note your 10-character Team ID, shown under Membership details.
5. In the dashboard, open Settings, Apple Music, choose the `.p8` file (or paste its text), fill
   in the key ID and team ID, and save. It asks for your password again.

The key is stored sealed with the server's secret key and never shown again; the dashboard shows
only the key ID, team ID and when it was saved. Replace it to rotate it, or Remove it to stop
Apple Music in Tracks. Backups carry it sealed, so a restore needs the same `secret.key`
(`backup.include_secret_key`). If that file is lost, save the key again.

## Trackside Cloud (dashboard only)

The Tracks apps can sync their own data (listening history, sessions, stations, app settings,
blocked artists, playlists, likes) to your server instead of iCloud. Three dashboard settings
control it; they are not configuration keys:

| Setting | Default | Meaning |
|---|---|---|
| `cloud.enabled` | `true` | off makes every cloud route answer `404` and hides it from `capabilities` |
| `cloud.quota_items` | `2000000` | records per user, deletes included; new records past it are refused |
| `cloud.quota_bytes` | `"1GiB"` | bytes per user, the same way |

Each person sees only their own synced data. Admins see how many records and bytes a user keeps
on that user's page, never the records themselves, and the dashboard has no view of them.
