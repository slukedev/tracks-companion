# Troubleshooting

Start with the dashboard's **Overview**: it shows the version, free space, ffmpeg, each
library's state, scans, and recent errors. Then the log:

```sh
docker compose logs --tail 200 trackside
sudo journalctl -u trackside -n 200
```

More detail with `TRACKSIDE_LOG__LEVEL=debug` (or a filter such as
`info,trackside_scan=debug`). Logs never contain passwords, keys or tokens; you can paste them
into an issue.

`trackside check` verifies the database and configuration without starting the server, and
`trackside config show` prints every setting in effect, one `key = value  # source` line each,
with secrets hidden ([configuration.md](configuration.md)). `trackside config show | grep -v
'# default$'` shows only what you changed.

## The server won't start

| The log says something like | Cause | Fix |
|---|---|---|
| `another Trackside server is using <folder>` | another Trackside is using the same data folder | stop the other one; only one process per data folder |
| `the database ... is on a network filesystem` | the data folder is on NFS or SMB | move `/data` to a local disk (music can stay on the NAS), or set `database.allow_network_fs = true` if you accept the risk |
| `the database ... is from a newer Trackside` | you started an older release after a newer one | run the newer release, or restore a pre-migration backup from the folder it names ([upgrading.md](upgrading.md#downgrading)) |
| `permission denied` on `/data` | the folder isn't writable by the server's user | `sudo chown -R 65532:65532 <folder>` for the image, or set `user:` |
| `address already in use` | something else has port 4747 | change the published port, or `server.listen` |
| `database disk image is malformed` | the database is damaged | `trackside backup restore` the newest backup |

## No music shows up

- The library's folder is the path inside the container (`/music`), not on the host.
- **Libraries** shows each library's state and problems. "Unavailable" means the folder is
  missing or empty right now, often an unmounted NAS share.
- Permission problems are listed per folder. The server's user needs to read files and list
  folders ([libraries.md](libraries.md#permissions)).
- Files with unknown extensions are skipped; `scanner.extensions` lists what's read.
- A folder with `.trackside-ignore`, `.nomedia` or `.ndignore` in it is skipped.

## New files take a long time to appear

The watcher needs one inotify watch per folder. Large libraries can run out, and the server
then falls back to a quick scan every 30 minutes and says so on the overview. Raise the limit
on the host (not inside the container):

```sh
echo fs.inotify.max_user_watches=524288 | sudo tee /etc/sysctl.d/90-trackside.conf
sudo sysctl --system
```

NFS and SMB shares don't send change events at all; the periodic quick scan covers them.

## Tracks can't connect

- `curl http://<address>:4747/api/v1/ping` from another machine should return the server id.
  If it doesn't, it's the network or firewall, not Tracks.
- On iPhone, Tracks needs Local Network permission (Settings, Privacy & Security, Local
  Network) to reach a LAN address.
- Away from home, the LAN address can't work. Check which addresses the server knows under
  **Settings**, Addresses, and add yours under Other addresses
  ([remote-access.md](remote-access.md)).
- QR pairing carries only Docker's internal address with bridge networking. Add your LAN
  address in the dashboard or use host networking.

## Songs won't play, or stop on cellular

- The overview says whether ffmpeg was found. The `slim` image has none, so songs in formats
  Apple devices can't play (Opus, Vorbis, WavPack, APE) won't play in Tracks.
- "Library unavailable" errors mean the NAS holding that library is offline.
- Behind a proxy, check buffering and timeouts ([reverse-proxy.md](reverse-proxy.md)).

## New songs show 0 added on the first scan

Files modified in the last 10 seconds (`scanner.settle`) may still be copying, so a scan
leaves them for a follow-up pass 15 seconds later. A scan of a folder you just copied in can
therefore report 0 added, with a message saying how many files were still being written; they
appear a few seconds later without another scan.

## Gaps between songs (gapless playback)

AAC adds 1024 samples of encoder delay at the start and pads the last frame. Converted AAC
files (`aac-256`, `aac-128`) carry an MP4 edit list (`elst`) that trims both, the way Apple's
own encoder writes them; Trackside does not write the older `iTunSMPB` tag. To check a
converted file, download it with `?profile=aac-256` and look at its edit list:

```sh
curl -H "Authorization: Bearer $KEY" -o song.m4a \
  "http://<server>:4747/api/v1/songs/<id>/download?profile=aac-256"
ffprobe -v error -show_entries stream=start_time,duration -of default=nw=1 song.m4a
# or: MP4Box -info song.m4a, or mp4dump song.m4a | grep -A3 elst
```

The edit's media time is 1024 and its duration ends where the source does; `ffprobe` shows the
duration of the original, not a few milliseconds more. To check playback on a device:

- Play an album with continuous audio (a live album or a DJ mix) in Tracks over cellular or
  with a quality limit, so it plays converted files, and listen at each track change.
- Do it twice: the first play of a song may stream as HLS while the conversion runs, and HLS
  segments carry no edit list, so a short gap there is expected; the second play uses the
  cached file and should be seamless.
- Downloads for offline listening are always complete files with the edit list.
- Original files play as they are; gapless playback of MP3 originals depends on their LAME
  header, and FLAC and ALAC originals need nothing.

## The live updates don't arrive

Tracks gets changes through `/api/v1/events`. A proxy that buffers responses delays them until
the buffer fills. Turn buffering off for that path ([reverse-proxy.md](reverse-proxy.md)).

## Disk full

When the data disk fills, writes fail but playing keeps working. Caches shrink first. Free
space, then check backups weren't skipped. `cache/` can be deleted at any time while the server
is stopped.

## Running on a NAS

- Keep `/data` on the NAS's app or SSD storage. An array of spinning disks behind a share makes
  everything slower and may be refused as a network filesystem.
- Synology and QNAP run containers as the user you choose. Set `user:` to one that owns the data
  folder and can read the music share.
- A NAS that sleeps its disks wakes them for the quick scan every 30 minutes. Set
  `scanner.quick_interval` higher (for example `"6h"`) if that matters to you; the watcher
  still catches changes made through the NAS itself.

## Asking for help

Open a [discussion](https://github.com/slukedev/tracks-companion/discussions) or an issue with the
version (`trackside version`), how you installed it, `trackside config show`, and the log around
the problem.
